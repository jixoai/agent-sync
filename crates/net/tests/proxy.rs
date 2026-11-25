//! 意图（2026-11-24）：验证「把对端 TCP 映射到本机回环」这条唯一还没被自动化覆盖的路径。
//!   单测只能证明能 bind；这里走真 iroh 栈：两个 endpoint、一个 echo 服务、
//!   一条经过 iroh 的 TCP 连接，来回应答必须完整。
//!
//! 覆盖：OP_TCP 分流、双向搬运、大包（>单个 UDP datagram）、并发连接。
//! 不覆盖：跨 NAT 打洞（需真机/异网，由真机验收）。

use agents_sync_net::{new_secret_hex, secret_from_hex, Node};
use std::net::SocketAddr;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// 起一个 echo 服务，返回其地址。
async fn echo_server() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8 * 1024];
                loop {
                    match sock.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if sock.write_all(&buf[..n]).await.is_err() {
                                break;
                            }
                        }
                    }
                }
            });
        }
    });
    addr
}

async fn free_port() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    l.local_addr().unwrap().port()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tcp_traffic_traverses_iroh() {
    let echo = echo_server().await;

    // 服务端：暴露 echo
    let server = Node::bind(secret_from_hex(&new_secret_hex()).unwrap())
        .await
        .unwrap();
    let server_addr = server.addr();
    {
        let s = server.clone();
        tokio::spawn(async move {
            let _ = s.serve(echo, |_msg, _peer| {}).await;
        });
    }

    // 客户端：把对端映射到本机回环
    let client = Node::bind(secret_from_hex(&new_secret_hex()).unwrap())
        .await
        .unwrap();
    let listen_port = free_port().await;
    let listen: SocketAddr = ([127, 0, 0, 1], listen_port).into();
    {
        let c = client.clone();
        let target = server_addr.clone();
        tokio::spawn(async move {
            let _ = c.forward_tcp_addr(target, listen).await;
        });
    }

    // 等转发端口就绪（最多 5 秒）
    for _ in 0..100 {
        if TcpStream::connect(listen).await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // 小包往返
    let mut sock = TcpStream::connect(listen).await.expect("连接本地转发端口");
    sock.write_all(b"hello").await.unwrap();
    let mut buf = [0u8; 5];
    tokio::time::timeout(Duration::from_secs(10), sock.read_exact(&mut buf))
        .await
        .expect("等待回显超时")
        .expect("读取回显失败");
    assert_eq!(&buf, b"hello", "经 iroh 的 TCP 必须原样往返");

    // 大包：跨多个 UDP datagram，验证流式语义没有被消息边界截断
    let big: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
    sock.write_all(&big).await.unwrap();
    let mut got = vec![0u8; big.len()];
    tokio::time::timeout(Duration::from_secs(20), sock.read_exact(&mut got))
        .await
        .expect("等待大包回显超时")
        .expect("读取大包失败");
    assert_eq!(got, big, "大包必须完整往返");

    // 并发连接：两条独立 TCP 各自往返，不串流
    let mut a = TcpStream::connect(listen).await.unwrap();
    let mut b = TcpStream::connect(listen).await.unwrap();
    a.write_all(b"AAA").await.unwrap();
    b.write_all(b"BBB").await.unwrap();
    let mut ra = [0u8; 3];
    let mut rb = [0u8; 3];
    a.read_exact(&mut ra).await.unwrap();
    b.read_exact(&mut rb).await.unwrap();
    assert_eq!(&ra, b"AAA", "并发连接不得串流");
    assert_eq!(&rb, b"BBB", "并发连接不得串流");

    client.close().await;
    server.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn broadcast_reaches_handler() {
    let server = Node::bind(secret_from_hex(&new_secret_hex()).unwrap())
        .await
        .unwrap();
    let server_addr = server.addr();
    let client = Node::bind(secret_from_hex(&new_secret_hex()).unwrap())
        .await
        .unwrap();

    let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(4);
    let tx2 = tx.clone();
    {
        let s = server.clone();
        tokio::spawn(async move {
            let _ = s
                .serve(
                    "127.0.0.1:9".parse().unwrap(),
                    move |msg, _peer| {
                        if let agents_sync_net::Message::Changed { from, .. } = msg {
                            let _ = tx2.try_send(from);
                        }
                    },
                )
                .await;
        });
    }

    // 给 serve 一点时间进入 accept 循环
    tokio::time::sleep(Duration::from_millis(200)).await;

    client
        .send_message_addr(
            server_addr.clone(),
            &agents_sync_net::Message::Changed {
                from: "peer-a".into(),
                tree_hash: "abc".into(),
            },
        )
        .await
        .expect("广播应成功");

    let got = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("等待广播超时")
        .expect("广播通道关闭");
    assert_eq!(got, "peer-a");

    client.close().await;
    server.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn direct_addr_by_explicit_ip_works() {
    // 复刻生产路径：两端都用固定端口绑定，连接方只用 EndpointAddr::new(id).with_ip_addr(...)。
    // 若这个测试失败，说明 with_ip_addr 的用法有问题，而不是网络问题。
    let port_a: u16 = 22521;
    let port_b: u16 = 22522;

    let server = Node::bind_on(secret_from_hex(&new_secret_hex()).unwrap(), Some(port_a))
        .await
        .unwrap();
    let server_id = server.id();
    {
        let s = server.clone();
        tokio::spawn(async move {
            let _ = s.serve("127.0.0.1:9".parse().unwrap(), |_m, _p| {}).await;
        });
    }
    let client = Node::bind_on(secret_from_hex(&new_secret_hex()).unwrap(), Some(port_b))
        .await
        .unwrap();

    let addr = Node::direct_addr(
        server_id,
        "127.0.0.1",
        Some(port_a),
    );
    let msg = agents_sync_net::Message::Ping { from: "t".into() };
    let res = tokio::time::timeout(std::time::Duration::from_secs(8), client.send_message_addr(addr, &msg));
    match res.await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => panic!("显式 IP 连接失败（API 用法问题）：{e:#}"),
        Err(_) => panic!("显式 IP 连接超时（8s）：with_ip_addr 可能没被采用"),
    }

    client.close().await;
    server.close().await;
}
