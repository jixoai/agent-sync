//! 意图（2026-11-24 Owner）：最终只提供一个 CLI。同一个二进制按 argv 分三种态：
//!   · cli    —— 一次性动作（sync/status/init/doctor/install…），跑完即退
//!   · daemon —— 无界面常驻服务（服务器/无 GUI 场景）
//!   · app    —— 菜单栏应用，内部自带 daemon（日常形态）
//!   这来自 reply9842 的做法：一个文件，行为由参数决定，bundle 由自己运行时生成。
//!
//! 本文件职责（4 个正交意图）：
//!   1. argv 分派（含「已在 bundle 内」的防重入）
//!   2. 本地一次性动作（不依赖 daemon 也能工作 —— 离线可用是硬要求）
//!   3. 与 daemon 的 IPC 客户端（daemon 在就用它，不在就自己跑）
//!   4. install / uninstall 入口
//! 不承载：同步语义（core）、网络实现（net）、界面（ui）

mod enroll;
mod install;

use agents_sync_core::config::{DeviceRegistry, SyncConf};
use agents_sync_core::lock::DirLock;
use agents_sync_core::mirror::apply::ApplyOptions;
use agents_sync_core::paths::SyncPaths;
use agents_sync_core::sync::{Broadcaster, SyncEngine};
use agents_sync_net::{parse_ticket, Message, Node};
use anyhow::{Context, Result};
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<()> {
    // 默认只留我们自己 crate 的 info：iroh 的 per-packet INFO 会把日志淹掉
    // （实测 48KB 日志里 95% 是 poll_send），排查时反而看不到关键行。
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(
        "warn,agents_sync_core=info,agents_sync_daemon=info,agents_sync_net=info",
    ))
    .init();

    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().cloned().unwrap_or_else(|| "status".into());
    args.remove(0);

    // `--version` / `-V` 必须在任何路径解析之前：脚本与 npm shim 只想知道版本。
    if matches!(cmd.as_str(), "--version" | "-V" | "version") {
        println!("agents-sync {}", agents_sync_core::update::CURRENT);
        return Ok(());
    }

    let paths = SyncPaths::from_env()?;
    paths.ensure_runtime_dirs()?;

    match cmd.as_str() {
        "help" | "--help" | "-h" => {
            print_help();
            Ok(())
        }
        "init" => cmd_init(paths).await,
        "status" => cmd_status(paths).await,
        "sync" | "publish" | "pull" => cmd_sync(paths, &cmd).await,
        "update" => cmd_update(&paths, &args),
        "daemon" => cmd_daemon(paths).await,
        "app" => cmd_app(paths).await,
        "install" => cmd_install(&paths),
        "uninstall" => {
            install::uninstall(&paths.home)?;
            println!("已卸载：{}", install::bundle_path(&paths.home).display());
            Ok(())
        }
        "install-status" => {
            println!("{}", serde_json::to_string_pretty(&install::status(&paths.home))?);
            Ok(())
        }
        "ticket" => cmd_ticket(&args),
        "enroll" => cmd_enroll(&paths, &args),
        // 只由对端 enroll 经 SSH 调用：把两条设备记录并进本机注册表
        "enroll-apply" => cmd_enroll_apply(&paths),
        "net" => cmd_net(&paths).await,
        "ping" => cmd_ping(&paths, args.first().map(|s| s.as_str())).await,
        "doctor" => cmd_doctor(paths).await,
        other => {
            eprintln!("未知命令：{other}\n");
            print_help();
            std::process::exit(2);
        }
    }
}

fn print_help() {
    println!(
        r#"agents-sync —— 多设备配置同步（jj 管状态 / iroh 只组网）

  install            在 ~/Applications 注入菜单栏应用并注册 launchd
  uninstall          移除应用与 launchd 单元
  install-status     查看注入状态

  app                前台运行菜单栏应用（launchd 用的就是它）
  daemon             无界面常驻（服务器 / 无 GUI）
  sync               立即同步一次（daemon 在则交给它）
  status             汇总状态（daemon 在则读它）
  init               初始化仓库并建立 main 基线
  ticket [--json]    打印本机 iroh ticket
  update [--check] [--force]  自更新（npm registry → 校验 → 替换 → 重启）
  enroll <user@host> 经 SSH 交换双方身份（identity 漂移后的一条命令自愈）
  net                组网诊断（本机身份 + 各 peer 可达性）
  doctor             只读体检
"#
    );
}

async fn cmd_init(paths: SyncPaths) -> Result<()> {
    let device = agents_sync_core::local_device_id()?;
    let engine = SyncEngine::with_device_id(paths.clone(), device.clone())?;
    engine.init()?;
    // 首次 init 把本机 ticket 写进 devices.json（本机视角配置，不入库）
    let mut reg = DeviceRegistry::load(&paths.devices_json())?;
    if reg.devices.iter().all(|d| d.id != device) {
        reg.upsert(agents_sync_core::config::Device {
            id: device.clone(),
            label: hostname(),
            ticket: String::new(),
            local_port: Some(22301),
            ssh_user: whoami(),
            lan: String::new(),
            repo_path: String::new(),
            net_port: Some(22401),
            relay_url: String::new(),
        });
        reg.save(&paths.devices_json())?;
    }
    println!("初始化完成，设备 id={device}");
    println!("下一步：把对端 ticket 填进 {}", paths.devices_json().display());
    Ok(())
}

async fn cmd_status(paths: SyncPaths) -> Result<()> {
    // 优先问 daemon：它掌握实时状态
    let sock = agents_sync_daemon::default_socket(&paths);
    if sock.exists() {
        match agents_sync_daemon::call(&sock, &agents_sync_daemon::Request::Status).await {
            Ok(resp) => {
                println!("{}", serde_json::to_string_pretty(&resp)?);
                return Ok(());
            }
            Err(e) => log::warn!("daemon 不可用（{e}），回落到本地状态"),
        }
    }
    let engine = SyncEngine::new(paths.clone())?;
    let changed = engine.jj.has_changes(&paths.root).unwrap_or(false);
    let tree = agents_sync_core::checks::tree_hash(&paths.payload())?;
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "device": engine.device_id,
            "jj_version": engine.jj.ensure_supported()?,
            "repo": paths.root.display().to_string(),
            "payload_tree_hash": tree,
            "uncommitted": changed,
            "peers": engine.registry.peers(&engine.device_id).map(|d| d.id.clone()).collect::<Vec<_>>(),
            "links": paths.links_json().display().to_string(),
        }))?
    );
    Ok(())
}

async fn cmd_sync(paths: SyncPaths, which: &str) -> Result<()> {
    // daemon 在跑就交给它，避免两路同时写活目录
    let sock = agents_sync_daemon::default_socket(&paths);
    if sock.exists() {
        let req = match which {
            "publish" => agents_sync_daemon::Request::Publish,
            "pull" => agents_sync_daemon::Request::Pull,
            _ => agents_sync_daemon::Request::Sync,
        };
        match agents_sync_daemon::call(&sock, &req).await {
            Ok(_) => {
                println!("已交给常驻服务处理");
                return Ok(());
            }
            Err(e) => log::warn!("daemon 不可用（{e}），本进程直接执行"),
        }
    }

    let _lock = DirLock::acquire(&paths.locks_dir(), "repo", Duration::from_secs(30))?;
    let engine = SyncEngine::new(paths.clone())?;
    let broadcaster = match make_broadcaster(&paths, &engine).await {
        Ok(b) => b,
        Err(e) => {
            log::warn!("组网不可用（{e:#}）：本次只做本地同步");
            None
        }
    };
    let nb: &dyn Broadcaster = match &broadcaster {
        Some(b) => b,
        None => &Noop,
    };

    if which != "pull" {
        let r = engine.publish(nb)?;
        println!("publish: 采集 {} 个文件，提交={}", r.captured, r.committed);
    }
    if which != "publish" {
        let r = engine.pull(&ApplyOptions {
            prune: engine.conf.prune,
            backup_keep: engine.conf.backup_keep,
            not_after: None,
        })?;
        println!(
            "pull: 拉取 {:?}，落盘 {:?}",
            r.fetched,
            r.applied.as_ref().map(|a| a.files_written)
        );
        // 失败必须默认可见：只报成功会让「什么也没发生」看起来像正常。
        for (peer, why) in &r.failed_fetch {
            eprintln!("  ! 拉取 {peer} 失败：{why}");
        }
        if !r.conflicts.is_empty() {
            eprintln!("  ! 存在冲突：{}", r.conflicts.join("; "));
        }
    }
    Ok(())
}

async fn cmd_daemon(paths: SyncPaths) -> Result<()> {
    let cfg = daemon_config(paths)?;
    agents_sync_daemon::run(cfg).await
}

async fn cmd_app(paths: SyncPaths) -> Result<()> {
    // 菜单栏应用：没有常驻 daemon 时才在后台线程起一个，再进 GUI。
    //
    // 为什么必须检查：install 会同时注册 UI 与 daemon 两个 launchd 单元。
    // 若 UI 每次都再起一份 daemon，同一密钥会开出两个 iroh endpoint：
    // 端口被抢的那个退化到临时端口，表现为持续的对端 `authentication failed`
    // 与重复广播（实测两台机器各多跑一个，日志被刷了 229 条）。
    let sock = agents_sync_daemon::default_socket(&paths);
    let already_running = sock.exists()
        && matches!(
            agents_sync_daemon::call(&sock, &agents_sync_daemon::Request::Status).await,
            Ok(r) if r.ok
        );
    if already_running {
        log::info!("已有 daemon 在运行（{}），UI 不再重复启动", sock.display());
    } else {
        let cfg = daemon_config(paths)?;
        std::thread::spawn(move || {
            let rt = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
                Ok(rt) => rt,
                Err(e) => {
                    log::error!("daemon 运行时创建失败：{e}");
                    return;
                }
            };
            if let Err(e) = rt.block_on(agents_sync_daemon::run(cfg)) {
                log::error!("daemon 退出：{e:#}");
            }
        });
    }

    // 以兄弟二进制启动 GUI：CLI 本身不链接 AppKit。
    let exe = std::env::current_exe()?;
    let sibling = exe.with_file_name("agents-sync-app");
    if !sibling.exists() {
        anyhow::bail!(
            "未找到 GUI 组件 {}：请与 agents-sync 一起安装（install 会把两个二进制放进 bundle）",
            sibling.display()
        );
    }
    let status = std::process::Command::new(&sibling).status()?;
    if !status.success() {
        anyhow::bail!("GUI 退出码 {:?}", status.code());
    }
    Ok(())
}

fn daemon_config(paths: SyncPaths) -> Result<agents_sync_daemon::DaemonConfig> {
    let device_id = agents_sync_core::local_device_id()?;
    // 与 SyncEngine 同源：config.toml 的 [sync]
    let settings = agents_sync_core::settings::Settings::load(&paths.root).unwrap_or_default();
    let conf = SyncConf::from_settings(&settings.sync);
    let registry = DeviceRegistry::load(&paths.devices_json())?;
    let ipc_socket = agents_sync_daemon::default_socket(&paths);
    Ok(agents_sync_daemon::DaemonConfig {
        paths,
        device_id,
        conf,
        registry,
        ipc_socket,
    })
}

fn cmd_install(paths: &SyncPaths) -> Result<()> {
    let r = install::install(&paths.home)?;
    println!("已注入：{}", r.bundle.display());
    println!("可执行：{}", r.binary.display());
    println!("launchd：{}（已加载={}）", r.plist.display(), r.reloaded);
    Ok(())
}

fn cmd_ticket(args: &[String]) -> Result<()> {
    let paths = SyncPaths::from_env()?;
    let device = agents_sync_core::local_device_id()?;
    let hex = load_or_create_secret(&paths, &device)?;
    // ticket 完全由 secret 推导，不需要绑网络：这样首次配置也能离线完成。
    let id = agents_sync_net::secret_from_hex(&hex)?.public();
    let ticket = format!("endpoint{}", id.to_z32());
    if args.iter().any(|a| a == "--json") {
        println!(
            "{}",
            serde_json::to_string(&serde_json::json!({ "id": device, "ticket": ticket }))?
        );
    } else {
        println!("{ticket}");
    }
    Ok(())
}

/// 自更新：默认检查并安装；`--check` 只看；`--force` 同版本重装。
///
/// 为什么放在 CLI 而不是 daemon 里单独实现：UI 的「更新」按钮、手动运维、
/// daemon 的自动更新都调用同一条路径，行为只有一份。
fn cmd_update(paths: &SyncPaths, args: &[String]) -> Result<()> {
    let settings = agents_sync_core::settings::Settings::load(&paths.root).unwrap_or_default();
    let mut proxy = settings.update.proxy.clone();
    let mut mirror = settings.update.mirror.clone();
    let mut check_only = false;
    let mut force = false;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--check" => check_only = true,
            "--force" => force = true,
            "--proxy" => proxy = it.next().cloned().unwrap_or_default(),
            "--mirror" => mirror = it.next().cloned().unwrap_or_default(),
            other => anyhow::bail!("update 不认识的选项：{other}"),
        }
    }
    let current = agents_sync_core::update::CURRENT;
    if check_only {
        let rel = agents_sync_core::update::check(&proxy, &mirror)?;
        let newer = agents_sync_core::update::is_newer(&rel.version, current);
        println!(
            "当前 {current}，registry {}（{}）",
            rel.version,
            if newer { "可更新" } else { "已是最新" }
        );
        return Ok(());
    }
    let r = agents_sync_core::update::apply(paths, &proxy, &mirror, force)?;
    println!("已更新 {} → {}（{}）", r.from, r.to, r.binary.display());
    if let Some(b) = &r.bundle {
        println!("app：{}", b.display());
    }
    if r.restarted.is_empty() {
        println!("未重启任何单元（launchd 未加载）；下次启动即用新版本");
    } else {
        println!("已重启：{}", r.restarted.join("、"));
    }
    Ok(())
}

fn cmd_enroll(paths: &SyncPaths, args: &[String]) -> Result<()> {
    let parsed = enroll::Args::parse(args)?;
    let device = agents_sync_core::local_device_id()?;
    let hex = load_or_create_secret(paths, &device)?;
    let ticket = format!(
        "endpoint{}",
        agents_sync_net::secret_from_hex(&hex)?.public().to_z32()
    );
    enroll::run(paths, &parsed, &ticket)
}

/// 对端 `enroll` 经 SSH 调用；只 upsert 传入的设备，保留本机对未来设备的记录。
fn cmd_enroll_apply(paths: &SyncPaths) -> Result<()> {
    use std::io::Read;
    let mut text = String::new();
    std::io::stdin().read_to_string(&mut text)?;
    #[derive(serde::Deserialize)]
    struct Payload {
        devices: Vec<agents_sync_core::config::Device>,
    }
    let payload: Payload = serde_json::from_str(&text).context("enroll-apply 输入不是合法 JSON")?;
    let self_id = agents_sync_core::local_device_id()?;
    let mut reg = DeviceRegistry::load(&paths.devices_json())?;
    for d in payload.devices {
        reg.upsert(d);
    }
    // 本机条目的 ticket 必须由本机 secret 推导；若对端带来的值不同，以本机为准
    if let Some(me) = reg.devices.iter_mut().find(|d| d.id == self_id) {
        let hex = load_or_create_secret(paths, &self_id)?;
        me.ticket = format!(
            "endpoint{}",
            agents_sync_net::secret_from_hex(&hex)?.public().to_z32()
        );
    }
    reg.save(&paths.devices_json())?;
    println!(
        "{}",
        serde_json::to_string(&serde_json::json!({
            "ok": true,
            "self": self_id,
            "devices": reg.devices.len(),
        }))?
    );
    Ok(())
}

/// 单独验证「能不能叫醒对端」。
///
/// 存在的理由：广播是跨机同步的唯一触发器（对端不会自己知道我有新提交），
/// 而它失败时表现和「内容没变」完全一样。把它做成可直接调用的诊断命令，
/// 就不用每次都起两个 daemon 再猜。
async fn cmd_ping(paths: &SyncPaths, peer_id: Option<&str>) -> Result<()> {
    let engine = SyncEngine::new(paths.clone())?;
    let device = engine.device_id.clone();
    let peer_id = peer_id.context("用法：agents-sync ping <peer-id>")?;
    let peer = engine
        .registry
        .peers(&device)
        .find(|p| p.id == peer_id)
        .with_context(|| format!("注册表里没有 peer {peer_id}"))?
        .clone();

    let hex = load_or_create_secret(paths, &device)?;
    let node = Node::bind(agents_sync_net::secret_from_hex(&hex)?).await?;
    let id = parse_ticket(&peer.ticket).with_context(|| format!("peer {} 的 ticket 非法", peer.id))?;
    let chain = Node::target(id, &peer.lan, peer.net_port, &peer.relay_url);
    let kind = if peer.lan.is_empty() { "discovery" } else { "direct" };

    let started = std::time::Instant::now();
    let result = node
        .send_message_chain(
            chain.clone(),
            &agents_sync_net::Message::Ping { from: device.clone() },
        )
        .await;
    let ms = started.elapsed().as_millis();
    node.close().await;

    match result {
        Ok(_) => {
            println!("ping {} ok（{kind}，{ms}ms）", peer.id);
            Ok(())
        }
        Err(e) => {
            println!("ping {} 失败（{kind}，{ms}ms）：{e:#}", peer.id);
            std::process::exit(1);
        }
    }
}

async fn cmd_net(paths: &SyncPaths) -> Result<()> {
    let engine = SyncEngine::new(paths.clone())?;
    let device = engine.device_id.clone();
    let hex = load_or_create_secret(paths, &device)?;
    let node = Node::bind_on(agents_sync_net::secret_from_hex(&hex)?, None).await?;
    // relay 地址要先 online 才有；等不到就说明出网被限，纯局域网时才可忽略
    let online = node.wait_online(std::time::Duration::from_secs(12)).await;
    let relay = node.relay_url().unwrap_or_default();
    node.close().await;

    let peers: Vec<_> = engine
        .registry
        .peers(&device)
        .map(|p| {
            serde_json::json!({
                "id": p.id,
                "ticket": p.ticket,
                "ticket_ok": parse_ticket(&p.ticket).is_ok(),
                "local_port": p.local_port,
                "net_port": p.net_port,
                "lan": p.lan,
                "relay_url": p.relay_url,
            })
        })
        .collect();

    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "self": {
                "id": device,
                "ticket": format!("endpoint{}", agents_sync_net::secret_from_hex(&hex)?.public().to_z32()),
                "relay_url": relay,
                "relay_online": online,
            },
            "peers": peers,
        }))?
    );
    Ok(())
}

async fn cmd_doctor(paths: SyncPaths) -> Result<()> {
    let mut fails = 0;
    let mut warn = 0;
    let mut say = |ok: bool, hard: bool, msg: String| {
        if !ok {
            if hard {
                fails += 1;
            } else {
                warn += 1;
            }
            println!("  {} {}", if hard { "❌" } else { "⚠️ " }, msg);
        } else {
            println!("  ✅ {msg}");
        }
    };

    match agents_sync_core::vcs::Jj::locate() {
        Ok(jj) => match jj.ensure_supported() {
            Ok(v) => say(true, true, format!("jj {v}（{}）", jj.bin().display())),
            Err(e) => say(false, true, format!("{e}")),
        },
        Err(e) => say(false, true, format!("{e}")),
    }

    let engine = SyncEngine::new(paths.clone());
    match &engine {
        Ok(e) => {
            let problems = e.registry.validate(&e.device_id);
            if problems.is_empty() {
                say(true, true, "设备注册表合法".into());
            } else {
                for p in problems {
                    say(false, true, p);
                }
            }
        }
        Err(e) => say(false, true, format!("初始化引擎失败：{e}")),
    }

    let hits = agents_sync_core::checks::scan(&paths.payload(), &[])?;
    if hits.is_empty() {
        say(true, true, "payload 无高置信凭据".into());
    } else {
        say(false, true, format!("payload 检出 {} 处疑似凭据", hits.len()));
        for h in hits.iter().take(5) {
            println!("      {h}");
        }
    }

    let st = install::status(&paths.home);
    say(st.bundle_exists, false, format!("bundle: {}", st.bundle));
    say(st.loaded, false, format!("launchd 已加载：{}", st.loaded));

    println!("\n结论：{fails} 项失败，{warn} 项告警");
    if fails > 0 {
        std::process::exit(1);
    }
    Ok(())
}

/// 组网广播器：把 Message::Changed 发给每台 peer。
struct IrohBroadcaster {
    node: Node,
    ids: Vec<(String, Vec<agents_sync_net::EndpointAddr>)>,
}

impl Broadcaster for IrohBroadcaster {
    fn wake(&self, device: &agents_sync_core::config::Device) -> Result<()> {
        let Some((_, addr)) = self.ids.iter().find(|(name, _)| name == &device.id) else {
            anyhow::bail!("peer {} 没有可用 ticket", device.id);
        };
        let node = self.node.clone();
        let addr = addr.clone();
        let from = device.id.clone();
        // 广播是尽力而为：不值得阻塞 publish
        tokio::runtime::Handle::try_current()
            .map(|h| {
                h.spawn(async move {
                    let _ = node
                        .send_message_chain(
                            addr,
                            &Message::Changed {
                                from,
                                tree_hash: String::new(),
                            },
                        )
                        .await;
                });
            })
            .ok();
        Ok(())
    }
}

struct Noop;
impl Broadcaster for Noop {
    fn wake(&self, _d: &agents_sync_core::config::Device) -> Result<()> {
        Ok(())
    }
}

async fn make_broadcaster(
    paths: &SyncPaths,
    engine: &SyncEngine,
) -> Result<Option<IrohBroadcaster>> {
    let device = engine.device_id.clone();
    let hex = load_or_create_secret(paths, &device)?;
    let node = Node::bind(agents_sync_net::secret_from_hex(&hex)?).await?;
    let mut ids = Vec::new();
    for peer in engine.registry.peers(&device) {
        if let Ok(id) = parse_ticket(&peer.ticket) {
            ids.push((
                peer.id.clone(),
                Node::target(id, &peer.lan, peer.net_port, &peer.relay_url),
            ));
        }
    }
    Ok(Some(IrohBroadcaster { node, ids }))
}

pub(crate) fn load_or_create_secret(paths: &SyncPaths, device: &str) -> Result<String> {
    let p = paths.secrets_dir().join(format!("{device}.iroh"));
    if p.exists() {
        return Ok(std::fs::read_to_string(&p)?);
    }
    std::fs::create_dir_all(paths.secrets_dir())?;
    let hex = agents_sync_net::new_secret_hex();
    std::fs::write(&p, &hex)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(hex)
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

fn whoami() -> String {
    std::env::var("USER").unwrap_or_else(|_| "unknown".into())
}
