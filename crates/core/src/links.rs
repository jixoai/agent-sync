//! 意图（2026-11-24）：符号链接是这套同步里唯一「不是内容」的状态。
//!   `~/.zcode/skills` 整层是软链农场（30 条），`~/.agents/skills/ego-browser`
//!   指向外部应用。复制本体既浪费又错误，只能登记「链接定义」再在对端重建。
//!
//! 本文件职责（3 个正交意图）：
//!   1. 扫描活目录里的符号链接并分类（决定跨机能否重建）
//!   2. 按分类重建链接（相对原样、home 重算、external 告警、broken 跳过）
//!   3. 拒绝覆盖真实目录（用户内容优先于自动化）
//! 不承载：文件内容复制（capture/apply 的职责）。

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// 链接的可移植性分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LinkKind {
    /// 目标是相对路径：跨机原样可用（农场里的主力形态）。
    Relative,
    /// 绝对路径但位于 $HOME 内：换成目标机的 HOME 后重建。
    Home,
    /// 绝对路径且在 $HOME 外：原样记录但不可移植。
    External,
    /// 兼容旧格式：bash 版把「目标不存在」也记成一种 kind。
    /// 新格式改用独立的 `broken` 布尔，但历史 payload 必须仍能读。
    #[serde(other)]
    LegacyBroken,
}

/// 一条链接定义。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkDef {
    /// 相对所在树根的路径。
    pub path: String,
    /// 链接目标（原样，不解析）。
    pub target: String,
    pub kind: LinkKind,
    /// 记录时目标是否不存在。与 `kind` 正交：一个外部链接完全可能同时是坏链，
    /// 分开记才能既保住「不可移植」的判断，又不在对端制造悬空链接。
    #[serde(default)]
    pub broken: bool,
}

/// `payload/_meta/links.json` 的结构。
/// 刻意**不含设备身份** —— 它一旦进 payload，每次对齐都会与对端产生 2-sided 冲突。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LinksDoc {
    /// key 是树名（`agents` / `zcode`），value 是该树的链接列表（按 path 排序）。
    #[serde(default)]
    pub roots: BTreeMap<String, Vec<LinkDef>>,
}

impl LinksDoc {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("读取 {}", path.display()))?;
        Ok(serde_json::from_str(&text)?)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, format!("{}\n", serde_json::to_string_pretty(self)?))?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
}

/// 目标的可移植性分类。`home` 用于判断绝对路径是否在 HOME 内。
pub fn classify(target: &Path, home: &Path) -> LinkKind {
    if target.is_absolute() {
        if target == home || target.starts_with(home) {
            LinkKind::Home
        } else {
            LinkKind::External
        }
    } else {
        LinkKind::Relative
    }
}

/// 记录时目标是否解析得到（链接本身可能存在但悬空）。
pub fn resolves(link_path: &Path) -> bool {
    std::fs::metadata(link_path).is_ok()
}

/// 扫描某棵树根下的全部符号链接（不跟随目录链接）。
///
/// `skip_top` 是顶层要整体跳过的目录名（运行态、仓库自身）。
/// `skip_name` 是任意层级要跳过的目录名（.git 等）。
pub fn scan_tree(
    root: &Path,
    home: &Path,
    skip_top: &[&str],
    skip_name: &[&str],
) -> Result<Vec<LinkDef>> {
    let mut out = Vec::new();
    if !root.exists() {
        return Ok(out);
    }
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            let rel = path.strip_prefix(root).unwrap_or(&path);
            let rel_str = rel.to_string_lossy().replace('\\', "/");
            let is_top = !rel_str.contains('/');

            let file_type = match entry.file_type() {
                Ok(t) => t,
                Err(_) => continue,
            };

            if file_type.is_symlink() {
                let target = std::fs::read_link(&path)?;
                out.push(LinkDef {
                    path: rel_str,
                    kind: classify(&target, home),
                    broken: !resolves(&path),
                    target: target.to_string_lossy().to_string(),
                });
                continue;
            }
            if file_type.is_dir() {
                if skip_name.iter().any(|s| *s == name) {
                    continue;
                }
                if is_top && skip_top.iter().any(|s| *s == name) {
                    continue;
                }
                // references/<repo> 这类两层跳过交给调用方用 skip_top 的斜杠形式处理
                if skip_top.iter().any(|s| {
                    s.contains('/') && rel_str == *s
                }) {
                    continue;
                }
                stack.push(path);
            }
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

/// 重建结果，供 UI/日志展示。
#[derive(Debug, Default)]
pub struct RebuildReport {
    pub created: usize,
    pub unchanged: usize,
    pub skipped_existing: Vec<String>,
    pub skipped_broken: Vec<String>,
    pub external: Vec<String>,
    pub errors: Vec<String>,
}

/// 把链接目标换算成目标机上的实际目标。
/// `home` 类需要按本机 HOME 重算：`/Users/<someone>/x` → `<home>/x`。
fn retarget(kind: LinkKind, target: &str, home: &Path) -> Option<PathBuf> {
    match kind {
        LinkKind::Relative | LinkKind::External => Some(PathBuf::from(target)),
        LinkKind::LegacyBroken => None,
        LinkKind::Home => {
            let p = Path::new(target);
            let mut comps = p.components();
            // 跳过 "/" 与用户名两段，其余拼到本机 HOME
            let _root = comps.next()?;
            let _user = comps.next()?;
            let rest: PathBuf = comps.collect();
            Some(home.join(rest))
        }
    }
}

/// 按 links.json 重建链接。
///
/// 安全底线：目标位置若是**真实目录/文件**（非链接），一律保留用户内容，
/// 只记入 `skipped_existing`。自动化不覆盖人手放的东西。
pub fn rebuild(doc: &LinksDoc, home: &Path, roots: &BTreeMap<&str, PathBuf>) -> RebuildReport {
    let mut report = RebuildReport::default();
    for (tree, links) in &doc.roots {
        let Some(root) = roots.get(tree.as_str()) else {
            continue;
        };
        for link in links {
            let dest = root.join(&link.path);
            if let Ok(meta) = std::fs::symlink_metadata(&dest) {
                if meta.file_type().is_symlink() {
                    if std::fs::read_link(&dest)
                        .map(|t| t.to_string_lossy() == link.target)
                        .unwrap_or(false)
                    {
                        report.unchanged += 1;
                        continue;
                    }
                    // 旧链接指向不同目标：替换是安全的（它本来就是链接）
                    let _ = std::fs::remove_file(&dest);
                } else {
                    report.skipped_existing.push(link.path.clone());
                    continue;
                }
            }

            // 坏链不重建：否则对端会得到一个同样悬空的链接，却以为同步成功。
            if link.broken || link.kind == LinkKind::LegacyBroken {
                report.skipped_broken.push(link.path.clone());
                continue;
            }
            if link.kind == LinkKind::External {
                report.external.push(format!("{} -> {}", link.path, link.target));
            }
            let Some(target) = retarget(link.kind, &link.target, home) else {
                report.skipped_broken.push(link.path.clone());
                continue;
            };
            if let Some(parent) = dest.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            #[cfg(unix)]
            match std::os::unix::fs::symlink(&target, &dest) {
                Ok(_) => report.created += 1,
                Err(e) => report
                    .errors
                    .push(format!("{}: {}", link.path, e)),
            }
            #[cfg(not(unix))]
            {
                let _ = &target;
                report.errors.push("仅支持 Unix 符号链接".to_string());
            }
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn scan_classifies_four_kinds() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path();
        let root = home.join(".zcode");
        std::fs::create_dir_all(root.join("skills")).unwrap();
        std::fs::create_dir_all(home.join(".agents/skills/real")).unwrap();

        std::os::unix::fs::symlink("../../.agents/skills/real", root.join("skills/farm")).unwrap();
        std::os::unix::fs::symlink(home.join(".agents/skills/real"), root.join("skills/homeabs"))
            .unwrap();
        std::os::unix::fs::symlink("/opt/elsewhere", root.join("skills/ext")).unwrap();
        std::os::unix::fs::symlink("/nonexistent/x", root.join("skills/broken")).unwrap();

        let links = scan_tree(&root, home, &[], &[]).unwrap();
        let by_path: BTreeMap<_, _> = links
            .iter()
            .map(|l| (l.path.as_str(), (l.kind, l.broken)))
            .collect();
        assert_eq!(by_path["skills/farm"], (LinkKind::Relative, false));
        assert_eq!(by_path["skills/homeabs"], (LinkKind::Home, false));
        // /opt/elsewhere 不存在：可移植性仍是 External，同时标记 broken
        assert_eq!(by_path["skills/ext"], (LinkKind::External, true));
        assert_eq!(by_path["skills/broken"], (LinkKind::External, true));
    }

    #[cfg(unix)]
    #[test]
    fn rebuild_recreates_and_rewrites_home_links() {
        let src = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(src.path().join("skills/real")).unwrap();
        let doc = LinksDoc {
            roots: BTreeMap::from([(
                "agents".to_string(),
                vec![
                    LinkDef {
                        path: "skills/rel".into(),
                        target: "real".into(),
                        kind: LinkKind::Relative,
                        broken: false,
                    },
                    LinkDef {
                        path: "skills/home".into(),
                        target: format!("{}/skills/real", src.path().display()),
                        kind: LinkKind::Home,
                        broken: false,
                    },
                ],
            )]),
        };

        // 目标机：HOME 不同
        let dst_home = tempfile::tempdir().unwrap();
        let dst_root = dst_home.path().join(".agents");
        std::fs::create_dir_all(dst_root.join("skills/real")).unwrap();

        let roots = BTreeMap::from([("agents", dst_root.clone())]);
        let report = rebuild(&doc, dst_home.path(), &roots);
        assert_eq!(report.created, 2);
        assert_eq!(
            std::fs::read_link(dst_root.join("skills/rel")).unwrap(),
            PathBuf::from("real")
        );
        // home 类链接被重算到目标机 HOME
        let rewritten = std::fs::read_link(dst_root.join("skills/home")).unwrap();
        assert!(rewritten.starts_with(dst_home.path()));
    }

    #[cfg(unix)]
    #[test]
    fn rebuild_never_clobbers_real_directories() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join(".agents");
        std::fs::create_dir_all(root.join("skills/mine")).unwrap();
        std::fs::write(root.join("skills/mine/keep.md"), "user content").unwrap();

        let doc = LinksDoc {
            roots: BTreeMap::from([(
                "agents".to_string(),
                vec![LinkDef {
                    path: "skills/mine".into(),
                    target: "elsewhere".into(),
                    kind: LinkKind::Relative,
                    broken: false,
                }],
            )]),
        };
        let roots = BTreeMap::from([("agents", root.clone())]);
        let report = rebuild(&doc, d.path(), &roots);
        assert_eq!(report.created, 0);
        assert_eq!(report.skipped_existing, vec!["skills/mine".to_string()]);
        assert!(root.join("skills/mine/keep.md").exists(), "用户内容必须保留");
    }

    #[test]
    fn doc_roundtrip_has_no_device_field() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("links.json");
        let doc = LinksDoc {
            roots: BTreeMap::from([("agents".to_string(), vec![])]),
        };
        doc.save(&p).unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(!text.contains("\"device\""));
        let back = LinksDoc::load(&p).unwrap();
        assert!(back.roots.contains_key("agents"));
    }
}
