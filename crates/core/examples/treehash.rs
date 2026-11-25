//! 迁移期对拍工具：打印某目录的树哈希。
//! 与 bash 版 `lib/fsops.py tree-hash` 必须逐字节一致，
//! 否则两套实现切换后「收敛判据」就失去可比性。
//!
//! 用法：cargo run -p agents-sync-core --example treehash -- <dir>

fn main() {
    let Some(dir) = std::env::args().nth(1) else {
        eprintln!("用法: treehash <dir>");
        std::process::exit(2);
    };
    match agents_sync_core::checks::tree_hash(std::path::Path::new(&dir)) {
        Ok(h) => println!("{h}"),
        Err(e) => {
            eprintln!("{e:#}");
            std::process::exit(1);
        }
    }
}
