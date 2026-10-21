//! P1：运行历史 SQLite 存储（设计文档 §11）。
//!
//! 在 feature = "sqlite" 下启用；`runs` / `step_runs` 表结构见文档 §11.2。
//! 配置本身永远不进 SQLite——配置文件要能 git 管理、能 diff、能手动改。
