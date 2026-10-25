//! 运行历史（SQLite，设计文档 §11.2，P1）。
//!
//! 三层分离原则：配置（YAML）永远不进库；SQLite 只存运行历史；日志原文落文件，
//! 库里只存路径。`config_snapshot` 记录当时跑的 job 配置（YAML 原文）——
//! 三个月后排查"当时跑的是 JDK17 还是 21"靠它。
//!
//! 开关：config.yaml `save_history`（默认开）；保留期复用 `log_retention_days`
//!（按 runs.started_at 清理，step_runs 经外键级联）。
//! 入库点：GUI / CLI 的事件转发任务——所有 LogEvent 都过那里，天然是录制点。

use crate::models::{JobStatus, LogEvent};
use chrono::Utc;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use std::str::FromStr as _;

/// history.db 路径（~/.remote-build/data/history.db）
pub fn db_path() -> std::path::PathBuf {
    crate::config::app_root().join("data").join("history.db")
}

/// 打开（不存在则创建）连接池 + 建表。每次运行调用一次（SQLite 本地文件，开销毫秒级）。
pub async fn open() -> anyhow::Result<SqlitePool> {
    let path = db_path();
    let dir = path.parent().expect("data dir");
    std::fs::create_dir_all(dir)?;
    let url = format!("sqlite://{}", path.display());
    let opts = SqliteConnectOptions::from_str(&url)?
        .create_if_missing(true)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
        .foreign_keys(true);
    let pool = SqlitePoolOptions::new().max_connections(2).connect_with(opts).await?;
    migrate(&pool).await?;
    Ok(pool)
}

/// 轻量迁移：PRAGMA user_version 记录 schema 版本，逐版本向前滚。
/// v0（空库）→ v1：初始两张表。此后加字段/加表都在这里追加 `if ver < N` 分支，
/// 不要改旧分支——老库按版本逐级升级，新库一次到位。
async fn migrate(pool: &SqlitePool) -> anyhow::Result<()> {
    let ver: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(pool)
        .await
        .unwrap_or(0);
    if ver < 1 {
        sqlx::query(
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
            )",
        )
        .execute(pool)
        .await?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS step_runs (
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
            )",
        )
        .execute(pool)
        .await?;
        sqlx::query("PRAGMA user_version = 1").execute(pool).await?;
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

/// 运行录制器：插 runs 行（running）→ 事件流更新 step_runs / runs → JobEnd 收尾。
/// 每个方法独立容错（库满/锁失败不阻塞运行本身，只 eprintln）。
pub struct RunRecorder {
    pool: SqlitePool,
    row_id: i64,
    /// 主流程步骤数：只录主流程的 step 事件（handlers / preflight 的 index 超界跳过）
    steps_len: usize,
    started: std::time::Instant,
}

impl RunRecorder {
    /// 插入 running 行；失败返回 None（调用方无录制器照常运行）
    pub async fn start(meta: &RunMeta, snapshot_yaml: &str, steps_len: usize) -> Option<Self> {
        let pool = match open().await {
            Ok(p) => p,
            Err(e) => {
                eprintln!("[history] 打开库失败（历史不记录）: {e:#}");
                return None;
            }
        };
        let row_id = sqlx::query_scalar::<_, i64>(
            "INSERT INTO runs
                (run_id, project_name, job_name, target, trigger_type, status, started_at,
                 log_path, config_snapshot)
             VALUES (?1, ?2, ?3, ?4, ?5, 'running', ?6, ?7, ?8)
             RETURNING id",
        )
        .bind(&meta.run_id)
        .bind(&meta.project)
        .bind(&meta.job)
        .bind(&meta.target)
        .bind(meta.trigger)
        .bind(now())
        .bind(&meta.log_path)
        .bind(snapshot_yaml)
        .fetch_one(&pool)
        .await;
        match row_id {
            Ok(id) => Some(Self { pool, row_id: id, steps_len, started: std::time::Instant::now() }),
            Err(e) => {
                eprintln!("[history] 记录运行失败（历史不记录）: {e:#}");
                None
            }
        }
    }

    pub async fn on_event(&self, ev: &LogEvent) {
        let res: anyhow::Result<()> = async {
            match ev {
                LogEvent::StepStart { step_index, name, skipped, .. }
                    if *step_index < self.steps_len =>
                {
                    let status = if *skipped { "skipped" } else { "running" };
                    sqlx::query(
                        "INSERT INTO step_runs
                            (run_id, step_index, step_name, status, started_at)
                         VALUES (?1, ?2, ?3, ?4, ?5)
                         ON CONFLICT(run_id, step_index) DO UPDATE SET
                            step_name = excluded.step_name,
                            status = excluded.status,
                            started_at = excluded.started_at",
                    )
                    .bind(self.row_id)
                    .bind(*step_index as i64)
                    .bind(name)
                    .bind(status)
                    .bind(now())
                    .execute(&self.pool)
                    .await?;
                }
                LogEvent::StepEnd { step_index, exit_code, duration_ms, .. }
                    if *step_index < self.steps_len =>
                {
                    let status = if *exit_code == 0 { "success" } else { "failed" };
                    sqlx::query(
                        "UPDATE step_runs
                         SET status = ?3, exit_code = ?4, duration_ms = ?5, finished_at = ?6
                         WHERE run_id = ?1 AND step_index = ?2",
                    )
                    .bind(self.row_id)
                    .bind(*step_index as i64)
                    .bind(status)
                    .bind(*exit_code as i64)
                    .bind(*duration_ms as i64)
                    .bind(now())
                    .execute(&self.pool)
                    .await?;
                }
                LogEvent::JobEnd { status, .. } => {
                    let duration = self.started.elapsed().as_millis() as i64;
                    sqlx::query(
                        "UPDATE runs
                         SET status = ?2, finished_at = ?3, duration_ms = ?4
                         WHERE id = ?1",
                    )
                    .bind(self.row_id)
                    .bind(status_key(status))
                    .bind(now())
                    .bind(duration)
                    .execute(&self.pool)
                    .await?;
                }
                _ => {}
            }
            Ok(())
        }
        .await;
        if let Err(e) = res {
            eprintln!("[history] 更新历史失败: {e:#}");
        }
    }

    /// 收尾（JobEnd 后由事件循环自然走到；显式关池避免 WAL 文件句柄滞留）
    pub async fn close(self) {
        self.pool.close().await;
    }
}

/// 最近运行列表（设置页「运行历史」展示用）
pub async fn recent_runs(limit: u32) -> anyhow::Result<Vec<crate::storage::HistoryRow>> {
    let pool = open().await?;
    let rows = sqlx::query_as::<_, RawRow>(
        "SELECT run_id, project_name, job_name, target, trigger_type, status,
                started_at, duration_ms, log_path
         FROM runs ORDER BY id DESC LIMIT ?1",
    )
    .bind(limit)
    .fetch_all(&pool)
    .await?;
    pool.close().await;
    Ok(rows
        .into_iter()
        .map(|r| crate::storage::HistoryRow {
            run_id: r.run_id,
            project: r.project_name,
            job: r.job_name,
            target: r.target,
            trigger: r.trigger_type,
            status: r.status,
            started_at: r.started_at,
            duration_ms: r.duration_ms,
            log_path: r.log_path,
        })
        .collect())
}

/// sqlx 直接映射的原始行（对外的 HistoryRow 不依赖 sqlx 派生）
#[derive(sqlx::FromRow)]
struct RawRow {
    run_id: String,
    project_name: String,
    job_name: String,
    target: Option<String>,
    trigger_type: String,
    status: String,
    started_at: String,
    duration_ms: Option<i64>,
    log_path: Option<String>,
}

/// 保留期清理（启动时调用）：删 started_at 早于 N 天前的 runs（step_runs 级联）；
/// 顺带把超过 24h 仍 running 的行标记 cancelled（进程被杀 / 断电留下的残行）
pub async fn prune_old(days: u32) -> anyhow::Result<u64> {
    let pool = open().await?;
    let cutoff = (Utc::now() - chrono::Duration::days(days as i64)).to_rfc3339();
    let res = sqlx::query("DELETE FROM runs WHERE started_at < ?1")
        .bind(&cutoff)
        .execute(&pool)
        .await?;
    let stale_cutoff = (Utc::now() - chrono::Duration::hours(24)).to_rfc3339();
    sqlx::query(
        "UPDATE runs SET status = 'cancelled', finished_at = ?1
         WHERE status = 'running' AND started_at < ?1",
    )
    .bind(&stale_cutoff)
    .execute(&pool)
    .await?;
    pool.close().await;
    Ok(res.rows_affected())
}

/// 清空全部历史
pub async fn clear_all() -> anyhow::Result<()> {
    let pool = open().await?;
    sqlx::query("DELETE FROM runs").execute(&pool).await?;
    pool.close().await;
    Ok(())
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
