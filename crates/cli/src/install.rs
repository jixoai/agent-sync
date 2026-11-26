//! 意图（2026-11-24 Owner）：「最终只提供一个 cli，但是会直接通 cli 的权限，
//!   在操作系统中注入一个应用程序，参考 reply9842 的思路」。
//!   本文件就是那个注入器：把当前可执行文件复制成最小 `.app`，再写 launchd 单元。
//!   bundle 由用户自己的二进制在运行时生成，因此不需要签名与公证。
//!
//! 本文件职责（4 个正交意图）：
//!   1. 生成/刷新 `~/Applications/AgentsSync.app`（幂等，内容不变不重写）
//!   2. 生成并加载 launchd 单元（RunAtLoad + KeepAlive，指向 bundle 内的副本）
//!   3. 卸载（先卸 launchd 再删 bundle）
//!   4. 状态查询（bundle 是否存在、单元是否加载）
//! 不承载：同步逻辑与 UI。

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

/// bundle id 与固定可执行名。名字固定而不是取 argv[0]，
/// 否则用户重命名二进制时 bundle 会跟着变形，launchd 也会失配。
pub const BUNDLE_ID: &str = "dev.gaubee.agents-sync";
pub const EXEC_NAME: &str = "agents-sync";
/// GUI 组件名（与 CLI 分离，见 ui crate 的说明）。
pub const GUI_EXEC_NAME: &str = "agents-sync-app";
/// daemon 单元标签。UI 与 daemon 是两个独立常驻体：UI 是 nsh bundle，
/// daemon 是 CLI 二进制，二者都需 launchd 托管，缺一不可。
const DAEMON_LABEL: &str = "dev.gaubee.agents-sync.daemon";

/// CLI 的安装位置（用户可直接调用，也是 daemon 单元的执行体）。
fn installed_cli(home: &Path) -> PathBuf {
    home.join(".local/bin").join(EXEC_NAME)
}

fn daemon_plist(home: &Path) -> PathBuf {
    home.join("Library/LaunchAgents")
        .join(format!("{DAEMON_LABEL}.plist"))
}

/// 把 CLI 装到 ~/.local/bin（幂等，内容变化才写）。
fn install_cli(home: &Path, exe: &Path) -> Result<PathBuf> {
    let dst = installed_cli(home);
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
    }
    write_if_changed_from(exe, &dst)?;
    Ok(dst)
}

fn daemon_unit(cli: &Path, home: &Path) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>{DAEMON_LABEL}</string>
  <key>ProgramArguments</key><array>
    <string>{cli}</string><string>daemon</string>
  </array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key>
  <dict><key>SuccessfulExit</key><false/></dict>
  <key>ThrottleInterval</key><integer>10</integer>
  <key>LimitLoadToSessionType</key><string>Aqua</string>
  <key>StandardOutPath</key><string>{log}</string>
  <key>StandardErrorPath</key><string>{log}</string>
</dict></plist>
"#,
        cli = cli.display(),
        log = home.join("Library/Logs/agents-sync.daemon.log").display()
    )
}

/// 装载一个 launchd 单元（先 unload 再 load，保证配置生效）。
fn load_unit(plist: &Path) -> bool {
    let _ = Command::new("/bin/launchctl")
        .args(["unload", plist.to_str().unwrap_or("")])
        .output();
    Command::new("/bin/launchctl")
        .args(["load", plist.to_str().unwrap_or("")])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}
pub const APP_NAME: &str = "AgentsSync";
const LAUNCHD_LABEL: &str = "dev.gaubee.agents-sync";

/// 环境标记：防止「从 bundle 副本启动后又去装 bundle」造成的无限 re-exec。
#[allow(dead_code)]
const ENV_BUNDLED: &str = "AGENTS_SYNC_BUNDLED";

pub fn bundle_path(home: &Path) -> PathBuf {
    home.join("Applications").join(format!("{APP_NAME}.app"))
}

pub fn launchd_plist(home: &Path) -> PathBuf {
    home.join("Library/LaunchAgents")
        .join(format!("{LAUNCHD_LABEL}.plist"))
}

/// 当前是否已经在 bundle 内运行。
#[allow(dead_code)]
pub fn running_in_bundle() -> bool {
    std::env::var(ENV_BUNDLED).as_deref() == Ok("1")
}

pub struct InstallReport {
    pub bundle: PathBuf,
    pub binary: PathBuf,
    pub plist: PathBuf,
    pub reloaded: bool,
}

/// 注入应用：写 bundle、写 launchd、加载。
/// 幂等：内容没变的文件不重写（避免每次启动都改 mtime、触发备份工具）。
/// 找一个可投放的 nsh `.app`。
///
/// mixed 形态下 UI 是 bundle（runner + bundle.nshc），不是裸二进制；
/// 查找顺序：显式环境变量 → CLI 同级的 AgentsSync.app → 仓库内 `nsh package` 的产物。
fn find_app_bundle(exe: &Path) -> Option<PathBuf> {
    if let Ok(p) = std::env::var("AGENTS_SYNC_APP_BUNDLE") {
        let p = PathBuf::from(p);
        if p.is_dir() {
            return Some(p);
        }
    }
    let dir = exe.parent()?;
    for cand in [
        dir.join("AgentsSync.app"),
        // npm 包布局：bin/agents-sync + lib/AgentsSync.app
        dir.join("../lib/AgentsSync.app"),
        dir.join("../app/.nsh/out/AgentsSync.app"),
    ] {
        if cand.is_dir() {
            return Some(cand);
        }
    }
    None
}

/// 递归复制目录（bundle 需要保留结构）。
fn copy_dir_all(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let target = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_all(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

pub fn install(home: &Path) -> Result<InstallReport> {
    let exe = std::env::current_exe().context("无法定位当前可执行文件")?;
    let exe = std::fs::canonicalize(&exe).unwrap_or(exe);

    let bundle = bundle_path(home);

    // 分支一：有 nsh bundle 就整体投放（UI 由 nsh host 承载）
    if let Some(src_app) = find_app_bundle(&exe) {
        if bundle.exists() {
            std::fs::remove_dir_all(&bundle)?;
        }
        copy_dir_all(&src_app, &bundle).with_context(|| format!("投放 {}", src_app.display()))?;
        let runner = bundle.join("Contents/MacOS").join(APP_NAME);
        let plist_path = launchd_plist(home);
        if let Some(parent) = plist_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // nsh bundle 的 runner 只承载界面：daemon 由独立单元负责，绝不带 `app`
        write_if_changed(&plist_path, launchd_unit(&runner, false).as_bytes(), 0o644)?;
        let loaded = load_unit(&plist_path);

        // CLI + daemon：UI 只是界面，同步引擎必须另有常驻体。
        let cli = install_cli(home, &exe)?;
        let dplist = daemon_plist(home);
        if let Some(parent) = dplist.parent() {
            std::fs::create_dir_all(parent)?;
        }
        write_if_changed(&dplist, daemon_unit(&cli, home).as_bytes(), 0o644)?;
        let daemon_loaded = load_unit(&dplist);
        log::info!("daemon 单元已装载：{daemon_loaded}");

        return Ok(InstallReport { bundle, binary: runner, plist: plist_path, reloaded: loaded });
    }

    let contents = bundle.join("Contents");
    let macos = contents.join("MacOS");
    let resources = contents.join("Resources");
    std::fs::create_dir_all(&macos)?;
    std::fs::create_dir_all(&resources)?;

    let target = macos.join(EXEC_NAME);
    if exe != target {
        write_if_changed_from(&exe, &target)?;
    }

    // 同时投放 GUI 组件：launchd 跑的是 `app` 子命令，它会 exec 兄弟二进制
    // `agents-sync-app`。只放 CLI 会让 app 启动即失败（实测）。
    let sibling_src = exe.with_file_name(GUI_EXEC_NAME);
    if sibling_src.exists() {
        write_if_changed_from(&sibling_src, &macos.join(GUI_EXEC_NAME))?;
    } else {
        log::warn!(
            "未找到 GUI 组件 {}：注入的 app 将无法启动界面（只保留 daemon 能力）",
            sibling_src.display()
        );
    }

    // Info.plist：LSUIElement 让它是菜单栏应用（无 Dock 图标）
    let plist = info_plist();
    write_if_changed(&contents.join("Info.plist"), plist.as_bytes(), 0o644)?;

    let plist_path = launchd_plist(home);
    if let Some(parent) = plist_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // 回退形态：单个二进制兼任 CLI 与 UI，需要 `app` 参数进入应用模式
    let unit = launchd_unit(&target, true);
    write_if_changed(&plist_path, unit.as_bytes(), 0o644)?;

    let cli = install_cli(home, &exe)?;
    let dplist = daemon_plist(home);
    if let Some(parent) = dplist.parent() {
        std::fs::create_dir_all(parent)?;
    }
    write_if_changed(&dplist, daemon_unit(&cli, home).as_bytes(), 0o644)?;
    let _ = load_unit(&dplist);

    // 重新加载：先 unload 再 load，保证新配置生效
    let _ = Command::new("/bin/launchctl")
        .args(["unload", plist_path.to_str().unwrap_or("")])
        .output();
    let loaded = Command::new("/bin/launchctl")
        .args(["load", plist_path.to_str().unwrap_or("")])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);

    Ok(InstallReport {
        bundle,
        binary: target,
        plist: plist_path,
        reloaded: loaded,
    })
}

/// 卸载：先卸 launchd，再删 bundle 与 plist。
pub fn uninstall(home: &Path) -> Result<()> {
    for p in [launchd_plist(home), daemon_plist(home)] {
        let _ = Command::new("/bin/launchctl")
            .args(["unload", p.to_str().unwrap_or("")])
            .output();
        let _ = std::fs::remove_file(&p);
    }
    let bundle = bundle_path(home);
    if bundle.exists() {
        std::fs::remove_dir_all(&bundle)
            .with_context(|| format!("删除 {}", bundle.display()))?;
    }
    Ok(())
}

#[derive(Debug, serde::Serialize)]
pub struct InstallStatus {
    pub bundle: String,
    pub bundle_exists: bool,
    pub plist: String,
    pub plist_exists: bool,
    pub loaded: bool,
    pub binary_matches_source: bool,
}

pub fn status(home: &Path) -> InstallStatus {
    let bundle = bundle_path(home);
    let plist = launchd_plist(home);
    let target = bundle.join("Contents/MacOS").join(EXEC_NAME);

    let loaded = Command::new("/bin/launchctl")
        .arg("list")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains(LAUNCHD_LABEL))
        .unwrap_or(false);

    let same = match (std::env::current_exe().ok(), std::fs::read(&target).ok()) {
        (Some(exe), Some(bundle_bytes)) => {
            let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
            std::fs::read(exe)
                .map(|cur| cur == bundle_bytes)
                .unwrap_or(false)
        }
        _ => false,
    };

    InstallStatus {
        bundle: bundle.display().to_string(),
        bundle_exists: bundle.exists(),
        plist: plist.display().to_string(),
        plist_exists: plist.exists(),
        loaded,
        binary_matches_source: same,
    }
}

/// 最小 Info.plist。字段用途：
///   CFBundleIdentifier —— LaunchServices 身份，重复 id 会让两个 app 抢图标
///   CFBundleExecutable —— bundle 内可执行文件名
///   LSUIElement       —— 菜单栏应用，不进 Dock
///   NSHighResolutionCapable —— 不开则 Retina 上界面被放大成 2 倍模糊
fn info_plist() -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleIdentifier</key><string>{BUNDLE_ID}</string>
  <key>CFBundleName</key><string>{APP_NAME}</string>
  <key>CFBundleDisplayName</key><string>{APP_NAME}</string>
  <key>CFBundleExecutable</key><string>{EXEC_NAME}</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
  <key>CFBundleShortVersionString</key><string>{version}</string>
  <key>CFBundleVersion</key><string>{version}</string>
  <key>LSMinimumSystemVersion</key><string>14.0</string>
  <key>LSUIElement</key><true/>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
"#,
        version = env!("CARGO_PKG_VERSION")
    )
}

/// launchd 单元：跑 `app` 子命令（菜单栏应用，内部带 daemon）。
///
/// LimitLoadToSessionType=Aqua 是必须的：没有 GUI 会话时不启动，
/// 否则会在 ssh/后台会话里反复崩溃重启。
fn launchd_unit(binary: &Path, app_mode: bool) -> String {
    // `app_mode`：二进制是否需要 `app` 参数（回退形态的单体二进制）。
    // nsh bundle 的 runner 是纯界面宿主，传 `app` 没有意义。
    let arg = if app_mode { "<string>app</string>" } else { "" };
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>{LAUNCHD_LABEL}</string>
  <key>ProgramArguments</key><array>
    <string>{binary}</string>{arg}
  </array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key>
  <dict><key>SuccessfulExit</key><false/></dict>
  <key>ThrottleInterval</key><integer>5</integer>
  <key>LimitLoadToSessionType</key><string>Aqua</string>
  <key>ProcessType</key><string>Interactive</string>
  <key>StandardOutPath</key><string>{log}</string>
  <key>StandardErrorPath</key><string>{log}</string>
</dict></plist>
"#,
        binary = binary.display(),
        arg = arg,
        log = dirs_log_path()
    )
}

fn dirs_log_path() -> String {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    format!("{home}/Library/Logs/agents-sync.app.log")
}

fn write_if_changed(path: &Path, data: &[u8], mode: u32) -> Result<()> {
    if let Ok(old) = std::fs::read(path) {
        if old == data {
            return Ok(());
        }
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, data)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn write_if_changed_from(src: &Path, dst: &Path) -> Result<()> {
    let new = std::fs::read(src)?;
    if let Ok(old) = std::fs::read(dst) {
        if old == new {
            return Ok(());
        }
    }
    let tmp = dst.with_extension("tmp");
    std::fs::copy(src, &tmp)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
    }
    std::fs::rename(&tmp, dst)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plist_declares_menu_bar_app() {
        let p = info_plist();
        assert!(p.contains("<key>LSUIElement</key><true/>"));
        assert!(p.contains(BUNDLE_ID));
        assert!(p.contains(EXEC_NAME));
    }

    #[test]
    fn unit_runs_app_mode_from_bundle() {
        let u = launchd_unit(Path::new("/tmp/x/AgentsSync.app/Contents/MacOS/agents-sync"), true);
        assert!(u.contains("<string>app</string>"), "必须跑 app 子命令");
        assert!(u.contains("LimitLoadToSessionType"));
        assert!(u.contains("<key>KeepAlive</key>"));
    }

    #[test]
    fn write_if_changed_is_idempotent() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("x");
        write_if_changed(&p, b"a", 0o644).unwrap();
        let first = std::fs::metadata(&p).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        write_if_changed(&p, b"a", 0o644).unwrap();
        let second = std::fs::metadata(&p).unwrap().modified().unwrap();
        assert_eq!(first, second, "内容未变时不得重写（避免 mtime 抖动）");
        write_if_changed(&p, b"b", 0o644).unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"b");
    }
}
