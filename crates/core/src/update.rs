//! 自更新：从 npm registry 取最新版，校验后替换 CLI 与 `.app`，再重启 launchd 单元。
//!
//! 为什么走 npm 而不是 GitHub Releases：发布链路已经由 npm trusted publishing
//! 保证（GitHub OIDC + provenance），多一个渠道就多一套签名与校验要维护。
//! 这里只依赖系统自带工具（curl / shasum / tar），不引入 HTTP 与解压依赖——
//! 自更新本身必须尽量少地依赖「自己」。
//!
//! 安全顺序（任何一步失败都不动现有安装）：
//!   1. 取 `latest` 元数据，版本必须严格更新
//!   2. 下载 tarball，校验 registry 给出的 sha1
//!   3. 解包并校验包内版本号与二进制存在
//!   4. 先备份旧 CLI（`agents-sync.old`），再原子 rename 换新
//!   5. `.app` 用「拷贝到临时目录 → 交换」的方式，避免半份 bundle
//!   6. 最后才 kickstart，让新二进制接管

use crate::lock::DirLock;
use crate::paths::SyncPaths;
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

/// npm 包名。改它等于换发布通道，必须与 workflow 和 trusted publisher 同步改。
pub const PACKAGE: &str = "@jixoai/agent-sync";
const REGISTRY: &str = "https://registry.npmjs.org";

/// 编译进二进制的版本（Cargo.toml）。
pub const CURRENT: &str = env!("CARGO_PKG_VERSION");

const DAEMON_LABEL: &str = "dev.gaubee.agents-sync.daemon";
const UI_LABEL: &str = "dev.gaubee.agents-sync";

#[derive(Debug, Clone, Deserialize)]
pub struct Release {
    pub version: String,
    pub dist: Dist,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Dist {
    pub tarball: String,
    #[serde(default)]
    pub shasum: String,
}

/// `latest` 元数据的 URL。npm 的 scope 用 `%2F` 编码是官方接受的写法。
///
/// `AGENTS_SYNC_REGISTRY` 可覆盖（测试用：本地起一个假 registry 就能完整演练
/// 下载、校验、替换，不必真的发版）。
pub fn latest_url() -> String {
    let base = std::env::var("AGENTS_SYNC_REGISTRY").unwrap_or_else(|_| REGISTRY.to_string());
    format!(
        "{}/{}/latest",
        base.trim_end_matches('/'),
        PACKAGE.replace('/', "%2F")
    )
}

/// 语义化版本（只认 `x.y.z`；带预发布后缀的一律视为「不自动更新」）。
pub fn parse_version(v: &str) -> Option<(u64, u64, u64)> {
    let core = v.trim().trim_start_matches('v');
    if core.contains('-') || core.contains('+') {
        return None;
    }
    let mut it = core.split('.');
    let major = it.next()?.parse().ok()?;
    let minor = it.next()?.parse().ok()?;
    let patch = it.next()?.parse().ok()?;
    if it.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

pub fn is_newer(latest: &str, current: &str) -> bool {
    match (parse_version(latest), parse_version(current)) {
        (Some(l), Some(c)) => l > c,
        _ => false,
    }
}

/// 查询最新版。失败信息要能指向「网络/代理」而不是泛泛的「更新失败」。
pub fn check(proxy: &str) -> Result<Release> {
    let body = curl(&latest_url(), proxy).context("查询 npm registry 失败")?;
    let rel: Release = serde_json::from_str(&body)
        .with_context(|| format!("npm 返回的不是预期 JSON：{}", body.chars().take(200).collect::<String>()))?;
    Ok(rel)
}

#[derive(Debug, Clone)]
pub struct ApplyReport {
    pub from: String,
    pub to: String,
    pub binary: PathBuf,
    pub bundle: Option<PathBuf>,
    pub restarted: Vec<&'static str>,
}

/// 下载并安装最新版。`force` 允许在同版本时重装（修复半坏安装用）。
pub fn apply(paths: &SyncPaths, proxy: &str, force: bool) -> Result<ApplyReport> {
    let rel = check(proxy)?;
    if !force && !is_newer(&rel.version, CURRENT) {
        bail!("已是最新版本（{}，registry 为 {}）", CURRENT, rel.version);
    }

    // 同一时刻只允许一个更新进程：两个 daemon 抢着换文件会留下半份安装。
    let _lock = DirLock::acquire(&paths.locks_dir(), "update", Duration::from_secs(10))?;

    let work = paths.state_dir().join("update");
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).context("创建更新临时目录失败")?;
    let tgz = work.join("pkg.tgz");

    let out = Command::new("/usr/bin/curl")
        .args(["-fsSL", "--max-time", "120", "-o"])
        .arg(&tgz)
        .args(proxy_flags(proxy))
        .arg(&rel.dist.tarball)
        .output()
        .context("启动 curl 失败")?;
    if !out.status.success() {
        bail!(
            "下载失败（curl {:?}）：{}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }

    verify_shasum(&tgz, &rel.dist.shasum)?;

    let out = Command::new("/usr/bin/tar")
        .args(["-xzf"])
        .arg(&tgz)
        .args(["-C"])
        .arg(&work)
        .output()
        .context("启动 tar 失败")?;
    if !out.status.success() {
        bail!("解包失败：{}", String::from_utf8_lossy(&out.stderr).trim());
    }

    let pkg = work.join("package");
    let new_bin = pkg.join("bin/agents-sync");
    if !new_bin.is_file() {
        bail!("包里没有 bin/agents-sync（发布产物结构不对）");
    }
    let packed = read_package_version(&pkg).unwrap_or_default();
    if !packed.is_empty() && packed != rel.version {
        bail!("包内版本 {packed} 与 registry 的 {} 不一致", rel.version);
    }

    // ---- 换 CLI（原子 rename）----
    let dest = paths.home.join(".local/bin/agents-sync");
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if dest.exists() {
        let _ = std::fs::copy(&dest, dest.with_extension("old"));
    }
    let staged = dest.with_extension("new");
    std::fs::copy(&new_bin, &staged).context("写入新 CLI 失败")?;
    set_executable(&staged)?;
    std::fs::rename(&staged, &dest).context("替换 CLI 失败")?;

    // ---- 换 .app（先拷临时目录再交换）----
    let app_src = pkg.join("lib/AgentsSync.app");
    let bundle = if app_src.is_dir() {
        let dir = paths.home.join("Applications");
        std::fs::create_dir_all(&dir)?;
        let dst = dir.join("AgentsSync.app");
        let staging = dir.join(".AgentsSync.app.new");
        let backup = dir.join("AgentsSync.app.old");
        let _ = std::fs::remove_dir_all(&staging);
        copy_dir(&app_src, &staging)?;
        if dst.exists() {
            let _ = std::fs::remove_dir_all(&backup);
            std::fs::rename(&dst, &backup).context("备份旧 app 失败")?;
        }
        std::fs::rename(&staging, &dst).context("替换 app 失败")?;
        Some(dst)
    } else {
        None
    };

    // ---- 重启单元（新二进制接管；本进程若在 daemon 内会被 kickstart 结束）----
    let mut restarted = Vec::new();
    for label in [DAEMON_LABEL, UI_LABEL] {
        if kickstart(label) {
            restarted.push(if label == DAEMON_LABEL { "daemon" } else { "ui" });
        }
    }

    let _ = std::fs::remove_dir_all(&work);
    Ok(ApplyReport {
        from: CURRENT.to_string(),
        to: rel.version,
        binary: dest,
        bundle,
        restarted,
    })
}

fn proxy_flags(proxy: &str) -> Vec<String> {
    if proxy.trim().is_empty() {
        Vec::new()
    } else {
        vec!["--proxy".into(), proxy.trim().to_string()]
    }
}

fn curl(url: &str, proxy: &str) -> Result<String> {
    let out = Command::new("/usr/bin/curl")
        .args(["-fsSL", "--max-time", "25"])
        .args(proxy_flags(proxy))
        .arg(url)
        .output()
        .context("启动 curl 失败")?;
    if !out.status.success() {
        bail!(
            "curl 退出 {:?}：{}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn verify_shasum(file: &Path, expect: &str) -> Result<()> {
    if expect.trim().is_empty() {
        bail!("registry 未提供 shasum，拒绝安装");
    }
    let out = Command::new("/usr/bin/shasum")
        .args(["-a", "1"])
        .arg(file)
        .output()
        .context("启动 shasum 失败")?;
    if !out.status.success() {
        bail!("计算 sha1 失败");
    }
    let got = String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_lowercase();
    if got != expect.trim().to_lowercase() {
        bail!("sha1 不匹配（期望 {expect}，实际 {got}），已中止");
    }
    Ok(())
}

fn read_package_version(pkg: &Path) -> Option<String> {
    let text = std::fs::read_to_string(pkg.join("package.json")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    v.get("version")?.as_str().map(str::to_string)
}

fn set_executable(p: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}

fn copy_dir(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let to = dst.join(entry.file_name());
        let ft = entry.file_type()?;
        if ft.is_dir() {
            copy_dir(&entry.path(), &to)?;
        } else if ft.is_symlink() {
            let target = std::fs::read_link(entry.path())?;
            let _ = std::fs::remove_file(&to);
            std::os::unix::fs::symlink(target, &to)?;
        } else {
            std::fs::copy(entry.path(), &to)?;
        }
    }
    Ok(())
}

fn kickstart(label: &str) -> bool {
    // 测试/编排场景：只换文件、不重启（否则更新脚本会把正在跑的 daemon 打断）。
    if std::env::var_os("AGENTS_SYNC_NO_RESTART").is_some() {
        return false;
    }
    let Ok(out) = Command::new("/usr/bin/id").arg("-u").output() else {
        return false;
    };
    let uid = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if uid.is_empty() {
        return false;
    }
    Command::new("/bin/launchctl")
        .args(["kickstart", "-k", &format!("gui/{uid}/{label}")])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_compare_is_strict() {
        assert!(is_newer("0.1.0", "0.0.9"));
        assert!(is_newer("1.0.0", "0.9.9"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.0-rc.1", "0.0.1"), "预发布不自动更新");
        assert!(!is_newer("garbage", "0.1.0"));
        assert_eq!(parse_version("v1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_version("1.2"), None);
    }

    #[test]
    fn latest_url_encodes_scope() {
        assert_eq!(
            latest_url(),
            "https://registry.npmjs.org/@jixoai%2Fagent-sync/latest"
        );
    }
}
