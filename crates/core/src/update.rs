//! 自更新：从 GitHub Release 取最新版（npm 包只是几 KB 的启动器，不承载二进制）。
//!
//! 为什么这样分发（参考 reply9842）：npm 适合放几 KB 的 JS 启动器，几十 MB 的
//! 原生二进制放在 GitHub Release 更合适——用户只为自己的平台、自己的版本付一次
//! 下载，npm 包本身秒装。国内直连 GitHub 常超时，所以和 npm 启动器共用同一套
//! 镜像前缀约定（`AGENTS_SYNC_MIRROR` / `[update] mirror`）。
//!
//! 安全顺序（任何一步失败都不动现有安装）：
//!   1. 取 `releases/latest/download/SHA256SUMS.txt`，从文件名解析版本
//!   2. 版本必须严格更新；没更新就直接返回
//!   3. 断点续传下载 `agents-sync-v<ver>-darwin-arm64.tar.gz`（弱网多轮重试）
//!   4. 校验 SHA256 → 解包 → 替换 CLI（旧版留 `.old`）
//!   5. 让新 CLI 自己 `install`（它内嵌菜单栏 app），最后 kickstart 单元

use crate::lock::DirLock;
use crate::paths::SyncPaths;
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

/// 发布仓库。写死而不是配置：只服务这一个项目，可配置只会制造
/// 「配错了下载到别人的二进制」的风险。
pub const REPO: &str = "jixoai/agent-sync";

/// 编译进二进制的版本（Cargo.toml）。
pub const CURRENT: &str = env!("CARGO_PKG_VERSION");

const DAEMON_LABEL: &str = "dev.gaubee.agents-sync.daemon";
const UI_LABEL: &str = "dev.gaubee.agents-sync";

/// 内置镜像前缀（与 npm 启动器的 lib/mirror.js 保持一致）。
const DEFAULT_MIRRORS: &[&str] = &[
    "https://gh-proxy.com/",
    "https://ghfast.top/",
    "https://ghproxy.net/",
    "",
];

#[derive(Debug, Clone)]
pub struct Release {
    pub version: String,
    /// Release 里的压缩包文件名（含版本号，便于两端解析）。
    pub artifact: String,
    /// 直接下载地址（未套镜像）。
    pub url: String,
    pub sha256: String,
}

/// 本平台的产物后缀。目前只发布 Apple Silicon 版。
pub fn platform_tag() -> &'static str {
    "darwin-arm64"
}

pub fn artifact_name(version: &str) -> String {
    format!("agents-sync-v{version}-{}.tar.gz", platform_tag())
}

/// Release 根地址。`AGENTS_SYNC_RELEASE_BASE` 可覆盖（自测用本地假 Release）。
fn release_base() -> String {
    std::env::var("AGENTS_SYNC_RELEASE_BASE")
        .unwrap_or_else(|_| format!("https://github.com/{REPO}/releases"))
}

/// `latest` 的 SHA256SUMS.txt 稳定地址（GitHub 会把 latest 重定向到具体版本）。
pub fn latest_sums_url() -> String {
    format!("{}/latest/download/SHA256SUMS.txt", release_base())
}

pub fn artifact_url(version: &str, artifact: &str) -> String {
    format!("{}/download/v{version}/{artifact}", release_base())
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

/// 从 `SHA256SUMS.txt` 解析本平台产物的版本与哈希。
///
/// 文件名形如 `agents-sync-v0.1.7-darwin-arm64.tar.gz`，版本直接从文件名取，
/// 不需要再调 GitHub API（少一次限流面，也少一个失败点）。
pub fn parse_sums(text: &str) -> Option<Release> {
    let tag = platform_tag();
    for line in text.lines() {
        // `<sha256>  <name>`（`*` 前缀是某些工具的二进制作标记）
        let mut it = line.split_whitespace();
        let (Some(hash), Some(name)) = (it.next(), it.next()) else {
            continue;
        };
        // `shasum ./*.tar.gz` 会写成 `./name`；统一取 basename 再匹配。
        let name = name.trim_start_matches('*');
        let name = name.rsplit('/').next().unwrap_or(name);
        let Some(rest) = name.strip_prefix("agents-sync-v") else {
            continue;
        };
        let Some(ver_part) = rest.strip_suffix(&format!("-{tag}.tar.gz")) else {
            continue;
        };
        if hash.len() != 64 || !hash.chars().all(|c| c.is_ascii_hexdigit()) {
            continue;
        }
        let version = ver_part.to_string();
        if parse_version(&version).is_none() {
            continue;
        }
        return Some(Release {
            artifact: name.to_string(),
            url: artifact_url(&version, name),
            sha256: hash.to_lowercase(),
            version,
        });
    }
    None
}

/// 解析镜像前缀：`[update] mirror` / `AGENTS_SYNC_MIRROR` 覆盖，否则用内置列表。
/// 格式与 npm 启动器一致：逗号分隔，`direct`/空串表示直连。
pub fn mirror_prefixes(config: &str) -> Vec<String> {
    let raw = if !config.trim().is_empty() {
        config.to_string()
    } else if let Ok(env) = std::env::var("AGENTS_SYNC_MIRROR") {
        env
    } else {
        DEFAULT_MIRRORS.join(",")
    };
    let mut out: Vec<String> = Vec::new();
    for token in raw.split(',') {
        let token = token.trim();
        let value = if token.eq_ignore_ascii_case("direct") { "" } else { token };
        let normalized = if value.is_empty() {
            String::new()
        } else if value.ends_with('/') {
            value.to_string()
        } else {
            format!("{value}/")
        };
        if !normalized.is_empty() && !normalized.starts_with("http") {
            continue;
        }
        if !out.contains(&normalized) {
            out.push(normalized);
        }
    }
    if !out.iter().any(|p| p.is_empty()) {
        out.push(String::new());
    }
    out
}

fn with_prefix(prefix: &str, url: &str) -> String {
    if prefix.is_empty() {
        url.to_string()
    } else {
        format!("{prefix}{url}")
    }
}

/// 查询最新版：依次尝试各镜像取 `SHA256SUMS.txt`。
pub fn check(proxy: &str, mirror: &str) -> Result<Release> {
    let mut errors = Vec::new();
    for prefix in mirror_prefixes(mirror) {
        let url = with_prefix(&prefix, &latest_sums_url());
        match curl_text(&url, proxy) {
            Ok(text) => {
                if let Some(rel) = parse_sums(&text) {
                    return Ok(rel);
                }
                errors.push(format!(
                    "{}: SHA256SUMS.txt 里没有本平台产物",
                    if prefix.is_empty() { "direct" } else { &prefix }
                ));
            }
            Err(e) => errors.push(format!(
                "{}: {e}",
                if prefix.is_empty() { "direct" } else { &prefix }
            )),
        }
    }
    bail!("检查更新失败（全部镜像）：\n  {}", errors.join("\n  "))
}

#[derive(Debug, Clone)]
pub struct ApplyReport {
    pub from: String,
    pub to: String,
    pub binary: PathBuf,
    pub bundle: Option<PathBuf>,
    pub restarted: Vec<&'static str>,
}

/// 下载并安装最新版。`force` 允许同版本重装（修复半坏安装用）。
pub fn apply(paths: &SyncPaths, proxy: &str, mirror: &str, force: bool) -> Result<ApplyReport> {
    let rel = check(proxy, mirror)?;
    if !force && !is_newer(&rel.version, CURRENT) {
        bail!("已是最新版本（{}，release 为 {}）", CURRENT, rel.version);
    }

    let _lock = DirLock::acquire(&paths.locks_dir(), "update", Duration::from_secs(10))?;

    let work = paths.state_dir().join("update");
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).context("创建更新临时目录失败")?;
    let tarball = work.join(&rel.artifact);

    download_with_mirrors(&rel, &tarball, proxy, mirror)?;
    verify_sha256(&tarball, &rel.sha256)?;

    let out = Command::new("/usr/bin/tar")
        .arg("-xzf")
        .arg(&tarball)
        .arg("-C")
        .arg(&work)
        .output()
        .context("启动 tar 失败")?;
    if !out.status.success() {
        bail!("解包失败：{}", String::from_utf8_lossy(&out.stderr).trim());
    }

    let new_bin = work.join("agents-sync");
    if !new_bin.is_file() {
        bail!("压缩包里没有 agents-sync（发布产物结构不对）");
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

    // ---- 换 .app：新 CLI 内嵌 app，让它自己解出来 ----
    let out = Command::new(&dest)
        .arg("install")
        .output()
        .context("运行新 CLI install 失败")?;
    if !out.status.success() {
        bail!(
            "新 CLI install 失败：{}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let bundle = Some(paths.home.join("Applications/AgentsSync.app"));

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

/// 依次尝试镜像；每个镜像内部再做断点续传与多轮重试。
fn download_with_mirrors(rel: &Release, out: &Path, proxy: &str, mirror: &str) -> Result<()> {
    let mut errors = Vec::new();
    for prefix in mirror_prefixes(mirror) {
        let url = with_prefix(&prefix, &rel.url);
        let _ = std::fs::remove_file(out);
        match download_resumable(&url, out, proxy) {
            Ok(()) => return Ok(()),
            Err(e) => errors.push(format!(
                "{}: {e}",
                if prefix.is_empty() { "direct" } else { &prefix }
            )),
        }
    }
    bail!("下载失败（全部镜像）：\n  {}", errors.join("\n  "))
}

/// 断点续传下载：每轮速度感知（低于 10KB/s 持续 20s 断开），最多 5 轮。
fn download_resumable(url: &str, out: &Path, proxy: &str) -> Result<()> {
    let mut last_err = String::new();
    for attempt in 1..=5 {
        let res = Command::new("/usr/bin/curl")
            .args([
                "-fsSL",
                "--retry",
                "3",
                "--retry-all-errors",
                "--speed-limit",
                "10240",
                "--speed-time",
                "20",
                "-C",
                "-",
                "--max-time",
                "300",
                "-o",
            ])
            .arg(out)
            .args(proxy_flags(proxy))
            .arg(url)
            .output()
            .context("启动 curl 失败")?;
        if res.status.success() {
            // 服务端忽略 Range 时 curl 会从头写，文件仍完整；交给 sha256 兜底
            return Ok(());
        }
        last_err = String::from_utf8_lossy(&res.stderr).trim().to_string();
        log::warn!(
            "下载第 {attempt} 轮失败（已得 {} 字节）：{last_err}",
            out.metadata().map(|m| m.len()).unwrap_or(0)
        );
        std::thread::sleep(Duration::from_secs(2));
    }
    bail!("下载失败（5 轮重试后）：{last_err}");
}

fn proxy_flags(proxy: &str) -> Vec<String> {
    if proxy.trim().is_empty() {
        Vec::new()
    } else {
        vec!["--proxy".into(), proxy.trim().to_string()]
    }
}

fn curl_text(url: &str, proxy: &str) -> Result<String> {
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

fn verify_sha256(file: &Path, expect: &str) -> Result<()> {
    let out = Command::new("/usr/bin/shasum")
        .args(["-a", "256"])
        .arg(file)
        .output()
        .context("启动 shasum 失败")?;
    if !out.status.success() {
        bail!("计算 sha256 失败");
    }
    let got = String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_lowercase();
    if got != expect.trim().to_lowercase() {
        bail!("sha256 不匹配（期望 {expect}，实际 {got}），已中止");
    }
    Ok(())
}

fn set_executable(p: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755))?;
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
    fn sums_parsing_finds_our_artifact() {
        let text = "\
d1e8b1b2c3d4e5f60718293a4b5c6d7e8f9012345678  agents-sync-v0.2.3-linux-x64.tar.gz
0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef  agents-sync-v0.2.3-darwin-arm64.tar.gz
";
        let rel = parse_sums(text).expect("找到 darwin-arm64");
        assert_eq!(rel.version, "0.2.3");
        assert_eq!(rel.artifact, "agents-sync-v0.2.3-darwin-arm64.tar.gz");
        assert!(rel.url.ends_with("/download/v0.2.3/agents-sync-v0.2.3-darwin-arm64.tar.gz"));
    }

    #[test]
    fn mirror_prefixes_config_and_direct() {
        assert_eq!(
            mirror_prefixes("https://m.example.com"),
            vec!["https://m.example.com/".to_string(), String::new()]
        );
        assert_eq!(mirror_prefixes("direct"), vec![String::new()]);
    }
}
