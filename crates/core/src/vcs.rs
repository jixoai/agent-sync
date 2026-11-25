//! 意图（2026-11-24 Owner）：用 jj 管状态，每台设备只写自己的分支，main 作合并基线。
//!   本文件是唯一与 jj / git 二进制打交道的地方：所有命令、所有 absolute path
//!   解析、所有输出解析都收敛在这里，上层只看到 Result。
//!
//! 本文件职责（4 个正交意图）：
//!   1. 定位二进制（launchd 与非登录 ssh 拿不到 brew 的 PATH，必须绝对路径）
//!   2. 版本门（jj ≥ 0.46，否则 bookmark 语义不一致会制造伪冲突）
//!   3. 提交与搬运（describe / bookmark set / new / fetch / push / rebase）
//!   4. 只读观测（status / diff / conflicts），一律 `--ignore-working-copy`
//! 不承载：远端配置内容（config 层）、传输路径（net 层）。

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// jj 的可执行文件位置。刻意不依赖 PATH：
/// launchd 起的进程与 `ssh host cmd` 都拿不到 `.zprofile` 里的 brew shellenv。
const JJ_CANDIDATES: &[&str] = &[
    "/opt/homebrew/bin/jj",
    "/usr/local/bin/jj",
    "/usr/bin/jj",
];

/// 基线版本：0.46 是 colocated + bookmark 语义的落地版。
const JJ_MIN_MINOR: u32 = 46;

#[derive(Debug, Clone)]
pub struct Jj {
    bin: PathBuf,
    /// 传给 git 的 ssh 命令。自带 known_hosts 与超时，
    /// **不依赖用户 ~/.ssh/config**——否则在干净 HOME / CI / 新机器上必然
    /// `Host key verification failed`（实测）。
    ssh_command: Option<String>,
}

/// 一条命令的执行结果，便于统一报错。
struct Output {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

impl std::fmt::Debug for Output {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Output")
            .field("code", &self.code)
            .field("stdout_len", &self.stdout.len())
            .field("stderr_len", &self.stderr.len())
            .finish()
    }
}

impl Jj {
    /// 定位 jj；缺失或过旧都返回可读的错误，调用方（doctor）直接展示。
    pub fn locate() -> Result<Self> {
        for cand in JJ_CANDIDATES {
            let p = Path::new(cand);
            if p.is_file() {
                return Ok(Self {
                    bin: p.to_path_buf(),
                    ssh_command: None,
                });
            }
        }
        anyhow::bail!(
            "未找到 jj（查找过 {}）。安装：brew install jj",
            JJ_CANDIDATES.join(", ")
        );
    }

    pub fn bin(&self) -> &Path {
        &self.bin
    }

    /// 指定 git 使用的 ssh 命令（含 known_hosts 与超时）。
    pub fn with_ssh_command(mut self, cmd: impl Into<String>) -> Self {
        self.ssh_command = Some(cmd.into());
        self
    }

    /// 版本门。低于 0.46 直接拒绝——bookmark 语义差异会静默制造伪冲突。
    pub fn ensure_supported(&self) -> Result<String> {
        let out = self.run(&["--version"], None)?;
        // jj 的 config 解析失败时 --version 也会空输出（退出码仍为 0）
        let ver = out
            .stdout
            .split_whitespace()
            .nth(1)
            .map(|s| s.to_string())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "jj 无版本输出（疑似 ~/.config/jj/config.toml 解析失败）：{}",
                    out.stderr.trim()
                )
            })?;
        let (major, minor) = parse_version(&ver)?;
        if major == 0 && minor < JJ_MIN_MINOR {
            anyhow::bail!("jj {ver} 过旧，需要 ≥ 0.46（bookmark 语义不一致会制造伪冲突）");
        }
        Ok(ver)
    }

    /// 执行 jj，并**强制超时**。
    ///
    /// 为什么必须超时：`jj git fetch/push` 走 ssh 隧道，对端在线但 sshd 不响应时
    /// 会无限挂起。实测一次挂起就把 daemon 主循环彻底卡死（之后再无任何同步），
    /// 表现为「对端掉线后本机同步永久停摆」。
    fn run(&self, args: &[&str], cwd: Option<&Path>) -> Result<Output> {
        let is_transfer = args.iter().any(|a| *a == "fetch" || *a == "push");
        // 传输类给 90s；其余 120s——首次同步要快照大量新文件，
        // 60s 会在正常但较慢的首次快照上误判超时（实测）。
        let timeout = if is_transfer {
            Duration::from_secs(90)
        } else {
            Duration::from_secs(120)
        };

        let mut cmd = Command::new(&self.bin);
        cmd.args(args);
        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }
        // 关掉分页器，否则无 tty 时会挂住
        cmd.env("JJ_PAGER", "cat");
        if let Some(ssh) = &self.ssh_command {
            cmd.env("GIT_SSH_COMMAND", ssh);
        }
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

        let mut child = cmd
            .spawn()
            .with_context(|| format!("启动 {} {}", self.bin.display(), args.join(" ")))?;
        let deadline = Instant::now() + timeout;
        loop {
            match child.try_wait()? {
                Some(_) => {
                    let out = child.wait_with_output()
                        .with_context(|| format!("收集 {} 输出", args.join(" ")))?;
                    return Ok(Output {
                        code: out.status.code(),
                        stdout: String::from_utf8_lossy(&out.stdout).to_string(),
                        stderr: String::from_utf8_lossy(&out.stderr).to_string(),
                    });
                }
                None => {
                    if Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        anyhow::bail!(
                            "jj {} 超时（{}s）：对端可能在线但 sshd 不响应，已强制终止",
                            args.join(" "),
                            timeout.as_secs()
                        );
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }
    }

    fn run_ok(&self, args: &[&str], cwd: &Path) -> Result<String> {
        let out = self.run(args, Some(cwd))?;
        if out.code != Some(0) {
            anyhow::bail!(
                "jj {} 失败（退出码 {:?}）：{}",
                args.first().unwrap_or(&""),
                out.code,
                out.stderr.trim()
            );
        }
        Ok(out.stdout)
    }

    /// 初始化 colocated 仓：同时是合法 git 仓，对端才能直接 fetch/push 进来。
    pub fn init_colocate(&self, dir: &Path) -> Result<()> {
        std::fs::create_dir_all(dir)?;
        // jj 默认拒绝快照 >1MiB 的新文件，而 skill 里常见 1.2MB 的 bundle。
        // 被拒的文件永远进不了提交，于是「树哈希说变了、提交里却没有」→ 无限循环。
        // 实测这是跨机不同步的根因。16MiB 足够覆盖现有 skill 资产。
        let _ = std::process::Command::new(&self.bin)
            .args(["config", "set", "--repo", "snapshot.max-new-file-size", "16777216"])
            .current_dir(dir)
            .output();
        if dir.join(".jj").exists() {
            return Ok(());
        }
        self.run_ok(&["git", "init", "--colocate"], dir)?;
        // 对端会直接 push 到本仓（非 bare）。默认拒绝，这里放行但**不更新工作副本**
        // （updateInstead 会改写工作区，与 align+apply 流程冲突）。
        let _ = Command::new("/usr/bin/git")
            .args(["config", "receive.denyCurrentBranch", "ignore"])
            .current_dir(dir)
            .output();
        Ok(())
    }

    /// 把当前工作副本描述成一条具名 change（jj 会自动快照文件，无需 git add）。
    pub fn describe(&self, dir: &Path, msg: &str) -> Result<()> {
        self.run_ok(&["describe", "-m", msg], dir)?;
        Ok(())
    }

    /// bookmark 没有「当前」概念，必须显式 set。
    pub fn bookmark_set(&self, dir: &Path, name: &str, rev: &str) -> Result<()> {
        self.run_ok(&["bookmark", "set", name, "-r", rev], dir)?;
        Ok(())
    }

    pub fn bookmark_exists(&self, dir: &Path, name: &str) -> bool {
        // 注意：`jj bookmark list <name>` 对不存在的 bookmark 返回 0 且输出空。
        // 必须看输出而非退出码，否则会漏建 main。
        self.run(&["bookmark", "list", name, "-T", "name"], Some(dir))
            .map(|o| !o.stdout.trim().is_empty())
            .unwrap_or(false)
    }

    /// 列出可用于合并的 revset（本机分支 + 各 peer 的 dev-*@remote）。
    ///
    /// 排除两类：`@git` 是 colocated 仓把 git refs 导入的镜像（与本地同名分支同提交，
    /// 一起当父会重复）；其它本地 `dev-*` 同样只是远端提交的镜像，真正的源头是 `@<remote>`。
    pub fn merge_revsets(&self, dir: &Path, self_id: &str) -> Result<Vec<String>> {
        let stdout = self.run_ok(
            &[
                "bookmark",
                "list",
                "--all-remotes",
                "-T",
                r#"name ++ if(remote, "@" ++ remote, "") ++ "\n""#,
            ],
            dir,
        )?;
        let self_bookmark = format!("dev-{self_id}");
        let mut out = Vec::new();
        for line in stdout.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if line.ends_with("@git") {
                continue;
            }
            if line == self_bookmark {
                out.push(line.to_string());
            } else if line.starts_with("dev-") && line.contains('@') {
                out.push(line.to_string());
            }
        }
        out.sort();
        out.dedup();
        Ok(out)
    }

    /// 建一个新提交，父为给定 revset。`jj new` 的语义就是「在其上开一个新工作副本」。
    pub fn new_commit(&self, dir: &Path, revs: &[String], msg: &str) -> Result<()> {
        let mut args: Vec<&str> = vec!["new"];
        for r in revs {
            args.push(r);
        }
        args.push("-m");
        args.push(msg);
        self.run_ok(&args, dir)?;
        Ok(())
    }

    /// 纯空提交（为下一轮改动准备 @）。
    pub fn new_empty(&self, dir: &Path, parent: &str) -> Result<()> {
        self.run_ok(&["new", parent], dir)?;
        Ok(())
    }

    pub fn fetch(&self, dir: &Path, remote: &str) -> Result<()> {
        self.run_ok(&["git", "fetch", "--remote", remote], dir)?;
        Ok(())
    }

    /// 只推本机分支，绝不推 main（main 是本地基线，多机各算各的）。
    pub fn push_bookmark(&self, dir: &Path, remote: &str, bookmark: &str) -> Result<()> {
        self.run_ok(
            &["git", "push", "--remote", remote, "--bookmark", bookmark],
            dir,
        )?;
        Ok(())
    }

    pub fn rebase_onto(&self, dir: &Path, branch: &str, onto: &str) -> Result<()> {
        self.run_ok(&["rebase", "-b", branch, "-d", onto], dir)?;
        Ok(())
    }

    /// 冲突清单。jj 把冲突物化进树而不阻塞，所以必须显式检查后才允许落盘。
    pub fn conflicts(&self, dir: &Path) -> Result<Vec<String>> {
        let out = self.run(&["resolve", "--list"], Some(dir))?;
        Ok(out
            .stdout
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect())
    }

    /// 是否有未落账改动。
    ///
    /// 刻意**不带** `--ignore-working-copy`：那个 flag 会阻止 jj 快照工作副本，
    /// 于是新建/修改的文件永远不会出现在 diff 里，检测恒为 false。
    /// （`--ignore-working-copy` 只适合「只想看状态、不想产生快照」的巡检，
    /// 例如 conflicts / merge_revsets。）
    pub fn has_changes(&self, dir: &Path) -> Result<bool> {
        let out = self.run(&["diff", "--summary"], Some(dir))?;
        if !out.stdout.trim().is_empty() {
            return Ok(true);
        }
        let status = self.run(&["status"], Some(dir))?;
        Ok(status.stdout.contains("Working copy changes"))
    }

    /// 把工作副本的指定路径恢复成某提交的内容。
    pub fn restore_from(&self, dir: &Path, rev: &str, paths: &[&str]) -> Result<()> {
        let mut args: Vec<&str> = vec!["restore", "--from", rev, "--"];
        args.extend_from_slice(paths);
        self.run_ok(&args, dir)?;
        Ok(())
    }

    /// 只读查询：某 revision 是否存在。
    pub fn rev_exists(&self, dir: &Path, rev: &str) -> bool {
        self.run(&["log", "-r", rev, "--no-graph", "-T", "commit_id"], Some(dir))
            .map(|o| o.code == Some(0))
            .unwrap_or(false)
    }

    /// 只读查询：`rev` 是否已被 `in_rev` 包含（祖先或相同）。
    ///
    /// 用途：align 前判断「还值得造 merge 提交吗」。不判断的话每次同步都会
    /// 生成一个空 merge，@ 不断变化、历史无限膨胀（实测每 20 秒一个 align 提交）。
    pub fn is_contained(&self, dir: &Path, rev: &str, in_rev: &str) -> bool {
        // revset：rev 中不属于 in_rev 祖先集的提交。为空即已包含。
        let expr = format!("{rev} & ~::{in_rev}");
        self.run(
            &["log", "-r", &expr, "--no-graph", "-T", "commit_id.short()"],
            Some(dir),
        )
        .map(|o| o.code == Some(0) && o.stdout.trim().is_empty())
        .unwrap_or(false)
    }
}

fn parse_version(v: &str) -> Result<(u32, u32)> {
    let core = v.split('-').next().unwrap_or(v);
    let mut it = core.split('.');
    let major: u32 = it.next().unwrap_or("0").parse().unwrap_or(0);
    let minor: u32 = it.next().unwrap_or("0").parse().unwrap_or(0);
    Ok((major, minor))
}

/// git 侧只做少量 jj 不管的事（remote 配置、receive 策略）。
#[derive(Debug, Clone)]
pub struct Git {
    bin: PathBuf,
}

impl Git {
    pub fn locate() -> Self {
        for cand in ["/usr/bin/git", "/opt/homebrew/bin/git"] {
            if Path::new(cand).is_file() {
                return Self {
                    bin: PathBuf::from(cand),
                };
            }
        }
        Self {
            bin: PathBuf::from("git"),
        }
    }

    fn run(&self, args: &[&str], cwd: &Path) -> Result<Output> {
        let out = Command::new(&self.bin)
            .args(args)
            .current_dir(cwd)
            .output()
            .with_context(|| format!("执行 git {}", args.join(" ")))?;
        Ok(Output {
            code: out.status.code(),
            stdout: String::from_utf8_lossy(&out.stdout).to_string(),
            stderr: String::from_utf8_lossy(&out.stderr).to_string(),
        })
    }

    pub fn remote_url(&self, dir: &Path, name: &str) -> Option<String> {
        self.run(&["remote", "get-url", name], dir)
            .ok()
            .filter(|o| o.code == Some(0))
            .map(|o| o.stdout.trim().to_string())
            .filter(|s| !s.is_empty())
    }

    /// 设置 remote：不存在则新增；已存在且是 ssh 形式则纠正端口/路径，
    /// 测试或手工覆盖的 file:// 路径不动（那是有意为之）。
    pub fn remote_ensure(&self, dir: &Path, name: &str, url: &str) -> Result<()> {
        match self.remote_url(dir, name) {
            None => {
                self.run(&["remote", "add", name, url], dir)
                    .map_err(|e| anyhow::anyhow!("git remote add {name}: {e}"))?;
            }
            Some(cur) if cur == url => {}
            Some(cur) if cur.starts_with("ssh://") || cur.contains("@127.0.0.1:") => {
                self.run(&["remote", "set-url", name, url], dir)?;
            }
            Some(_) => {}
        }
        Ok(())
    }

    pub fn allow_push_into_worktree(&self, dir: &Path) -> Result<()> {
        self.run(&["config", "receive.denyCurrentBranch", "ignore"], dir)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jj() -> Jj {
        Jj::locate().expect("测试环境需要 jj")
    }

    fn init_repo(dir: &Path) {
        jj().init_colocate(dir).unwrap();
        // jj 需要 author 身份。写进仓库级配置而不是环境变量：
        // edition 2024 里 set_var 是 unsafe，且测试并行时会互相污染。
        for (k, v) in [("user.name", "t"), ("user.email", "t@t")] {
            let out = std::process::Command::new(jj().bin())
                .args(["config", "set", "--repo", k, v])
                .current_dir(dir)
                .output()
                .unwrap();
            assert!(out.status.success(), "设置 {k} 失败: {:?}", out);
        }
    }

    #[test]
    fn locate_and_version_gate() {
        let j = jj();
        let v = j.ensure_supported().unwrap();
        assert!(!v.is_empty());
        let (major, minor) = parse_version(&v).unwrap();
        assert!(major >= 1 || minor >= 46, "需要 ≥0.46，实际 {v}");
    }

    #[test]
    fn bookmark_list_output_not_exit_code() {
        let d = tempfile::tempdir().unwrap();
        init_repo(d.path());
        // 关键回归：`bookmark list <不存在>` 退出码为 0 但输出空。
        assert!(!jj().bookmark_exists(d.path(), "main"));
        jj().bookmark_set(d.path(), "dev-a", "@").unwrap();
        assert!(jj().bookmark_exists(d.path(), "dev-a"));
    }

    #[test]
    fn merge_revsets_excludes_git_mirror_and_other_local_branches() {
        let d = tempfile::tempdir().unwrap();
        init_repo(d.path());
        jj().bookmark_set(d.path(), "dev-a", "@").unwrap();
        jj().bookmark_set(d.path(), "dev-other", "@").unwrap();
        let revs = jj().merge_revsets(d.path(), "a").unwrap();
        assert!(revs.contains(&"dev-a".to_string()));
        assert!(!revs.iter().any(|r| r.ends_with("@git")), "必须排除 @git 镜像");
        assert!(
            !revs.contains(&"dev-other".to_string()),
            "其它本地 dev-* 只是镜像，不应作为父"
        );
    }

    #[test]
    fn describe_and_has_changes() {
        let d = tempfile::tempdir().unwrap();
        init_repo(d.path());
        assert!(!jj().has_changes(d.path()).unwrap());
        std::fs::write(d.path().join("f.md"), "hello").unwrap();
        assert!(jj().has_changes(d.path()).unwrap());
        jj().describe(d.path(), "first").unwrap();
        jj().bookmark_set(d.path(), "dev-a", "@").unwrap();
        jj().new_empty(d.path(), "@").unwrap();
    }

    #[test]
    fn conflicts_detected_after_merge() {
        // 同一文件两种内容做多父合并，必然冲突
        let d = tempfile::tempdir().unwrap();
        init_repo(d.path());
        std::fs::write(d.path().join("x.md"), "base\n").unwrap();
        jj().describe(d.path(), "base").unwrap();
        jj().bookmark_set(d.path(), "base", "@").unwrap();
        jj().new_empty(d.path(), "base").unwrap();

        std::fs::write(d.path().join("x.md"), "from-a\n").unwrap();
        jj().describe(d.path(), "a").unwrap();
        jj().bookmark_set(d.path(), "dev-a", "@").unwrap();

        jj().new_commit(d.path(), &["base".to_string()], "tmp").unwrap();
        std::fs::write(d.path().join("x.md"), "from-b\n").unwrap();
        jj().describe(d.path(), "b").unwrap();
        jj().bookmark_set(d.path(), "dev-b", "@").unwrap();

        let revs = vec!["dev-a".to_string(), "dev-b".to_string()];
        jj().new_commit(d.path(), &revs, "align").unwrap();
        let conflicts = jj().conflicts(d.path()).unwrap();
        assert!(!conflicts.is_empty(), "应当检出冲突");
        assert!(conflicts.iter().any(|c| c.contains("x.md")));
    }

    #[test]
    fn git_remote_ensure_is_idempotent() {
        let d = tempfile::tempdir().unwrap();
        init_repo(d.path());
        let git = Git::locate();
        let url = "ssh://kzf@127.0.0.1:22302/Users/kzf/.agents/sync";
        git.remote_ensure(d.path(), "imac", url).unwrap();
        assert_eq!(git.remote_url(d.path(), "imac").unwrap(), url);
        // 再设一次不报错、不改值
        git.remote_ensure(d.path(), "imac", url).unwrap();
        assert_eq!(git.remote_url(d.path(), "imac").unwrap(), url);
        // 端口变了要纠正
        let url2 = "ssh://kzf@127.0.0.1:22399/Users/kzf/.agents/sync";
        git.remote_ensure(d.path(), "imac", url2).unwrap();
        assert_eq!(git.remote_url(d.path(), "imac").unwrap(), url2);
    }
}
