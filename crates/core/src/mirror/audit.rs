//! 只读预演：payload（合并后的真值）落到活目录会发生什么。
//!
//! 为什么需要它：首次接入一台机器时，apply 会用对端内容覆盖本地同名文件。
//! 这个动作平时是对的（本地就是上一轮的镜像），但在「新机器第一次拉取」时，
//! 被覆盖的可能是用户自己的版本。所以提供一个不改任何文件的清单：
//! 哪些会被新建、哪些会被覆盖（带本地修改时间）、哪些完全一致。
//!
//! 本文件不写盘、不删文件；`agents-sync audit` 直接打印它的结果。

use crate::exclude;
use crate::paths::SyncPaths;
use crate::settings::{Settings, TreeSpec};
use anyhow::Result;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

#[derive(Debug, Clone)]
pub struct Entry {
    pub path: PathBuf,
    /// 本地文件的修改时间（unix 秒）；新建时为空。
    pub local_mtime: Option<u64>,
}

#[derive(Debug, Default)]
pub struct AuditReport {
    /// 活目录没有、会新建。
    pub create: Vec<Entry>,
    /// 活目录有且内容不同、会被覆盖。
    pub overwrite: Vec<Entry>,
    /// 内容一致，无需动作。
    pub identical: usize,
    /// zcode 软链农场里的条目：不参与内容同步。
    pub skipped_symlink: usize,
}

pub fn audit(paths: &SyncPaths) -> Result<AuditReport> {
    let settings = Settings::load(&paths.root)?;
    let agents = settings.tree("agents").cloned().unwrap_or_else(|| {
        Settings::default().tree("agents").expect("默认配置").clone()
    });
    let zcode = settings.tree("zcode").cloned().unwrap_or_else(|| {
        Settings::default().tree("zcode").expect("默认配置").clone()
    });
    let codex = settings.tree("codex").cloned().unwrap_or_else(|| {
        Settings::default().tree("codex").expect("默认配置").clone()
    });

    let mut report = AuditReport::default();
    // agents / codex：payload 相对路径 = 活目录相对路径
    walk_plain(&paths.payload().join("agents"), &paths.live_agents(), &agents, &mut report)?;
    walk_plain(&paths.payload().join("codex"), &paths.live_codex(), &codex, &mut report)?;
    // zcode：AGENTS.md / agents/ 同路径；skills-real/<name> → skills/<name>
    walk_zcode(paths, &zcode, &mut report)?;
    Ok(report)
}

fn walk_plain(
    src_root: &Path,
    dst_root: &Path,
    spec: &TreeSpec,
    report: &mut AuditReport,
) -> Result<()> {
    if !src_root.exists() {
        return Ok(());
    }
    for entry in WalkDir::new(src_root).follow_links(false) {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }
        let rel = match entry.path().strip_prefix(src_root) {
            Ok(r) if !r.as_os_str().is_empty() => r,
            _ => continue,
        };
        if !exclude::allows(spec, rel) {
            continue;
        }
        classify(entry.path(), &dst_root.join(rel), report);
    }
    Ok(())
}

fn walk_zcode(paths: &SyncPaths, spec: &TreeSpec, report: &mut AuditReport) -> Result<()> {
    let src_root = paths.payload().join("zcode");
    let dst_root = paths.live_zcode();
    if !src_root.exists() {
        return Ok(());
    }
    // AGENTS.md 与 agents/
    for rel in ["AGENTS.md"] {
        if src_root.join(rel).is_file() {
            classify(&src_root.join(rel), &dst_root.join(rel), report);
        }
    }
    let agents_dir = src_root.join("agents");
    if agents_dir.is_dir() {
        for entry in WalkDir::new(&agents_dir).follow_links(false) {
            let entry = entry?;
            if !entry.file_type().is_file() {
                continue;
            }
            let rel = entry.path().strip_prefix(&src_root)?;
            if !exclude::allows(spec, rel) {
                continue;
            }
            classify(entry.path(), &dst_root.join(rel), report);
        }
    }
    // skills-real/<name>/** → skills/<name>/**
    let skills = src_root.join("skills-real");
    if skills.is_dir() {
        for entry in std::fs::read_dir(&skills)?.filter_map(Result::ok) {
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let name = entry.file_name();
            let dst_skill = dst_root.join("skills").join(&name);
            // 活目录是软链（农场形态）：内容不归这里管
            if std::fs::symlink_metadata(&dst_skill)
                .map(|m| m.file_type().is_symlink())
                .unwrap_or(false)
            {
                report.skipped_symlink += 1;
                continue;
            }
            for sub in WalkDir::new(entry.path()).follow_links(false) {
                let sub = sub?;
                if !sub.file_type().is_file() {
                    continue;
                }
                let rel = sub.path().strip_prefix(entry.path())?;
                classify(sub.path(), &dst_skill.join(rel), report);
            }
        }
    }
    Ok(())
}

fn classify(src: &Path, dst: &Path, report: &mut AuditReport) {
    let Ok(meta) = std::fs::symlink_metadata(dst) else {
        report.create.push(Entry {
            path: dst.to_path_buf(),
            local_mtime: None,
        });
        return;
    };
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs());
    if meta.is_file() && same_content(src, dst) {
        report.identical += 1;
    } else {
        report.overwrite.push(Entry {
            path: dst.to_path_buf(),
            local_mtime: mtime,
        });
    }
}

fn same_content(a: &Path, b: &Path) -> bool {
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(ma), Ok(mb)) if ma.len() == mb.len() => {
            matches!((std::fs::read(a), std::fs::read(b)), (Ok(x), Ok(y)) if x == y)
        }
        _ => false,
    }
}
