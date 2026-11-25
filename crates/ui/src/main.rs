//! 菜单栏应用的独立可执行文件。
//!
//! 刻意与 CLI 分开：把 nsh-platform-macos（AppKit）链进 CLI，会让 CLI 在
//! 非 bundle 环境下启动路径被改变——实测 `--version` 直接静默退出（rc=0、无输出）。
//! CLI 必须保持纯 headless；GUI 自己一个二进制，由 bundle 承载。
fn main() -> anyhow::Result<()> {
    agents_sync_ui::run()
}
