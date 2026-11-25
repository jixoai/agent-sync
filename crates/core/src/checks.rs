//! 意图（2026-11-24）：为「凭据不得进仓库」与「多设备内容一致」提供两个可复算的判定。
//!   前者是硬门（命中即中止同步），后者是收敛判据（树哈希相同才算一致）。
//!
//! 本文件职责（2 个正交意图，同一主题＝可复算的校验）：
//!   1. 高置信凭据扫描：只报文件/行号/模式名，绝不回显命中内容（否则二次泄漏）
//!   2. payload 树哈希：与 bash 版字节级同构，便于迁移期对拍
//! 不承载：策略决策（何时中止由调用方决定）。

use anyhow::{Context, Result};
use regex::Regex;
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

/// 一条凭据命中。刻意不含匹配文本。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretHit {
    pub file: PathBuf,
    pub line: usize,
    pub pattern: &'static str,
}

impl std::fmt::Display for SecretHit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}:{}", self.file.display(), self.line, self.pattern)
    }
}

/// 高置信模式。刻意不放宽：宁可漏报键名，也不要把文档里的示例文本当凭据。
const PATTERNS: &[(&str, &str)] = &[
    ("openai_sk", r"\bsk-[A-Za-z0-9_\-]{20,}"),
    ("github_token", r"\bgh[pousr]_[A-Za-z0-9]{30,}"),
    ("aws_akid", r"\bAKIA[0-9A-Z]{16}\b"),
    ("private_key", r"-----BEGIN [A-Z ]*PRIVATE KEY-----"),
    ("slack_token", r"\bxox[abprs]-[A-Za-z0-9\-]{10,}"),
    (
        "bearer_jwt",
        r"\beyJ[A-Za-z0-9_\-]{10,}\.[A-Za-z0-9_\-]{10,}\.[A-Za-z0-9_\-]{10,}",
    ),
    // 赋值型：要求右侧是足够长的真实字面量，排除 "password" 之类的键名本身
    (
        "api_key_assign",
        r#"(?i)\bapi[_-]?key\b\s*[:=]\s*['"]([A-Za-z0-9_\-]{24,})['"]"#,
    ),
    (
        "secret_assign",
        r#"(?i)\bclient[_-]?secret\b\s*[:=]\s*['"]([A-Za-z0-9_\-]{24,})['"]"#,
    ),
];

/// 只扫描这些扩展名（含无扩展名），避免把二进制与图片读进内存。
const TEXT_EXT: &[&str] = &[
    "md", "txt", "json", "json5", "toml", "yaml", "yml", "ini", "conf", "sh", "bash", "zsh",
    "py", "js", "mjs", "cjs", "ts", "tsx", "jsx", "svelte", "vue", "rs", "go", "rb", "pl", "env",
    "xml", "plist", "html", "css", "sql", "lock", "",
];

/// 单文件扫描上限：超过则跳过（配置类文件不该这么大）。
const MAX_SCAN_BYTES: u64 = 2 * 1024 * 1024;

const SKIP_DIRS: &[&str] = &[".git", ".jj", "node_modules", "__pycache__"];

fn is_text_ext(path: &Path) -> bool {
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    TEXT_EXT.contains(&ext.as_str())
}

/// 扫描根目录下所有文本文件，返回命中列表（按路径与行号排序，结果稳定）。
///
/// `allow` 是放行清单（相对路径），用于已知的无害样本，例如文档里的示例密钥。
pub fn scan(root: &Path, allow: &[String]) -> Result<Vec<SecretHit>> {
    let compiled: Vec<(&'static str, Regex)> = PATTERNS
        .iter()
        .map(|(name, pat)| {
            let re = Regex::new(pat).with_context(|| format!("编译凭据模式 {name}"))?;
            Ok((*name, re))
        })
        .collect::<Result<_>>()?;

    let mut hits = Vec::new();
    if !root.exists() {
        return Ok(hits);
    }

    for entry in WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            let name = e.file_name().to_string_lossy();
            !SKIP_DIRS.iter().any(|s| *s == name)
        })
        .filter_map(Result::ok)
    {
        if !entry.file_type().is_file() || entry.file_type().is_symlink() {
            continue;
        }
        let path = entry.path();
        let rel = path.strip_prefix(root).unwrap_or(path);
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        if allow.iter().any(|a| *a == rel_str) {
            continue;
        }
        if !is_text_ext(path) {
            continue;
        }
        let meta = match entry.metadata() {
            Ok(m) if m.len() <= MAX_SCAN_BYTES => m,
            _ => continue,
        };
        let _ = meta;

        let Ok(text) = std::fs::read_to_string(path) else {
            continue; // 非 UTF-8 视为二进制
        };
        for (name, re) in &compiled {
            if let Some(m) = re.find(&text) {
                let line = text[..m.start()].bytes().filter(|b| *b == b'\n').count() + 1;
                hits.push(SecretHit {
                    file: rel.to_path_buf(),
                    line,
                    pattern: name,
                });
            }
        }
    }
    hits.sort_by(|a, b| a.file.cmp(&b.file).then(a.line.cmp(&b.line)));
    Ok(hits)
}

/// payload 树哈希：`sha256( for each sorted rel: rel ++ 0x00 ++ ('L'+target | content) ++ 0x00 )`。
///
/// 与 bash 版 `fsops.py tree-hash` 字节级同构 —— 迁移期可以用同一个 hash 对拍两套实现，
/// 也能让「收敛判据」在切换实现后继续可比。
pub fn tree_hash(root: &Path) -> Result<String> {
    let mut entries: Vec<(String, PathBuf)> = Vec::new();
    if root.exists() {
        for entry in WalkDir::new(root)
            .follow_links(false)
            .into_iter()
            .filter_entry(|e| {
                let name = e.file_name().to_string_lossy();
                !SKIP_DIRS.iter().any(|s| *s == name) && name != ".DS_Store"
            })
            .filter_map(Result::ok)
        {
            let path = entry.path().to_path_buf();
            if entry.file_type().is_dir() {
                continue;
            }
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            entries.push((rel, path));
        }
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));

    let mut hasher = Sha256::new();
    for (rel, path) in entries {
        hasher.update(rel.as_bytes());
        hasher.update([0u8]);
        let symlink_meta = std::fs::symlink_metadata(&path);
        match symlink_meta {
            Ok(m) if m.file_type().is_symlink() => {
                let target = std::fs::read_link(&path)?;
                hasher.update(b"L");
                hasher.update(target.to_string_lossy().as_bytes());
            }
            _ => {
                let mut f = std::fs::File::open(&path)
                    .with_context(|| format!("读取 {}", path.display()))?;
                let mut buf = [0u8; 64 * 1024];
                loop {
                    let n = f.read(&mut buf)?;
                    if n == 0 {
                        break;
                    }
                    hasher.update(&buf[..n]);
                }
            }
        }
        hasher.update([0u8]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_high_confidence_secrets_without_echoing() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join("leak.md"),
            "intro\nkey=sk-abcdefghijklmnopqrstuvwxyz123456\n",
        )
        .unwrap();
        std::fs::write(d.path().join("clean.md"), "password: mention only\n").unwrap();

        let hits = scan(d.path(), &[]).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].file, PathBuf::from("leak.md"));
        assert_eq!(hits[0].line, 2);
        assert_eq!(hits[0].pattern, "openai_sk");
        // Display 只含文件/行/模式名
        let s = hits[0].to_string();
        assert!(!s.contains("sk-abcdefghij"));
    }

    #[test]
    fn allow_list_suppresses_known_samples() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("doc.md"), "sk-abcdefghijklmnopqrstuvwxyz123456\n").unwrap();
        let hits = scan(d.path(), &["doc.md".to_string()]).unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn skips_binary_and_big_files() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("img.png"), [0u8, 159, 146, 150]).unwrap();
        let hits = scan(d.path(), &[]).unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn tree_hash_is_stable_and_order_independent() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        for (root, order) in [(a.path(), [("x/y.md", "1"), ("a.md", "2")]), (b.path(), [("a.md", "2"), ("x/y.md", "1")])] {
            for (rel, content) in order {
                let p = root.join(rel);
                std::fs::create_dir_all(p.parent().unwrap()).unwrap();
                std::fs::write(p, content).unwrap();
            }
        }
        assert_eq!(tree_hash(a.path()).unwrap(), tree_hash(b.path()).unwrap());
    }

    #[test]
    fn tree_hash_changes_with_content() {
        let a = tempfile::tempdir().unwrap();
        std::fs::write(a.path().join("f.md"), "one").unwrap();
        let h1 = tree_hash(a.path()).unwrap();
        std::fs::write(a.path().join("f.md"), "two").unwrap();
        let h2 = tree_hash(a.path()).unwrap();
        assert_ne!(h1, h2);
    }

    #[cfg(unix)]
    #[test]
    fn tree_hash_sees_symlink_targets() {
        let d = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink("alpha", d.path().join("link")).unwrap();
        let h1 = tree_hash(d.path()).unwrap();
        std::fs::remove_file(d.path().join("link")).unwrap();
        std::os::unix::fs::symlink("beta", d.path().join("link")).unwrap();
        let h2 = tree_hash(d.path()).unwrap();
        assert_ne!(h1, h2, "符号链接目标变化必须改变树哈希");
    }
}
