//! 意图（2026-11-24）：活目录与 payload 之间的镜像语义。
//!   capture 是「活 → 仓库」，apply 是「仓库 → 活」。两者共用同一套准入规则，
//!   任何「能采进来但落不回去」的不对称都会造成静默数据丢失。
//!
//! 本文件职责（3 个共享原语，不含任何树的具体取舍）：
//!   1. 保权限复制（可执行位与只读位必须原样过去）
//!   2. 目录创建与「路径是否已存在」的判定
//!   3. 两份报告类型，供 CLI / UI / 日志统一展示
//! 不承载：具体树的采集与落盘（见 capture / apply 子模块）。

use anyhow::{Context, Result};
use std::path::Path;

pub mod apply;
pub mod audit;
pub mod capture;

pub use apply::{apply_all, rollback, ApplyOptions, ApplyReport};
pub use capture::{capture_all, CaptureReport};

/// 复制文件并保留权限位。符号链接由 links 层单独处理，这里只负责普通文件。
pub fn copy_file(src: &Path, dst: &Path) -> Result<()> {
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("创建目录 {}", parent.display()))?;
    }
    std::fs::copy(src, dst)
        .with_context(|| format!("复制 {} → {}", src.display(), dst.display()))?;
    Ok(())
}

/// 若目标已存在且内容相同，跳过写入（避免无意义地改动 mtime，
/// 否则 watchman/FSEvents 会被自己的写入反复唤醒）。
pub fn copy_file_if_changed(src: &Path, dst: &Path) -> Result<bool> {
    let src_meta = std::fs::symlink_metadata(src)?;
    if let Ok(dst_meta) = std::fs::symlink_metadata(dst) {
        if dst_meta.is_file()
            && dst_meta.len() == src_meta.len()
            && dst_meta.permissions().readonly() == src_meta.permissions().readonly()
            && files_equal(src, dst)
        {
            return Ok(false);
        }
    }
    copy_file(src, dst)?;
    Ok(true)
}

fn files_equal(a: &Path, b: &Path) -> bool {
    let (Ok(a), Ok(b)) = (std::fs::read(a), std::fs::read(b)) else {
        return false;
    };
    a == b
}

/// 目录是否存在（不跟随符号链接：指向目录的链接不算「已有目录」）。
pub fn is_real_dir(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .map(|m| m.is_dir())
        .unwrap_or(false)
}
