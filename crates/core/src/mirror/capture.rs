//! 意图（2026-11-24）：把三棵活目录翻译成 payload 镜像。
//!   没有现成工具能做这件事：文件同步器按文件粒度搬运活目录，会带走软链农场、
//!   1.3G 证据目录与凭证；而我们要的是「有取舍的翻译」。
//!
//! 本文件职责（4 个正交意图）：
//!   1. 三棵树各自的采集策略（agents 全量+排除；zcode 白名单；codex 三文件）
//!   2. 符号链接只登记定义、不复制本体
//!   3. 凭据硬门（命中即中止，绝不提交）
//!   4. 采集结果的自述（树哈希 + 本机采集集），供 status 与对端比对
//! 不承载：落盘方向（apply）、版本控制（vcs）。
//!
//! 关键语义（踩坑后修正）：**「本机没有」不等于「应当删除」**。
//!   payload 在 merge 之后会包含对端独有的文件。如果采集时「活目录没有就清掉」，
//!   等于替对端声明删除——实测把对端独有的 103 个文件从两端一起抹掉。
//!   删除只能由「本机上次亲自采集过、这次没有了」证明。

use super::{copy_file, copy_file_if_changed};
use crate::checks;
use crate::config::SyncConf;
use crate::exclude;
use crate::settings::{Settings, TreeSpec};
use crate::links::{self, LinksDoc};
use crate::paths::SyncPaths;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

/// 一次采集的结果。
#[derive(Debug, Default)]
pub struct CaptureReport {
    pub files_copied: usize,
    pub files_unchanged: usize,
    pub links: usize,
    pub broken_links: usize,
    pub tree_hash: String,
}

/// 本机采集状态：每棵树**本机亲自复制进 payload** 的文件集合（payload 相对路径）。
///
/// 它是删除传播的唯一依据。没有它就只能靠「payload 里没有」猜删除，
/// 而那会把对端独特的文件一并抹掉（实测）。
#[derive(Debug, Default, Serialize, Deserialize)]
struct CaptureState {
    #[serde(default)]
    device: String,
    #[serde(default)]
    captured_at: String,
    #[serde(default)]
    tree_hash: String,
    #[serde(default)]
    trees: BTreeMap<String, BTreeSet<String>>,
}

impl CaptureState {
    fn load(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }
}


/// 采集全部三棵树。任何一步失败都中止（半份 payload 比没有更危险）。
pub fn capture_all(paths: &SyncPaths, _conf: &SyncConf) -> Result<CaptureReport> {
    let mut report = CaptureReport::default();

    // 目录映射与规则来自配置（默认配置等价于原先写死的三棵树）
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

    // 上一轮「本机亲自采集」的集合：唯一的删除依据
    let prev = CaptureState::load(&paths.capture_state());
    let mut next = CaptureState::default();

    // 暂停的树：不采集，也不动它的 payload 与采集集（重新启用后不会丢）。
    macro_rules! tree {
        ($name:literal, $spec:expr, $f:ident) => {
            if settings.is_tree_enabled($name) {
                $f(paths, $spec, &prev, &mut next, &mut report)?;
            } else if let Some(set) = prev.trees.get($name) {
                next.trees.insert($name.into(), set.clone());
            }
        };
    }
    tree!("agents", &agents, capture_agents);
    tree!("zcode", &zcode, capture_zcode);
    tree!("codex", &codex, capture_codex);
    collect_links(paths, &mut report)?;

    // 硬门：凭据不进仓库
    let hits = checks::scan(&paths.payload(), &[])?;
    if !hits.is_empty() {
        let list = hits
            .iter()
            .take(10)
            .map(|h| format!("  {h}"))
            .collect::<Vec<_>>()
            .join("\n");
        anyhow::bail!(
            "密钥门拦截：以下文件疑似含凭据，未提交\n{list}\n（共 {} 处）",
            hits.len()
        );
    }

    report.tree_hash = checks::tree_hash(&paths.payload())?;
    write_state(paths, &report.tree_hash, &next)?;
    Ok(report)
}

/// 记录一个 payload 相对路径（统一用 `/` 分隔，跨平台稳定）。
fn key(dst_root: &Path, dst: &Path) -> Option<String> {
    dst.strip_prefix(dst_root)
        .ok()
        .map(|r| r.to_string_lossy().replace('\\', "/"))
}

/// agents 树：`~/.agents` 全量镜像，但受排除规则约束。
/// `sync/` 必须排除：它就在这棵树里，否则会把仓库复制进自己的 payload。
fn capture_agents(
    paths: &SyncPaths,
    spec: &TreeSpec,
    _prev: &CaptureState,
    next: &mut CaptureState,
    report: &mut CaptureReport,
) -> Result<()> {
    let src_root = paths.live_agents();
    let dst_root = paths.payload().join("agents");
    if !src_root.exists() {
        return Ok(());
    }
    std::fs::create_dir_all(&dst_root)?;

    let mut seen: BTreeSet<String> = BTreeSet::new();
    for entry in WalkDir::new(&src_root).follow_links(false) {
        let entry = entry.with_context(|| format!("遍历 {}", src_root.display()))?;
        let src = entry.path();
        let rel = match src.strip_prefix(&src_root) {
            Ok(r) if !r.as_os_str().is_empty() => r,
            _ => continue,
        };
        if !exclude::allows(spec, rel) {
            continue;
        }
        let dst = dst_root.join(rel);
        let ft = entry.file_type();
        if ft.is_dir() {
            std::fs::create_dir_all(&dst)?;
        } else if ft.is_symlink() {
            // 只登记定义，本体由 links 层负责；镜像里不出现链接
            continue;
        } else if ft.is_file() {
            if let Some(k) = key(&dst_root, &dst) {
                seen.insert(k);
            }
            if copy_file_if_changed(src, &dst)? {
                report.files_copied += 1;
            } else {
                report.files_unchanged += 1;
            }
        }
    }

    // 删除不传播：payload 是「各设备内容的并集」。
    //
    // 独立快照 + jj 三方合并里，「一端删除、另一端恢复相同内容」与「从未改动」
    // 无法区分，删除会反复复活或误删（实测丢过 103 个文件，且两端来回抖动）。
    // 采集集仍记录（doctor 与将来显式删除命令会用到），但这里绝不动 payload。
    next.trees.insert("agents".into(), seen);
    Ok(())
}

/// zcode 树：白名单。
///   真实目录的 skills（如 agent-orchestration）复制到 `skills-real/<name>`；
///   软链农场只登记，不复制。
fn capture_zcode(
    paths: &SyncPaths,
    spec: &TreeSpec,
    _prev: &CaptureState,
    next: &mut CaptureState,
    report: &mut CaptureReport,
) -> Result<()> {
    let src_root = paths.live_zcode();
    let dst_root = paths.payload().join("zcode");
    if !src_root.exists() {
        return Ok(());
    }
    std::fs::create_dir_all(&dst_root)?;
    let mut seen: BTreeSet<String> = BTreeSet::new();

    // AGENTS.md
    let agents_md = src_root.join("AGENTS.md");
    if agents_md.is_file() {
        let dst = dst_root.join("AGENTS.md");
        seen.insert("AGENTS.md".into());
        if copy_file_if_changed(&agents_md, &dst)? {
            report.files_copied += 1;
        } else {
            report.files_unchanged += 1;
        }
    }

    // agents/*.md
    let agents_dir = src_root.join("agents");
    if agents_dir.is_dir() {
        for entry in WalkDir::new(&agents_dir).follow_links(false) {
            let entry = entry?;
            if !entry.file_type().is_file() || entry.file_type().is_symlink() {
                continue;
            }
            let rel = entry.path().strip_prefix(&src_root)?;
            if !exclude::allows(spec, rel) {
                continue;
            }
            let dst = dst_root.join(rel);
            if let Some(k) = key(&dst_root, &dst) {
                seen.insert(k);
            }
            if copy_file_if_changed(entry.path(), &dst)? {
                report.files_copied += 1;
            } else {
                report.files_unchanged += 1;
            }
        }
    }

    // skills：只有真实目录要复制内容；软链留给 links 层
    let skills_dir = src_root.join("skills");
    if skills_dir.is_dir() {
        for entry in std::fs::read_dir(&skills_dir)?.filter_map(Result::ok) {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') || name.contains(".bak-") {
                continue;
            }
            let ft = entry.file_type()?;
            if ft.is_symlink() || !ft.is_dir() {
                continue;
            }
            // 真实目录 → skills-real/<name>
            let dst_skill = dst_root.join("skills-real").join(&name);
            for sub in WalkDir::new(&path).follow_links(false) {
                let sub = sub?;
                if sub.file_type().is_symlink() {
                    continue;
                }
                let rel = sub.path().strip_prefix(&path)?;
                if rel.as_os_str().is_empty() {
                    continue;
                }
                let dst = dst_skill.join(rel);
                if sub.file_type().is_dir() {
                    std::fs::create_dir_all(&dst)?;
                } else if sub.file_type().is_file() {
                    if let Some(k) = key(&dst_root, &dst) {
                        seen.insert(k);
                    }
                    if copy_file_if_changed(sub.path(), &dst)? {
                        report.files_copied += 1;
                    } else {
                        report.files_unchanged += 1;
                    }
                }
            }
        }
    }

    // 同 agents：删除不传播（见上）。
    next.trees.insert("zcode".into(), seen);
    Ok(())
}

/// codex 树：只取 canonical 规则实际引用的三个文件（被 AGENTS.md 指向）。
fn capture_codex(
    paths: &SyncPaths,
    spec: &TreeSpec,
    _prev: &CaptureState,
    next: &mut CaptureState,
    report: &mut CaptureReport,
) -> Result<()> {
    let src_root = paths.live_codex();
    let dst_root = paths.payload().join("codex");
    if !src_root.exists() {
        return Ok(());
    }
    // 白名单来自配置；默认配置里正是这三个文件
    let wanted: Vec<String> = if spec.include.is_empty() {
        vec!["AGENTS.md".into(), "git-committer.md".into(), "rules/default.rules".into()]
    } else {
        spec.include.clone()
    };
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for rel in wanted {
        let rel = rel.as_str();
        let src = src_root.join(rel);
        if !src.is_file() {
            continue;
        }
        let dst = dst_root.join(rel);
        seen.insert(rel.replace('\\', "/"));
        if copy_file_if_changed(&src, &dst)? {
            report.files_copied += 1;
        } else {
            report.files_unchanged += 1;
        }
    }

    // 同 agents：删除不传播（见上）。
    next.trees.insert("codex".into(), seen);
    Ok(())
}

/// 登记两棵树的符号链接。
fn collect_links(paths: &SyncPaths, report: &mut CaptureReport) -> Result<()> {
    let home = &paths.home;
    let agents = links::scan_tree(
        &paths.live_agents(),
        home,
        &["sync", ".git", "references/design-skills", "references/ojo-design-skills"],
        &[".git", ".jj", "node_modules", "tmp"],
    )?;
    let zcode = links::scan_tree(
        &paths.live_zcode(),
        home,
        &[
            "cli", "v2", "backup", "models", "workspace", "computer-use",
            "preinstalled-tools", "tmp", "plugin-workspace", "scripts",
        ],
        &[".git", ".jj", "node_modules"],
    )?;

    report.links = agents.len() + zcode.len();
    report.broken_links = agents.iter().chain(zcode.iter()).filter(|l| l.broken).count();

    let doc = LinksDoc {
        roots: BTreeMap::from([
            ("agents".to_string(), agents),
            ("zcode".to_string(), zcode),
        ]),
    };
    doc.save(&paths.links_json())?;
    Ok(())
}

/// 采集自述：树哈希、时间与本机采集集。放在 `.state/`（已 gitignore）——
/// 它归属本机，若混入 payload 每次对齐都会与对端冲突。
fn write_state(paths: &SyncPaths, tree_hash: &str, next: &CaptureState) -> Result<()> {
    let device = crate::paths::local_device_id().unwrap_or_else(|_| "unknown".into());
    let state = CaptureState {
        device,
        captured_at: now_rfc3339(),
        tree_hash: tree_hash.to_string(),
        trees: next.trees.clone(),
    };
    std::fs::create_dir_all(paths.state_dir())?;
    let tmp = paths.capture_state().with_extension("json.tmp");
    std::fs::write(&tmp, format!("{}", serde_json::to_string_pretty(&state)?))?;
    std::fs::rename(&tmp, paths.capture_state())?;
    Ok(())
}

/// RFC3339 UTC 时间戳。避免引入 chrono：只需要一个可读时间。
fn now_rfc3339() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // 以 UTC 日期换算（自 1970 起的民用日算法）
    let days = secs / 86_400;
    let rem = secs % 86_400;
    let (y, m, d) = civil_from_days(days as i64);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Howard Hinnant 的 civil_from_days：把「1970 起的天数」换成公历年月日。
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

/// 供 apply 与 doctor 复用：payload 里某棵树的根。
pub fn payload_tree(paths: &SyncPaths, tree: &str) -> PathBuf {
    paths.payload().join(tree)
}

/// 供 apply 复用：判断 payload 里是否存在该相对文件。
pub fn payload_has(paths: &SyncPaths, tree: &str, rel: &Path) -> bool {
    payload_tree(paths, tree).join(rel).exists()
}

/// 只用于测试与 doctor：某个活目录文件是否在受管范围内。
pub fn is_managed(spec: &TreeSpec, rel: &Path) -> bool {
    exclude::allows(spec, rel)
}

/// 复制一个已有目录树（apply 的落盘也会用到，保持单一实现）。
pub fn copy_tree(src: &Path, dst: &Path, filter: impl Fn(&Path) -> bool) -> Result<usize> {
    let mut n = 0;
    for entry in WalkDir::new(src).follow_links(false) {
        let entry = entry?;
        let rel = entry.path().strip_prefix(src)?;
        if rel.as_os_str().is_empty() || !filter(rel) {
            continue;
        }
        let target = dst.join(rel);
        if entry.file_type().is_dir() {
            std::fs::create_dir_all(&target)?;
        } else if entry.file_type().is_file() {
            copy_file(entry.path(), &target)?;
            n += 1;
        }
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn fixture(home: &Path) -> SyncPaths {
        let p = SyncPaths::new(home);
        std::fs::create_dir_all(p.live_agents().join("prompts/refs")).unwrap();
        std::fs::create_dir_all(p.live_agents().join("skills/private-a/tmp")).unwrap();
        std::fs::create_dir_all(p.live_agents().join("references/nested-repo")).unwrap();
        std::fs::create_dir_all(p.live_zcode().join("agents")).unwrap();
        std::fs::create_dir_all(p.live_zcode().join("skills/real-skill")).unwrap();
        std::fs::create_dir_all(p.live_zcode().join("cli")).unwrap();
        std::fs::create_dir_all(p.live_zcode().join("v2")).unwrap();
        std::fs::create_dir_all(p.live_codex().join("rules")).unwrap();

        std::fs::write(p.live_agents().join("AGENTS.md"), "# agents\n").unwrap();
        std::fs::write(p.live_agents().join("prompts/refs/x.md"), "prompt\n").unwrap();
        std::fs::write(p.live_agents().join("skills/private-a/SKILL.md"), "---\nname: a\n---\n").unwrap();
        std::fs::write(p.live_agents().join("skills/private-a/tmp/junk.bin"), "huge\n").unwrap();
        std::fs::write(p.live_agents().join("AGENTS.md.bak-2026-01-01"), "old\n").unwrap();
        std::fs::write(p.live_agents().join("references/nested-repo/README.md"), "ref\n").unwrap();
        std::fs::write(p.live_agents().join("references/README.md"), "index\n").unwrap();
        // 仓库自身
        std::fs::create_dir_all(p.root.join("bin")).unwrap();
        std::fs::write(p.root.join("bin/tool"), "self\n").unwrap();

        std::fs::write(p.live_zcode().join("AGENTS.md"), "# zcode\n").unwrap();
        std::fs::write(p.live_zcode().join("agents/slow.md"), "---\nname: slow\n---\n").unwrap();
        std::fs::write(p.live_zcode().join("skills/real-skill/SKILL.md"), "real\n").unwrap();
        std::fs::write(p.live_zcode().join("cli/huge.bin"), "cache\n").unwrap();
        std::fs::write(p.live_zcode().join("v2/setting.json"), "{\"passwordCredentialKey\":\"x\"}\n").unwrap();
        symlink("../../.agents/skills/private-a", p.live_zcode().join("skills/farm")).unwrap();
        symlink("/nonexistent/place", p.live_zcode().join("skills/broken")).unwrap();

        std::fs::write(p.live_codex().join("AGENTS.md"), "# codex\n").unwrap();
        std::fs::write(p.live_codex().join("rules/default.rules"), "rule\n").unwrap();

        p
    }

    #[test]
    fn capture_mirrors_content_and_excludes_runtime() {
        let d = tempfile::tempdir().unwrap();
        let paths = fixture(d.path());
        let report = capture_all(&paths, &SyncConf::default()).unwrap();
        assert!(report.files_copied > 0);

        let pay = paths.payload();
        assert!(pay.join("agents/AGENTS.md").exists());
        assert!(pay.join("agents/prompts/refs/x.md").exists());
        assert!(pay.join("agents/skills/private-a/SKILL.md").exists());
        assert!(pay.join("agents/references/README.md").exists());

        // 排除项
        assert!(!pay.join("agents/skills/private-a/tmp").exists(), "tmp 必须排除");
        assert!(!pay.join("agents/AGENTS.md.bak-2026-01-01").exists(), "bak 必须排除");
        assert!(!pay.join("agents/references/nested-repo").exists(), "嵌套仓库必须排除");
        assert!(!pay.join("agents/sync").exists(), "自引用必须排除");
        assert!(!pay.join("zcode/cli").exists(), "运行态必须排除");
        assert!(!pay.join("zcode/v2").exists(), "凭证目录必须排除");

        // zcode 真实目录 vs 软链
        assert!(pay.join("zcode/AGENTS.md").exists());
        assert!(pay.join("zcode/agents/slow.md").exists());
        assert!(pay.join("zcode/skills-real/real-skill/SKILL.md").exists());
        assert!(!pay.join("zcode/skills").exists(), "软链农场不进内容");

        // codex
        assert!(pay.join("codex/AGENTS.md").exists());
        assert!(pay.join("codex/rules/default.rules").exists());
    }

    #[test]
    fn capture_records_links_without_device_field() {
        let d = tempfile::tempdir().unwrap();
        let paths = fixture(d.path());
        capture_all(&paths, &SyncConf::default()).unwrap();
        let text = std::fs::read_to_string(paths.links_json()).unwrap();
        assert!(text.contains("skills/farm"));
        assert!(text.contains("skills/broken"));
        assert!(!text.contains("\"device\""), "links.json 不得含设备身份");
        let doc = LinksDoc::load(&paths.links_json()).unwrap();
        assert!(doc.roots["zcode"].iter().any(|l| l.path == "skills/broken" && l.broken));
    }

    #[test]
    fn capture_rejects_credentials() {
        let d = tempfile::tempdir().unwrap();
        let paths = fixture(d.path());
        std::fs::write(
            paths.live_agents().join("prompts/leak.md"),
            "key=sk-abcdefghijklmnopqrstuvwxyz123456\n",
        )
        .unwrap();
        let err = capture_all(&paths, &SyncConf::default()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("密钥门"), "应报密钥门：{msg}");
        assert!(!msg.contains("sk-abcdefghij"), "错误信息不得回显凭据：{msg}");
    }

    #[test]
    fn capture_is_idempotent_and_hash_stable() {
        let d = tempfile::tempdir().unwrap();
        let paths = fixture(d.path());
        let h1 = capture_all(&paths, &SyncConf::default()).unwrap().tree_hash;
        let h2 = capture_all(&paths, &SyncConf::default()).unwrap().tree_hash;
        assert_eq!(h1, h2, "重复采集必须得到同一树哈希");
    }

    /// 语义约定：采集只**增**不删。
    ///
    /// 本机删除一个文件后，payload 里仍保留——它可能来自对端，删掉等于替对端
    /// 声明删除。想清理请显式做（目前只能在两端都删，或开启 prune 并接受风险）。
    #[test]
    fn capture_does_not_propagate_deletions() {
        let d = tempfile::tempdir().unwrap();
        let paths = fixture(d.path());
        capture_all(&paths, &SyncConf::default()).unwrap();
        assert!(paths.payload().join("agents/prompts/refs/x.md").exists());
        std::fs::remove_file(paths.live_agents().join("prompts/refs/x.md")).unwrap();
        capture_all(&paths, &SyncConf::default()).unwrap();
        assert!(
            paths.payload().join("agents/prompts/refs/x.md").exists(),
            "采集不得因「本机没有」删 payload（否则会误删对端内容）"
        );
    }

    /// 核心回归：对端独有的文件不能被「本机没有」杀掉。
    ///
    /// 事故现场：一台机器缺 103 个文件，采集时按「活目录没有就清 payload」处理，
    /// 等于替对端声明删除，merge 后 103 个文件从两端一起消失。
    #[test]
    fn capture_never_deletes_files_it_did_not_capture() {
        let d = tempfile::tempdir().unwrap();
        let paths = fixture(d.path());
        capture_all(&paths, &SyncConf::default()).unwrap();

        // 模拟 merge 之后 payload 里出现对端独有的文件
        let peer_file = paths.payload().join("agents/prompts/from-peer.md");
        std::fs::create_dir_all(peer_file.parent().unwrap()).unwrap();
        std::fs::write(&peer_file, "peer only\n").unwrap();

        capture_all(&paths, &SyncConf::default()).unwrap();
        assert!(
            peer_file.exists(),
            "本机从未采集过它，就没有资格判断它该被删除"
        );

        // 本机自己删了文件，对端的文件也不能受影响
        std::fs::remove_file(paths.live_agents().join("prompts/refs/x.md")).unwrap();
        capture_all(&paths, &SyncConf::default()).unwrap();
        assert!(peer_file.exists(), "对端文件仍然保留");
    }

    #[test]
    fn capture_writes_state_outside_payload() {
        let d = tempfile::tempdir().unwrap();
        let paths = fixture(d.path());
        capture_all(&paths, &SyncConf::default()).unwrap();
        assert!(paths.capture_state().exists());
        assert!(!paths.payload().join("_meta/capture.json").exists(), "本机观察不得进 payload");

        // 采集集必须落盘：它是删除传播的唯一依据
        let text = std::fs::read_to_string(paths.capture_state()).unwrap();
        assert!(text.contains("\"trees\""), "状态里必须有本机采集集：{text}");
        assert!(text.contains("prompts/refs/x.md"));
    }
}
