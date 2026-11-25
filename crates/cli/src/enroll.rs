//! `agents-sync enroll <user@host>`：经 SSH 的带外路径交换双方的 iroh 身份。
//!
//! 为什么需要它：本机 identity 由 `.secrets/<device>.iroh` 推导。该文件一旦被
//! 重建（重装、迁移、清空仓库目录），本机 ticket 就变了，而对端 devices.json
//! 里记的还是旧 id —— iroh 握手会以 `authentication failed` 拒绝，表现为隧道
//! 「连上了但永远等不到 banner」。这不是罕见意外：真机切换时就踩了一次。
//!
//! 为什么用 SSH 自举而不是 iroh：SSH 的身份由主机密钥保证，局域网直连不依赖
//! 任何我们自己的服务。把「谁是谁」这一次性事实从可信路径带过来，之后的传输
//! 仍全部走 iroh。
//!
//! 幂等：重复执行只是把两端的 ticket 再对齐一次，不动端口、路径等人工配置。

use agents_sync_core::config::{Device, DeviceRegistry};
use agents_sync_core::paths::SyncPaths;
use anyhow::{bail, Context, Result};
use std::io::Write;
use std::process::{Command, Stdio};

/// `enroll` 的参数（位置参数之后的部分）。
pub struct Args {
    /// `[user@]host`；不带 user 时沿用注册表里该 peer 的 ssh_user。
    pub target: String,
    /// 远端 CLI 的路径；缺省 `$HOME/.local/bin/agents-sync`。
    pub remote_bin: String,
    /// 交换完成后是否重启两端 daemon（默认 true）。
    pub restart: bool,
}

impl Args {
    pub fn parse(raw: &[String]) -> Result<Self> {
        let mut target = None;
        let mut remote_bin = String::from("$HOME/.local/bin/agents-sync");
        let mut restart = true;
        let mut it = raw.iter();
        while let Some(a) = it.next() {
            match a.as_str() {
                "--no-restart" => restart = false,
                "--remote-bin" => {
                    remote_bin = it.next().context("--remote-bin 需要一个路径")?.clone();
                }
                other if other.starts_with('-') => bail!("enroll 不认识的选项：{other}"),
                other => {
                    if target.replace(other.to_string()).is_some() {
                        bail!("enroll 只接受一个目标（形如 kzf@192.168.1.106）");
                    }
                }
            }
        }
        Ok(Args {
            target: target.context("用法：agents-sync enroll <user@host> [--no-restart]")?,
            remote_bin,
            restart,
        })
    }
}

/// 执行 enroll。返回是否成功（CLI 直接据此设退出码）。
pub fn run(paths: &SyncPaths, args: &Args, local_ticket: &str) -> Result<()> {
    let self_id = agents_sync_core::local_device_id()?;
    let (target_user, target_host) = split_target(&args.target);

    // 1. 从对端取它的真实身份（它自己算的，必然是对的）
    let remote = ssh_capture(
        paths,
        &args.target,
        &format!("{} ticket --json", args.remote_bin),
    )
    .with_context(|| {
        format!(
            "无法在对端取 ticket（{}）。请确认对端已装 CLI：agents-sync install，\
             且 ssh {} 可免密登录",
            args.target, args.target
        )
    })?;
    let remote: RemoteTicket = serde_json::from_str(remote.trim())
        .with_context(|| format!("对端 ticket 输出无法解析：{remote}"))?;

    // 2. 本地注册表：修正对端条目 + 校正本机条目
    let mut reg = DeviceRegistry::load(&paths.devices_json())?;
    let old_ticket = reg
        .devices
        .iter()
        .find(|d| d.id == remote.id)
        .map(|d| d.ticket.clone())
        .unwrap_or_default();

    // 端口：已有条目一律沿用（人工配置优先）；新设备才分配，且一次算定两个端口。
    let existing = reg.devices.iter().find(|d| d.id == remote.id).cloned();
    let (local_port, net_port) = match &existing {
        Some(d) => (d.local_port, d.net_port),
        None => {
            let lp = next_local_port(&reg);
            (lp, lp.map(|p| p + 120))
        }
    };

    let peer = Device {
        id: remote.id.clone(),
        label: reg
            .devices
            .iter()
            .find(|d| d.id == remote.id)
            .map(|d| d.label.clone())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| target_host.clone()),
        ticket: remote.ticket.clone(),
        local_port,
        net_port,
        ssh_user: target_user
            .clone()
            .or_else(|| existing.as_ref().map(|d| d.ssh_user.clone()))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "unknown".into()),
        lan: if target_host.parse::<std::net::IpAddr>().is_ok() {
            target_host.clone()
        } else {
            existing.as_ref().map(|d| d.lan.clone()).unwrap_or_default()
        },
        repo_path: existing
            .as_ref()
            .map(|d| d.repo_path.clone())
            .unwrap_or_default(),
        relay_url: existing
            .as_ref()
            .map(|d| d.relay_url.clone())
            .unwrap_or_default(),
    };
    reg.upsert(peer.clone());

    // 本机条目也必须自洽：ticket 由本机 secret 推导，不能沿用别人的旧值。
    let mut me = reg
        .devices
        .iter()
        .find(|d| d.id == self_id)
        .cloned()
        .unwrap_or_else(|| Device {
            id: self_id.clone(),
            label: hostname(),
            ticket: String::new(),
            local_port: Some(22301),
            net_port: Some(22421),
            ssh_user: std::env::var("USER").unwrap_or_else(|_| "unknown".into()),
            lan: String::new(),
            repo_path: String::new(),
            relay_url: String::new(),
        });
    me.ticket = local_ticket.to_string();
    reg.upsert(me.clone());
    reg.save(&paths.devices_json())?;

    let problems = reg.validate(&self_id);
    for p in &problems {
        eprintln!("⚠️  {p}");
    }

    // 3. 把两端条目推给对端（只 upsert 这两条，保留它对未来设备的记录）
    let payload = serde_json::json!({ "devices": [me, peer] }).to_string();
    let out = ssh_with_stdin(
        paths,
        &args.target,
        &format!("{} enroll-apply", args.remote_bin),
        &payload,
    )
    .with_context(|| format!("对端 {} 写入 devices.json 失败", args.target))?;
    let applied: serde_json::Value =
        serde_json::from_str(out.trim()).with_context(|| format!("对端回执无法解析：{out}"))?;

    // 4. 两端重启，让转发链用新 ticket 重绑
    let mut restarted = Vec::new();
    if args.restart {
        if kickstart(&[]).is_ok() {
            restarted.push("本机");
        }
        let remote_cmd = format!(
            "launchctl kickstart -k gui/$(id -u)/dev.gaubee.agents-sync.daemon >/dev/null 2>&1 && echo restarted || echo skipped"
        );
        if let Ok(o) = ssh_capture(paths, &args.target, &remote_cmd) {
            if o.trim() == "restarted" {
                restarted.push("对端");
            }
        }
    }

    // 5. 汇报（ticket 只显示前缀，完整值在两个 devices.json 与 net 输出里）
    let short = |t: &str| -> String {
        let body = t.strip_prefix("endpoint").unwrap_or(t);
        body.chars().take(10).collect::<String>() + "…"
    };
    println!("对端 {}（{}）", peer.id, args.target);
    if old_ticket == peer.ticket {
        println!("  ticket  {}（未变）", short(&peer.ticket));
    } else {
        println!(
            "  ticket  {} → {}",
            if old_ticket.is_empty() {
                "（空）".into()
            } else {
                short(&old_ticket)
            },
            short(&peer.ticket)
        );
    }
    println!("本机 {}  ticket {}", self_id, short(&me.ticket));
    println!(
        "已写入两端 devices.json（对端现有 {} 台设备）",
        applied
            .get("devices")
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
    );
    if args.restart {
        if restarted.is_empty() {
            println!("⚠️  未重启 daemon（launchd 单元可能未加载）；需要时手动 kickstart");
        } else {
            println!("已重启 daemon：{}", restarted.join("、"));
        }
    }
    if old_ticket != peer.ticket {
        println!("下一步：agents-sync ping {}（应当 ok）", peer.id);
    }
    Ok(())
}

#[derive(serde::Deserialize)]
struct RemoteTicket {
    id: String,
    ticket: String,
}

/// 拆 `[user@]host`；IPv6 不在此支持（局域网场景用 IPv4/主机名）。
fn split_target(target: &str) -> (Option<String>, String) {
    match target.split_once('@') {
        Some((u, h)) => (Some(u.to_string()), h.to_string()),
        None => (None, target.to_string()),
    }
}

/// 选一个没被占用的本地端口（从 22301 起）。
fn next_local_port(reg: &DeviceRegistry) -> Option<u16> {
    let used: Vec<u16> = reg.devices.iter().filter_map(|d| d.local_port).collect();
    (22301..22999).find(|p| !used.contains(p))
}

fn ssh_bin(paths: &SyncPaths) -> Command {
    let known = paths.state_dir().join("known_hosts");
    let _ = std::fs::create_dir_all(paths.state_dir());
    let mut c = Command::new("/usr/bin/ssh");
    c.args([
        "-o",
        "BatchMode=yes",
        "-o",
        "StrictHostKeyChecking=accept-new",
        "-o",
        &format!("UserKnownHostsFile={}", known.display()),
        "-o",
        "ConnectTimeout=10",
    ]);
    c
}

fn ssh_capture(paths: &SyncPaths, target: &str, remote: &str) -> Result<String> {
    let out = ssh_bin(paths)
        .arg(target)
        .arg(remote)
        .output()
        .context("启动 ssh 失败")?;
    if !out.status.success() {
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

fn ssh_with_stdin(
    paths: &SyncPaths,
    target: &str,
    remote: &str,
    stdin: &str,
) -> Result<String> {
    let mut child = ssh_bin(paths)
        .arg(target)
        .arg(remote)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("启动 ssh 失败")?;
    child
        .stdin
        .as_mut()
        .context("ssh stdin 不可用")?
        .write_all(stdin.as_bytes())?;
    let out = child.wait_with_output()?;
    if !out.status.success() {
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

fn kickstart(_extra: &[&str]) -> Result<()> {
    let uid = String::from_utf8_lossy(
        &Command::new("/usr/bin/id").arg("-u").output()?.stdout,
    )
    .trim()
    .to_string();
    let st = Command::new("/bin/launchctl")
        .args([
            "kickstart",
            "-k",
            &format!("gui/{uid}/dev.gaubee.agents-sync.daemon"),
        ])
        .status()?;
    if !st.success() {
        bail!("launchctl kickstart 退出码 {:?}", st.code());
    }
    Ok(())
}

fn hostname() -> String {
    Command::new("hostname")
        .arg("-s")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".into())
}
