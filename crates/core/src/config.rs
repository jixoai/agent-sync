//! 意图（2026-11-24）：把「本机配置」与「设备注册表」分开建模。
//!   `sync.conf` 跨设备共享（间隔、抖动、保留份数）；
//!   `devices.json` 是本机视角（ticket / 端口 / 局域网地址只对本机有意义），
//!   故意不入库 —— 它若进 payload，每次对齐都会与对端产生 2-sided 冲突。
//!
//! 本文件职责（3 个正交意图）：
//!   1. sync.conf 的解析（宽容：未知键忽略，缺失取默认）
//!   2. devices.json 的读写与 peer 过滤
//!   3. 一切路径类配置的合法性检查（端口范围、id 字符集）
//! 不承载：网络连接本身（net 的职责）。

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// 跨设备共享的运行参数。
#[derive(Debug, Clone)]
pub struct SyncConf {
    /// 兜底轮询间隔（秒）。主路径是事件驱动，这里只补齐丢失的广播。
    pub tick_interval: u64,
    /// 文件变动的抖动窗口（秒）：编辑器连续保存合并为一次同步。
    pub watch_cooldown: u64,
    /// apply 前快照保留份数。
    pub backup_keep: usize,
    /// 落盘时是否删除 payload 中已不存在的受管文件（镜像语义，默认关）。
    pub prune: bool,
}

impl Default for SyncConf {
    fn default() -> Self {
        Self {
            tick_interval: 1800,
            watch_cooldown: 15,
            backup_keep: 20,
            prune: false,
        }
    }
}

impl SyncConf {
    /// 从 `config.toml` 的 `[sync]` 投影。
    ///
    /// 为什么保留这个类型而不直接用 `SyncSettings`：引擎/守护进程/采集层
    /// 只需要这几个标量，用窄类型让调用方无法误以为自己能改目录映射。
    pub fn from_settings(s: &crate::settings::SyncSettings) -> Self {
        Self {
            tick_interval: s.tick_interval,
            watch_cooldown: s.watch_cooldown,
            backup_keep: s.backup_keep,
            prune: s.prune,
        }
    }
}

impl SyncConf {
    /// 读取 `key = value` 形式的配置；缺失或格式异常一律回落默认值。
    /// 宽容是有意的：配置文件坏掉不该让同步停摆，它只是降低便利性。
    pub fn load(path: &Path) -> Self {
        let mut conf = Self::default();
        let Ok(text) = std::fs::read_to_string(path) else {
            return conf;
        };
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            let (k, v) = (k.trim(), v.trim());
            match k {
                "tick_interval" => {
                    if let Ok(n) = v.parse() {
                        conf.tick_interval = n;
                    }
                }
                "watch_cooldown" => {
                    if let Ok(n) = v.parse() {
                        conf.watch_cooldown = n;
                    }
                }
                "backup_keep" => {
                    if let Ok(n) = v.parse() {
                        conf.backup_keep = n;
                    }
                }
                _ => {}
            }
        }
        conf
    }
}

/// 一台已注册的设备。ticket 由固定 IROH_SECRET 离线推导，长期稳定。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Device {
    pub id: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub ticket: String,
    #[serde(default)]
    pub local_port: Option<u16>,
    /// 该设备 iroh 监听的 UDP 端口。固定它，`lan` 才能构成可直连的地址。
    #[serde(default)]
    pub net_port: Option<u16>,
    #[serde(default)]
    pub ssh_user: String,
    #[serde(default)]
    pub lan: String,
    /// 对端数据仓的绝对路径；缺省为 `${HOME}/.agents/sync`。
    #[serde(default)]
    pub repo_path: String,
    /// 对端的 iroh relay 地址。填了即可纯出站连通（受限网络下的可靠路径）。
    #[serde(default)]
    pub relay_url: String,
}

impl Device {
    /// 本机连接该 peer 的本地端口；缺失则无法建隧道。
    pub fn port(&self) -> Result<u16> {
        self.local_port
            .with_context(|| format!("设备 {} 缺少 local_port", self.id))
    }

    /// 对端数据仓的绝对路径。
    ///
    /// 关键：**不能用本机 HOME 拼**。这里要的是对端机器上的路径，
    /// 用本机 `$HOME` 拼出来的会指向自己（实测导致 fetch 连到本机空端口）。
    /// 优先级：
    ///   1. `repo_path`（config.local.toml 显式配置，最可靠）
    ///   2. 按对端用户名猜 `/Users/<user>/.agents/sync`（macOS 布局）
    /// 猜错时请在 config.local.toml 里写 `repo_path`。
    pub fn repo_path_or_default(&self, _local_home: &Path) -> String {
        if !self.repo_path.is_empty() {
            return self.repo_path.clone();
        }
        let user = if self.ssh_user.is_empty() {
            std::env::var("USER").unwrap_or_else(|_| "unknown".into())
        } else {
            self.ssh_user.clone()
        };
        format!("/Users/{user}/.agents/sync")
    }
}

/// 本机设备注册表。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DeviceRegistry {
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub devices: Vec<Device>,
}

impl DeviceRegistry {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self {
                version: 1,
                devices: Vec::new(),
            });
        }
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("读取 {}", path.display()))?;
        let reg: Self = serde_json::from_str(&text)
            .with_context(|| format!("解析 {}", path.display()))?;
        Ok(reg)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = serde_json::to_string_pretty(self)?;
        // 先写临时文件再 rename：避免半截文件让下次启动读不到注册表。
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, format!("{text}\n"))?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// 新增或替换同 id 的设备（幂等）。
    pub fn upsert(&mut self, device: Device) {
        self.devices.retain(|d| d.id != device.id);
        self.devices.push(device);
    }

    /// 除本机外的所有对端。
    pub fn peers<'a>(&'a self, self_id: &'a str) -> impl Iterator<Item = &'a Device> + 'a {
        self.devices.iter().filter(move |d| d.id != self_id)
    }

    /// 校验：id 字符集、端口不重复。doctor 与 enroll 都依赖它。
    pub fn validate(&self, self_id: &str) -> Vec<String> {
        let mut problems = Vec::new();
        let mut seen_ports = std::collections::HashMap::new();
        for d in &self.devices {
            if d.id.is_empty()
                || !d
                    .id
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
            {
                problems.push(format!("设备 id 非法（需 [a-z0-9-]）：{}", d.id));
            }
            if let Some(port) = d.local_port {
                if port < 1024 {
                    problems.push(format!("设备 {} 端口 {} 属特权端口", d.id, port));
                }
                if let Some(prev) = seen_ports.insert(port, d.id.clone()) {
                    problems.push(format!("端口 {} 被 {} 与 {} 同时占用", port, prev, d.id));
                }
            }
            if d.id != self_id && d.ticket.is_empty() {
                problems.push(format!("对端 {} 缺 ticket，无法建 iroh 隧道", d.id));
            }
        }
        problems
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conf_defaults_when_file_missing_or_broken() {
        let d = tempfile::tempdir().unwrap();
        let missing = d.path().join("nope.conf");
        let c = SyncConf::load(&missing);
        assert_eq!(c.watch_cooldown, 15);

        let broken = d.path().join("bad.conf");
        std::fs::write(&broken, "watch_cooldown = \nbackup_keep = abc\nxx = 1\n").unwrap();
        let c = SyncConf::load(&broken);
        assert_eq!(c.watch_cooldown, 15);
        assert_eq!(c.backup_keep, 20);
    }

    #[test]
    fn conf_reads_values() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("sync.conf");
        std::fs::write(&p, "tick_interval = 60\nwatch_cooldown = 5\nbackup_keep = 3\n").unwrap();
        let c = SyncConf::load(&p);
        assert_eq!(c.tick_interval, 60);
        assert_eq!(c.watch_cooldown, 5);
        assert_eq!(c.backup_keep, 3);
    }

    #[test]
    fn registry_roundtrip_and_upsert() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("devices.json");
        let mut reg = DeviceRegistry::load(&p).unwrap();
        assert!(reg.devices.is_empty());

        reg.upsert(Device {
            id: "mini".into(),
            label: "Mac mini".into(),
            ticket: "endpointaaa".into(),
            local_port: Some(22301),
            ssh_user: "kzf".into(),
            lan: "192.168.1.115".into(),
            repo_path: String::new(),
            net_port: None,
            relay_url: String::new(),
        });
        reg.upsert(Device {
            id: "imac".into(),
            label: "iMac".into(),
            ticket: "endpointbbb".into(),
            local_port: Some(22302),
            ssh_user: "kzf".into(),
            lan: "192.168.1.106".into(),
            repo_path: String::new(),
            net_port: Some(22402),
            relay_url: String::new(),
        });
        reg.save(&p).unwrap();

        let back = DeviceRegistry::load(&p).unwrap();
        assert_eq!(back.devices.len(), 2);
        let peers: Vec<_> = back.peers("mini").map(|d| d.id.as_str()).collect();
        assert_eq!(peers, vec!["imac"]);
    }

    #[test]
    fn validate_flags_duplicate_ports_and_bad_ids() {
        let reg = DeviceRegistry {
            version: 1,
            devices: vec![
                Device {
                    id: "mini".into(),
                    label: String::new(),
                    ticket: "t".into(),
                    local_port: Some(22301),
                    ssh_user: String::new(),
                    lan: String::new(),
                    repo_path: String::new(),
                    net_port: None,
                    relay_url: String::new(),
                },
                Device {
                    id: "Bad_ID".into(),
                    label: String::new(),
                    ticket: String::new(),
                    local_port: Some(22301),
                    ssh_user: String::new(),
                    lan: String::new(),
                    repo_path: String::new(),
                    net_port: None,
                    relay_url: String::new(),
                },
            ],
        };
        let problems = reg.validate("mini");
        assert!(problems.iter().any(|p| p.contains("设备 id 非法")));
        assert!(problems.iter().any(|p| p.contains("同时占用")));
    }
}
