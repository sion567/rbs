//! 项目级运行互斥（GHA concurrency「cancel-in-progress」语义）。
//!
//! 目标：同项目同一时刻只有一次运行。GUI 天然单运行槽，但 CLI 是独立进程，
//! GUI + CLI、CLI + CLI 可以同项目并行互踩（target/ 文件锁、重复上传）。
//!
//! 跨进程协调用锁文件（`<app_root>/runs/<项目>.lock`，YAML）：
//! - 新运行 acquire 时直接**接管**锁（覆盖 run_id）；
//! - 旧运行的 watcher 每 500ms 检查锁归属，发现被接管 → 向自己的取消信道发信号
//!   → 引擎现有的取消机制（ProcTreeGuard 整树终止）自然接管，返回 Cancelled；
//! - 引擎步骤间再查一次锁归属作兜底（watcher 失效时）；
//! - 旧运行的守卫 Drop 时只删除 run_id 属于自己的锁文件，不会误删新运行的。
//!
//! 不终止旧运行进程本身（那可能连 GUI 一起杀）——旧进程自杀，新进程接管。

use anyhow::Context as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use tokio::sync::broadcast;

/// 锁文件内容
#[derive(Debug, Clone, Serialize, Deserialize)]
struct LockContent {
    run_id: String,
    pid: u32,
    job: String,
    started_at: String,
}

pub fn lock_path(dir: &Path, project: &str) -> PathBuf {
    // 原始名短哈希后缀：文件名净化会折叠非法字符（dev:h5 → dev_h5），两个
    // 不同的合法工程名可能折到同一锁文件互杀——哈希后缀让它们各归各位
    let digest = Sha256::digest(project.as_bytes());
    let hash: String = digest[..4].iter().map(|b| format!("{b:02x}")).collect();
    dir.join(format!("{}-{hash}.lock", crate::config::sanitize_filename(project)))
}

fn read_lock(path: &Path) -> Option<LockContent> {
    let text = std::fs::read_to_string(path).ok()?;
    yaml_serde::from_str(&text).ok()
}

fn write_lock(path: &Path, run_id: &str, job: &str) -> anyhow::Result<()> {
    let content = LockContent {
        run_id: run_id.to_string(),
        pid: std::process::id(),
        job: job.to_string(),
        started_at: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
    };
    // tmp + rename 原子落盘：并发 reader（接管方/旧 watcher）不会读到半截
    // YAML 而误判"锁丢了"触发自取消
    let tmp = path.with_extension(format!("lock.{}.tmp", std::process::id()));
    std::fs::write(&tmp, yaml_serde::to_string(&content)?)
        .with_context(|| format!("写入运行锁失败: {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("写入运行锁失败: {}", path.display()))?;
    Ok(())
}

/// 目标 PID 是否存活（陈旧锁检测，仅用于提示文案）
fn pid_alive(pid: u32) -> bool {
    #[cfg(windows)]
    {
        use winapi::um::handleapi::CloseHandle;
        use winapi::um::processthreadsapi::OpenProcess;
        use winapi::um::winnt::PROCESS_QUERY_LIMITED_INFORMATION;
        if pid == std::process::id() {
            return true;
        }
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if handle.is_null() {
            return false;
        }
        let mut exit_code: u32 = 0;
        unsafe {
            // STILL_ACTIVE = 259
            let ok = winapi::um::processthreadsapi::GetExitCodeProcess(handle, &mut exit_code);
            CloseHandle(handle);
            ok != 0 && exit_code == 259
        }
    }
    #[cfg(not(windows))]
    {
        let _ = pid;
        true // 非 Windows 保守视为存活（本项目主链路面向 Windows）
    }
}

/// 运行锁守卫：Drop 时若锁文件仍属于自己则删除（不误删接管者的）
pub struct RunLockGuard {
    path: PathBuf,
    run_id: String,
}

impl Drop for RunLockGuard {
    fn drop(&mut self) {
        if read_lock(&self.path).map(|c| c.run_id == self.run_id).unwrap_or(false) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// 获取项目运行锁（GHA cancel-in-progress：直接接管，旧运行自行取消）。
/// 返回 (守卫, 警告)；警告非空 = 接管时旧运行进程还活着（应为旧版本二进制 /
/// 无响应运行，其 watcher 会尽快自杀；若持续无响应需人工干预）。
pub fn acquire(
    dir: &Path,
    project: &str,
    run_id: &str,
    job_name: &str,
) -> anyhow::Result<(RunLockGuard, Option<String>)> {
    std::fs::create_dir_all(dir)
        .with_context(|| format!("创建运行锁目录失败: {}", dir.display()))?;
    let path = lock_path(dir, project);

    let warning = match read_lock(&path) {
        Some(old) if old.run_id != run_id && pid_alive(old.pid) => Some(format!(
            "同项目已有运行中（PID {}，run {}），已接管锁；旧运行将自动取消",
            old.pid, old.run_id
        )),
        _ => None,
    };

    write_lock(&path, run_id, job_name)?;
    // 并发启动冲突兜底：两个进程同时 acquire 会互相写锁——写完复核归属，
    // 抢不到的显式失败（否则双方 watcher 都见到外来 run_id，一起自杀）
    if !is_mine(&path, run_id) {
        anyhow::bail!("同项目并发启动冲突：运行锁被其他运行抢先（{project}），请稍后重试");
    }
    Ok((RunLockGuard { path: path.clone(), run_id: run_id.to_string() }, warning))
}

/// 锁是否仍属于本次运行（引擎步骤间兜底检查；文件丢失 = 被接管/清理）
pub fn is_mine(path: &Path, run_id: &str) -> bool {
    read_lock(path).map(|c| c.run_id == run_id).unwrap_or(false)
}

/// 锁归属监视任务：每 500ms 检查锁文件，发现被接管/丢失 → 发取消信号。
/// 由引擎在运行结束时 abort。
pub fn spawn_watcher(
    cancel_tx: broadcast::Sender<()>,
    path: PathBuf,
    run_id: String,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            if !is_mine(&path, &run_id) {
                let _ = cancel_tx.send(());
                break;
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rbs_runlock_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn acquire_release_and_steal() {
        let dir = tmp_dir("basic");

        // 首次获取
        let (g1, w1) = acquire(&dir, "proj", "run-1", "build").unwrap();
        assert!(w1.is_none(), "空目录获取不应有警告");
        assert!(lock_path(&dir, "proj").exists());

        // 另一运行接管（同进程，PID 存活但 run_id 不同 → 有警告）
        let (g2, w2) = acquire(&dir, "proj", "run-2", "build").unwrap();
        assert!(w2.is_some(), "接管活进程的锁应有警告: {w2:?}");

        // 旧守卫释放不误删新锁
        drop(g1);
        assert!(lock_path(&dir, "proj").exists(), "旧守卫不应删除接管者的锁");

        // 引擎兜底检查
        assert!(!is_mine(&lock_path(&dir, "proj"), "run-1"));
        assert!(is_mine(&lock_path(&dir, "proj"), "run-2"));

        // 新守卫释放删锁
        drop(g2);
        assert!(!lock_path(&dir, "proj").exists());

        // 陈旧锁（PID 不可能存活的大数）：接管无警告
        let stale = lock_path(&dir, "proj");
        std::fs::write(
            &stale,
            yaml_serde::to_string(&LockContent {
                run_id: "old".into(),
                pid: u32::MAX - 1,
                job: "build".into(),
                started_at: "2020-01-01 00:00:00".into(),
            })
            .unwrap(),
        )
        .unwrap();
        let (_g3, w3) = acquire(&dir, "proj", "run-3", "build").unwrap();
        assert!(w3.is_none(), "陈旧锁接管不应警告: {w3:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn projects_are_isolated() {
        let dir = tmp_dir("isolated");
        let (_ga, _) = acquire(&dir, "alpha", "run-a", "build").unwrap();
        let (gb, wb) = acquire(&dir, "beta", "run-b", "build").unwrap();
        assert!(wb.is_none(), "不同项目互不干扰");
        assert!(lock_path(&dir, "alpha").exists());
        drop(gb);
        assert!(lock_path(&dir, "alpha").exists(), "beta 释放不应影响 alpha");
    }

    /// 锁文件名带原始名短哈希：净化折叠非法字符后，两个不同工程名不再共享
    /// 同一锁文件互杀（dev:h5 与 dev_h5 此前都折成 dev_h5.lock）
    #[test]
    fn lock_names_differ_for_sanitizing_collisions() {
        let dir = tmp_dir("hash");
        assert_ne!(
            lock_path(&dir, "dev:h5"),
            lock_path(&dir, "dev_h5"),
            "净化折叠名的两个工程锁文件必须不同"
        );
    }

    /// 写锁原子化：锁文件是合法 YAML（tmp+rename 保证不会读到半截）
    #[test]
    fn lock_file_is_valid_yaml_after_write() {
        let dir = tmp_dir("atomic");
        let (_g, _) = acquire(&dir, "proj", "run-1", "build").unwrap();
        let content = std::fs::read_to_string(lock_path(&dir, "proj")).unwrap();
        let parsed: Option<LockContent> = yaml_serde::from_str(&content).ok();
        assert!(parsed.is_some(), "锁文件应可完整解析: {content}");
        assert_eq!(parsed.unwrap().run_id, "run-1");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
