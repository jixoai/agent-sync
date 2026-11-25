//! 面板真实数据的来源：供 `App.tsx` 通过 `asui/native` 调用。
//!
//! 设计取舍：Rust 只返回 **JSON 字符串**，TSX 负责渲染。
//! 不用强类型 Props 的理由：native module 的 TS 生成器只映射标量签名，
//! 而面板需要的是「一屏快照」（设备 + 对端 + 目录 + 状态），
//! 用 JSON 传一层比堆十几个 Props 更直接，也便于以后加字段。
//!
//! 数据来源严格分层，UI 不发明状态：
//!   · 实时运行态 —— daemon 的 unix socket（`{"cmd":"status"}`）
//!   · 设备清单   —— `config/devices.json`（本机视角）
//!   · 行为配置   —— `config.toml` + `config.local.toml`
//! 我们绝不在这里猜「应该是什么」。

use nsh_core::prelude::*;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use agents_sync_core::config::DeviceRegistry;
use agents_sync_core::paths::SyncPaths;
use agents_sync_core::settings::Settings;

/// 面板能力集合。
#[native_module("asui/native")]
fn native(registry: &mut Registry) {
    registry.module::<Sync>();
}

/// 本机同步面板的数据与动作。
#[module]
pub struct Sync;

#[module]
impl Sync {
    /// 一屏快照：本机身份、daemon 状态、对端、目录映射、有效配置。
    pub fn overview() -> String {
        to_json(overview_value())
    }

    /// 动作：`sync` / `pause` / `resume` / `quit`。返回 daemon 回包。
    pub fn action(cmd: String) -> String {
        let req = match cmd.as_str() {
            "sync" | "pause" | "resume" | "quit" => json!({ "cmd": cmd }),
            other => return to_json(json!({ "ok": false, "error": format!("未知动作：{other}") })),
        };
        match ipc_call(&req) {
            Ok(v) => to_json(v),
            Err(e) => to_json(json!({ "ok": false, "error": e })),
        }
    }

    /// 修改本机同步参数（写 `config.local.toml` 的 `[sync]`）。
    ///
    /// 输入格式：`{"tick_interval":1800,"watch_cooldown":15,"backup_keep":20,"prune":true}`，
    /// 只处理出现的键。
    pub fn set_sync(json_text: String) -> String {
        let patch: Value = match serde_json::from_str(&json_text) {
            Ok(v) => v,
            Err(e) => return to_json(json!({ "ok": false, "error": format!("参数不是 JSON：{e}") })),
        };
        let Some(obj) = patch.as_object() else {
            return to_json(json!({ "ok": false, "error": "参数必须是对象" }));
        };
        let mut applied = Vec::new();
        let res = update_local(|root| {
            let sync = table_at(root, &["sync"]);
            for (k, v) in obj {
                match k.as_str() {
                    "tick_interval" | "watch_cooldown" | "backup_keep" => {
                        if let Some(n) = v.as_i64() {
                            sync.insert(k.clone(), toml::Value::Integer(n));
                            applied.push(k.clone());
                        }
                    }
                    "prune" => {
                        if let Some(b) = v.as_bool() {
                            sync.insert(k.clone(), toml::Value::Boolean(b));
                            applied.push(k.clone());
                        }
                    }
                    _ => {}
                }
            }
        });
        match res {
            Ok(()) => to_json(json!({ "ok": true, "applied": applied })),
            Err(e) => to_json(json!({ "ok": false, "error": e })),
        }
    }

    /// 启停一棵树（写 `config.local.toml` 的 `[local.tree_enabled]`）。
    pub fn set_tree_enabled(name: String, enabled: bool) -> String {
        let res = update_local(|root| {
            let local = table_at(root, &["local"]);
            let slot = local
                .entry("tree_enabled".to_string())
                .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
            if !slot.is_table() {
                *slot = toml::Value::Table(toml::map::Map::new());
            }
            slot.as_table_mut()
                .expect("刚保证过是表")
                .insert(name.clone(), toml::Value::Boolean(enabled));
        });
        match res {
            Ok(()) => to_json(json!({ "ok": true })),
            Err(e) => to_json(json!({ "ok": false, "error": e })),
        }
    }

    /// 用系统默认编辑器打开 `config.toml`（缺失则先写模板）。
    pub fn open_config() -> String {
        let Ok(paths) = SyncPaths::from_env() else {
            return to_json(json!({ "ok": false, "error": "无法解析 HOME" }));
        };
        let p = paths.root.join("config.toml");
        if !p.exists() {
            if let Some(parent) = p.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if let Err(e) = std::fs::write(&p, agents_sync_core::settings::CONFIG_TEMPLATE) {
                return to_json(json!({ "ok": false, "error": format!("写配置失败：{e}") }));
            }
        }
        // 不等待编辑器：用户关不关窗口与面板无关
        match std::process::Command::new("/usr/bin/open").arg("-t").arg(&p).spawn() {
            Ok(_) => to_json(json!({ "ok": true, "path": p.display().to_string() })),
            Err(e) => to_json(json!({ "ok": false, "error": format!("打开失败：{e}") })),
        }
    }
}

// ---------- 数据组装 ----------

fn overview_value() -> Value {
    let Ok(paths) = SyncPaths::from_env() else {
        return json!({ "ok": false, "error": "无法解析 HOME（daemon 需要用户会话）" });
    };
    let settings = Settings::load(&paths.root).unwrap_or_default();
    let device = agents_sync_core::local_device_id().unwrap_or_else(|_| "unknown".into());
    let registry = DeviceRegistry::load(&paths.devices_json()).unwrap_or_default();

    let status = ipc_call(&json!({ "cmd": "status" })).ok();
    let state = status.as_ref().and_then(|v| v.get("state"));
    let last = state.and_then(|s| s.get("last"));
    let failed: Vec<String> = last
        .and_then(|l| l.get("failed_fetch"))
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|p| p.as_array().and_then(|x| x.first()).and_then(|x| x.as_str()))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let fetched: Vec<String> = last
        .and_then(|l| l.get("fetched"))
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str()).map(str::to_string).collect())
        .unwrap_or_default();

    let peers: Vec<Value> = registry
        .peers(&device)
        .map(|p| {
            json!({
                "id": p.id,
                "label": p.label,
                "lan": p.lan,
                "ticket": shorten(&p.ticket),
                "last": if failed.iter().any(|x| x == &p.id) {
                    "failed"
                } else if fetched.iter().any(|x| x == &p.id) {
                    "ok"
                } else {
                    "unknown"
                },
            })
        })
        .collect();

    let trees: Vec<Value> = settings
        .trees
        .iter()
        .map(|t| {
            json!({
                "name": t.name,
                "source": t.source,
                "payload": t.payload_name(),
                "target": t.target_or_source(),
                "enabled": settings.is_tree_enabled(&t.name),
                "files": count_files(&paths.payload().join(t.payload_name())),
            })
        })
        .collect();

    json!({
        "ok": true,
        "device": device,
        "label": hostname(),
        "config_path": paths.root.join("config.toml").display().to_string(),
        "jj_version": jj_version(),
        "daemon": {
            "running": status.as_ref().and_then(|v| v.get("ok")).and_then(|v| v.as_bool()).unwrap_or(false),
            "paused": state.and_then(|s| s.get("paused")).and_then(|v| v.as_bool()).unwrap_or(false),
            "syncing": state.and_then(|s| s.get("syncing")).and_then(|v| v.as_bool()).unwrap_or(false),
            "pid": state.and_then(|s| s.get("pid")).cloned().unwrap_or(Value::Null),
        },
        "last": last.cloned().unwrap_or(Value::Null),
        "sync": {
            "tick_interval": settings.sync.tick_interval,
            "watch_cooldown": settings.sync.watch_cooldown,
            "backup_keep": settings.sync.backup_keep,
            "prune": settings.sync.prune,
        },
        "peers": peers,
        "trees": trees,
    })
}

/// daemon 不在时 `status` 会失败；UI 据此显示「未运行」而不是假装有数据。
fn ipc_call(req: &Value) -> Result<Value, String> {
    let paths = SyncPaths::from_env().map_err(|e| format!("无法解析 HOME：{e}"))?;
    let sock = paths.logs_dir().join("daemon.sock");
    let mut s = UnixStream::connect(&sock)
        .map_err(|e| format!("daemon 未运行（{}）：{e}", sock.display()))?;
    s.set_read_timeout(Some(Duration::from_secs(30))).ok();
    s.write_all(format!("{req}\n").as_bytes())
        .map_err(|e| format!("写 IPC 失败：{e}"))?;
    let mut line = String::new();
    BufReader::new(s)
        .read_line(&mut line)
        .map_err(|e| format!("读 IPC 失败：{e}"))?;
    serde_json::from_str(&line).map_err(|e| format!("daemon 回包无法解析：{e}"))
}

/// 把 `config.local.toml` 读出 → 就地改 → 写回（临时文件 + rename）。
///
/// 为什么要做「读-改-写」而不是整份生成：这里只该动本机覆盖项，
/// 用户可能已经在文件里写了 `[local.peers]`，覆盖式写入会把它抹掉。
fn update_local<F: FnOnce(&mut toml::Value)>(f: F) -> Result<(), String> {
    let paths = SyncPaths::from_env().map_err(|e| format!("无法解析 HOME：{e}"))?;
    let p = paths.root.join("config.local.toml");
    let mut root: toml::Value = match std::fs::read_to_string(&p) {
        Ok(text) => toml::from_str(&text).map_err(|e| format!("现有配置无法解析：{e}"))?,
        Err(_) => toml::Value::Table(toml::map::Map::new()),
    };
    f(&mut root);
    let text = toml::to_string(&root).map_err(|e| format!("配置序列化失败：{e}"))?;
    let tmp = p.with_extension("toml.tmp");
    std::fs::write(&tmp, format!("# 由 agents-sync 面板写入：本机私有覆盖项\n{text}"))
        .map_err(|e| format!("写配置失败：{e}"))?;
    std::fs::rename(&tmp, &p).map_err(|e| format!("替换配置失败：{e}"))?;
    Ok(())
}

/// 取（必要时创建）嵌套表。
fn table_at<'a>(
    root: &'a mut toml::Value,
    path: &[&str],
) -> &'a mut toml::map::Map<String, toml::Value> {
    let mut cur = root;
    for key in path {
        if !cur.is_table() {
            *cur = toml::Value::Table(toml::map::Map::new());
        }
        let table = cur.as_table_mut().expect("刚保证过是表");
        cur = table
            .entry((*key).to_string())
            .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
    }
    if !cur.is_table() {
        *cur = toml::Value::Table(toml::map::Map::new());
    }
    cur.as_table_mut().expect("刚保证过是表")
}

fn count_files(dir: &Path) -> usize {
    walk_files(dir).len()
}

/// 极简递归计数（不引入 walkdir：只要文件数，不需要错误语义）。
fn walk_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return out;
    };
    for e in rd.filter_map(Result::ok) {
        let p = e.path();
        let Ok(ft) = e.file_type() else { continue };
        if ft.is_dir() {
            out.extend(walk_files(&p));
        } else if ft.is_file() {
            out.push(p);
        }
    }
    out
}

fn shorten(ticket: &str) -> String {
    let body = ticket.strip_prefix("endpoint").unwrap_or(ticket);
    body.chars().take(10).collect::<String>() + "…"
}

fn jj_version() -> String {
    for bin in ["/opt/homebrew/bin/jj", "/usr/local/bin/jj", "jj"] {
        if let Ok(o) = std::process::Command::new(bin).arg("--version").output() {
            if o.status.success() {
                return String::from_utf8_lossy(&o.stdout).trim().to_string();
            }
        }
    }
    "jj 未找到".into()
}

fn hostname() -> String {
    std::process::Command::new("hostname")
        .arg("-s")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".into())
}

fn to_json(v: Value) -> String {
    serde_json::to_string(&v).unwrap_or_else(|_| "{\"ok\":false}".into())
}
