//! 意图（2026-11-24 Owner 原始需求）：
//!   「我局域网有一台 iMac，希望和那台 iMac 的 ~/zcode 里面的提示词、skills、插件、指令
//!    以及 ~/.agents 保持一致。有些用 npx skills 管理就保持一致的管理方法，有些是私有的
//!    就试着同步。未来可能引入更多设备。gaubee-cloud 只做组网；用 jj 管状态，每台设备
//!    有自己的分支，main 用来合并对齐。」
//!   后续决议：改成 Rust；用 nsh 做原生菜单栏应用；最终只提供一个 CLI，
//!   由它按 CLI 权限在操作系统中注入应用（参考 reply9842 的运行时 bundle 注入）。
//!
//! 本文件职责（4 个正交意图）：
//!   1. 路径常量：数据仓、payload、配置、状态、日志，全部集中在 SyncPaths
//!   2. 目录创建与权限（secrets 必须 0700）
//!   3. 设备 id 推导（主机名规范化，供 dev-<id> bookmark 与锁命名）
//!   4. 路径是否落在受管子树内的判定（capture/apply 的安全前提）
//! 不承载：任何跨目录读写逻辑（那是 mirror 的职责）。

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// 同步系统的全部磁盘位置。真实路径只在这里推导一次，其余模块只读。
#[derive(Debug, Clone)]
pub struct SyncPaths {
    /// 用户主目录。
    pub home: PathBuf,
    /// 数据仓根：`~/.agents/sync`（本身是 colocated jj 仓）。
    pub root: PathBuf,
}

impl SyncPaths {
    pub fn new(home: impl Into<PathBuf>) -> Self {
        let home = home.into();
        let root = home.join(".agents").join("sync");
        Self { home, root }
    }

    /// 从 $HOME 推导；HOME 缺失时报错而不是猜测（猜错会写进别人的目录）。
    pub fn from_env() -> Result<Self> {
        let home = std::env::var_os("HOME").context("HOME 未设置，拒绝猜测主目录")?;
        Ok(Self::new(PathBuf::from(home)))
    }

    pub fn payload(&self) -> PathBuf {
        self.root.join("payload")
    }
    pub fn config_dir(&self) -> PathBuf {
        self.root.join("config")
    }
    pub fn state_dir(&self) -> PathBuf {
        self.root.join(".state")
    }
    pub fn secrets_dir(&self) -> PathBuf {
        self.root.join(".secrets")
    }
    pub fn logs_dir(&self) -> PathBuf {
        self.root.join("logs")
    }
    pub fn locks_dir(&self) -> PathBuf {
        self.root.join("locks")
    }
    pub fn backups_dir(&self) -> PathBuf {
        self.root.join("backups")
    }
    pub fn devices_json(&self) -> PathBuf {
        self.config_dir().join("devices.json")
    }
    pub fn sync_conf(&self) -> PathBuf {
        self.config_dir().join("sync.conf")
    }
    /// 软链清单属于**本机观察**（每台机器的农场各不相同），放 .state/。
    /// 若放进 payload，两台机器各自生成的内容必然 2-sided 冲突（实测）。
    pub fn links_json(&self) -> PathBuf {
        self.state_dir().join("links.json")
    }
    pub fn capture_state(&self) -> PathBuf {
        self.state_dir().join("capture.json")
    }

    /// 活目录：payload 三个子树的来源，也是 apply 的落点。
    pub fn live_agents(&self) -> PathBuf {
        self.home.join(".agents")
    }
    pub fn live_zcode(&self) -> PathBuf {
        self.home.join(".zcode")
    }
    pub fn live_codex(&self) -> PathBuf {
        self.home.join(".codex")
    }

    /// 建立运行期目录。secrets 单独收紧到 0700——它存 iroh 身份与同步私钥。
    pub fn ensure_runtime_dirs(&self) -> Result<()> {
        for d in [
            self.logs_dir(),
            self.locks_dir(),
            self.backups_dir(),
            self.state_dir(),
        ] {
            std::fs::create_dir_all(&d).with_context(|| format!("创建 {}", d.display()))?;
        }
        let secrets = self.secrets_dir();
        std::fs::create_dir_all(&secrets)
            .with_context(|| format!("创建 {}", secrets.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&secrets, std::fs::Permissions::from_mode(0o700))
                .with_context(|| format!("收紧 {} 权限", secrets.display()))?;
        }
        Ok(())
    }

    /// 本仓库自身所在位置必须被排除：它就在 `~/.agents/sync` 里，
    /// 若不排除，capture 会把仓库复制进自己的 payload（无限增长），
    /// apply --prune 则会把工具本身删掉。
    pub fn is_self(&self, path: &Path) -> bool {
        let root = self.root.as_path();
        path == root || path.starts_with(root)
    }
}

/// 设备 id：主机名短写规范化。用于 `dev-<id>` bookmark、锁名、devices 注册表。
/// 只保留 `a-z0-9-`，因为 bookmark 名与文件路径都要安全。
pub fn device_id_from_hostname(host: &str) -> String {
    let short = host.split('.').next().unwrap_or(host);
    let mut out = String::with_capacity(short.len());
    for ch in short.chars() {
        let c = ch.to_ascii_lowercase();
        if c.is_ascii_alphanumeric() || c == '-' {
            out.push(c);
        }
    }
    if out.is_empty() {
        // 极端情况（全非 ASCII 主机名）退化为固定前缀，避免生成空 bookmark。
        "device".to_string()
    } else {
        out
    }
}

/// 取本机设备 id。
///
/// 优先 `AGENTS_SYNC_DEVICE_ID`：同机跑多实例（测试、双身份调试）时必须能区分，
/// 否则两者都会把自己和对端当成同一个 id，导致「ssh 连自己」这类荒谬失败。
pub fn local_device_id() -> Result<String> {
    if let Ok(id) = std::env::var("AGENTS_SYNC_DEVICE_ID") {
        let id = id.trim().to_string();
        if !id.is_empty() {
            return Ok(id);
        }
    }
    // hostname(1) 在 macOS/BSD 上稳定；避免引入 libc 依赖只为读一个名字。
    let out = std::process::Command::new("hostname")
        .arg("-s")
        .output()
        .context("执行 hostname -s")?;
    let host = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if host.is_empty() {
        anyhow::bail!("hostname -s 无输出");
    }
    Ok(device_id_from_hostname(&host))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_id_normalizes() {
        assert_eq!(device_id_from_hostname("bngjdeMac-mini-7.local"), "bngjdemac-mini-7");
        assert_eq!(device_id_from_hostname("Mac mini"), "macmini");
        assert_eq!(device_id_from_hostname("***"), "device");
    }

    #[test]
    fn self_detection_covers_root_and_children() {
        let p = SyncPaths::new("/Users/x");
        assert!(p.is_self(Path::new("/Users/x/.agents/sync")));
        assert!(p.is_self(Path::new("/Users/x/.agents/sync/payload/agents")));
        assert!(!p.is_self(Path::new("/Users/x/.agents/skills")));
    }
}
