//! 意图（2026-11-25 Owner）：「所有你现在可能是写死的东西，它最好都是动态的。」
//!   准入规则从配置的 `[[trees]]` 读，而不是编译进代码。
//!
//! 本文件职责（3 个正交意图）：
//!   1. 模式匹配：`include` / `exclude` 支持精确、目录前缀、单段通配 `*`
//!   2. 永久噪声规则：`.git`、`.DS_Store`、`*.bak-*` 等——这些是**正确性**要求，
//!      不交给用户配置（配错会直接损坏仓库）
//!   3. 判定入口 `allows(spec, rel)`，capture 与 apply 共用同一份
//! 不承载：目录遍历与复制。

use crate::settings::TreeSpec;
use std::path::Path;

/// 任何层级都不进仓库的名字：版本库元数据、依赖目录与系统垃圾。
///
/// `__pycache__` 是 Python 字节码：目标机器、解释器版本各不相同，
/// 同步它只会带来无意义的变更与冲突（实测 `cpython-314.pyc` 混进 payload）。
const ALWAYS_DENY_NAMES: &[&str] = &[".git", ".jj", "node_modules", ".DS_Store", "__pycache__"];

/// 噪声文件：备份与临时产物。
fn is_noise(name: &str) -> bool {
    name.ends_with(".bak")
        || name.contains(".bak-")
        || name.starts_with(".uuremote_")
        || name.ends_with(".pre-sync")
        || name.contains(".pre-sync-")
        || name.ends_with(".pyc")
        || name.ends_with(".pyo")
}

fn normalize(rel: &Path) -> String {
    rel.to_string_lossy().replace('\\', "/")
}

/// 模式匹配。
///
/// 语义：
///   `AGENTS.md`      —— 精确匹配该条目（文件或目录）
///   `prompts`        —— 匹配该目录**及其全部后代**
///   `*/tmp`          —— `*` 匹配**恰好一段**，故命中 `skills/x/tmp` 而不命中 `a/b/c/tmp`
///   `*.bak-*`        —— `*` 在该段内匹配任意字符
///
/// 之所以让 `*` 只匹配一段：`skills/*/tmp` 这种写法意图明确，避免 `*` 跨目录
/// 造成「意外排除了深层目录」的静默错误。
fn matches_pattern(pattern: &str, rel: &str) -> bool {
    let pat = pattern.trim_end_matches('/');
    if pat.is_empty() {
        return false;
    }
    // 目录前缀：命中自身或全部后代
    if rel == pat || rel.starts_with(&format!("{pat}/")) {
        return true;
    }
    let pat_segs: Vec<&str> = pat.split('/').collect();
    let rel_segs: Vec<&str> = rel.split('/').collect();

    // 前缀 `*/` 表示「任意深度」：`*/tmp` 命中 `a/tmp`、`a/b/tmp/x`。
    // 这是配置里最常用的写法（排除任意层的 tmp / node_modules），
    // 若按单段 `*` 处理，用户必须写出精确层数，极易配错。
    if let Some(rest) = pat.strip_prefix("*/") {
        let rest_segs: Vec<&str> = rest.split('/').collect();
        let n = rest_segs.len();
        if rel_segs.len() >= n {
            for start in 0..=(rel_segs.len() - n) {
                if rest_segs
                    .iter()
                    .zip(&rel_segs[start..start + n])
                    .all(|(p, r)| glob_one(p, r))
                {
                    return true;
                }
            }
        }
    }

    match_segments(&pat_segs, &rel_segs)
}

/// 逐段匹配，`*` 匹配一段内的任意字符；段数必须相等（不跨层）。
fn match_segments(pat: &[&str], rel: &[&str]) -> bool {
    // 模式可以比路径短：此时它描述的是**目录前缀**（`skills/*/tmp` 命中
    // `skills/wcd/tmp/a.bin`）。要求段数相等会让所有「排除目录」的写法失效。
    if rel.len() < pat.len() {
        return false;
    }
    pat.iter().zip(rel.iter()).all(|(p, r)| glob_one(p, r))
}

fn glob_one(pattern: &str, text: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if !pattern.contains('*') {
        return pattern == text;
    }
    // 极简 glob：按 * 切段，逐段定位（支持引号中的多个 *）
    let parts: Vec<&str> = pattern.split('*').collect();
    let mut cursor = 0usize;
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        if i == 0 {
            if !text[cursor..].starts_with(part) {
                return false;
            }
            cursor += part.len();
        } else if i == parts.len() - 1 {
            if !text[cursor..].ends_with(part) {
                return false;
            }
            cursor = text.len();
        } else {
            match text[cursor..].find(part) {
                Some(pos) => cursor += pos + part.len(),
                None => return false,
            }
        }
    }
    true
}

/// 判定某相对路径在该树下是否允许。
///
/// 顺序：永久噪声 → exclude → include（空 include 表示「除 exclude 外全要」）。
pub fn allows(spec: &TreeSpec, rel: &Path) -> bool {
    let rel_str = normalize(rel);
    if rel_str.is_empty() {
        return false;
    }
    for comp in rel.components() {
        let name = comp.as_os_str().to_string_lossy();
        if ALWAYS_DENY_NAMES.iter().any(|s| *s == name) || is_noise(&name) {
            return false;
        }
    }
    if spec.exclude.iter().any(|p| matches_pattern(p, &rel_str)) {
        return false;
    }
    if spec.include.is_empty() {
        return true;
    }
    spec.include
        .iter()
        .any(|p| matches_pattern(p, &rel_str))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::Settings;
    use std::path::PathBuf;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }
    fn spec(name: &str) -> TreeSpec {
        Settings::default()
            .tree(name)
            .expect("默认配置应含该 tree")
            .clone()
    }

    #[test]
    fn agents_whitelist_allows_only_declared() {
        let s = spec("agents");
        assert!(allows(&s, &p("AGENTS.md")));
        assert!(allows(&s, &p("prompts/x/y.md")));
        assert!(allows(&s, &p("skills/tdd/SKILL.md")));
        assert!(allows(&s, &p(".skill-lock.json")));
        assert!(allows(&s, &p("references/README.md")));
        // 白名单外
        assert!(!allows(&s, &p("pqproxy-aiweb-xin.env")), "顶层未知文件必须拒绝");
        assert!(!allows(&s, &p("skill-wiki/x.md")));
    }

    #[test]
    fn zcode_never_leaks_runtime() {
        let s = spec("zcode");
        assert!(allows(&s, &p("AGENTS.md")));
        assert!(allows(&s, &p("agents/slow.md")));
        assert!(allows(&s, &p("skills/x/SKILL.md")));
        assert!(!allows(&s, &p("cli/a.png")));
        assert!(!allows(&s, &p("v2/setting.json")));
        assert!(!allows(&s, &p("models/x")));
    }

    #[test]
    fn noise_and_vcs_metadata_always_denied() {
        for tree in ["agents", "zcode", "codex"] {
            let s = spec(tree);
            assert!(!allows(&s, &p("AGENTS.md.bak-2026-01-01")), "{tree}: bak");
            assert!(!allows(&s, &p("x/.DS_Store")), "{tree}: DS_Store");
            assert!(!allows(&s, &p(".git/config")), "{tree}: git");
        }
    }

    #[test]
    fn single_star_matches_exactly_one_segment() {
        assert!(matches_pattern("skills/*/tmp", "skills/wcd/tmp/a.bin"));
        assert!(matches_pattern("*/tmp", "skills/private-a/tmp/junk.bin"), "前导 */ 表示任意深度");
        assert!(matches_pattern("*/tmp", "a/b/c/tmp/x"));
        assert!(matches_pattern("skills/wcd/tmp", "skills/wcd/tmp"));
        assert!(!matches_pattern("skills/*/tmp", "skills/a/b/tmp"), "星号不得跨层");
        assert!(matches_pattern("skills/ego-browser", "skills/ego-browser"));
    }

    #[test]
    fn glob_inside_segment() {
        assert!(matches_pattern("*.bak-*", "AGENTS.md.bak-2026-01-01"));
        assert!(!matches_pattern("*.bak-*", "AGENTS.md"));
        assert!(glob_one("a*c", "abbc"));
        assert!(!glob_one("a*c", "abbd"));
    }

    #[test]
    fn empty_include_means_allow_all_but_excludes() {
        let mut s = spec("agents");
        s.include.clear();
        assert!(allows(&s, &p("anything/else.md")));
        // exclude 仍然生效
        assert!(!allows(&s, &p("sync/payload/x")));
    }

    #[test]
    fn custom_tree_from_config_is_honored() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join("config.toml"),
            r#"
[[trees]]
name = "dotfiles"
source = "~/src/dotfiles"
include = ["nvim", "zsh/.zshrc"]
exclude = ["nvim/plugin"]
"#,
        )
        .unwrap();
        let settings = Settings::load(d.path()).unwrap();
        let s = settings.tree("dotfiles").unwrap();
        assert!(allows(s, &p("nvim/init.lua")));
        assert!(allows(s, &p("zsh/.zshrc")));
        assert!(!allows(s, &p("nvim/plugin/x.vim")), "exclude 优先");
        assert!(!allows(s, &p("other/file")), "不在 include 内");
    }
}
