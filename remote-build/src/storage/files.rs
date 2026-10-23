//! YAML 文件读写与项目发现（设计文档 §10.1 / §16 storage/files.rs）。

use crate::models::{Job, Project};
use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::path::{Path, PathBuf};

pub fn load_yaml<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("读取失败: {}", path.display()))?;
    yaml_serde::from_str(&text).with_context(|| format!("YAML 解析失败: {}", path.display()))
}

pub fn save_yaml<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = yaml_serde::to_string(value)?;
    // 先写临时文件再改名，避免写一半崩溃留下残缺配置
    let tmp = path.with_extension("yaml.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path).with_context(|| format!("写入失败: {}", path.display()))?;
    Ok(())
}

#[derive(Debug, Clone)]
pub struct JobEntry {
    /// job.yaml 文件路径（保存时用）
    pub path: PathBuf,
    pub job: Job,
}

#[derive(Debug, Clone)]
pub struct ProjectEntry {
    pub dir: PathBuf,
    pub project: Project,
    /// 目录下除 project.yaml 外的所有 *.yaml 作业，按文件名排序
    pub jobs: Vec<JobEntry>,
}

/// 扫描 workspace_root 下所有 <dir>/project.yaml（无法解析的作业/项目文件记入错误列表）
pub fn discover_projects(root: &Path) -> Vec<ProjectEntry> {
    discover_with_errors(root).0
}

/// 带错误报告的扫描：CLI 模式把解析失败打印给用户，避免"流程莫名消失"
pub fn discover_with_errors(root: &Path) -> (Vec<ProjectEntry>, Vec<String>) {
    let mut errors = Vec::new();
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return (out, errors);
    };

    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();

    for dir in dirs {
        let project_file = dir.join("project.yaml");
        if !project_file.exists() {
            continue;
        }
        let project = match load_yaml::<Project>(&project_file) {
            Ok(p) => p,
            Err(e) => {
                errors.push(format!(
                    "项目文件无法解析（已跳过）: {} —— {e:#}",
                    project_file.display()
                ));
                continue;
            }
        };

        let mut jobs = Vec::new();
        if let Ok(files) = std::fs::read_dir(&dir) {
            let mut names: Vec<PathBuf> = files
                .flatten()
                .map(|e| e.path())
                .filter(|p| {
                    p.extension().map(|e| e == "yaml").unwrap_or(false)
                        && p.file_name().map(|n| n != "project.yaml").unwrap_or(false)
                })
                .collect();
            names.sort();
            for f in names {
                match load_yaml::<Job>(&f) {
                    Ok(job) => jobs.push(JobEntry { path: f, job }),
                    Err(e) => errors.push(format!(
                        "作业文件无法解析（已跳过）: {} —— {e:#}",
                        f.display()
                    )),
                }
            }
        }
        out.push(ProjectEntry { dir, project, jobs });
    }
    (out, errors)
}
