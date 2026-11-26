//! 意图（2026-11-25 Owner）：「我希望最终的成品是 cli + config.toml 这样的结构。
//!   所有你现在可能是写死的东西，它最好都是动态的，比如 HOME、上游仓库、目录映射等等。」
//!
//! 本文件职责（4 个正交意图）：
//!   1. 唯一的配置模型：目录映射（tree）、上游仓库（clone）、同步与网络参数
//!   2. 两级合并：`config.toml`（跨设备共享）+ `config.local.toml`（本机私有）
//!   3. 路径展开（`~` / 环境变量），让同一份配置在任意 HOME 上成立
//!   4. 校验：名字合法、路径不重叠、端口不冲突——错在启动时暴露，而不是运行到一半
//! 不承载：文件搬运与合并；此处只声明「是什么」，不执行「怎么做」。
//!
//! 为什么分两份：ticket/端口/局域网地址每台机器各不相同。若放进共享的
//! `config.toml`，每次对齐都会与对端产生 2-sided 冲突（这个坑实测踩过）。

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// 一棵被同步的目录树：源目录 → payload 子目录 → 对端落点。
///
/// 三端都显式写出，不再假设「源和落点相同」——将来要同步到非 HOME 位置
/// （比如外部盘、另一个用户的目录）只需改配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TreeSpec {
    /// 稳定标识，用于 payload 子目录名与日志。限 `[a-z0-9-]`。
    pub name: String,
    /// 活目录（源）。支持 `~`。
    pub source: String,
    /// payload 里的子目录名，默认与 name 相同。
    #[serde(default)]
    pub payload: String,
    /// 落点（对端写到哪里）。留空表示与 `source` 相同。
    #[serde(default)]
    pub target: String,
    /// 白名单：相对 source 的条目。空表示「除 exclude 外全收」。
    #[serde(default)]
    pub include: Vec<String>,
    /// 排除模式（前缀或 `*` 通配），任意层级命中即跳过。
    #[serde(default)]
    pub exclude: Vec<String>,
    /// 是否镜像（重建式采集，删除会传播）。默认 true。
    #[serde(default = "yes")]
    pub mirror: bool,
}

fn yes() -> bool {
    true
}

impl TreeSpec {
    pub fn payload_name(&self) -> &str {
        if self.payload.is_empty() {
            &self.name
        } else {
            &self.payload
        }
    }
    pub fn target_or_source(&self) -> &str {
        if self.target.is_empty() {
            &self.source
        } else {
            &self.target
        }
    }
}

/// 一个上游 git 仓库（不进 payload，单独维护）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloneSpec {
    /// 相对 HOME 的路径，支持 `~`。
    pub path: String,
    pub url: String,
    #[serde(default = "main_branch")]
    pub branch: String,
}

fn main_branch() -> String {
    "main".into()
}

/// 同步行为参数。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncSettings {
    #[serde(default = "d1800")]
    pub tick_interval: u64,
    #[serde(default = "d15")]
    pub watch_cooldown: u64,
    #[serde(default = "d20")]
    pub backup_keep: usize,
    /// 落盘时是否删除 payload 中已不存在的受管文件。**默认关闭**。
    ///
    /// 为什么默认关：独立快照式合并里，「删除」与「从未改动」在 jj 的三方
    /// 合并下不可区分——一端删掉、另一端把同样内容恢复回来时，merge 仍可能
    /// 判成删除并把内容从两端一起抹掉（实测反复复活/误删）。开启即接受这个风险，
    /// 换取严格镜像语义；写入前的快照仍可回滚。
    #[serde(default)]
    pub prune: bool,
}

fn d1800() -> u64 { 1800 }
fn d15() -> u64 { 15 }
fn d20() -> usize { 20 }

impl Default for SyncSettings {
    fn default() -> Self {
        Self { tick_interval: d1800(), watch_cooldown: d15(), backup_keep: d20(), prune: false }
    }
}

/// 网络参数。端口与目标都可改，不写死在代码里。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetSettings {
    /// iroh 监听的 UDP 端口。固定它，「局域网直连」才有确定的地址。
    #[serde(default = "d22401")]
    pub bind_port: u16,
    /// 要暴露给对端的本机 TCP 服务（通常是 sshd）。
    #[serde(default = "d_ssh")]
    pub serve_target: String,
    /// 每个 peer 在本机占用的转发端口从哪个基数开始分配。
    #[serde(default = "d22301")]
    pub forward_port_base: u16,
}

fn d22401() -> u16 { 22401 }
fn d22301() -> u16 { 22301 }
fn d_ssh() -> String { "127.0.0.1:22".into() }

impl Default for NetSettings {
    fn default() -> Self {
        Self { bind_port: d22401(), serve_target: d_ssh(), forward_port_base: d22301() }
    }
}

/// 本机私有的一段：设备身份与对端地址。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LocalSettings {
    /// 留空则从主机名推导。
    #[serde(default)]
    pub device_id: String,
    /// 本机 iroh secret 的存放路径（相对 sync_root）。
    #[serde(default)]
    pub secret_file: String,
    /// 对端：id → 连接信息。
    #[serde(default)]
    pub peers: BTreeMap<String, PeerSettings>,
    /// 每棵树的启停（缺省 = 启用）。本机私有：同一棵树可以只在这台机器暂停。
    ///
    /// UI 上的开关直接落到这里：暂停的树既不上传也不落盘，
    /// 但它在 payload 里的既有内容保留，重新启用时不会丢。
    #[serde(default)]
    pub tree_enabled: BTreeMap<String, bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PeerSettings {    #[serde(default)]
    pub ticket: String,
    /// 本机连接该 peer 时使用的回环端口（留空则按基数分配）。
    #[serde(default)]
    pub forward_port: Option<u16>,
    /// 对端 iroh 的 UDP 端口。
    #[serde(default)]
    pub net_port: Option<u16>,
    /// 对端局域网地址，用于绕过 n0 发现。
    #[serde(default)]
    pub lan: String,
    /// 对端数据仓的绝对路径（留空按对端 HOME 推导）。
    #[serde(default)]
    pub repo_path: String,
    #[serde(default)]
    pub ssh_user: String,
    /// 对端 relay 地址：填了就纯出站连通。
    #[serde(default)]
    pub relay_url: String,
}

/// 自更新参数。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateSettings {
    /// 有新版本时自动安装（daemon 检查到后执行，最多每 `check_interval` 一次）。
    #[serde(default = "yes")]
    pub auto: bool,
    /// 检查间隔（秒），默认 6 小时。
    #[serde(default = "d21600")]
    pub check_interval: u64,
    /// 查询/下载使用的代理；留空则沿用环境变量（curl 默认行为）。
    #[serde(default)]
    pub proxy: String,
}

fn d21600() -> u64 {
    21600
}

impl Default for UpdateSettings {
    fn default() -> Self {
        Self {
            auto: true,
            check_interval: d21600(),
            proxy: String::new(),
        }
    }
}

/// 完整配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default = "d_sync_root")]
    pub sync_root: String,
    #[serde(default)]
    pub sync: SyncSettings,
    #[serde(default)]
    pub net: NetSettings,
    #[serde(default)]
    pub update: UpdateSettings,
    #[serde(default = "default_trees")]
    pub trees: Vec<TreeSpec>,
    #[serde(default = "default_clones")]
    pub clones: Vec<CloneSpec>,
    #[serde(default)]
    pub local: LocalSettings,
}

fn d_sync_root() -> String {
    "~/.agents/sync".into()
}

/// `config.toml` 的初始内容（缺失时创建）。
///
/// 放在 core 里而不是各处手写：daemon 启动时的补齐与 UI 的「打开配置…」
/// 必须给出同一份模板，否则用户会在两个入口看到不同默认值。
pub const CONFIG_TEMPLATE: &str = r#"# agents-sync 共享配置（随仓库同步；本机私有项写 config.local.toml）
# 修改后 daemon 会在下一轮同步读取；重启可立即生效。

[sync]
# 兜底轮询间隔（秒）。主路径是事件驱动，这里只补齐丢失的广播。
tick_interval = 1800
# 编辑器连续保存的合并窗口（秒）
watch_cooldown = 15
# 落盘前快照保留份数
backup_keep = 20
# 删除也传播（镜像语义）。关闭后一端删除不会影响另一端。
prune = true

# [net]
# bind_port = 22401          # 本机 iroh 监听 UDP 端口（固定才有确定的局域网直连地址）
# serve_target = "127.0.0.1:22"  # 暴露给对端的本机 TCP 服务（sshd）

# [update]
# auto = true               # 有新版本时自动安装（daemon 空闲时执行）
# check_interval = 21600    # 检查间隔（秒），默认 6 小时
# proxy = ""                # 查询/下载代理；留空沿用环境变量

# 目录映射与上游仓库见默认值；要覆盖时取消注释并写全字段。
# [[trees]]
# name = "agents"
# source = "~/.agents"
# payload = "agents"
# target = "~/.agents"
# include = ["AGENTS.md", "prompts", "skills", "documents", ".skill-lock.json"]
# exclude = ["sync/", "*/tmp/", "*/node_modules/"]
# mirror = true
"#;

impl Default for Settings {
    fn default() -> Self {
        Self {
            sync_root: d_sync_root(),
            sync: SyncSettings::default(),
            net: NetSettings::default(),
            update: UpdateSettings::default(),
            trees: default_trees(),
            clones: default_clones(),
            local: LocalSettings::default(),
        }
    }
}

/// 出厂目录映射。用户可整体替换；这里是「原先写死在代码里的那些 @2026-11-24 约定」。
fn default_trees() -> Vec<TreeSpec> {
    vec![
        TreeSpec {
            name: "agents".into(),
            source: "~/.agents".into(),
            payload: "agents".into(),
            target: String::new(),
            include: vec![
                "AGENTS.md".into(),
                "prompts".into(),
                "skills".into(),
                "documents".into(),
                ".skill-lock.json".into(),
                "references/README.md".into(),
            ],
            // 白名单之外的一切都不采集：~/.agents 里可能放私密文件（实测有 .env 凭据）
            exclude: vec![
                "sync/".into(),
                "*/tmp/".into(),
                "*/node_modules/".into(),
                "skills/ego-browser".into(),
                // 三个独立 git 仓由 `clone-list` + `agents-sync refs` 管理，
                // 不能让它们的**工作区内容**进 payload：两边各自改过就会 2-sided 冲突
                // （实测 world-class-designer 的 scripts 就这样炸过）。
                "references/design-skills/".into(),
                "references/ojo-design-skills/".into(),
                "skills/world-class-designer/".into(),
            ],
            mirror: true,
        },
        TreeSpec {
            name: "zcode".into(),
            source: "~/.zcode".into(),
            payload: "zcode".into(),
            target: String::new(),
            include: vec!["AGENTS.md".into(), "agents".into(), "skills".into()],
            exclude: vec![
                "cli/".into(), "v2/".into(), "backup/".into(), "models/".into(),
                "workspace/".into(), "computer-use/".into(), "preinstalled-tools/".into(),
                "tmp/".into(), "plugin-workspace/".into(), "scripts/".into(),
            ],
            mirror: true,
        },
        TreeSpec {
            name: "codex".into(),
            source: "~/.codex".into(),
            payload: "codex".into(),
            target: String::new(),
            include: vec![
                "AGENTS.md".into(),
                "git-committer.md".into(),
                "rules/default.rules".into(),
            ],
            exclude: vec![],
            mirror: true,
        },
    ]
}

fn default_clones() -> Vec<CloneSpec> {
    vec![
        CloneSpec {
            path: "~/.agents/references/design-skills".into(),
            url: "https://github.com/memi-design/design-skills".into(),
            branch: "main".into(),
        },
        CloneSpec {
            path: "~/.agents/references/ojo-design-skills".into(),
            url: "https://github.com/touchine-ojo/OJO-Design-Skills".into(),
            branch: "main".into(),
        },
        CloneSpec {
            path: "~/.agents/skills/world-class-designer".into(),
            url: "https://github.com/Gaubee/world-class-designer-skills.git".into(),
            branch: "main".into(),
        },
    ]
}

/// 深合并：表递归合并，其它值（含数组）由后者整体替换。
fn deep_merge(base: &mut toml::Value, over: toml::Value) {
    match (base, over) {
        (toml::Value::Table(b), toml::Value::Table(o)) => {
            for (k, v) in o {
                match b.get_mut(&k) {
                    Some(slot) => deep_merge(slot, v),
                    None => {
                        b.insert(k, v);
                    }
                }
            }
        }
        (slot, v) => *slot = v,
    }
}

/// 展开 `~` 与 `$HOME`。所有配置里的路径都走这里，代码里不再出现字面量。
pub fn expand(path: &str, home: &Path) -> PathBuf {
    let p = path.trim();
    if let Some(rest) = p.strip_prefix("~/") {
        return home.join(rest);
    }
    if p == "~" {
        return home.to_path_buf();
    }
    if let Some(rest) = p.strip_prefix("$HOME/") {
        return home.join(rest);
    }
    PathBuf::from(p)
}

impl Settings {
    /// 从 `sync_root` 读取两级配置：`config.toml` 覆盖默认，`config.local.toml` 覆盖前者。
    /// 两个文件都可以不存在（此时用出厂默认）。
    pub fn load(sync_root: &Path) -> Result<Self> {
        // 先做 TOML 层的深合并，再一次性反序列化。
        //
        // 为什么不能「每个文件反序列化成 Settings 再逐字段覆盖」：缺失的字段会被
        // serde 填成默认值，于是第二份文件里没写的 `[sync]` 会把第一份的值
        // 悄悄改回默认（实测 tick_interval 60 被改回 1800）。深合并天然只覆盖
        // 「确实写了的键」。
        let mut merged = toml::Value::Table(toml::map::Map::new());
        for name in ["config.toml", "config.local.toml"] {
            let p = sync_root.join(name);
            if !p.exists() {
                continue;
            }
            let text = std::fs::read_to_string(&p)
                .with_context(|| format!("读取 {}", p.display()))?;
            let layer: toml::Value = toml::from_str(&text)
                .with_context(|| format!("解析 {}（TOML 语法或字段类型有误）", p.display()))?;
            deep_merge(&mut merged, layer);
        }
        if merged.as_table().map(|t| t.is_empty()).unwrap_or(true) {
            return Ok(Settings::default());
        }
        let settings: Settings = merged
            .try_into()
            .context("配置字段类型与 schema 不符")?;
        Ok(settings)
    }

    pub fn root(&self, home: &Path) -> PathBuf {
        expand(&self.sync_root, home)
    }

    pub fn tree(&self, name: &str) -> Option<&TreeSpec> {
        self.trees.iter().find(|t| t.name == name)
    }

    /// 这棵树在本机是否启用（缺省启用）。
    pub fn is_tree_enabled(&self, name: &str) -> bool {
        self.local.tree_enabled.get(name).copied().unwrap_or(true)
    }

    /// 校验：名字合法、映射不重复、端口不冲突。启动时一次报全。
    pub fn validate(&self) -> Vec<String> {
        let mut problems = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for t in &self.trees {
            if t.name.is_empty()
                || !t.name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
            {
                problems.push(format!("tree 名非法（需 [a-z0-9-]）：{}", t.name));
            }
            if !seen.insert(t.payload_name().to_string()) {
                problems.push(format!("payload 子目录重复：{}", t.payload_name()));
            }
            if t.include.is_empty() && t.exclude.is_empty() {
                problems.push(format!(
                    "tree {} 既无 include 也无 exclude：会整棵同步，风险过高",
                    t.name
                ));
            }
        }
        let mut ports = std::collections::HashSet::new();
        for (id, peer) in &self.local.peers {
            if let Some(p) = peer.forward_port {
                if !ports.insert(p) {
                    problems.push(format!("转发端口 {p} 被多个 peer 占用（{id}）"));
                }
            }
        }
        problems
    }

    /// 生成一份可读的默认 `config.toml` 文本（首次 init 时落盘）。
    pub fn example_toml() -> String {
        toml::to_string_pretty(&Settings::default()).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_cover_all_three_trees() {
        let s = Settings::default();
        assert_eq!(s.trees.len(), 3);
        assert!(s.tree("agents").unwrap().include.contains(&"AGENTS.md".to_string()));
        assert!(s.validate().is_empty(), "默认配置必须自洽：{:?}", s.validate());
    }

    #[test]
    fn tilde_expands_against_any_home() {
        let home = Path::new("/Users/someone-else");
        assert_eq!(expand("~/.agents", home), home.join(".agents"));
        assert_eq!(expand("~", home), home);
        assert_eq!(expand("$HOME/x", home), home.join("x"));
        assert_eq!(expand("/abs/path", home), Path::new("/abs/path"));
    }

    #[test]
    fn local_layer_overrides_shared() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join("config.toml"),
            "sync_root = \"~/.alt/sync\"\n[sync]\ntick_interval = 60\n",
        )
        .unwrap();
        std::fs::write(
            d.path().join("config.local.toml"),
            "[local]\ndevice_id = \"mini\"\n[local.peers.imac]\nticket = \"endpointx\"\nforward_port = 22302\n",
        )
        .unwrap();
        let s = Settings::load(d.path()).unwrap();
        assert_eq!(s.sync_root, "~/.alt/sync");
        assert_eq!(s.sync.tick_interval, 60);
        assert_eq!(s.local.device_id, "mini");
        assert_eq!(s.local.peers["imac"].ticket, "endpointx");
        // 未指定的字段沿用默认
        assert_eq!(s.sync.backup_keep, 20);
        assert_eq!(s.trees.len(), 3);
    }

    #[test]
    fn custom_trees_replace_defaults() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join("config.toml"),
            r#"
[[trees]]
name = "dotfiles"
source = "~/src/dotfiles"
payload = "dotfiles"
include = ["nvim"]
exclude = ["nvim/plugin/"]
"#,
        )
        .unwrap();
        let s = Settings::load(d.path()).unwrap();
        assert_eq!(s.trees.len(), 1);
        assert_eq!(s.trees[0].name, "dotfiles");
        assert_eq!(s.trees[0].target_or_source(), "~/src/dotfiles");
        assert!(s.validate().is_empty());
    }

    #[test]
    fn validate_catches_duplicate_payload_and_ports() {
        let mut s = Settings::default();
        s.trees.push(TreeSpec {
            name: "dup".into(),
            source: "~/.x".into(),
            payload: "agents".into(),
            target: String::new(),
            include: vec!["a".into()],
            exclude: vec![],
            mirror: true,
        });
        s.local.peers.insert(
            "p1".into(),
            PeerSettings { forward_port: Some(1234), ..Default::default() },
        );
        s.local.peers.insert(
            "p2".into(),
            PeerSettings { forward_port: Some(1234), ..Default::default() },
        );
        let problems = s.validate();
        assert!(problems.iter().any(|p| p.contains("payload 子目录重复")));
        assert!(problems.iter().any(|p| p.contains("被多个 peer 占用")));
    }

    #[test]
    fn bad_toml_reports_file() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("config.toml"), "trees = [").unwrap();
        let err = Settings::load(d.path()).unwrap_err().to_string();
        assert!(err.contains("config.toml"), "错误应指出文件：{err}");
    }

    #[test]
    fn example_toml_roundtrips() {
        let text = Settings::example_toml();
        assert!(text.contains("[[trees]]"));
        assert!(text.contains("name = \"agents\""));
        let back: Settings = toml::from_str(&text).unwrap();
        assert_eq!(back.trees.len(), 3);
    }
}
