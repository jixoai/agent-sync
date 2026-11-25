//! 意图（2026-11-24）：用真实 jj + 真实 git 跑一遍「两台机器」的完整闭环。
//!   这是切换前的主验收：bash 版曾用它验证过 32 条断言，Rust 版必须在同一场景下
//!   给出同样的结论，否则不允许替换。
//!
//! 覆盖：采集 → 推送 → 新设备克隆共享历史 → 拉取落盘 → 双向同步 → 树哈希收敛 →
//!       冲突拒绝落盘。不覆盖：网络传输（需真实设备，另由真机验收）。

use agents_sync_core::config::{Device, DeviceRegistry, SyncConf};
use agents_sync_core::mirror::apply::{self, ApplyOptions};
use agents_sync_core::paths::SyncPaths;
use agents_sync_core::sync::{Broadcaster, NoopBroadcaster, SyncEngine};
use agents_sync_core::checks;
use std::path::{Path, PathBuf};
use std::process::Command;

/// 造一台设备的活目录：AGENTS/prompts/skills/运行态各就位。
fn seed_home(root: &Path) -> SyncPaths {
    let p = SyncPaths::new(root);
    std::fs::create_dir_all(p.live_agents().join("prompts/refs")).unwrap();
    std::fs::create_dir_all(p.live_agents().join("skills/private-a")).unwrap();
    std::fs::create_dir_all(p.live_zcode().join("agents")).unwrap();
    std::fs::create_dir_all(p.live_zcode().join("skills/real-skill")).unwrap();
    std::fs::create_dir_all(p.live_zcode().join("cli")).unwrap();
    std::fs::create_dir_all(p.live_zcode().join("v2")).unwrap();
    std::fs::create_dir_all(p.live_codex().join("rules")).unwrap();

    std::fs::write(p.live_agents().join("AGENTS.md"), "# 共享基线\n").unwrap();
    std::fs::write(p.live_agents().join("prompts/refs/x.md"), "prompt\n").unwrap();
    std::fs::write(p.live_agents().join("skills/private-a/SKILL.md"), "私有\n").unwrap();
    std::fs::write(p.live_zcode().join("AGENTS.md"), "# zcode\n").unwrap();
    std::fs::write(p.live_zcode().join("agents/slow.md"), "slow\n").unwrap();
    std::fs::write(p.live_zcode().join("skills/real-skill/SKILL.md"), "real\n").unwrap();
    // 运行态与凭证：apply 绝不可破坏
    std::fs::write(p.live_zcode().join("cli/cache.bin"), "cache\n").unwrap();
    std::fs::write(
        p.live_zcode().join("v2/setting.json"),
        "{\"passwordCredentialKey\":\"local\"}\n",
    )
    .unwrap();
    std::fs::write(p.live_codex().join("AGENTS.md"), "# codex\n").unwrap();
    std::fs::write(p.live_codex().join("rules/default.rules"), "rule\n").unwrap();
    p
}

/// 建立 jj 身份（仓库级，避免并行测试互相污染环境变量）。
fn jj_config(dir: &Path) {
    for (k, v) in [("user.name", "test"), ("user.email", "test@local")] {
        let out = Command::new("/opt/homebrew/bin/jj")
            .args(["config", "set", "--repo", k, v])
            .current_dir(dir)
            .output()
            .expect("执行 jj config");
        assert!(out.status.success(), "jj config {k} 失败");
    }
}

/// 把 remote 指向本地工作仓（测试里不跑隧道）。
/// 用 set-url 优先：引擎的 ensure_remotes 可能已经建过同名 remote，
/// 此时 `git remote add` 会静默失败，留下那条连不通的 ssh URL。
fn set_remote(dir: &Path, name: &str, url: &str) {
    let ok = Command::new("/usr/bin/git")
        .args(["remote", "set-url", name, url])
        .current_dir(dir)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !ok {
        Command::new("/usr/bin/git")
            .args(["remote", "add", name, url])
            .current_dir(dir)
            .output()
            .expect("git remote add");
    }
}

fn engine(home: &Path, id: &str) -> SyncEngine {
    let paths = SyncPaths::new(home);
    let mut e = SyncEngine::with_device_id(paths, id).expect("构造引擎");
    e.conf = SyncConf::default();
    e
}

fn write_registry(home: &Path, devices: Vec<Device>) {
    let reg = DeviceRegistry {
        version: 1,
        devices,
    };
    reg.save(&SyncPaths::new(home).devices_json()).unwrap();
}

fn dev(id: &str, port: u16) -> Device {
    Device {
        id: id.into(),
        label: id.into(),
        ticket: format!("endpoint-{id}"),
        local_port: Some(port),
        ssh_user: "test".into(),
        lan: String::new(),
        repo_path: String::new(),
        net_port: None,
        relay_url: String::new(),
    }
}

#[test]
fn two_nodes_converge_bidirectionally() {
    let a_home = tempfile::tempdir().unwrap();
    let b_home = tempfile::tempdir().unwrap();
    seed_home(a_home.path());
    seed_home(b_home.path());

    // ---- A 初始化并发布一台设备的改动 ----
    let a = engine(a_home.path(), "device-a");
    write_registry(a_home.path(), vec![dev("device-a", 22301), dev("device-b", 22302)]);
    // 预置 file remote，避免 ensure_remotes 用 ssh URL 覆盖（测试不跑隧道）
    let a = {
        let mut e = a;
        e.registry = DeviceRegistry::load(&e.paths.devices_json()).unwrap();
        e
    };
    a.init().expect("A init");

    std::fs::write(
        a.paths.live_agents().join("AGENTS.md"),
        "# 来自 device-a 的改动\n",
    )
    .unwrap();
    let pub_report = a.publish(&NoopBroadcaster).expect("A publish");
    assert!(pub_report.captured > 0 || pub_report.committed);

    // ---- B 以既有历史为起点（真实 enroll 路径：克隆而不是各自 init）----
    let b_root = SyncPaths::new(b_home.path()).root.clone();
    std::fs::remove_dir_all(&b_root).ok();
    let clone = Command::new("/usr/bin/git")
        .args([
            "clone",
            "-q",
            a.paths.root.to_str().unwrap(),
            b_root.to_str().unwrap(),
        ])
        .output()
        .expect("git clone");
    assert!(clone.status.success(), "克隆失败: {:?}", clone);

    // 克隆不带 .jj（gitignore）与 devices.json（本机视角配置），需各自建立
    let out = Command::new("/opt/homebrew/bin/jj")
        .args(["git", "init", "--colocate"])
        .current_dir(&b_root)
        .output()
        .expect("jj init");
    assert!(out.status.success(), "jj init 失败: {:?}", out);
    jj_config(&b_root);

    let b = engine(b_home.path(), "device-b");
    write_registry(
        b_home.path(),
        vec![dev("device-a", 22301), dev("device-b", 22302)],
    );
    // B 指向 A 的工作仓（file 路径，不依赖隧道）
    set_remote(&b_root, "device-a", a.paths.root.to_str().unwrap());
    let _ = Command::new("/usr/bin/git")
        .args(["config", "receive.denyCurrentBranch", "ignore"])
        .current_dir(&a.paths.root)
        .output();

    let mut b = b;
    b.registry = DeviceRegistry::load(&b.paths.devices_json()).unwrap();
    let pull = b
        .pull(&ApplyOptions { prune: false, backup_keep: 5, not_after: None })
        .expect("B pull");

    assert!(pull.aligned);
    assert!(pull.conflicts.is_empty());
    assert!(pull.applied.is_some());

    // B 的活目录收到 A 的内容
    let b_agents = std::fs::read_to_string(b.paths.live_agents().join("AGENTS.md")).unwrap();
    assert!(b_agents.contains("来自 device-a 的改动"), "B 应收到 A 的内容：{b_agents}");
    assert!(b.paths.live_zcode().join("agents/slow.md").exists());
    // 运行态与凭证不得被破坏
    assert!(b.paths.live_zcode().join("cli/cache.bin").exists(), "运行态必须保留");
    assert_eq!(
        std::fs::read_to_string(b.paths.live_zcode().join("v2/setting.json")).unwrap(),
        "{\"passwordCredentialKey\":\"local\"}\n",
        "凭证文件不得被覆盖"
    );

    // ---- 反向：B 改一处 → 推 → A 收到 ----
    std::fs::write(
        b.paths.live_agents().join("AGENTS.md"),
        "# 来自 device-b 的改动\n",
    )
    .unwrap();
    let b_pub = b.publish(&NoopBroadcaster).expect("B publish");
    assert!(b_pub.committed, "B 必须产生提交");

    // A 需要登记 B 才能 fetch 它
    set_remote(&a.paths.root, "device-b", b.paths.root.to_str().unwrap());
    let _ = Command::new("/usr/bin/git")
        .args(["config", "receive.denyCurrentBranch", "ignore"])
        .current_dir(&b.paths.root)
        .output();

    let mut a2 = engine(a_home.path(), "device-a");
    a2.registry = DeviceRegistry::load(&a2.paths.devices_json()).unwrap();
    let pull_a = a2
        .pull(&ApplyOptions { prune: false, backup_keep: 5, not_after: None })
        .expect("A pull");
    assert!(
        pull_a.fetched.contains(&"device-b".to_string()),
        "A 应成功 fetch device-b，实际：{:?}（失败 {:?}）",
        pull_a.fetched,
        pull_a.failed_fetch
    );
    assert!(pull_a.conflicts.is_empty(), "不应有冲突：{:?}", pull_a.conflicts);
    let a_agents = std::fs::read_to_string(a2.paths.live_agents().join("AGENTS.md")).unwrap();
    assert!(a_agents.contains("来自 device-b 的改动"), "A 应收到 B 的改动：{a_agents}");

    // ---- 收敛判据：两节点 payload 树哈希一致 ----
    let ha = checks::tree_hash(&a2.paths.payload()).unwrap();
    let hb = checks::tree_hash(&b.paths.payload()).unwrap();
    assert_eq!(ha, hb, "两节点 payload 必须字节一致");
}

#[test]
fn conflict_blocks_apply_and_leaves_live_tree_clean() {
    let a_home = tempfile::tempdir().unwrap();
    let b_home = tempfile::tempdir().unwrap();
    seed_home(a_home.path());
    seed_home(b_home.path());

    let a = engine(a_home.path(), "device-a");
    write_registry(a_home.path(), vec![dev("device-a", 22301), dev("device-b", 22302)]);
    let mut a = a;
    a.registry = DeviceRegistry::load(&a.paths.devices_json()).unwrap();
    a.init().expect("A init");
    a.publish(&NoopBroadcaster).expect("A publish");

    // B 克隆共享历史
    let b_root = SyncPaths::new(b_home.path()).root.clone();
    std::fs::remove_dir_all(&b_root).ok();
    let out = Command::new("/usr/bin/git")
        .args(["clone", "-q", a.paths.root.to_str().unwrap(), b_root.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(out.status.success());
    Command::new("/opt/homebrew/bin/jj")
        .args(["git", "init", "--colocate"])
        .current_dir(&b_root)
        .output()
        .unwrap();
    jj_config(&b_root);
    set_remote(&b_root, "device-a", a.paths.root.to_str().unwrap());
    let _ = Command::new("/usr/bin/git")
        .args(["config", "receive.denyCurrentBranch", "ignore"])
        .current_dir(&a.paths.root)
        .output();

    let mut b = engine(b_home.path(), "device-b");
    write_registry(
        b_home.path(),
        vec![dev("device-a", 22301), dev("device-b", 22302)],
    );
    b.registry = DeviceRegistry::load(&b.paths.devices_json()).unwrap();
    b.pull(&ApplyOptions { prune: false, backup_keep: 5, not_after: None })
        .expect("初次 pull");

    // 两边改同一文件的同一行 → 真 3-way 冲突
    std::fs::write(a.paths.live_agents().join("prompts/refs/x.md"), "conflict-from-a\n").unwrap();
    a.publish(&NoopBroadcaster).expect("A publish 冲突前");
    std::fs::write(b.paths.live_agents().join("prompts/refs/x.md"), "conflict-from-b\n").unwrap();
    b.publish(&NoopBroadcaster).expect("B publish 冲突前");

    let err = b
        .pull(&ApplyOptions { prune: false, backup_keep: 5, not_after: None })
        .expect_err("有冲突必须拒绝落盘");
    let msg = err.to_string();
    assert!(msg.contains("冲突"), "错误应指明冲突：{msg}");

    // 活目录必须保持干净：没有冲突标记
    let live = std::fs::read_to_string(b.paths.live_agents().join("prompts/refs/x.md")).unwrap();
    assert!(
        !live.contains("<<<<<<<") && !live.contains(">>>>>>>"),
        "冲突标记不得写进活目录：{live}"
    );
    assert_eq!(live, "conflict-from-b\n", "活目录应保持 B 自己的版本");
}

/// 一个记录调用的广播器，验证 publish 确实尝试唤醒每台 peer。
struct RecordingBroadcaster {
    woken: std::sync::Mutex<Vec<String>>,
}

impl Broadcaster for RecordingBroadcaster {
    fn wake(&self, device: &Device) -> anyhow::Result<()> {
        self.woken.lock().unwrap().push(device.id.clone());
        Ok(())
    }
}

#[test]
fn publish_attempts_to_wake_every_peer() {
    let home = tempfile::tempdir().unwrap();
    seed_home(home.path());
    write_registry(
        home.path(),
        vec![dev("device-a", 22301), dev("device-b", 22302), dev("device-c", 22303)],
    );

    let mut e = engine(home.path(), "device-a");
    e.registry = DeviceRegistry::load(&e.paths.devices_json()).unwrap();
    e.init().unwrap();

    let rec = RecordingBroadcaster {
        woken: std::sync::Mutex::new(Vec::new()),
    };
    std::fs::write(e.paths.live_agents().join("AGENTS.md"), "# 变更\n").unwrap();
    e.publish(&rec).unwrap();

    let mut woken = rec.woken.lock().unwrap().clone();
    woken.sort();
    assert_eq!(woken, vec!["device-b".to_string(), "device-c".to_string()]);
}

#[test]
fn apply_options_default_is_conservative() {
    let d = ApplyOptions::default();
    assert!(!d.prune, "默认不删活文件");
}

#[test]
fn paths_are_under_home() {
    let h = PathBuf::from("/tmp/x");
    let p = SyncPaths::new(&h);
    assert!(p.root.starts_with(&h));
    assert_eq!(apply::rollback(&p, "nope").is_err(), true);
}
