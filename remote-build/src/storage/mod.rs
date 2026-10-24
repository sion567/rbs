//! 存储层入口。

pub mod files;

/// 运行历史行（设置页展示用；sqlite feature 关闭时列表恒为空）
#[derive(Debug, Clone, Default)]
pub struct HistoryRow {
    pub run_id: String,
    pub project: String,
    pub job: String,
    pub target: Option<String>,
    pub trigger: String,
    pub status: String,
    pub started_at: String,
    pub duration_ms: Option<i64>,
    pub log_path: Option<String>,
}

// sqlite 模块（P1 运行历史）在 feature = "sqlite" 下启用，见 §11：
// 配置本身永远不进 SQLite；SQLite 只存运行历史与日志路径。
#[cfg(feature = "sqlite")]
pub mod sqlite;
