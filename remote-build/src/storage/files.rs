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

        // 流程文件：jobs/ 子目录（现布局）优先，根目录 *.yaml 兼容旧布局
        //（项目配置与代码分离后，旧项目根目录的流程 yaml 继续可用，编辑仍原位保存）。
        // 同名时以 jobs/ 为准（迁移后双份并存不产生重复流程）
        let mut jobs = Vec::new();
        let mut seen: std::collections::HashSet<String> = Default::default();
        let mut scan = |files: Vec<PathBuf>, jobs: &mut Vec<JobEntry>, seen: &mut std::collections::HashSet<String>| {
            let mut names = files;
            names.sort();
            for f in names {
                let Some(name) = f.file_name().map(|n| n.to_string_lossy().to_string()) else {
                    continue;
                };
                if !seen.insert(name) {
                    continue;
                }
                match load_yaml::<Job>(&f) {
                    Ok(job) => jobs.push(JobEntry { path: f, job }),
                    Err(e) => errors.push(format!(
                        "作业文件无法解析（已跳过）: {} —— {e:#}",
                        f.display()
                    )),
                }
            }
        };
        let jobs_dir_files = std::fs::read_dir(dir.join("jobs"))
            .map(|it| {
                it.flatten()
                    .map(|e| e.path())
                    .filter(|p| p.extension().map(|e| e == "yaml").unwrap_or(false))
                    .collect()
            })
            .unwrap_or_default();
        scan(jobs_dir_files, &mut jobs, &mut seen);
        let legacy_files = std::fs::read_dir(&dir)
            .map(|it| {
                it.flatten()
                    .map(|e| e.path())
                    .filter(|p| {
                        p.is_file()
                            && p.extension().map(|e| e == "yaml").unwrap_or(false)
                            && p.file_name().map(|n| n != "project.yaml").unwrap_or(false)
                    })
                    .collect()
            })
            .unwrap_or_default();
        scan(legacy_files, &mut jobs, &mut seen);
        out.push(ProjectEntry { dir, project, jobs });
    }
    (out, errors)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{FailureAction, Job, Step, StepType};

    fn job_named(name: &str, marker: &str) -> Job {
        Job {
            name: name.to_string(),
            project: "demo".to_string(),
            description: Some(marker.to_string()),
            steps: vec![Step {
                name: Some(marker.to_string()),
                command: "echo x".to_string(),
                step_type: StepType::Exec,
                enabled: true,
                ..Default::default()
            }],
            on_failure: FailureAction::Stop,
            ..Default::default()
        }
    }

    /// 双布局发现：jobs/ 子目录（现布局）与根目录 *.yaml（旧布局）都能被发现；
    /// 同名时以 jobs/ 为准，不产生重复流程；非 yaml / project.yaml 不算流程
    #[test]
    fn discovers_jobs_subdir_and_legacy_root() {
        let root = std::env::temp_dir().join(format!("rbs_files_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dir = root.join("demo"); // discover 扫描 root 下的项目子目录
        std::fs::create_dir_all(dir.join("jobs")).unwrap();

        crate::storage::files::save_yaml(&dir.join("project.yaml"), &crate::models::Project {
            name: "demo".to_string(),
            local_path: dir.to_string_lossy().to_string(),
            jobs: vec!["build".to_string(), "legacy".to_string()],
            ..Default::default()
        })
        .unwrap();
        // 现布局：jobs/build.yaml
        crate::storage::files::save_yaml(&dir.join("jobs").join("build.yaml"), &job_named("build", "new-layout")).unwrap();
        // 旧布局：根目录 legacy.yaml；与 jobs/ 同名的旧文件应被忽略
        crate::storage::files::save_yaml(&dir.join("legacy.yaml"), &job_named("legacy", "old-layout")).unwrap();
        crate::storage::files::save_yaml(&dir.join("build.yaml"), &job_named("build", "stale-root")).unwrap();
        // 干扰项：yml 扩展名 / 非流程 yaml 名（project.yaml 已排除）
        std::fs::write(dir.join("readme.yml"), "name: x").unwrap();

        let found = discover_projects(&root);
        assert_eq!(found.len(), 1);
        let entry = &found[0];
        assert_eq!(entry.jobs.len(), 2, "{:?}", entry.jobs.iter().map(|j| j.job.name.clone()).collect::<Vec<_>>());

        let build = entry.jobs.iter().find(|j| j.job.name == "build").unwrap();
        assert_eq!(build.job.description.as_deref(), Some("new-layout"), "同名以 jobs/ 为准");
        assert!(build.path.ends_with("jobs/build.yaml"));
        let legacy = entry.jobs.iter().find(|j| j.job.name == "legacy").unwrap();
        assert!(legacy.path.ends_with("legacy.yaml"), "旧布局根目录流程仍可发现");

        let _ = std::fs::remove_dir_all(&root);
    }
}
