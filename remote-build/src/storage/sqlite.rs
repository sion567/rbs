//! 运行历史（SQLite，设计文档 §11.2，P1）。
//!
//! 三层分离原则：配置（YAML）永远不进库；SQLite 只存运行历史；日志原文落文件，
//! 库里只存路径。`config_snapshot` 记录当时跑的 job 配置（YAML 原文）——
//! 三个月后排查"当时跑的是 JDK17 还是 21"靠它。
//!
//! 开关：config.yaml `save_history`（默认开）；保留期复用 `log_retention_days`
//!（按 runs.started_at 清理，step_runs 经外键级联）。
//! 入库点：GUI / CLI 的事件转发任务——所有 LogEvent 都过那里，天然是录制点。
//!
//! 存储：rusqlite（同步 API）+ bundled SQLite（源码编进 exe，用户机器零依赖）。
//! 对外保持 async 签名（调用侧是 tokio 任务），内部 spawn_blocking 落库——
//! 单条写入微秒级，不打扰事件循环；连接不共享（每次 open 独立 + WAL +
//! busy_timeout），录制的 Connection 走 Arc<Mutex> 供阻塞线程安全取用。

use crate::models::{JobStatus, LogEvent};
use chrono::Utc;
use rusqlite::Connection;
use std::sync::{Arc, Mutex};

/// history.db 路径（~/.remote-build/data/history.db）
pub fn db_path() -> std::path::PathBuf {
    crate::config::app_root().join("data").join("history.db")
}

/// 打开（不存在则创建）连接 + WAL/外键/忙等 + 建表。同步函数，
/// 调用方负责放进 spawn_blocking；连接用完显式 close（WAL 句柄不滞留）。
fn open() -> anyhow::Result<Connection> {
    let path = db_path();
    std::fs::create_dir_all(path.parent().expect("data dir"))?;
    let conn = Connection::open(&path)?;
    // busy_timeout：GUI 查询与运行录制偶发并发，等锁而不是报 locked
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON;")?;
    migrate(&conn)?;
    Ok(conn)
}

/// 轻量迁移：PRAGMA user_version 记录 schema 版本，逐版本向前滚。
/// v0（空库）→ v1：初始两张表。此后加字段/加表都在这里追加 `if ver < N` 分支，
/// 不要改旧分支——老库按版本逐级升级，新库一次到位。
fn migrate(conn: &Connection) -> anyhow::Result<()> {
    let ver: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap_or(0);
    if ver < 1 {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS runs (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                run_id TEXT NOT NULL UNIQUE,
                project_name TEXT NOT NULL,
                job_name TEXT NOT NULL,
                target TEXT,
                trigger_type TEXT NOT NULL DEFAULT 'manual',
                status TEXT NOT NULL DEFAULT 'running',
                started_at TEXT NOT NULL,
                finished_at TEXT,
                duration_ms INTEGER,
                log_path TEXT,
                config_snapshot TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS step_runs (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                run_id INTEGER NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
                step_index INTEGER NOT NULL,
                step_name TEXT,
                status TEXT NOT NULL DEFAULT 'running',
                exit_code INTEGER,
                duration_ms INTEGER,
                started_at TEXT NOT NULL,
                finished_at TEXT,
                UNIQUE(run_id, step_index)
            );
            PRAGMA user_version = 1;",
        )?;
    }
    Ok(())
}

/// JobStatus → 库内状态键（短、稳定，别用带中文的 short()）
pub fn status_key(s: &JobStatus) -> &'static str {
    match s {
        JobStatus::Running => "running",
        JobStatus::Success => "success",
        JobStatus::SucceededWithFailures { .. } => "success-warn",
        JobStatus::Failed { .. } => "failed",
        JobStatus::Cancelled => "cancelled",
        JobStatus::Timeout => "timeout",
    }
}

fn now() -> String {
    Utc::now().to_rfc3339()
}

/// 一次运行的元信息（入库时的快照）
pub struct RunMeta {
    pub run_id: String,
    pub project: String,
    pub job: String,
    pub target: Option<String>,
    /// manual / watch / cli
    pub trigger: &'static str,
    pub log_path: Option<String>,
}

/// on_event 提取出的落库动作（owned：可跨线程进 spawn_blocking）
enum Write {
    StepStart { step_index: i64, name: String, status: &'static str },
    StepEnd { step_index: i64, exit_code: i64, duration_ms: i64 },
    JobEnd { status: &'static str, duration_ms: i64 },
}

/// 运行录制器：插 runs 行（running）→ 事件流更新 step_runs / runs → JobEnd 收尾。
/// 每个方法独立容错（库满/锁失败不阻塞运行本身，只 eprintln）。
pub struct RunRecorder {
    conn: Arc<Mutex<Connection>>,
    row_id: i64,
    /// 主流程步骤数：只录主流程的 step 事件（handlers / preflight 的 index 超界跳过）
    steps_len: usize,
    started: std::time::Instant,
}

impl RunRecorder {
    /// 插入 running 行；失败返回 None（调用方无录制器照常运行）
    pub async fn start(meta: &RunMeta, snapshot_yaml: &str, steps_len: usize) -> Option<Self> {
        let run_id = meta.run_id.clone();
        let project = meta.project.clone();
        let job = meta.job.clone();
        let target = meta.target.clone();
        let trigger = meta.trigger;
        let log_path = meta.log_path.clone();
        let snapshot = snapshot_yaml.to_string();
        let opened = tokio::task::spawn_blocking(move || {
            let conn = open()?;
            let id: i64 = conn.query_row(
                "INSERT INTO runs
                    (run_id, project_name, job_name, target, trigger_type, status, started_at,
                     log_path, config_snapshot)
                 VALUES (?1, ?2, ?3, ?4, ?5, 'running', ?6, ?7, ?8)
                 RETURNING id",
                rusqlite::params![run_id, project, job, target, trigger, now(), log_path, snapshot],
                |r| r.get(0),
            )?;
            Ok::<(Connection, i64), anyhow::Error>((conn, id))
        })
        .await;
        match opened {
            Ok(Ok((conn, id))) => {
                Some(Self { conn: Arc::new(Mutex::new(conn)), row_id: id, steps_len, started: std::time::Instant::now() })
            }
            Ok(Err(e)) => {
                eprintln!("[history] 打开库/记录运行失败（历史不记录）: {e:#}");
                None
            }
            Err(e) => {
                eprintln!("[history] 录制任务失败（历史不记录）: {e}");
                None
            }
        }
    }

    pub async fn on_event(&self, ev: &LogEvent) {
        // steps_len 过滤在提取时做：handlers / preflight 的 index 超界不入库
        let write = match ev {
            LogEvent::StepStart { step_index, name, skipped, .. }
                if *step_index < self.steps_len =>
            {
                Write::StepStart {
                    step_index: *step_index as i64,
                    name: name.clone(),
                    status: if *skipped { "skipped" } else { "running" },
                }
            }
            LogEvent::StepEnd { step_index, exit_code, duration_ms, .. }
                if *step_index < self.steps_len =>
            {
                Write::StepEnd {
                    step_index: *step_index as i64,
                    exit_code: *exit_code as i64,
                    duration_ms: *duration_ms as i64,
                }
            }
            LogEvent::JobEnd { status, .. } => Write::JobEnd {
                status: status_key(status),
                duration_ms: self.started.elapsed().as_millis() as i64,
            },
            _ => return,
        };
        let conn = Arc::clone(&self.conn);
        let row_id = self.row_id;
        let res = tokio::task::spawn_blocking(move || {
            let conn = conn.lock().map_err(|_| anyhow::anyhow!("连接锁中毒"))?;
            match write {
                Write::StepStart { step_index, name, status } => {
                    conn.execute(
                        "INSERT INTO step_runs
                            (run_id, step_index, step_name, status, started_at)
                         VALUES (?1, ?2, ?3, ?4, ?5)
                         ON CONFLICT(run_id, step_index) DO UPDATE SET
                            step_name = excluded.step_name,
                            status = excluded.status,
                            started_at = excluded.started_at",
                        rusqlite::params![row_id, step_index, name, status, now()],
                    )?;
                }
                Write::StepEnd { step_index, exit_code, duration_ms } => {
                    conn.execute(
                        "UPDATE step_runs
                         SET status = ?3, exit_code = ?4, duration_ms = ?5, finished_at = ?6
                         WHERE run_id = ?1 AND step_index = ?2",
                        rusqlite::params![
                            row_id,
                            step_index,
                            if exit_code == 0 { "success" } else { "failed" },
                            exit_code,
                            duration_ms,
                            now()
                        ],
                    )?;
                }
                Write::JobEnd { status, duration_ms } => {
                    conn.execute(
                        "UPDATE runs
                         SET status = ?2, finished_at = ?3, duration_ms = ?4
                         WHERE id = ?1",
                        rusqlite::params![row_id, status, now(), duration_ms],
                    )?;
                }
            }
            Ok::<(), anyhow::Error>(())
        })
        .await;
        match res {
            Ok(Ok(())) => {}
            Ok(Err(e)) => eprintln!("[history] 更新历史失败: {e:#}"),
            Err(e) => eprintln!("[history] 录制任务失败: {e}"),
        }
    }

    /// 收尾（JobEnd 后由事件循环自然走到；显式关连接避免 WAL 文件句柄滞留）
    pub async fn close(self) {
        let _ = tokio::task::spawn_blocking(move || {
            if let Ok(mutex) = Arc::try_unwrap(self.conn) {
                if let Ok(conn) = mutex.into_inner() {
                    let _ = conn.close();
                }
            }
        })
        .await;
    }
}

/// 最近运行列表（设置页「运行历史」展示用）
pub async fn recent_runs(limit: u32) -> anyhow::Result<Vec<crate::storage::HistoryRow>> {
    tokio::task::spawn_blocking(move || {
        let conn = open()?;
        // stmt 借用 conn：查询在块内完成（stmt drop）后再 close
        let rows = {
            let mut stmt = conn.prepare(
                "SELECT run_id, project_name, job_name, target, trigger_type, status,
                        started_at, duration_ms, log_path
                 FROM runs ORDER BY id DESC LIMIT ?1",
            )?;
            let mapped = stmt.query_map(rusqlite::params![limit], |r| {
                Ok(crate::storage::HistoryRow {
                    run_id: r.get(0)?,
                    project: r.get(1)?,
                    job: r.get(2)?,
                    target: r.get(3)?,
                    trigger: r.get(4)?,
                    status: r.get(5)?,
                    started_at: r.get(6)?,
                    duration_ms: r.get(7)?,
                    log_path: r.get(8)?,
                })
            })?;
            mapped.collect::<Result<Vec<_>, _>>()?
        };
        let _ = conn.close();
        Ok(rows)
    })
    .await?
}

/// 保留期清理（启动时调用）：删 started_at 早于 N 天前的 runs（step_runs 级联）；
/// 顺带把超过 24h 仍 running 的行标记 cancelled（进程被杀 / 断电留下的残行）
pub async fn prune_old(days: u32) -> anyhow::Result<u64> {
    tokio::task::spawn_blocking(move || {
        let conn = open()?;
        let cutoff = (Utc::now() - chrono::Duration::days(days as i64)).to_rfc3339();
        let n = conn.execute("DELETE FROM runs WHERE started_at < ?1", [&cutoff])? as u64;
        let stale_cutoff = (Utc::now() - chrono::Duration::hours(24)).to_rfc3339();
        conn.execute(
            "UPDATE runs SET status = 'cancelled', finished_at = ?1
             WHERE status = 'running' AND started_at < ?1",
            [&stale_cutoff],
        )?;
        let _ = conn.close();
        Ok(n)
    })
    .await?
}

/// 清空全部历史
pub async fn clear_all() -> anyhow::Result<()> {
    tokio::task::spawn_blocking(move || {
        let conn = open()?;
        conn.execute("DELETE FROM runs", [])?;
        let _ = conn.close();
        Ok(())
    })
    .await?
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Runtime::new().unwrap()
    }

    fn test_home(tag: &str) {
        let dir = std::env::temp_dir().join(format!("rbs_hist_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("REMOTE_BUILD_HOME", &dir);
    }

    fn cleanup() {
        if let Ok(dir) = std::env::var("REMOTE_BUILD_HOME") {
            let _ = std::fs::remove_dir_all(dir);
        }
        std::env::remove_var("REMOTE_BUILD_HOME");
    }

    #[test]
    fn record_run_lifecycle() {
        let rt = runtime();
        test_home("lifecycle");

        rt.block_on(async {
            let meta = RunMeta {
                run_id: "p-20260101".into(),
                project: "p".into(),
                job: "build".into(),
                target: Some("local".into()),
                trigger: "manual",
                log_path: Some("logs/p-20260101.log".into()),
            };
            let rec = RunRecorder::start(&meta, "name: build\n", 2).await.expect("recorder");
            rec.on_event(&LogEvent::StepStart {
                step_index: 0,
                name: "s0".into(),
                skipped: false,
            })
            .await;
            rec.on_event(&LogEvent::StepEnd { step_index: 0, exit_code: 0, duration_ms: 12 })
                .await;
            rec.on_event(&LogEvent::StepEnd { step_index: 1, exit_code: 3, duration_ms: 5 })
                .await; // 无 StepStart 的失败（引擎异常路径）
            rec.on_event(&LogEvent::JobEnd {
                run_id: "p-20260101".into(),
                status: JobStatus::Failed { step_index: 1, reason: "x".into() },
                total_duration_ms: 20,
            })
            .await;
            rec.close().await;

            let rows = recent_runs(10).await.unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].status, "failed");
            assert_eq!(rows[0].trigger, "manual");
            assert!(rows[0].duration_ms.unwrap() >= 0);

            // 保留期清理：0 天 = 全删
            assert!(prune_old(0).await.unwrap() >= 1);
            assert!(recent_runs(10).await.unwrap().is_empty());
        });
        cleanup();
    }
}
