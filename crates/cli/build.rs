//! 把打包好的菜单栏应用嵌进 CLI。
//!
//! 为什么这么做（参考 reply9842 的形态）：分发物应当只有**一个可执行文件**。
//! 如果 npm 包里同时放 `bin/agents-sync` 和 `lib/AgentsSync.app`，node_modules
//! 里会躺着一个 Mac 应用包，每次更新也要整包重下。嵌入之后：
//!   · npm 只发一个二进制；
//!   · `install` 在运行时把 `.app` 解出来；
//!   · 更新只替换这一个文件，版本天然一致。
//!
//! `AGENTS_SYNC_APP_TARBALL` 未设置时写入空文件：开发期的 `cargo build` 不依赖
//! 先打包 app，`install` 会退回到「找旁边的 AgentsSync.app」的老路径。

use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-env-changed=AGENTS_SYNC_APP_TARBALL");
    let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"))
        .join("app.tar.gz");
    match std::env::var("AGENTS_SYNC_APP_TARBALL") {
        Ok(p) if std::path::Path::new(&p).is_file() => {
            println!("cargo:rerun-if-changed={p}");
            std::fs::copy(&p, &out).expect("复制 app 压缩包失败");
            println!("cargo:warning=已嵌入菜单栏应用：{p}");
        }
        _ => {
            std::fs::write(&out, b"").expect("写占位文件失败");
        }
    }
}
