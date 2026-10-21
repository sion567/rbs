//! 存储层入口。

pub mod files;

// sqlite 模块（P1 运行历史）在 feature = "sqlite" 下启用，见 §11：
// 配置本身永远不进 SQLite；SQLite 只存运行历史与日志路径。
