//! 意图（2026-11-24 Owner）：gaubee-cloud 只做组网，不存状态；同步靠「主动广播我变了」
//!   让对端立刻来拉。故本层只解决两件事：**让对端 sshd 可达**、**叫醒对端**。
//!
//! 本文件职责（4 个正交意图）：
//!   1. 固定身份（secret → EndpointId），ticket 可离线推导、跨重启稳定
//!   2. 暴露本机 TCP（sshd）给对端：一个入站流 = 一条 TCP 连接
//!   3. 把对端 TCP 映射到本机回环端口（git 仍走 ssh，不重写传输协议）
//!   4. 广播 RPC（短消息，不落盘、不排队：丢了靠下一轮 align 补齐）
//! 不承载：状态与合并（core 的职责）；本层不读 payload、不碰 jj。

use anyhow::{Context, Result};
use iroh::endpoint::{Connection, Endpoint, RecvStream, SendStream};
pub use iroh::{EndpointAddr, EndpointId};
use iroh::SecretKey;
use std::net::SocketAddr;
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};

/// 单 ALPN 双用途：首字节区分「TCP 代理」与「RPC」。
/// 不为两类流量各开一个 endpoint —— 少一份 NAT 打洞与握手，也少一个要保活的进程。
pub const ALPN: &[u8] = b"agents-sync/1";

const OP_TCP: u8 = 0;
const OP_RPC: u8 = 1;

/// 广播消息。刻意做成可丢弃的「提示」而不是命令：
/// 收到的一方自己决定拉不拉，丢了也不影响最终一致。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Message {
    /// 「我有新提交，来拉」
    Changed { from: String, tree_hash: String },
    /// 对端探活（net 命令用）
    Ping { from: String },
}

/// 一个 iroh 节点。clone 廉价（内部 Arc）。
#[derive(Clone)]
pub struct Node {
    endpoint: Endpoint,
}

impl Node {
    /// 用固定 secret 建立身份。secret 存 `.secrets/<id>.iroh`（64 字符 hex），
    /// 这样 EndpointId 与 ticket 跨重启稳定，devices.json 里的记录不会失效。
    pub async fn bind(secret: SecretKey) -> Result<Self> {
        Self::bind_on(secret, None).await
    }

    /// 固定 UDP 端口绑定。
    ///
    /// 为什么需要：直连（局域网）必须能写出「IP:端口」。用随机端口的话，
    /// 对端只知道 IP 仍然连不上，只能退回 n0 发现——而它在 Fake-IP/代理网络里会失败。
    pub async fn bind_on(secret: SecretKey, port: Option<u16>) -> Result<Self> {
        let mut builder = Endpoint::builder(iroh::endpoint::presets::N0)
            .secret_key(secret)
            .alpns(vec![ALPN.to_vec()]);
        if let Some(port) = port {
            let addr: SocketAddr = ([0u8, 0, 0, 0], port).into();
            builder = builder
                .bind_addr(addr)
                .context("iroh bind_addr 失败")?;
        }
        let endpoint = builder.bind().await.context("iroh bind 失败")?;
        Ok(Self { endpoint })
    }

    /// 等 home relay 连上（最长 `timeout`）。relay 地址只有在 online 后才有。
    pub async fn wait_online(&self, timeout: Duration) -> bool {
        tokio::time::timeout(timeout, self.endpoint.online())
            .await
            .is_ok()
    }

    /// 本机当前使用的 relay 地址（若有）。
    ///
    /// 用途：把对端的 relay 写进配置后，连接可以**纯出站**经 relay 完成，
    /// 无需对端入站可达——实测 macOS 的本地网络限制会让入站 UDP 静默失败，
    /// 而 relay 不受影响。
    pub fn relay_url(&self) -> Option<String> {
        let addrs = self.endpoint.addr();
        for a in addrs.addrs.iter() {
            let text = format!("{a}");
            if text.starts_with("https://") || text.starts_with("http://") {
                return Some(text);
            }
            let dbg = format!("{a:?}");
            if let Some(rest) = dbg.find("https://") {
                return Some(dbg[rest..].trim_end_matches(['"', ')', ',', ' ']).to_string());
            }
        }
        None
    }

    /// 本机 iroh 实际监听的 UDP 端口（用于写进 devices.json 的 net_port）。
    pub fn udp_port(&self) -> Option<u16> {
        self.endpoint
            .bound_sockets()
            .iter()
            .find(|a| a.is_ipv4())
            .map(|a| a.port())
    }

    /// 由「局域网 IP + iroh 端口」构造直连地址；缺任一项则退回仅凭 id（走 n0）。
    pub fn direct_addr(id: EndpointId, lan: &str, port: Option<u16>) -> EndpointAddr {
        Self::endpoint_addr(id, lan, port, "")
    }

    /// 三级连接目标：直连+relay 优先，其次只走 relay，最后只凭 id 走发现。
    ///
    /// 为什么要三级而不是两级：relay 是**纯出站**路径，在入站被拦的网络里
    /// 是唯一可靠的方式；而「只凭 id」依赖 n0 DNS 发现，在本网络实测不可用。
    /// 之前的回退把 relay 一起丢了，等于退到了最不可靠的一级。
    pub fn target(id: EndpointId, lan: &str, port: Option<u16>, relay: &str) -> Vec<EndpointAddr> {
        let mut chain = Vec::new();

        // 第一级：**纯直连**，不带 relay。
        //
        // 为什么不把 relay 一起塞进第一个地址：实测一次失败会在毫秒级返回
        // `invalid peer certificate: UnknownIssuer`——本机代理 MITM 了到 relay 的
        // TLS，于是 iroh 选了 relay 路径并在 TLS 校验处立刻失败，直连根本没被试。
        // 分开两级后，直连真正有机会先跑完。
        if let Ok(sa) = format!("{}:{}", lan.trim(), port.unwrap_or(0)).parse::<SocketAddr>() {
            if !lan.trim().is_empty() && port.is_some() {
                chain.push(EndpointAddr::new(id).with_ip_addr(sa));
            }
        }

        // 第二级：只走 relay（纯出站；直连被拦时的兜底）
        if !relay.trim().is_empty() {
            if let Ok(url) = relay.trim().parse() {
                chain.push(EndpointAddr::new(id).with_relay_url(url));
            }
        }

        // 第三级：只凭 id（n0 发现）
        if chain.iter().all(|a| !a.addrs.is_empty()) {
            chain.push(EndpointAddr::new(id));
        }
        if chain.is_empty() {
            chain.push(EndpointAddr::new(id));
        }
        chain
    }

    /// 组合连接地址：局域网 IP（可选）+ relay（可选）。
    ///
    /// 两者都给时 iroh 会优先直连、失败走 relay；只给 relay 时纯出站即可连通，
    /// 这是受限网络（NAT、macOS 本地网络限制）下唯一可靠的方式。
    pub fn endpoint_addr(
        id: EndpointId,
        lan: &str,
        port: Option<u16>,
        relay: &str,
    ) -> EndpointAddr {
        let mut addr = EndpointAddr::new(id);
        if let ("", _) | (_, None) = (lan.trim(), port) {
            // 没有直连信息，跳过
        } else if let Ok(sa) = format!("{}:{}", lan.trim(), port.unwrap()).parse::<SocketAddr>() {
            addr = addr.with_ip_addr(sa);
        }
        if !relay.trim().is_empty() {
            if let Ok(url) = relay.trim().parse() {
                addr = addr.with_relay_url(url);
            }
        }
        addr
    }

    pub fn id(&self) -> EndpointId {
        self.endpoint.id()
    }

    /// ticket：`endpoint<z32>`，可离线推导，与 dumbpipe 短票同形。
    pub fn ticket(&self) -> String {
        format!("endpoint{}", self.endpoint.id().to_z32())
    }

    pub async fn close(self) {
        self.endpoint.close().await;
    }

    /// 唯一 accept 循环：首字节分流到 TCP 代理或 RPC。
    ///
    /// 关键：**只能有一个** accept 循环。两个循环抢同一个 endpoint 会互相吞连接
    /// （各自 accept 到对方的流量然后丢弃），这是最难查的一类「随机断连」。
    pub async fn serve<F>(&self, target: SocketAddr, on_message: F) -> Result<()>
    where
        F: Fn(Message, EndpointId) + Send + Sync + 'static,
    {
        let on_message = std::sync::Arc::new(on_message);
        log::info!("iroh serve 就绪（TCP 目标 {target}）");
        while let Some(incoming) = self.endpoint.accept().await {
            log::info!("收到入站 iroh 连接");
            let accepting = match incoming.accept() {
                Ok(a) => a,
                Err(e) => {
                    log::warn!("拒绝入站连接：{e}");
                    continue;
                }
            };
            let handler = on_message.clone();
            tokio::spawn(async move {
                let conn = match accepting.await {
                    Ok(c) => c,
                Err(e) => {
                    let mut msg = format!("拒绝入站连接：{e}");
                    // identity 漂移是最常见原因：对端 `.secrets` 重建过，本机还在用旧 id。
                    // 不给提示时，这个错只表现为「隧道永远超时」，排查成本很高（实测）。
                    if e.to_string().contains("authentication failed") {
                        msg.push_str(
                            "（对端记录的 ticket 已过期；在任一台运行 agents-sync enroll <user@host> 修复）",
                        );
                    }
                    log::warn!("{msg}");
                    return;
                }
                };
                if let Err(e) = dispatch(conn, target, handler).await {
                    log::debug!("入站连接结束：{e}");
                }
            });
        }
        Ok(())
    }

    /// 把对端的 TCP 服务映射到本机回环地址。
    /// 每个本地连接开一条 iroh bidi 流；ssh 与 git 完全不知道底下换了传输。
    pub async fn forward_tcp(&self, peer: EndpointId, listen: SocketAddr) -> Result<()> {
        self.forward_tcp_chain(vec![EndpointAddr::new(peer)], listen).await
    }

    pub async fn forward_tcp_addr(&self, addr: EndpointAddr, listen: SocketAddr) -> Result<()> {
        self.forward_tcp_chain(vec![addr], listen).await
    }

    /// 带直连地址的转发。跨网靠 n0 发现即可；同机/已知地址时用它，
    /// 省掉 DNS 发布与解析的几秒等待（测试与「局域网直连」都走这条）。
    pub async fn forward_tcp_chain(
        &self,
        chain: Vec<EndpointAddr>,
        listen: SocketAddr,
    ) -> Result<()> {
        let addr = chain.first().cloned().unwrap_or_else(|| EndpointAddr::new(self.id()));
        let listener = TcpListener::bind(listen)
            .await
            .with_context(|| format!("绑定 {listen}"))?;
        log::info!("转发就绪：{listen} → {}", addr.id.to_z32());
        loop {
            let (sock, _) = listener.accept().await?;
            let node = self.clone();
            let chain = chain.clone();
            tokio::spawn(async move {
                if let Err(e) = forward_one_chain(&node, chain, sock).await {
                    log::debug!("转发连接结束：{e}");
                }
            });
        }
    }

    /// 本节点的完整地址（含已绑定端口与 relay），供同机/局域网直连使用。
    pub fn addr(&self) -> EndpointAddr {
        self.endpoint.addr()
    }

    /// 广播：一条短消息，超时即放弃（对端离线不需要补发）。
    pub async fn send_message(&self, peer: EndpointId, msg: &Message) -> Result<()> {
        self.send_message_chain(vec![EndpointAddr::new(peer)], msg).await
    }

    /// 单地址版本（测试与已知直连场景用）。
    pub async fn send_message_addr(&self, addr: EndpointAddr, msg: &Message) -> Result<()> {
        self.send_message_chain(vec![addr], msg).await
    }

    /// 通用连接：先按给定地址（可能是局域网直连）尝试，失败再退回「仅凭 id」的
    /// 发现路径（n0 DNS + relay）。
    ///
    /// 为什么必须有这层回退：直连地址一旦打洞失败（防火墙、跨网、对端换了 IP），
    /// 只带 IP 的连接会直接失败，而 relay 明明可用。实测跨机时一个方向就是
    /// 这样断的。
    async fn connect_preferring(&self, chain: Vec<EndpointAddr>) -> Result<Connection> {
        let mut last: Option<anyhow::Error> = None;
        for (i, addr) in chain.iter().enumerate() {
            match self.connect_addr(addr.clone()).await {
                Ok(c) => {
                    if i > 0 {
                        log::info!("第 {} 级地址连通（relay/发现回退）", i + 1);
                    }
                    return Ok(c);
                }
                Err(e) => {
                    log::debug!("第 {} 级地址失败：{e}", i + 1);
                    last = Some(e);
                }
            }
        }
        Err(last.unwrap_or_else(|| anyhow::anyhow!("无可用地址")))
    }

    /// 带直连地址的广播。
    ///
    /// 为什么必须提供这条路径：`presets::N0` 的发现依赖 pkarr/DNS，在 Fake-IP、
    /// 企业代理或离线环境下会直接失败（实测报 `No addressing information available`）。
    /// 只要 devices.json 里记了局域网地址，就应优先用它，n0 仅作兜底。
    pub async fn send_message_chain(&self, chain: Vec<EndpointAddr>, msg: &Message) -> Result<()> {
        let conn = self.connect_preferring(chain).await?;
        let (mut send, mut recv) = conn.open_bi().await?;
        send.write_all(&[OP_RPC]).await?;
        let body = serde_json::to_vec(msg)?;
        send.write_all(&(body.len() as u32).to_be_bytes()).await?;
        send.write_all(&body).await?;
        send.finish()?;
        // 等对端一个字节：确认它真的处理了，而不是连接被中间设备吞掉
        let mut ack = [0u8; 1];
        let _ = tokio::time::timeout(Duration::from_secs(5), recv.read_exact(&mut ack)).await;
        conn.close(0u32.into(), b"done");
        Ok(())
    }

    pub(crate) async fn connect_addr(&self, addr: EndpointAddr) -> Result<Connection> {
        let conn =
            tokio::time::timeout(Duration::from_secs(10), self.endpoint.connect(addr, ALPN))
                .await
                .context("连接超时（对端离线或 NAT 未打通）")??;
        Ok(conn)
    }
}

/// 入站分流：首字节决定这是 TCP 代理还是 RPC。
async fn dispatch<F>(conn: Connection, target: SocketAddr, handler: std::sync::Arc<F>) -> Result<()>
where
    F: Fn(Message, EndpointId) + Send + Sync + 'static,
{
    let peer = conn.remote_id();
    let (mut send, mut recv) = conn.accept_bi().await?;
    let mut op = [0u8; 1];
    recv.read_exact(&mut op).await?;
    log::info!("入站流：op={} 来自 {}", op[0], peer.to_z32());
    match op[0] {
        OP_TCP => {
            let tcp = match TcpStream::connect(target).await {
                Ok(t) => t,
                Err(e) => {
                    log::warn!("连接本机 {target} 失败：{e}");
                    return Ok(());
                }
            };
            log::info!("已连上本机 {target}，开始双向搬运");
            pipe(tcp, send, recv).await;
        }
        OP_RPC => {
            if let Some(body) = read_frame(&mut recv).await? {
                match serde_json::from_slice::<Message>(&body) {
                    Ok(msg) => handler(msg, peer),
                    Err(e) => log::warn!("广播内容无法解析：{e}"),
                }
            }
            let _ = send.write_all(b"\x01").await;
            let _ = send.finish();
        }
        other => log::warn!("未知操作码 {other}"),
    }
    Ok(())
}

async fn forward_one_chain(node: &Node, chain: Vec<EndpointAddr>, sock: TcpStream) -> Result<()> {
    let conn = node.connect_preferring(chain).await?;
    let (mut send, recv) = conn.open_bi().await?;
    send.write_all(&[OP_TCP]).await?;
    pipe(sock, send, recv).await;
    Ok(())
}

/// 双向搬运：TCP ↔ iroh 流。
/// 手写 select 而不是 `copy_bidirectional`：iroh 的流类型不保证实现 tokio 的
/// AsyncRead/AsyncWrite，硬套会多一层适配与隐藏缓冲。
async fn pipe(tcp: TcpStream, mut send: SendStream, mut recv: RecvStream) {
    let t0 = std::time::Instant::now();
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (mut r, mut w) = tcp.into_split();
    let up = async {
        let mut buf = vec![0u8; 16 * 1024];
        let mut total = 0usize;
        loop {
            match r.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    total += n;
                    if send.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        let _ = send.finish();
        total
    };
    let down = async {
        let mut buf = vec![0u8; 16 * 1024];
        let mut total = 0usize;
        loop {
            match recv.read(&mut buf).await {
                Ok(Some(n)) if n > 0 => {
                    total += n;
                    if w.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                }
                _ => break,
            }
        }
        total
    };
    // 关键：必须等两个方向各自读到 EOF，不能「先完成者取消另一边」。
    //
    // 用 select! 会在一方结束时立刻拆掉连接，而 SSH 的密钥交换阶段正是
    // 「客户端发完 banner 后等待、不关闭」——于是 sshd 看到连接被重置，
    // 表现为 `kex_exchange_identification: read: Connection reset by peer`。
    // （实测：file:// 远端不经过此路径，所以双 HOME 测试没暴露它。）
    let (up_bytes, down_bytes) = tokio::join!(up, down);
    // 字节计数是这类「连上了但没数据」问题的唯一可靠线索
    log::info!(
        "tcp↔iroh 结束：上行 {up_bytes}B，下行 {down_bytes}B，耗时 {}ms",
        t0.elapsed().as_millis()
    );
}

/// 读 4 字节大端长度 + 正文。
async fn read_frame(recv: &mut RecvStream) -> Result<Option<Vec<u8>>> {
    let mut len = [0u8; 4];
    if recv.read_exact(&mut len).await.is_err() {
        return Ok(None);
    }
    let n = u32::from_be_bytes(len) as usize;
    if n == 0 || n > 64 * 1024 {
        anyhow::bail!("广播帧长度非法：{n}");
    }
    let mut body = vec![0u8; n];
    recv.read_exact(&mut body)
        .await
        .context("读取广播正文失败")?;
    Ok(Some(body))
}

/// 从 64 字符 hex 还原 SecretKey（key 文件由 `openssl rand -hex 32` 生成）。
pub fn secret_from_hex(hex: &str) -> Result<SecretKey> {
    let hex = hex.trim();
    if hex.len() != 64 {
        anyhow::bail!("secret 必须是 64 字符 hex，实际 {} 字符", hex.len());
    }
    let mut bytes = [0u8; 32];
    for (i, byte) in bytes.iter_mut().enumerate() {
        let s = &hex[i * 2..i * 2 + 2];
        *byte = u8::from_str_radix(s, 16)
            .with_context(|| format!("secret 第 {} 字节不是合法 hex：{s}", i))?;
    }
    Ok(SecretKey::from_bytes(&bytes))
}

/// 生成新的 hex secret（首次安装时用）。
pub fn new_secret_hex() -> String {
    SecretKey::generate()
        .to_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// 解析 `endpoint<z32>` 或裸 z32 形式的 ticket。
pub fn parse_ticket(ticket: &str) -> Result<EndpointId> {
    let t = ticket.trim().trim_start_matches("endpoint");
    EndpointId::from_z32(t).with_context(|| format!("ticket 无法解析：{ticket}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_roundtrip_and_ticket_parse() {
        let hex = new_secret_hex();
        assert_eq!(hex.len(), 64);
        let key = secret_from_hex(&hex).unwrap();
        let id = key.public();
        assert_eq!(parse_ticket(&format!("endpoint{}", id.to_z32())).unwrap(), id);
        assert_eq!(parse_ticket(&id.to_z32()).unwrap(), id);
    }

    #[test]
    fn bad_secret_rejected() {
        assert!(secret_from_hex("zz").is_err());
        assert!(secret_from_hex(&"g".repeat(64)).is_err());
    }

    #[tokio::test]
    async fn endpoints_bind_with_stable_ids() {
        let a = Node::bind(secret_from_hex(&new_secret_hex()).unwrap())
            .await
            .unwrap();
        let ticket = a.ticket();
        assert!(ticket.starts_with("endpoint"));
        // 同一 secret 重新 bind，id 必须一致（devices.json 才不会失效）
        let hex = new_secret_hex();
        let first = Node::bind(secret_from_hex(&hex).unwrap()).await.unwrap();
        let id1 = first.id();
        first.close().await;
        let second = Node::bind(secret_from_hex(&hex).unwrap()).await.unwrap();
        assert_eq!(id1, second.id());
        second.close().await;
        a.close().await;
    }
}

#[cfg(test)]
mod relay_url_tests {
    #[test]
    fn relay_url_display_roundtrips() {
        for s in [
            "https://aps1-1.relay.n0.iroh.link./",
            "https://aps1-1.relay.n0.iroh.link/",
            "https://usw1-1.relay.n0.iroh.link./",
        ] {
            let parsed = s.parse::<iroh::RelayUrl>();
            assert!(parsed.is_ok(), "解析失败：{s} → {:?}", parsed.err());
        }
    }
}
