//! 意图（2026-11-24 Owner）：「用 nsh 开发一个原生应用，带 daemon 的」。
//!   本文件是那个 daemon：常驻、无人值守、可被 CLI 与菜单栏 UI 共同指挥。
//!
//! 本文件职责（5 个正交意图）：
//!   1. 生命周期：单实例、优雅退出、无孤儿
//!   2. 触发：FSEvents 监听 + 抖动合并（替代 watchman）
//!   3. 执行：串行化同步（谈锁），永不并发写活目录
//!   4. IPC：unix socket 上新行分隔 JSON，供 CLI/UI 查询与下令
//!   5. 组网：iroh 暴露 sshd、为每个 peer 建本地转发、接收广播
//! 不承载：同步语义（core）、UI 渲染（ui crate）。

use agents_sync_core::config::{DeviceRegistry, SyncConf};
use agents_sync_core::lock::DirLock;
use agents_sync_core::mirror::apply::ApplyOptions;
use agents_sync_core::paths::SyncPaths;
use agents_sync_core::sync::SyncEngine;
use agents_sync_net::{parse_ticket, Message, Node};
use anyhow::{Context, Result};
use notify::{RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::mpsc;

/// 一次同步的结果摘要（UI 只关心这些）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SyncOutcome {
    pub at: String,
    pub ok: bool,
    pub captured: usize,
    pub committed: bool,
    pub pushed: Vec<String>,
    pub failed_push: Vec<(String, String)>,
    pub fetched: Vec<String>,
    pub failed_fetch: Vec<(String, String)>,
    pub applied_files: usize,
    pub removed_files: usize,
    pub conflicts: Vec<String>,
    pub error: Option<String>,
}

/// 进程内共享状态。UI 与 CLI 都读它。
#[derive(Debug, Default)]
pub struct DaemonState {
    pub last: Option<SyncOutcome>,
    pub paused: bool,
    pub watching: Vec<PathBuf>,
    pub started_at: Option<String>,
    pub syncing: bool,
    /// 本进程的版本（UI 展示与更新判断都用它）。
    pub version: String,
    /// 检查到的新版本；None 表示已是最新或还没查过。
    pub update_available: Option<String>,
}

/// CLI / UI 发来的指令。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    Status,
    Sync,
    Publish,
    Pull,
    Pause,
    Resume,
    Conflicts,
    Net,
    Quit,
}

/// 回包。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<serde_json::Value>,
}

pub struct DaemonConfig {
    pub paths: SyncPaths,
    pub device_id: String,
    pub conf: SyncConf,
    pub registry: DeviceRegistry,
    /// 同步的触发通道；容量 1 保证「多次触发合并成一次」。
    pub ipc_socket: PathBuf,
}

/// 触发源：文件变动、广播、IPC、定时兜底，最终都汇到这一个通道。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Trigger {
    Watcher,
    Broadcast,
    Manual,
    Tick,
}

pub async fn run(cfg: DaemonConfig) -> Result<()> {
    cfg.paths.ensure_runtime_dirs()?;

    // 单实例：daemon 自己的锁，与同步锁分开——同步锁只保护仓库写入。
    let _instance = DirLock::acquire(&cfg.paths.locks_dir(), "daemon", Duration::from_secs(0))
        .context("已有 daemon 在运行（单实例锁被占用）")?;

    let state = Arc::new(Mutex::new(DaemonState {
        watching: watch_paths(&cfg.paths),
        started_at: Some(now()),
        version: agents_sync_core::update::CURRENT.to_string(),
        ..Default::default()
    }));

    let (tx, mut rx) = mpsc::channel::<Trigger>(8);
    let paused = Arc::new(AtomicBool::new(false));

    // ---- 组网 ----
    let mut node = match start_network(&cfg, tx.clone()).await {
        Ok(n) => Some(n),
        Err(e) => {
            // 组网失败不阻塞本地同步：单机仍然要能工作（离线可用是硬要求）。
            log::warn!("组网未启动：{e:#}");
            None
        }
    };

    // ---- 文件监听 ----
    spawn_watcher(&cfg, tx.clone())?;

    // ---- IPC ----
    let ipc_path = cfg.ipc_socket.clone();
    let ipc_state = state.clone();
    let ipc_tx = tx.clone();
    let ipc_cfg = IpcCtx {
        paths: cfg.paths.clone(),
        device_id: cfg.device_id.clone(),
        paused: paused.clone(),
    };
    tokio::spawn(async move {
        if let Err(e) = serve_ipc(ipc_path, ipc_state, ipc_tx, ipc_cfg).await {
            log::error!("IPC 服务退出：{e:#}");
        }
    });

    // ---- 定时兜底：广播可能丢失，低频重扫 ----
    {
        let tx = tx.clone();
        let interval = cfg.conf.tick_interval.max(60);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(interval));
            ticker.tick().await; // 首次立即触发会与启动同步叠加，跳过
            loop {
                ticker.tick().await;
                let _ = tx.send(Trigger::Tick).await;
            }
        });
    }

    // ---- 自更新：定期查 npm registry ----
    spawn_updater(&cfg, state.clone(), paused.clone());

    // 隧道未就绪就开始同步只会得到 connection refused，看起来像「内容没同步」。
    // 首次启动阻塞等待（有上限），之后再进入事件循环。
    wait_for_forwards(&cfg, Duration::from_secs(30)).await;
    log::info!("daemon 就绪（设备 {}）", cfg.device_id);

    // ---- 主循环：串行执行同步 ----
    while let Some(trigger) = rx.recv().await {
        if paused.load(Ordering::Relaxed) {
            log::debug!("{trigger:?} 忽略：已暂停");
            continue;
        }
        // 合并窗口：把这段时间内到达的其它触发一起消费掉
        let mut burst = vec![trigger];
        while let Ok(t) = rx.try_recv() {
            burst.push(t);
        }
        log::info!("开始同步（触发：{:?}）", burst);

        if let Ok(mut s) = state.lock() {
            s.syncing = true;
        }
        let outcome = run_sync(&cfg);
        // 广播：「我变了，来拉」。只在**真的产生了新提交**时发。
        //
        // 无条件广播会形成回声：A 同步→广播→B 同步→广播→A…（实测每 20 秒一轮，
        // 两端 jj 持续空转）。对端在我这里没有新提交时无处可拉，喊它毫无意义。
        if outcome.ok && outcome.committed {
            if let Some(node) = &node {
                for peer in cfg.registry.peers(&cfg.device_id) {
                    let Ok(id) = parse_ticket(&peer.ticket) else {
                        continue;
                    };
                    let chain = Node::target(id, &peer.lan, peer.net_port, &peer.relay_url);
                    let node = node.clone();
                    let from = cfg.device_id.clone();
                    let peer_id = peer.id.clone();
                    tokio::spawn(async move {
                        match node
                            .send_message_chain(
                                chain,
                                &Message::Changed {
                                    from,
                                    tree_hash: String::new(),
                                },
                            )
                            .await
                        {
                            // 成功也记一条：跨机排查时「发没发出去」是第一分叉点。
                            Ok(_) => log::info!("已广播变更给 {peer_id}"),
                            Err(e) => log::warn!("广播给 {peer_id} 失败：{e:#}"),
                        }
                    });
                }
            }
        }
        // 失败必须留下痕迹：daemon 静默失败是最难排查的一类问题。
        if let Some(err) = &outcome.error {
            log::warn!("本轮同步未完成：{err}");
        } else {
            log::info!(
                "本轮同步完成：采集 {}，落盘 {}",
                outcome.captured,
                outcome.applied_files
            );
        }
        if let Ok(mut s) = state.lock() {
            s.syncing = false;
            s.last = Some(outcome);
        }
    }

    if let Some(node) = node.take() {
        node.close().await;
    }
    Ok(())
}

/// 在当前线程池外跑同步：它要调 jj/git/tar，是阻塞型工作。
fn run_sync(cfg: &DaemonConfig) -> SyncOutcome {
    // 同步开始时刻：apply 会跳过「此刻之后被修改」的文件，避免覆盖用户正在编辑的内容。
    let sync_started = std::time::SystemTime::now();
    let mut outcome = SyncOutcome {
        at: now(),
        ..Default::default()
    };

    // 跨进程锁：CLI 可能同时在下命令，定时兜底也可能撞上。
    let _lock = match DirLock::acquire(&cfg.paths.locks_dir(), "repo", Duration::from_secs(30)) {
        Ok(l) => l,
        Err(e) => {
            outcome.error = Some(format!("取锁失败：{e}"));
            return outcome;
        }
    };

    let engine = match SyncEngine::with_device_id(cfg.paths.clone(), cfg.device_id.clone()) {
        Ok(e) => e,
        Err(e) => {
            outcome.error = Some(format!("初始化引擎失败：{e}"));
            return outcome;
        }
    };

    // publish：本机改动落账并推给可达的 peer
    match engine.publish(&Noop) {
        Ok(r) => {
            outcome.captured = r.captured;
            outcome.committed = r.committed;
        }
        Err(e) => {
            outcome.error = Some(format!("publish：{e}"));
            return outcome;
        }
    }

    // pull：对齐并落盘（有冲突则拒绝）
    match engine.pull(&ApplyOptions { prune: cfg.conf.prune, backup_keep: cfg.conf.backup_keep,
        not_after: Some(sync_started) }) {
        Ok(r) => {
            outcome.fetched = r.fetched;
            outcome.failed_fetch = r.failed_fetch;
            if let Some(a) = r.applied {
                outcome.applied_files = a.files_written;
                outcome.removed_files = a.files_removed;
            }
            outcome.ok = true;
        }
        Err(e) => {
            let msg = e.to_string();
            outcome.conflicts = msg
                .lines()
                .filter(|l| l.contains("conflict") || l.contains("冲突"))
                .map(|l| l.trim().to_string())
                .collect();
            outcome.error = Some(msg);
        }
    }
    outcome
}

/// daemon 内部不需要广播（广播是 publish 之后通知对端，对端自己会拉）。
/// 真正的广播由 CLI 在 publish 后触发，或由本进程的 node 发——见 start_network。
struct Noop;
impl agents_sync_core::sync::Broadcaster for Noop {
    fn wake(&self, _d: &agents_sync_core::config::Device) -> Result<()> {
        Ok(())
    }
}

/// 建立 iroh 节点：暴露本机 sshd、为每个 peer 建本地转发、接收广播。
async fn start_network(cfg: &DaemonConfig, tx: mpsc::Sender<Trigger>) -> Result<Node> {
    let secret_path = cfg.paths.secrets_dir().join(format!("{}.iroh", cfg.device_id));
    let hex = if secret_path.exists() {
        std::fs::read_to_string(&secret_path)?
    } else {
        let hex = agents_sync_net::new_secret_hex();
        std::fs::create_dir_all(cfg.paths.secrets_dir())?;
        std::fs::write(&secret_path, &hex)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&secret_path, std::fs::Permissions::from_mode(0o600))?;
        }
        hex
    };
    // 绑定固定 UDP 端口：直连需要「IP:端口」，随机端口会让 devices.json 里的 lan 无从使用。
    let self_net_port = cfg
        .registry
        .devices
        .iter()
        .find(|d| d.id == cfg.device_id)
        .and_then(|d| d.net_port);
    let node = Node::bind_on(agents_sync_net::secret_from_hex(&hex)?, self_net_port).await?;
    if let Some(actual) = node.udp_port() {
        log::info!("iroh 监听 UDP {actual}");
    }
    log::info!("iroh 身份 {} ticket {}", node.id().to_z32(), node.ticket());

    // 接收广播 → 触发同步
    {
        let node = node.clone();
        let tx = tx.clone();
        tokio::spawn(async move {
            let handler = move |msg: Message, _peer| {
                if let Message::Changed { from, .. } = msg {
                    log::info!("收到 {from} 的变更广播");
                    let _ = tx.try_send(Trigger::Broadcast);
                }
            };
            if let Err(e) = node
                .serve("127.0.0.1:22".parse::<SocketAddr>().unwrap(), handler)
                .await
            {
                log::error!("iroh 服务退出：{e:#}");
            }
        });
    }

    // 为每个 peer 建本地转发端口
    for peer in cfg.registry.peers(&cfg.device_id) {
        let Ok(id) = parse_ticket(&peer.ticket) else {
            log::warn!("peer {} 的 ticket 无法解析，跳过转发", peer.id);
            continue;
        };
        let Ok(port) = peer.port() else {
            log::warn!("peer {} 缺 local_port，跳过转发", peer.id);
            continue;
        };
        let node = node.clone();
        let name = peer.id.clone();
        // 直连优先：devices.json 记了 lan + net_port 就不必依赖 n0 发现
        let chain = Node::target(id, &peer.lan, peer.net_port, &peer.relay_url);
        tokio::spawn(async move {
            let listen: SocketAddr = ([127, 0, 0, 1], port).into();
            if let Err(e) = node.forward_tcp_chain(chain, listen).await {
                log::error!("{name} 转发退出：{e:#}");
            }
        });
    }
    Ok(node)
}

/// FSEvents 监听。只监听「内容真源」路径，绝不监听整棵树——
/// 仓库自身在 ~/.agents/sync 内，监听整树会让同步写入触发自身、无限循环。
fn spawn_watcher(cfg: &DaemonConfig, tx: mpsc::Sender<Trigger>) -> Result<()> {
    let targets = watch_paths(&cfg.paths);
    let cooldown = Duration::from_secs(cfg.conf.watch_cooldown.max(1));
    let last = Arc::new(Mutex::new(Instant::now() - cooldown));

    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        let Ok(event) = res else { return };
        if event.kind.is_access() {
            return;
        }
        // 抖动合并：编辑器保存往往连发多个事件
        let mut last_fire = match last.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        if last_fire.elapsed() < cooldown {
            return;
        }
        *last_fire = Instant::now();
        let _ = tx.try_send(Trigger::Watcher);
    })
    .context("创建 FSEvents 监听器")?;

    for path in &targets {
        // 单个路径不存在不该让整个 daemon 起不来
        if let Err(e) = watcher.watch(path, RecursiveMode::Recursive) {
            log::warn!("监听 {} 失败：{e}", path.display());
        }
    }
    // watcher 必须活到进程结束；泄漏一个即可（进程级资源）
    std::mem::forget(watcher);
    Ok(())
}

/// 等待每个 peer 的本地转发端口可连接。
///
/// 为什么必须等：同步是「对端在 ssh://127.0.0.1:<port> 上取我的仓」，
/// 而那个端口由 iroh 打洞 + 转发建立，需要数秒。端口未就绪就开始同步，
/// 只会得到一堆 connection refused，且看起来像「内容没同步」。
async fn wait_for_forwards(cfg: &DaemonConfig, timeout: Duration) -> Vec<String> {
    let peers: Vec<(String, u16)> = cfg
        .registry
        .peers(&cfg.device_id)
        .filter_map(|p| p.port().ok().map(|port| (p.id.clone(), port)))
        .collect();
    if peers.is_empty() {
        return Vec::new();
    }
    let deadline = tokio::time::Instant::now() + timeout;
    let mut ready = Vec::new();
    let mut pending: Vec<(String, u16)> = peers;
    while !pending.is_empty() && tokio::time::Instant::now() < deadline {
        let mut still = Vec::new();
        for (id, port) in pending {
            let target: SocketAddr = ([127, 0, 0, 1], port).into();
            match tokio::time::timeout(
                Duration::from_millis(300),
                tokio::net::TcpStream::connect(target),
            )
            .await
            {
                Ok(Ok(_)) => {
                    log::info!("隧道就绪：{id} (127.0.0.1:{port})");
                    ready.push(id);
                }
                _ => still.push((id, port)),
            }
        }
        pending = still;
        if !pending.is_empty() {
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    }
    for (id, port) in &pending {
        log::warn!("隧道未就绪：{id} (127.0.0.1:{port})，本轮将跳过该 peer");
    }
    ready
}

fn watch_paths(paths: &SyncPaths) -> Vec<PathBuf> {
    let home = &paths.home;
    [
        ".agents/AGENTS.md",
        ".agents/prompts",
        ".agents/skills",
        ".agents/documents",
        ".agents/references/README.md",
        ".agents/.skill-lock.json",
        ".zcode/AGENTS.md",
        ".zcode/agents",
        ".zcode/skills",
    ]
    .iter()
    .map(|rel| home.join(rel))
    .filter(|p| p.exists())
    .collect()
}

struct IpcCtx {
    paths: SyncPaths,
    device_id: String,
    paused: Arc<AtomicBool>,
}

/// IPC：一行一个 JSON 请求，一行一个 JSON 回包。
/// 用 unix socket 而不是 TCP：绑定在文件系统权限上，天然只允许本用户。
async fn serve_ipc(
    path: PathBuf,
    state: Arc<Mutex<DaemonState>>,
    tx: mpsc::Sender<Trigger>,
    ctx: IpcCtx,
) -> Result<()> {
    if path.exists() {
        let _ = std::fs::remove_file(&path);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let listener = UnixListener::bind(&path).with_context(|| format!("绑定 {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    log::info!("IPC 就绪：{}", path.display());

    loop {
        let (stream, _) = listener.accept().await?;
        let state = state.clone();
        let tx = tx.clone();
        let ctx = IpcCtx {
            paths: ctx.paths.clone(),
            device_id: ctx.device_id.clone(),
            paused: ctx.paused.clone(),
        };
        tokio::spawn(async move {
            let (read, mut write) = stream.into_split();
            let mut lines = BufReader::new(read).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let resp = match serde_json::from_str::<Request>(&line) {
                    Ok(req) => handle(req, &state, &tx, &ctx).await,
                    Err(e) => Response {
                        ok: false,
                        error: Some(format!("请求无法解析：{e}")),
                        state: None,
                    },
                };
                let mut out = serde_json::to_vec(&resp).unwrap_or_default();
                out.push(b'\n');
                if write.write_all(&out).await.is_err() {
                    break;
                }
            }
        });
    }
}

async fn handle(
    req: Request,
    state: &Arc<Mutex<DaemonState>>,
    tx: &mpsc::Sender<Trigger>,
    ctx: &IpcCtx,
) -> Response {
    match req {
        Request::Status => Response {
            ok: true,
            error: None,
            state: Some(serde_json::to_value(snapshot(state, ctx)).unwrap_or_default()),
        },
        Request::Sync => {
            let _ = tx.send(Trigger::Manual).await;
            Response {
                ok: true,
                error: None,
                state: Some(serde_json::json!({"queued": true})),
            }
        }
        Request::Publish | Request::Pull => {
            // 目前二者都并入一次完整同步：先落账再拉取，顺序不可交换。
            let _ = tx.send(Trigger::Manual).await;
            Response {
                ok: true,
                error: None,
                state: Some(serde_json::json!({"queued": true})),
            }
        }
        Request::Pause => {
            ctx.paused.store(true, Ordering::Relaxed);
            if let Ok(mut s) = state.lock() {
                s.paused = true;
            }
            Response {
                ok: true,
                error: None,
                state: None,
            }
        }
        Request::Resume => {
            ctx.paused.store(false, Ordering::Relaxed);
            if let Ok(mut s) = state.lock() {
                s.paused = false;
            }
            Response {
                ok: true,
                error: None,
                state: None,
            }
        }
        Request::Conflicts => {
            let engine = SyncEngine::with_device_id(ctx.paths.clone(), ctx.device_id.clone());
            let conflicts = engine
                .ok()
                .and_then(|e| e.jj.conflicts(&ctx.paths.root).ok())
                .unwrap_or_default();
            Response {
                ok: true,
                error: None,
                state: Some(serde_json::json!({"conflicts": conflicts})),
            }
        }
        Request::Net => Response {
            ok: true,
            error: None,
            state: Some(serde_json::json!({
                "watching": watch_paths(&ctx.paths),
            })),
        },
        Request::Quit => {
            std::process::exit(0);
        }
    }
}

fn snapshot(state: &Arc<Mutex<DaemonState>>, ctx: &IpcCtx) -> serde_json::Value {
    let s = state.lock().ok();
    serde_json::json!({
        "device": ctx.device_id,
        "pid": std::process::id(),
        "paused": s.as_ref().map(|s| s.paused).unwrap_or(false),
        "syncing": s.as_ref().map(|s| s.syncing).unwrap_or(false),
        "started_at": s.as_ref().and_then(|s| s.started_at.clone()),
        "watch_count": s.as_ref().map(|s| s.watching.len()).unwrap_or(0),
        "last": s.as_ref().and_then(|s| s.last.clone()),
        "version": s.as_ref().map(|s| s.version.clone()).unwrap_or_default(),
        "update_available": s.as_ref().and_then(|s| s.update_available.clone()),
        "ipc": ctx.paths.logs_dir().join("daemon.sock").display().to_string(),
    })
}

/// 定期检查 npm 上的新版本；`[update] auto = true` 时直接安装。
///
/// 安装会 `launchctl kickstart -k` 本单元——本进程随即被结束，新二进制接管。
/// 因此这里不需要「更新后再恢复自身状态」：进程生命周期本来就是可丢弃的。
fn spawn_updater(cfg: &DaemonConfig, state: Arc<Mutex<DaemonState>>, paused: Arc<AtomicBool>) {
    let paths = cfg.paths.clone();
    tokio::spawn(async move {
        // 启动后等 90 秒：别和首轮同步抢网络与 CPU。
        tokio::time::sleep(Duration::from_secs(90)).await;
        loop {
            let settings = agents_sync_core::settings::Settings::load(&paths.root).unwrap_or_default();
            let upd = settings.update.clone();

            let probe = {
                let proxy = upd.proxy.clone();
                let mirror = upd.mirror.clone();
                tokio::task::spawn_blocking(move || {
                    agents_sync_core::update::check(&proxy, &mirror)
                })
                .await
            };
            match probe {
                Ok(Ok(rel)) => {
                    let newer =
                        agents_sync_core::update::is_newer(&rel.version, agents_sync_core::update::CURRENT);
                    if let Ok(mut s) = state.lock() {
                        s.update_available = newer.then(|| rel.version.clone());
                    }
                    if newer {
                        log::info!(
                            "发现新版本 {}（当前 {}，auto={}）",
                            rel.version,
                            agents_sync_core::update::CURRENT,
                            upd.auto
                        );
                        if upd.auto && !paused.load(Ordering::Relaxed) {
                            let p = paths.clone();
                            let proxy = upd.proxy.clone();
                            let mirror = upd.mirror.clone();
                            match tokio::task::spawn_blocking(move || {
                                agents_sync_core::update::apply(&p, &proxy, &mirror, false)
                            })
                            .await
                            {
                                Ok(Ok(r)) => log::info!("自动更新完成：{} → {}", r.from, r.to),
                                Ok(Err(e)) => log::warn!("自动更新失败：{e:#}"),
                                Err(e) => log::warn!("自动更新任务失败：{e}"),
                            }
                        }
                    }
                }
                Ok(Err(e)) => log::debug!("更新检查失败：{e:#}"),
                Err(e) => log::debug!("更新检查任务失败：{e}"),
            }

            tokio::time::sleep(Duration::from_secs(upd.check_interval.max(600))).await;
        }
    });
}

fn now() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = secs / 86_400;
    let rem = secs % 86_400;
    let z = days as i64 + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// daemon 的默认 IPC 路径（CLI 与 UI 用同一个）。
pub fn default_socket(paths: &SyncPaths) -> PathBuf {
    paths.logs_dir().join("daemon.sock")
}

/// 供 CLI 使用：连上 daemon，发一条请求，读一条回包。
pub async fn call(socket: &Path, req: &Request) -> Result<Response> {
    let stream = tokio::net::UnixStream::connect(socket)
        .await
        .with_context(|| format!("连接 daemon 失败（{}），它可能没在运行", socket.display()))?;
    let (read, mut write) = stream.into_split();
    let mut out = serde_json::to_vec(req)?;
    out.push(b'\n');
    write.write_all(&out).await?;
    let mut lines = BufReader::new(read).lines();
    let line = lines
        .next_line()
        .await?
        .context("daemon 没有回包（可能刚退出）")?;
    Ok(serde_json::from_str(&line)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agents_sync_core::config::Device;

    #[test]
    fn watch_list_only_content_sources() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join(".agents/prompts")).unwrap();
        std::fs::create_dir_all(home.path().join(".agents/sync/payload")).unwrap();
        std::fs::write(home.path().join(".agents/AGENTS.md"), "x").unwrap();

        let paths = SyncPaths::new(home.path());
        let w = watch_paths(&paths);
        assert!(w.iter().any(|p| p.ends_with(".agents/AGENTS.md")));
        assert!(w.iter().any(|p| p.ends_with(".agents/prompts")));
        assert!(
            !w.iter().any(|p| p.to_string_lossy().contains("/sync")),
            "绝不能监听仓库自身：会自激"
        );
    }

    #[tokio::test]
    async fn ipc_roundtrip_and_pause() {
        let d = tempfile::tempdir().unwrap();
        let paths = SyncPaths::new(d.path());
        paths.ensure_runtime_dirs().unwrap();
        std::fs::create_dir_all(paths.root.join(".jj")).unwrap();

        let sock = d.path().join("t.sock");
        let state = Arc::new(Mutex::new(DaemonState::default()));
        let (tx, mut rx) = mpsc::channel(8);
        let ctx = IpcCtx {
            paths: paths.clone(),
            device_id: "dev-x".into(),
            paused: Arc::new(AtomicBool::new(false)),
        };
        let s2 = sock.clone();
        tokio::spawn(async move {
            let _ = serve_ipc(s2, state, tx, ctx).await;
        });
        // 等 socket 出现
        for _ in 0..50 {
            if sock.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        let resp = call(&sock, &Request::Status).await.unwrap();
        assert!(resp.ok);
        let st = resp.state.unwrap();
        assert_eq!(st["device"], "dev-x");

        let resp = call(&sock, &Request::Sync).await.unwrap();
        assert!(resp.ok);
        // 手动触发应当进入通道
        assert!(rx.recv().await.is_some());

        let resp = call(&sock, &Request::Pause).await.unwrap();
        assert!(resp.ok);
        let resp = call(&sock, &Request::Status).await.unwrap();
        assert_eq!(resp.state.unwrap()["paused"], true);
    }

    #[test]
    fn outcome_is_serializable_for_ui() {
        let o = SyncOutcome {
            at: "2026-11-24T00:00:00Z".into(),
            ok: true,
            ..Default::default()
        };
        let j = serde_json::to_value(&o).unwrap();
        assert_eq!(j["ok"], true);
        let _ = Device {
            id: "x".into(),
            label: String::new(),
            ticket: String::new(),
            local_port: None,
            ssh_user: String::new(),
            lan: String::new(),
            repo_path: String::new(),
            net_port: None,
            relay_url: String::new(),
        };
    }
}
