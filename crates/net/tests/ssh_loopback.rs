//! 意图（2026-11-25）：把「代理能不能扛 SSH」从跨机变量里分离出来。
//!   同进程两个 endpoint，一个把本机 sshd 暴露出去，另一个映射到本地端口，
//!   然后**用真正的 ssh 客户端**穿过去。
//!
//! 为什么必须单独测：echo 测试能过，但 SSH 有「服务端先发 banner」的语义，
//! 若代理的某个方向没跑起来，echo 察觉不到（客户端先说话），SSH 会立刻卡住。
//!
//! 跳过条件：本机 127.0.0.1:22 不可达（CI 环境通常没有 sshd）。

use agents_sync_net::{new_secret_hex, secret_from_hex, Node};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;
use tokio::net::TcpListener;

fn sshd_reachable() -> bool {
    TcpStream::connect_timeout(
        &"127.0.0.1:22".parse().unwrap(),
        Duration::from_millis(300),
    )
    .is_ok()
}

async fn free_port() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    l.local_addr().unwrap().port()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ssh_traverses_the_iroh_proxy() {
    if !sshd_reachable() {
        eprintln!("跳过：本机 127.0.0.1:22 不可达（无 sshd）");
        return;
    }

    // 服务端：把本机 sshd 暴露出去
    let server = Node::bind(secret_from_hex(&new_secret_hex()).unwrap())
        .await
        .unwrap();
    let server_addr = server.addr();
    {
        let s = server.clone();
        tokio::spawn(async move {
            let _ = s
                .serve("127.0.0.1:22".parse::<SocketAddr>().unwrap(), |_m, _p| {})
                .await;
        });
    }

    // 客户端：映射到本地端口
    let client = Node::bind(secret_from_hex(&new_secret_hex()).unwrap())
        .await
        .unwrap();
    let port = free_port().await;
    let listen: SocketAddr = ([127, 0, 0, 1], port).into();
    {
        let c = client.clone();
        let target = server_addr.clone();
        tokio::spawn(async move {
            let _ = c.forward_tcp_addr(target, listen).await;
        });
    }
    for _ in 0..60 {
        if TcpStream::connect(listen).is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // 用真 ssh 穿过去。key 与 sshd 用本机已有的（与生产路径一致）。
    let home = std::env::var("HOME").unwrap_or_default();
    let home2 = home.clone();
    let port_str = port.to_string();
    let out = tokio::task::spawn_blocking(move || {
        std::process::Command::new("/usr/bin/ssh")
        .args([
            "-o", "BatchMode=yes",
            "-o", "ConnectTimeout=10",
            "-o", "StrictHostKeyChecking=no",
            "-o", "UserKnownHostsFile=/dev/null",
            "-o", "LogLevel=ERROR",
            "-p", &port_str,
            "127.0.0.1",
            "echo SSH_OVER_IROH_OK",
        ])
        .env("HOME", &home2)
        .output()
    })
    .await
    .expect("ssh 任务")
    .expect("启动 ssh");

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stdout.contains("SSH_OVER_IROH_OK"),
        "SSH 未能穿过代理。stdout={stdout:?} stderr={stderr:?}"
    );

    client.close().await;
    server.close().await;
}
