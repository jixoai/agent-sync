//! 意图（2026-11-24）：把镜像层与状态层编成三条可组合的动作。
//!   publish = 本机改动落账并推给对端；pull = 拉取对齐并落盘；sync = 两者串起来。
//!   顺序不可交换：必须先 publish 再 pull，否则本轮改动会和 align 结果抢工作副本。
//!
//! 本文件职责（4 个正交意图）：
//!   1. publish：capture → describe/bookmark → 推各 peer → 广播唤醒
//!   2. pull：fetch → align → 冲突门 → 恢复 payload → apply
//!   3. 远端登记（remote 名 = 设备 id，URL 用显式 127.0.0.1:端口）
//!   4. 结果汇总（供 CLI/UI/日志统一展示）
//! 不承载：传输实现（net 层）、文件搬运细节（mirror 层）。

use crate::config::{Device, DeviceRegistry, SyncConf};
use crate::mirror::{apply, capture};
use crate::paths::SyncPaths;
use crate::vcs::{Git, Jj};
use anyhow::Result;

/// 广播通道。由 CLI/daemon 注入具体实现（iroh），core 不依赖网络。
pub trait Broadcaster {
    /// 叫醒一台对端来拉取。失败不致命：下一轮 align 会补齐。
    fn wake(&self, device: &Device) -> Result<()>;

    fn wake_all(&self, devices: &[Device]) {
        for d in devices {
            if let Err(e) = self.wake(d) {
                log::warn!("唤醒 {} 失败：{e}", d.id);
            }
        }
    }
}

/// 什么都不做的广播器：单机模式与测试用。
pub struct NoopBroadcaster;
impl Broadcaster for NoopBroadcaster {
    fn wake(&self, _device: &Device) -> Result<()> {
        Ok(())
    }
}

#[derive(Debug, Default)]
pub struct PublishReport {
    pub captured: usize,
    pub committed: bool,
}

#[derive(Debug, Default)]
pub struct PullReport {
    pub fetched: Vec<String>,
    pub failed_fetch: Vec<(String, String)>,
    pub aligned: bool,
    pub applied: Option<apply::ApplyReport>,
    pub conflicts: Vec<String>,
}

/// 同步引擎：所有动作的入口。字段全是不可变配置，方法内部自取锁。
pub struct SyncEngine {
    pub paths: SyncPaths,
    pub jj: Jj,
    pub git: Git,
    pub device_id: String,
    pub registry: DeviceRegistry,
    pub conf: SyncConf,
}

impl SyncEngine {
    pub fn new(paths: SyncPaths) -> Result<Self> {
        let device_id = crate::paths::local_device_id()?;
        Self::with_device_id(paths, device_id)
    }

    /// 显式指定设备 id。测试与 `init --device` 用；
    /// 生产路径走 `new()`（从主机名推导），避免两台机器意外同 id。
    pub fn with_device_id(paths: SyncPaths, device_id: impl Into<String>) -> Result<Self> {
        let jj = Jj::locate()?.with_ssh_command(ssh_command_for(&paths));
        jj.ensure_supported()?;
        let device_id = device_id.into();
        // 唯一配置来源是 config.toml 的 [sync]（+ 本机 private 的 config.local.toml）。
        // 遗留的 config/sync.conf 只保留读取兼容，不再作为真值。
        // 缺文件在这里补齐：daemon 启动/UI 打开时 config.toml 就应当可编辑。
        if let Err(e) = bootstrap_data_repo(&paths) {
            log::debug!("基础设施文件补齐失败（不影响只读命令）：{e}");
        }
        let settings = crate::settings::Settings::load(&paths.root).unwrap_or_default();
        let conf = SyncConf::from_settings(&settings.sync);
        let registry = DeviceRegistry::load(&paths.devices_json())?;
        Ok(Self {
            git: Git::locate(),
            paths,
            jj,
            device_id,
            registry,
            conf,
        })
    }

    fn peers(&self) -> Vec<Device> {
        self.registry.peers(&self.device_id).cloned().collect()
    }

    /// 远端名 = 设备 id，URL 用显式 `127.0.0.1:端口`。
    ///
    /// 为什么不用主机别名：本机 DNS 存在 Fake-IP（保留域也会被解析进 198.18.0.0/15
    /// 且任意端口可达），任何走 DNS 的名字都会让 ssh 连上后卡在协议握手。
    pub fn ensure_remotes(&self) -> Result<()> {
        for peer in self.peers() {
            // 没有隧道端口的 peer 走的是普通 git 远端的局域网/直连路径（首次接机常见），
            // 不该因此让整轮同步失败。
            let Ok(port) = peer.port() else {
                log::debug!("peer {} 无隧道端口，沿用其 git remote", peer.id);
                continue;
            };
            let user = if peer.ssh_user.is_empty() {
                whoami()
            } else {
                peer.ssh_user.clone()
            };
            let repo = peer.repo_path_or_default(&self.paths.home);
            let url = format!("ssh://{user}@127.0.0.1:{port}{repo}");
            self.git.remote_ensure(&self.paths.root, &peer.id, &url)?;
        }
        Ok(())
    }

    /// 本机改动落账并推送。广播由调用方注入（core 不碰网络）。
    pub fn publish(&self, broadcaster: &dyn Broadcaster) -> Result<PublishReport> {
        let mut report = PublishReport::default();

        let cap = capture::capture_all(&self.paths, &self.conf)?;
        report.captured = cap.files_copied;

        // 用 payload 树哈希判断改动，而不是 `jj diff`：
        // 后者要触发工作副本快照，在 daemon 环境下实测会偶发卡满超时；
        // 而树哈希是我们自己的判据，和「要提交什么」严格一致。
        let payload_hash = crate::checks::tree_hash(&self.paths.payload())?;
        let state_file = self.paths.state_dir().join("last_committed_hash");
        let last_hash = std::fs::read_to_string(&state_file)
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        let changed = payload_hash != last_hash;
        log::info!(
            "publish：采集 {} 个文件，工作副本有改动={changed}",
            cap.files_copied
        );
        if changed {
            self.jj
                .describe(&self.paths.root, &format!("sync({}): {}", self.device_id, now()))?;
            self.jj
                .bookmark_set(&self.paths.root, &format!("dev-{}", self.device_id), "@")?;
            self.jj.new_empty(&self.paths.root, "@")?;
            report.committed = true;
            let _ = std::fs::create_dir_all(self.paths.state_dir());
            let _ = std::fs::write(&state_file, format!("{payload_hash}\n"));
            log::info!("publish：已提交并更新 dev-{}", self.device_id);
        }

        // 刻意**不 push**。
        //
        // 原因（实测）：把分支推进对端的工作仓，jj 会把 git ref 导入本地
        // bookmark，于是对端的 dev-<我> 与我的 dev-<我> 变成两条不同演化，
        // 下一次 push 报 `stale info` / non-fast-forward。
        // 在「无中心真值」模型里，每台设备是自己的权威：对端**来 fetch** 即可，
        // 我从不写别人的仓。push 的唯一作用（让对端知道）由广播承担。
        self.ensure_remotes()?;
        let peers = self.peers();
        broadcaster.wake_all(&peers);
        Ok(report)
    }

    /// 拉取对齐并落盘。有冲突时**拒绝**写活目录。
    pub fn pull(&self, opts: &apply::ApplyOptions) -> Result<PullReport> {
        let mut report = PullReport::default();
        self.ensure_remotes()?;

        let peers = self.peers();
        for peer in &peers {
            match self.jj.fetch(&self.paths.root, &peer.id) {
                Ok(_) => report.fetched.push(peer.id.clone()),
                Err(e) => {
                    // 单点不可达不致命：广播本来就可能丢失，靠下一轮补齐。
                    log::warn!("fetch {} 失败：{e}", peer.id);
                    report.failed_fetch.push((peer.id.clone(), e.to_string()));
                }
            }
        }

        self.align()?;
        report.aligned = true;

        let conflicts = self.jj.conflicts(&self.paths.root)?;
        if !conflicts.is_empty() {
            report.conflicts = conflicts;
            anyhow::bail!(
                "存在未解决冲突，拒绝落盘（避免把冲突标记写进活目录）：\n  {}",
                report.conflicts.join("\n  ")
            );
        }

        self.jj
            .restore_from(&self.paths.root, "main", &["payload/"])?;
        report.applied = Some(apply::apply_all(&self.paths, opts)?);
        Ok(report)
    }

    /// 对齐：所有合并源（本机 dev-self + 各 dev-*@remote）多父合并成新 main，
    /// 再把本机分支 rebase 上去。
    ///
    /// 为什么用 merge 而非线性 rebase：`main` 永不推远端，没有 CI/保护规则约束；
    /// merge 避免每轮都重写他机分支的哈希。
    pub fn align(&self) -> Result<()> {
        let dir = &self.paths.root;
        let mut revs = self.jj.merge_revsets(dir, &self.device_id)?;
        if revs.is_empty() {
            return Ok(());
        }
        let self_bookmark = format!("dev-{}", self.device_id);

        if self.jj.bookmark_exists(dir, "main") {
            // 已经全部被 main 包含时不要造 merge 提交。
            //
            // 否则每次同步都会 `jj new` 一个空 merge，@ 不断变化、历史无限膨胀。
            // 实测两端互相广播时每 20 秒生成一个 "align:" 空提交。
            if revs.iter().all(|r| self.jj.is_contained(dir, r, "main")) {
                // 本机领先 main（本机有新提交而 main 还没跟上）时快进 main。
                if self.jj.bookmark_exists(dir, &self_bookmark)
                    && !self.jj.is_contained(dir, &self_bookmark, "main")
                {
                    self.jj.bookmark_set(dir, "main", &self_bookmark)?;
                }
                return Ok(());
            }
            revs.insert(0, "main".to_string());
        }
        self.jj.new_commit(dir, &revs, &format!("align: {}", now()))?;
        self.jj.bookmark_set(dir, "main", "@")?;
        self.jj.new_empty(dir, "main")?;

        // 本机分支跟随 main。
        //
        // 关键：这里**不能** rebase。main 已经包含 dev-self 的全部提交，
        // rebase 会重写已经推送过的 commit id，对端下次 fetch 就看到
        // non-fast-forward（实测报 "Failed to push some bookmarks"）。
        // 直接把 dev-self 指到 main 是快进，推送永远单调前进。
        //
        // 但必须**确认 main 真的包含它**再重置：merge_revsets 读取书签与
        // 这里重置之间有竞态（用户手动 jj 提交、另一进程写入）。无条件重置
        // 会把刚落下的提交直接丢掉——实测手动删除的文件被这样「复活」过。
        if self.jj.bookmark_exists(dir, &self_bookmark)
            && self.jj.is_contained(dir, &self_bookmark, "main")
        {
            self.jj.bookmark_set(dir, &self_bookmark, "main")?;
        } else if self.jj.bookmark_exists(dir, &self_bookmark) {
            log::warn!(
                "{} 有未并入 main 的提交，跳过跟随（下一轮 align 会再合）",
                self_bookmark
            );
        }
        Ok(())
    }

    /// 首次初始化：建仓、把本机现状收进 dev-<self>，并用它建立 main 基线。
    /// 没有 main 基线时首次 align 会退化成无祖先的 2-sided merge（必然冲突）。
    pub fn init(&self) -> Result<()> {
        self.paths.ensure_runtime_dirs()?;
        self.jj.init_colocate(&self.paths.root)?;
        bootstrap_data_repo(&self.paths)?;
        let cap = capture::capture_all(&self.paths, &self.conf)?;
        log::info!("采集完成，树哈希 {}", cap.tree_hash);

        self.jj
            .describe(&self.paths.root, &format!("init({}): {}", self.device_id, now()))?;
        let bookmark = format!("dev-{}", self.device_id);
        self.jj.bookmark_set(&self.paths.root, &bookmark, "@")?;
        if !self.jj.bookmark_exists(&self.paths.root, "main") {
            self.jj.bookmark_set(&self.paths.root, "main", &bookmark)?;
        }
        self.jj.new_empty(&self.paths.root, "@")?;
        Ok(())
    }
}

/// 数据仓库自己的基础设施文件。
///
/// 为什么必须由 init 创建并提交：Rust 版里 `~/.agents/sync` 是 init 新建的目录，
/// 不再像 bash 版那样自带 config/.gitignore。缺了它们，被 clone 过来的新设备
/// 就没有 config/（devices.json 写不进去）、也没有 gitignore（logs/locks 会被跟踪）。

/// 确保 `.gitignore` 含指定行；缺了就补在末尾（幂等）。
fn ensure_ignore_lines(path: &std::path::Path, lines: &[&str]) -> Result<()> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let present: std::collections::HashSet<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect();
    let missing: Vec<&str> = lines
        .iter()
        .copied()
        .filter(|l| !present.contains(l))
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    let mut out = text;
    if !out.ends_with('\n') && !out.is_empty() {
        out.push('\n');
    }
    out.push_str("# 本机配置文件（自愈补齐）：入库会让两端改动在 merge 里互斗\n");
    for line in missing {
        out.push_str(line);
        out.push('\n');
    }
    std::fs::write(path, out)?;
    log::info!("已补齐 .gitignore 的本机配置文件规则");
    Ok(())
}

fn bootstrap_data_repo(paths: &SyncPaths) -> Result<()> {
    std::fs::create_dir_all(paths.config_dir())?;

    // 运行态与本地态不入库。这些目录每台机器各写各的，进仓库只会制造冲突。
    let gitignore = "logs/\nlocks/\nbackups/\n.state/\n.secrets/\nconfig/devices.json\nconfig.toml\nconfig.local.toml\n.jj/\n**/node_modules/\n**/.DS_Store\n";
    let p = paths.root.join(".gitignore");
    if !p.exists() {
        std::fs::write(&p, gitignore)?;
    }
    // 自愈：这些行一旦丢失（merge 回退、手改），config.toml 会变成被跟踪文件，
    // 随后 align 的 checkout 可能把它删掉——实测 config.local.toml 就这么丢的。
    ensure_ignore_lines(&p, &["config.toml", "config.local.toml"])?;

    // payload 必须字节级一致：关闭换行转换
    let attrs = "payload/** -text\n";
    let p = paths.root.join(".gitattributes");
    if !p.exists() {
        std::fs::write(&p, attrs)?;
    }

    // 唯一共享配置：config.toml。刻意带注释——这是用户会手改、UI 也会改的文件。
    let p = paths.root.join("config.toml");
    if !p.exists() {
        std::fs::write(&p, crate::settings::CONFIG_TEMPLATE)?;
    }
    Ok(())
}

/// 工具自带的 ssh 选项：专属 known_hosts、非交互、有限超时。
///
/// 为什么要自带：默认 ssh 会读用户的 `~/.ssh/config`，而同步工具可能以
/// 干净 HOME（测试、新机器、launchd）运行，此时 `Host key verification failed`
/// 会让 fetch 直接失败。传输是我们自己的责任，不该继承用户环境的偶然状态。
fn ssh_command_for(paths: &SyncPaths) -> String {
    let known = paths.state_dir().join("known_hosts");
    let _ = std::fs::create_dir_all(paths.state_dir());
    format!(
        "ssh -o BatchMode=yes -o StrictHostKeyChecking=accept-new -o UserKnownHostsFile={} -o ConnectTimeout=10 -o ServerAliveInterval=15",
        known.display()
    )
}

fn whoami() -> String {
    std::env::var("USER").unwrap_or_else(|_| "unknown".into())
}

/// 精简时间戳（UTC），只用于提交描述。
fn now() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = secs / 86_400;
    let rem = secs % 86_400;
    let z = days as i64 + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}
