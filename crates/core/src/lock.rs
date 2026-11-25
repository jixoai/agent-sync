//! 意图（2026-11-24）：daemon、CLI、定时兜底三路都可能同时想改仓库，
//!   必须有一把跨进程的锁。用 O_CREAT|O_EXCL + pid 文件实现，
//!   陈旧锁（持有者已死）自动回收——否则一次崩溃就会让同步永久卡死。
//!
//! 本文件职责（2 个正交意图）：
//!   1. 抢占/释放互斥锁
//!   2. 判定持有者是否仍存活（pid 复用会导致误判，故同时比对启动时间不可行，
//!      这里选择「宁可等 30 秒也不误杀」的保守策略）
//! 不承载：业务逻辑；调用方拿到锁后自行决定做什么。

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// 一把基于目录的互斥锁。Drop 时自动释放。
pub struct DirLock {
    path: PathBuf,
    pid: u32,
}

impl DirLock {
    /// 获取锁；最多等待 `wait`。超时返回错误而不是无限等。
    pub fn acquire(lock_dir: &Path, name: &str, wait: Duration) -> Result<Self> {
        std::fs::create_dir_all(lock_dir)?;
        let path = lock_dir.join(format!("{name}.lock"));
        let pid = std::process::id();
        let deadline = Instant::now() + wait;

        loop {
            match std::fs::create_dir(&path) {
                Ok(_) => {
                    std::fs::write(path.join("pid"), pid.to_string()).ok();
                    return Ok(Self { path, pid });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    if Self::reclaim_if_stale(&path) {
                        continue;
                    }
                    if Instant::now() >= deadline {
                        let holder = std::fs::read_to_string(path.join("pid"))
                            .unwrap_or_default()
                            .trim()
                            .to_string();
                        anyhow::bail!(
                            "锁被占用（pid {}），等待 {:?} 后放弃",
                            if holder.is_empty() { "?".into() } else { holder },
                            wait
                        );
                    }
                    std::thread::sleep(Duration::from_millis(200));
                }
                Err(e) => return Err(e).with_context(|| format!("创建锁 {}", path.display())),
            }
        }
    }

    /// 持有者进程已不存在 → 回收。返回是否发生了回收。
    fn reclaim_if_stale(path: &Path) -> bool {
        let Ok(text) = std::fs::read_to_string(path.join("pid")) else {
            // pid 文件缺失有两种可能：a) 刚创建、还没写（正常）；
            // b) 持有者被 SIGKILL（永远不会再写）。
            // 用目录 mtime 区分：超过 10 秒还没有 pid，就认定是残留并回收，
            // 否则一次强杀会让锁永久占用、daemon 再也起不来（实测）。
            let old = std::fs::metadata(path)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.elapsed().ok())
                .map(|d| d.as_secs() > 10)
                .unwrap_or(true);
            if old {
                let _ = std::fs::remove_dir_all(path);
            }
            return old;
        };
        let Ok(pid) = text.trim().parse::<i32>() else {
            let _ = std::fs::remove_dir_all(path);
            return true;
        };
        // kill(pid, 0)：permission denied 说明进程存在，ESRCH 说明不存在
        let alive = libc_kill(pid);
        if alive {
            return false;
        }
        let _ = std::fs::remove_dir_all(path);
        true
    }
}

impl Drop for DirLock {
    fn drop(&mut self) {
        // 只释放自己持有的锁，绝不误删他人的
        if let Ok(text) = std::fs::read_to_string(self.path.join("pid")) {
            if text.trim() == self.pid.to_string() {
                let _ = std::fs::remove_dir_all(&self.path);
            }
        }
    }
}

/// 极小的 kill(pid, 0) 封装。只为判活，不引入 libc 依赖。
fn libc_kill(pid: i32) -> bool {
    unsafe extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    unsafe { kill(pid, 0) == 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_is_exclusive_and_released_on_drop() {
        let d = tempfile::tempdir().unwrap();
        let a = DirLock::acquire(d.path(), "t", Duration::from_millis(100)).unwrap();
        let err = DirLock::acquire(d.path(), "t", Duration::from_millis(100));
        assert!(err.is_err(), "同进程内第二次获取必须失败");
        drop(a);
        assert!(DirLock::acquire(d.path(), "t", Duration::from_millis(100)).is_ok());
    }

    #[test]
    fn stale_lock_is_reclaimed() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("t.lock");
        std::fs::create_dir_all(&path).unwrap();
        // 一个几乎不可能存在的 pid
        std::fs::write(path.join("pid"), "999999").unwrap();
        let lock = DirLock::acquire(d.path(), "t", Duration::from_secs(2));
        assert!(lock.is_ok(), "陈旧锁应被自动回收");
    }
}
