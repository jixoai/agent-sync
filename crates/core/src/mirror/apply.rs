//! 意图（2026-11-24）：把 payload 安全地落回活目录。
//!   活目录里有 21G 运行态、凭证目录与三个嵌套 git 仓 —— 任何「一把梭解包」
//!   都会造成不可逆损失。故本层只做三种保守动作：先快照、逐路径写入、受管范围内删除。
//!
//! 本文件职责（4 个正交意图）：
//!   1. 写前快照（可回滚），并按份数上限清理旧快照
//!   2. 三棵树的逐路径落盘（zcode 树绝不整体覆盖）
//!   3. 可选 prune：只删「受管范围内且 payload 已不存在」的文件
//!   4. 符号链接重建（委托 links 层）
//! 不承载：采集（capture）、合并裁决（sync）。

use super::copy_file_if_changed;
use crate::exclude;
use crate::settings::{Settings, TreeSpec};
use crate::links::{self, LinksDoc};
use crate::paths::SyncPaths;
use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

/// apply 的行为开关。
#[derive(Debug, Clone, Default)]
pub struct ApplyOptions {
    /// 删除 payload 中已不存在的受管文件。
    ///
    /// 生产路径（daemon / CLI）开启：镜像的语义就是「payload 是唯一真值」，
    /// 关掉它会让对端的删除永远传播不过来（实测：删 4 个文件，另一端纹丝不动）。
    /// 安全性由三件事共同保证：写入前快照、prune 只碰受管路径、
    /// 以及 `not_after` 竞态护栏。测试里仍保留关闭路径以便观察。
    pub prune: bool,
    /// 快照保留份数。
    pub backup_keep: usize,
    /// 只覆盖「修改时间早于该时刻」的文件。
    ///
    /// 竞态背景：一次同步是「采集 → 提交 → 拉取 → 落盘」。若用户在采集之后、
    /// 落盘之前编辑了文件，落盘会把他的改动覆盖回旧内容，而且这次改动
    /// **从未被提交**——表现为「改了但没同步出去」，实测跨机反向就是如此。
    /// 传 `Some(同步开始时刻)` 即可让落盘跳过正在被编辑的文件。
    pub not_after: Option<std::time::SystemTime>,
}

/// 一次落盘的结果。
#[derive(Debug, Default)]
pub struct ApplyReport {
    pub files_written: usize,
    pub files_unchanged: usize,
    pub files_removed: usize,
    pub snapshot: Option<PathBuf>,
    pub links_created: usize,
    pub links_skipped: Vec<String>,
    pub notes: Vec<String>,
}

/// 落盘全流程。任何一步失败即中止（不做半应用）。
pub fn apply_all(paths: &SyncPaths, opts: &ApplyOptions) -> Result<ApplyReport> {
    let mut report = ApplyReport::default();

    // 先快照：只快照会被改动的区域，避免把 21G 运行态卷进去
    let ts = timestamp();
    report.snapshot = Some(snapshot(paths, &ts, opts.backup_keep)?);

    let settings = Settings::load(&paths.root)?;
    let agents = settings.tree("agents").cloned().unwrap_or_else(|| {
        Settings::default().tree("agents").expect("默认配置").clone()
    });
    let zcode = settings.tree("zcode").cloned().unwrap_or_else(|| {
        Settings::default().tree("zcode").expect("默认配置").clone()
    });
    // 暂停的树：活目录完全不动（不写、不删）。它在 payload 里的内容保留，
    // 只是不再参与本机的落盘——这是「本机暂停」而不是「删除」。
    if settings.is_tree_enabled("agents") {
        apply_agents(paths, &agents, opts, &mut report)?;
    }
    if settings.is_tree_enabled("zcode") {
        apply_zcode(paths, &zcode, opts, &mut report)?;
    }
    if settings.is_tree_enabled("codex") {
        apply_codex(paths, &mut report)?;
    }

    let doc = LinksDoc::load(&paths.links_json())?;
    let roots: BTreeMap<&str, PathBuf> = BTreeMap::from([
        ("agents", paths.live_agents()),
        ("zcode", paths.live_zcode()),
    ]);
    let rb = links::rebuild(&doc, &paths.home, &roots);
    report.links_created = rb.created;
    report.links_skipped.extend(rb.skipped_existing);
    report.links_skipped.extend(rb.skipped_broken);
    report.notes.extend(rb.external.iter().map(|e| format!("外部链接：{e}")));
    report.notes.extend(rb.errors);

    Ok(report)
}

/// agents 树：payload 是镜像，可 prune；但仓库自身、独立 git 仓与运行态永不删。
fn apply_agents(paths: &SyncPaths, spec: &TreeSpec, opts: &ApplyOptions, report: &mut ApplyReport) -> Result<()> {
    let src_root = paths.payload().join("agents");
    let dst_root = paths.live_agents();
    if !src_root.exists() {
        return Ok(());
    }
    std::fs::create_dir_all(&dst_root)?;

    // 写：payload → 活
    for entry in WalkDir::new(&src_root).follow_links(false) {
        let entry = entry?;
        let rel = entry.path().strip_prefix(&src_root)?;
        if rel.as_os_str().is_empty() {
            continue;
        }
        if !exclude::allows(spec, rel) {
            continue;
        }
        let dst = dst_root.join(rel);
        if entry.file_type().is_dir() {
            std::fs::create_dir_all(&dst)?;
        } else if entry.file_type().is_file() {
            if skip_if_just_edited(opts, &dst, report) {
                continue;
            }
            if copy_file_if_changed(entry.path(), &dst)? {
                report.files_written += 1;
            } else {
                report.files_unchanged += 1;
            }
        }
    }

    if opts.prune {
        prune_tree(spec, &dst_root, &src_root, opts, report)?;
    }
    Ok(())
}

/// 目标文件若在本轮同步开始之后被修改过，视为「用户正在编辑」，跳过覆盖。
fn skip_if_just_edited(opts: &ApplyOptions, dst: &Path, report: &mut ApplyReport) -> bool {
    let Some(since) = opts.not_after else {
        return false;
    };
    let Ok(meta) = std::fs::metadata(dst) else {
        return false;
    };
    let Ok(mtime) = meta.modified() else {
        return false;
    };
    if mtime > since {
        report
            .notes
            .push(format!("跳过（本轮开始后被编辑）：{}", dst.display()));
        true
    } else {
        false
    }
}

/// zcode 树：只碰三处（AGENTS.md、agents/、skills-real/）。
/// 绝不 rsync 到 `~/.zcode` 根 —— 那里有 cli/ v2/ 等运行态与凭证。
fn apply_zcode(paths: &SyncPaths, spec: &TreeSpec, opts: &ApplyOptions, report: &mut ApplyReport) -> Result<()> {
    let src_root = paths.payload().join("zcode");
    let dst_root = paths.live_zcode();
    if !src_root.exists() {
        return Ok(());
    }
    std::fs::create_dir_all(&dst_root)?;

    // AGENTS.md
    let src_md = src_root.join("AGENTS.md");
    if src_md.is_file() {
        if copy_file_if_changed(&src_md, &dst_root.join("AGENTS.md"))? {
            report.files_written += 1;
        } else {
            report.files_unchanged += 1;
        }
    }

    // agents/：整目录同步（这是我们的受管区，可以删多余文件）
    let src_agents = src_root.join("agents");
    if src_agents.is_dir() {
        let dst_agents = dst_root.join("agents");
        std::fs::create_dir_all(&dst_agents)?;
        sync_dir(&src_agents, &dst_agents, report, true)?;
    }

    // skills-real/<name>/：逐目录同步。
    // 真实目录才写；软链农场由 links 层重建，不能被当成「多余文件」删掉。
    let src_skills = src_root.join("skills-real");
    if src_skills.is_dir() {
        for entry in std::fs::read_dir(&src_skills)?.filter_map(Result::ok) {
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let name = entry.file_name();
            let dst_skill = dst_root.join("skills").join(&name);
            // 目标若是软链（农场形态），保留链接，不覆盖成目录
            if std::fs::symlink_metadata(&dst_skill)
                .map(|m| m.file_type().is_symlink())
                .unwrap_or(false)
            {
                report
                    .notes
                    .push(format!("保留软链 {}", dst_skill.display()));
                continue;
            }
            std::fs::create_dir_all(&dst_skill)?;
            sync_dir(&entry.path(), &dst_skill, report, false)?;
        }
    }
    if opts.prune {
        // zcode 只对受管的三处做 prune，且只删文件不删目录。
        // 关键映射：活目录 `skills/<name>/...` ↔ payload `skills-real/<name>/...`；
        // 软链农场（skills 下的符号链接）不归这里管，交给 links 层。
        prune_tree_mapped(spec, &dst_root, &src_root, opts, report, &|rel| {
            let s = rel.to_string_lossy();
            if let Some(rest) = s.strip_prefix("skills/") {
                let name = rest.split('/').next().unwrap_or("");
                let live_skill = dst_root.join("skills").join(name);
                if std::fs::symlink_metadata(&live_skill)
                    .map(|m| m.file_type().is_symlink())
                    .unwrap_or(false)
                {
                    return None; // 软链农场：不是受管内容
                }
                Some(PathBuf::from(format!("skills-real/{rest}")))
            } else {
                Some(rel.to_path_buf())
            }
        })?;
    }
    Ok(())
}

/// codex 树：三个文件，逐个覆盖。
fn apply_codex(paths: &SyncPaths, report: &mut ApplyReport) -> Result<()> {
    let src_root = paths.payload().join("codex");
    let dst_root = paths.live_codex();
    if !src_root.exists() {
        return Ok(());
    }
    for rel in ["AGENTS.md", "git-committer.md", "rules/default.rules"] {
        let src = src_root.join(rel);
        if !src.is_file() {
            continue;
        }
        if copy_file_if_changed(&src, &dst_root.join(rel))? {
            report.files_written += 1;
        } else {
            report.files_unchanged += 1;
        }
    }
    Ok(())
}

/// 目录级同步：把 src 写进 dst；`delete_extra` 为真时删掉 dst 里 src 没有的文件。
fn sync_dir(
    src: &Path,
    dst: &Path,
    report: &mut ApplyReport,
    delete_extra: bool,
) -> Result<()> {
    let mut wanted: Vec<PathBuf> = Vec::new();
    for entry in WalkDir::new(src).follow_links(false) {
        let entry = entry?;
        let rel = entry.path().strip_prefix(src)?;
        if rel.as_os_str().is_empty() {
            continue;
        }
        let target = dst.join(rel);
        if entry.file_type().is_dir() {
            std::fs::create_dir_all(&target)?;
        } else if entry.file_type().is_file() {
            wanted.push(rel.to_path_buf());
            if copy_file_if_changed(entry.path(), &target)? {
                report.files_written += 1;
            } else {
                report.files_unchanged += 1;
            }
        }
    }

    if delete_extra {
        for entry in WalkDir::new(dst).follow_links(false).contents_first(true) {
            let entry = entry?;
            if entry.file_type().is_symlink() {
                continue;
            }
            let rel = entry.path().strip_prefix(dst)?;
            if rel.as_os_str().is_empty() {
                continue;
            }
            if entry.file_type().is_file() && !wanted.iter().any(|w| w == rel) {
                std::fs::remove_file(entry.path())?;
                report.files_removed += 1;
            }
        }
    }
    Ok(())
}

/// 受管范围内的 prune：只删「活目录有、payload 没有」的受管文件。
/// 这保证了 `sync/`、`references/<repo>/`、`cli/`、`v2/` 等非受管区域永不被碰。
fn prune_tree(
    spec: &TreeSpec,
    live_root: &Path,
    payload_root: &Path,
    opts: &ApplyOptions,
    report: &mut ApplyReport,
) -> Result<()> {
    prune_tree_mapped(spec, live_root, payload_root, opts, report, &|rel| {
        Some(rel.to_path_buf())
    })
}

/// 带路径映射的 prune。
///
/// 为什么需要映射：zcode 树的活目录是 `skills/<name>/...`，而 payload 里叫
/// `skills-real/<name>/...`。按字面路径去查会「对不上」→ 把刚写进去的文件
/// 全删掉（实测 103 个 skill 文件每次同步被删一次）。映射函数负责把活目录的
/// 相对路径翻译成 payload 的相对路径；返回 `None` 表示不归 prune 管。
fn prune_tree_mapped(
    spec: &TreeSpec,
    live_root: &Path,
    payload_root: &Path,
    opts: &ApplyOptions,
    report: &mut ApplyReport,
    map: &dyn Fn(&Path) -> Option<PathBuf>,
) -> Result<()> {
    let mut to_remove: Vec<PathBuf> = Vec::new();
    for entry in WalkDir::new(live_root).follow_links(false) {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }
        let rel = match entry.path().strip_prefix(live_root) {
            Ok(r) if !r.as_os_str().is_empty() => r,
            _ => continue,
        };
        // 关键：只处理受管路径。非受管（仓库自身 / 运行态 / 凭证）一律跳过。
        if !exclude::allows(spec, rel) {
            continue;
        }
        let Some(payload_rel) = map(rel) else {
            continue;
        };
        let counter = payload_root.join(payload_rel);
        if !counter.exists() {
            // 与覆盖共用同一护栏：本轮开始后被编辑的文件可能是用户刚新建、
            // 尚未被采集的。删掉它就是丢数据，宁可留到下一轮。
            if skip_if_just_edited(opts, entry.path(), report) {
                continue;
            }
            to_remove.push(entry.path().to_path_buf());
        }
    }
    for path in to_remove {
        std::fs::remove_file(&path)
            .with_context(|| format!("prune 删除 {}", path.display()))?;
        report.files_removed += 1;
    }
    Ok(())
}

/// 快照：只打包会被改动的区域，tar 到 `backups/<ts>.tar.gz`，再按份数清理。
fn snapshot(paths: &SyncPaths, ts: &str, keep: usize) -> Result<PathBuf> {
    let backups = paths.backups_dir();
    std::fs::create_dir_all(&backups)?;
    let arc = backups.join(format!("{ts}.tar.gz"));

    let targets: Vec<&str> = [
        ".agents/AGENTS.md",
        ".agents/prompts",
        ".agents/skills",
        ".agents/documents",
        ".zcode/AGENTS.md",
        ".zcode/agents",
        ".zcode/skills",
        ".codex/AGENTS.md",
        ".codex/git-committer.md",
    ]
    .into_iter()
    .filter(|rel| paths.home.join(rel).exists())
    .collect();

    if !targets.is_empty() {
        // 必须排除证据/临时目录：`~/.agents/skills/world-class-designer/tmp`
        // 有 1.3G，卷进快照会让每次 apply 产出 1.1G 的 tar（实测）。
        let status = std::process::Command::new("/usr/bin/tar")
            .arg("-czf")
            .arg(&arc)
            .arg("-C")
            .arg(&paths.home)
            .args(["--exclude", "*/tmp", "--exclude", "*/.git", "--exclude", "*/node_modules"])
            .args(&targets)
            .status()
            .context("执行 tar 快照")?;
        if !status.success() {
            anyhow::bail!("tar 快照失败（退出码 {:?}）", status.code());
        }
    } else {
        // 全新机器：没有可快照的内容，写一个空归档保持「每轮都有快照」的语义
        std::fs::write(&arc, b"")?;
    }

    prune_snapshots(&backups, keep)?;
    Ok(arc)
}

/// 保留最近 `keep` 份快照（按文件名里的时间戳排序，即按时间）。
fn prune_snapshots(dir: &Path, keep: usize) -> Result<()> {
    let mut arcs: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().map(|e| e == "gz").unwrap_or(false))
        .collect();
    arcs.sort();
    if arcs.len() > keep {
        for old in &arcs[..arcs.len() - keep] {
            let _ = std::fs::remove_file(old);
        }
    }
    Ok(())
}

/// 从快照回滚（整份解包到 HOME）。
pub fn rollback(paths: &SyncPaths, ts: &str) -> Result<()> {
    let arc = paths.backups_dir().join(format!("{ts}.tar.gz"));
    if !arc.exists() {
        anyhow::bail!("找不到快照 {}", arc.display());
    }
    let status = std::process::Command::new("/usr/bin/tar")
        .arg("-xzf")
        .arg(&arc)
        .arg("-C")
        .arg(&paths.home)
        .status()
        .context("执行 tar 回滚")?;
    if !status.success() {
        anyhow::bail!("回滚失败（退出码 {:?}）", status.code());
    }
    Ok(())
}

fn timestamp() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = secs / 86_400;
    let rem = secs % 86_400;
    let (y, m, d) = civil_from_days(days as i64);
    format!(
        "{y:04}{m:02}{d:02}T{:02}{:02}{:02}",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::super::is_real_dir;
    use super::*;
    use crate::config::SyncConf;
    use crate::mirror::capture::capture_all;
    use std::os::unix::fs::symlink;

    fn seed(home: &Path) -> SyncPaths {
        let p = SyncPaths::new(home);
        std::fs::create_dir_all(p.live_agents().join("prompts")).unwrap();
        std::fs::create_dir_all(p.live_zcode().join("agents")).unwrap();
        std::fs::create_dir_all(p.live_zcode().join("skills/real-skill")).unwrap();
        std::fs::create_dir_all(p.live_zcode().join("cli")).unwrap();
        std::fs::create_dir_all(p.live_zcode().join("v2")).unwrap();
        std::fs::create_dir_all(p.root.join("bin")).unwrap();

        std::fs::write(p.live_agents().join("AGENTS.md"), "# 来源机器\n").unwrap();
        std::fs::write(p.live_agents().join("prompts/x.md"), "prompt\n").unwrap();
        std::fs::write(p.live_zcode().join("AGENTS.md"), "# zcode\n").unwrap();
        std::fs::write(p.live_zcode().join("agents/slow.md"), "slow\n").unwrap();
        std::fs::write(p.live_zcode().join("skills/real-skill/SKILL.md"), "real\n").unwrap();
        std::fs::write(p.live_zcode().join("cli/cache.bin"), "cache\n").unwrap();
        std::fs::write(p.live_zcode().join("v2/setting.json"), "{\"passwordCredentialKey\":\"x\"}\n").unwrap();
        std::fs::write(p.root.join("bin/tool"), "tool\n").unwrap();
        p
    }

    #[test]
    fn apply_restores_content_into_empty_home() {
        let src = tempfile::tempdir().unwrap();
        let sp = seed(src.path());
        capture_all(&sp, &SyncConf::default()).unwrap();

        // 目标机：全新 HOME，只有运行态与仓库自身
        let dst = tempfile::tempdir().unwrap();
        let dp = SyncPaths::new(dst.path());
        std::fs::create_dir_all(dp.root.join("bin")).unwrap();
        std::fs::write(dp.root.join("bin/tool"), "tool\n").unwrap();
        std::fs::create_dir_all(dp.live_zcode().join("cli")).unwrap();
        std::fs::write(dp.live_zcode().join("cli/other.bin"), "keep me\n").unwrap();
        std::fs::create_dir_all(dp.live_zcode().join("v2")).unwrap();
        std::fs::write(dp.live_zcode().join("v2/setting.json"), "local creds\n").unwrap();

        // 把来源的 payload 搬到目标（模拟 fetch 后的状态）
        copy_dir_all(&sp.payload(), &dp.payload()).unwrap();

        let report = apply_all(&dp, &ApplyOptions { prune: false, backup_keep: 5, not_after: None }).unwrap();
        assert!(report.files_written > 0);
        assert!(dp.live_agents().join("AGENTS.md").exists());
        assert!(dp.live_zcode().join("agents/slow.md").exists());
        assert!(dp.live_zcode().join("skills/real-skill/SKILL.md").exists());

        // 运行态与仓库自身不得被破坏
        assert!(dp.live_zcode().join("cli/other.bin").exists(), "运行态必须保留");
        assert_eq!(
            std::fs::read_to_string(dp.live_zcode().join("v2/setting.json")).unwrap(),
            "local creds\n",
            "凭证文件不得被覆盖（它不在 payload 里）"
        );
        assert!(dp.root.join("bin/tool").exists(), "仓库自身必须保留");
    }

    #[test]
    fn apply_prune_removes_only_managed_stale_files() {
        let src = tempfile::tempdir().unwrap();
        let sp = seed(src.path());
        capture_all(&sp, &SyncConf::default()).unwrap();

        let dst = tempfile::tempdir().unwrap();
        let dp = SyncPaths::new(dst.path());
        copy_dir_all(&sp.payload(), &dp.payload()).unwrap();
        apply_all(&dp, &ApplyOptions { prune: false, backup_keep: 5, not_after: None }).unwrap();

        // 目标机自己多出来的受管文件（应被 prune）与非受管文件（应保留）
        std::fs::write(dp.live_agents().join("prompts/stale.md"), "stale\n").unwrap();
        std::fs::write(dp.live_agents().join("local-only-notes.md"), "stale\n").unwrap();
        std::fs::create_dir_all(dp.root.join("bin")).unwrap();
        std::fs::write(dp.root.join("bin/tool"), "tool\n").unwrap();

        let report = apply_all(&dp, &ApplyOptions { prune: true, backup_keep: 5, not_after: None }).unwrap();
        assert!(report.files_removed >= 1);
        assert!(!dp.live_agents().join("prompts/stale.md").exists(), "受管陈旧文件应删除");
        assert!(dp.root.join("bin/tool").exists(), "仓库自身永不被 prune");
    }

    /// zcode 树的 prune 必须做路径映射，否则会把刚写入的 skill 全删掉。
    ///
    /// 事故现场：活目录 `~/.zcode/skills/<name>/...` 在 payload 里叫
    /// `skills-real/<name>/...`，按字面路径查不到就判成「陈旧」→
    /// 每次同步删 103 个文件。软链农场则完全不该参与 prune。
    #[test]
    fn apply_prune_maps_zcode_skills_to_skills_real() {
        let src = tempfile::tempdir().unwrap();
        let sp = seed(src.path());
        capture_all(&sp, &SyncConf::default()).unwrap();

        let dst = tempfile::tempdir().unwrap();
        let dp = SyncPaths::new(dst.path());
        copy_dir_all(&sp.payload(), &dp.payload()).unwrap();
        std::fs::create_dir_all(dp.live_zcode().join("skills")).unwrap();

        let report = apply_all(&dp, &ApplyOptions { prune: true, backup_keep: 5, not_after: None }).unwrap();

        assert!(
            dp.live_zcode().join("skills/real-skill/SKILL.md").exists(),
            "真实 skill 目录在 prune 下必须保留（payload 里有 skills-real 对应项）"
        );
        assert!(report.files_removed == 0, "没有陈旧文件时不该删任何东西：{report:?}");

        // 真正的陈旧 skill 文件（payload 里没有）才该删
        std::fs::write(dp.live_zcode().join("skills/real-skill/stale.md"), "old\n").unwrap();
        let report = apply_all(&dp, &ApplyOptions { prune: true, backup_keep: 5, not_after: None }).unwrap();
        assert_eq!(report.files_removed, 1);
        assert!(!dp.live_zcode().join("skills/real-skill/stale.md").exists());
        assert!(dp.live_zcode().join("skills/real-skill/SKILL.md").exists());
    }

    /// prune 与覆盖共用 `not_after` 护栏：同步窗口内刚新建、尚未被采集的文件
    /// 不能被「payload 里没有」这个事实杀掉。实测这类竞态会直接丢用户数据。
    #[test]
    fn apply_prune_spares_files_edited_during_sync() {
        let src = tempfile::tempdir().unwrap();
        let sp = seed(src.path());
        capture_all(&sp, &SyncConf::default()).unwrap();

        let dst = tempfile::tempdir().unwrap();
        let dp = SyncPaths::new(dst.path());
        copy_dir_all(&sp.payload(), &dp.payload()).unwrap();
        apply_all(&dp, &ApplyOptions { prune: false, backup_keep: 5, not_after: None }).unwrap();

        // 先建一个「本轮开始前就存在」的陈旧文件，再建一个「本轮开始后」的
        std::fs::write(dp.live_agents().join("prompts/old-stale.md"), "old\n").unwrap();
        let since = std::time::SystemTime::now();
        std::thread::sleep(std::time::Duration::from_millis(1100));
        std::fs::write(dp.live_agents().join("prompts/just-typed.md"), "new\n").unwrap();

        let report = apply_all(
            &dp,
            &ApplyOptions { prune: true, backup_keep: 5, not_after: Some(since) },
        )
        .unwrap();
        assert!(report.files_removed >= 1);
        assert!(!dp.live_agents().join("prompts/old-stale.md").exists());
        assert!(
            dp.live_agents().join("prompts/just-typed.md").exists(),
            "同步窗口内新建的文件必须保留到下一轮"
        );
    }

    #[test]
    fn apply_writes_snapshot_and_can_rollback() {
        let src = tempfile::tempdir().unwrap();
        let sp = seed(src.path());
        capture_all(&sp, &SyncConf::default()).unwrap();

        let dst = tempfile::tempdir().unwrap();
        let dp = SyncPaths::new(dst.path());
        std::fs::create_dir_all(dp.live_agents()).unwrap();
        std::fs::write(dp.live_agents().join("AGENTS.md"), "# 原来的\n").unwrap();
        copy_dir_all(&sp.payload(), &dp.payload()).unwrap();

        let report = apply_all(&dp, &ApplyOptions { prune: false, backup_keep: 5, not_after: None }).unwrap();
        let arc = report.snapshot.expect("必须产生快照");
        assert!(arc.exists());

        // 覆盖后回滚应恢复原文
        assert!(std::fs::read_to_string(dp.live_agents().join("AGENTS.md")).unwrap().contains("来源机器"));
        let ts = arc.file_stem().unwrap().to_string_lossy().to_string();
        let ts = ts.trim_end_matches(".tar").to_string();
        rollback(&dp, &ts).unwrap();
        assert!(
            std::fs::read_to_string(dp.live_agents().join("AGENTS.md")).unwrap().contains("原来的"),
            "回滚必须恢复快照内容"
        );
    }

    #[test]
    fn apply_never_overwrites_symlink_farm_with_directory() {
        let src = tempfile::tempdir().unwrap();
        let sp = sync_paths_with_real_skill(src.path());
        capture_all(&sp, &SyncConf::default()).unwrap();

        let dst = tempfile::tempdir().unwrap();
        let dp = SyncPaths::new(dst.path());
        std::fs::create_dir_all(dp.live_zcode().join("skills")).unwrap();
        std::fs::create_dir_all(dp.live_agents().join("skills/real-skill")).unwrap();
        std::fs::write(dp.live_agents().join("skills/real-skill/SKILL.md"), "real\n").unwrap();
        symlink("../../.agents/skills/real-skill", dp.live_zcode().join("skills/real-skill")).unwrap();
        copy_dir_all(&sp.payload(), &dp.payload()).unwrap();

        apply_all(&dp, &ApplyOptions { prune: false, backup_keep: 5, not_after: None }).unwrap();
        let meta = std::fs::symlink_metadata(dp.live_zcode().join("skills/real-skill")).unwrap();
        assert!(meta.file_type().is_symlink(), "农场软链不得被替换成真实目录");
    }

    fn sync_paths_with_real_skill(home: &Path) -> SyncPaths {
        let p = SyncPaths::new(home);
        std::fs::create_dir_all(p.live_agents().join("skills/real-skill")).unwrap();
        std::fs::write(p.live_agents().join("skills/real-skill/SKILL.md"), "real\n").unwrap();
        p
    }

    fn copy_dir_all(src: &Path, dst: &Path) -> Result<()> {
        for entry in WalkDir::new(src).follow_links(false) {
            let entry = entry?;
            let rel = entry.path().strip_prefix(src)?;
            if rel.as_os_str().is_empty() {
                continue;
            }
            let target = dst.join(rel);
            if entry.file_type().is_dir() {
                std::fs::create_dir_all(&target)?;
            } else if entry.file_type().is_file() {
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::copy(entry.path(), &target)?;
            }
        }
        Ok(())
    }

    #[test]
    fn real_dir_detection_does_not_follow_links() {
        let d = tempfile::tempdir().unwrap();
        let real = d.path().join("real");
        std::fs::create_dir_all(&real).unwrap();
        let link = d.path().join("link");
        symlink(&real, &link).unwrap();
        assert!(is_real_dir(&real));
        assert!(!is_real_dir(&link), "指向目录的软链不算真实目录");
    }

}
