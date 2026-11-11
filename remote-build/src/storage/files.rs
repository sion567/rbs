//! YAML 文件读写与项目发现（设计文档 §10.1 / §16 storage/files.rs）。

use crate::models::{Job, Project};
use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::path::{Path, PathBuf};

pub fn load_yaml<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("读取失败: {}", path.display()))?;
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

/// 扫描 projects_root 下所有 <dir>/project.yaml（无法解析的作业/项目文件记入错误列表）
pub fn discover_projects(root: &Path) -> Vec<ProjectEntry> {
    discover_with_errors(root).0
}

/// 带错误报告的扫描：CLI 模式把解析失败打印给用户，避免"流程莫名消失"。
/// 任意深度布局：目录含 project.yaml = 工程（不再往下扫，工程内部如 .git /
/// node_modules 不进发现）；不含 = 纯归类容器，继续下钻——目录层级即结构，
/// 不限制深度。工程的 category 字段缺省时回填相对 root 的父目录链（a/b 形式）。
pub fn discover_with_errors(root: &Path) -> (Vec<ProjectEntry>, Vec<String>) {
    let mut errors = Vec::new();
    let mut out = Vec::new();
    walk_projects(root, root, 0, &mut out, &mut errors);

    // 工程名要求全局唯一：CLI 查找、运行历史、锁文件、凭据键全部按名工作，
    // 重名会让 HashMap 缓存互相覆盖——保留两条能显示，但必须告警提醒改名
    let mut seen_names = std::collections::HashSet::new();
    for entry in &out {
        if !seen_names.insert(entry.project.name.clone()) {
            errors.push(format!(
                "工程名「{}」重复（{}）：CLI/历史/锁按工程名索引会互相干扰，请改名",
                entry.project.name,
                entry.dir.display()
            ));
        }
    }
    (out, errors)
}

/// 递归发现：dir 下的子目录，有 project.yaml 的是工程（category 回填相对链），
/// 没有的是容器，继续下钻。depth 只防 junction/符号链接环路（正常结构远用不到 16 层）。
fn walk_projects(
    root: &Path,
    dir: &Path,
    depth: usize,
    out: &mut Vec<ProjectEntry>,
    errors: &mut Vec<String>,
) {
    if depth > 16 {
        errors.push(format!(
            "目录层级超过 16 层，已停止下钻: {}（疑似 junction/symlink 环路）",
            dir.display()
        ));
        return;
    }
    for sub in sorted_subdirs(dir) {
        if sub.join("project.yaml").exists() {
            // 工程：category 只认 yaml 自己的字段（手写填了就照显示）；
            // 缺省回填相对 root 的父目录链（手挪目录的迁移路径）
            if let Some(mut entry) = load_project_dir(&sub, errors) {
                if entry.project.category.is_none() {
                    entry.project.category = relative_parent_chain(root, &sub);
                }
                out.push(entry);
            }
        } else {
            walk_projects(root, &sub, depth + 1, out, errors);
        }
    }
}

/// 工程目录相对 root 的父目录链（"a/b" 形式；直接在 root 下 = None）
fn relative_parent_chain(root: &Path, project_dir: &Path) -> Option<String> {
    let rel = project_dir.strip_prefix(root).ok()?;
    let mut segs = Vec::new();
    for c in rel.components() {
        segs.push(c.as_os_str().to_string_lossy().to_string());
    }
    segs.pop(); // 最后一段是工程目录自己，剩下的才是归类链
    if segs.is_empty() {
        None
    } else {
        Some(segs.join("/"))
    }
}

fn sorted_subdirs(dir: &Path) -> Vec<PathBuf> {
    let mut subs: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|it| {
            it.flatten()
                .map(|e| e.path())
                .filter(|p| p.is_dir())
                .collect()
        })
        .unwrap_or_default();
    subs.sort();
    subs
}

/// 加载单个工程目录（project.yaml + jobs/ 与旧布局根目录 *.yaml）；非工程目录返回 None
fn load_project_dir(dir: &Path, errors: &mut Vec<String>) -> Option<ProjectEntry> {
    let project_file = dir.join("project.yaml");
    if !project_file.exists() {
        return None;
    }
    let project = match load_yaml::<Project>(&project_file) {
        Ok(p) => p,
        Err(e) => {
            errors.push(format!(
                "项目文件无法解析（已跳过）: {} —— {e:#}",
                project_file.display()
            ));
            return None;
        }
    };

    // 流程文件：jobs/ 子目录（现布局）优先，根目录 *.yaml 兼容旧布局
    //（项目配置与代码分离后，旧项目根目录的流程 yaml 继续可用，编辑仍原位保存）。
    // 同名时以 jobs/ 为准（迁移后双份并存不产生重复流程）
    let mut jobs = Vec::new();
    let mut seen: std::collections::HashSet<String> = Default::default();
    let mut scan = |files: Vec<PathBuf>,
                    jobs: &mut Vec<JobEntry>,
                    seen: &mut std::collections::HashSet<String>| {
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
    let legacy_files = std::fs::read_dir(dir)
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
    Some(ProjectEntry {
        dir: dir.to_path_buf(),
        project,
        jobs,
    })
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

        crate::storage::files::save_yaml(
            &dir.join("project.yaml"),
            &crate::models::Project {
                name: "demo".to_string(),
                local_path: dir.to_string_lossy().to_string(),
                jobs: vec!["build".to_string(), "legacy".to_string()],
                ..Default::default()
            },
        )
        .unwrap();
        // 现布局：jobs/build.yaml
        crate::storage::files::save_yaml(
            &dir.join("jobs").join("build.yaml"),
            &job_named("build", "new-layout"),
        )
        .unwrap();
        // 旧布局：根目录 legacy.yaml；与 jobs/ 同名的旧文件应被忽略
        crate::storage::files::save_yaml(
            &dir.join("legacy.yaml"),
            &job_named("legacy", "old-layout"),
        )
        .unwrap();
        crate::storage::files::save_yaml(
            &dir.join("build.yaml"),
            &job_named("build", "stale-root"),
        )
        .unwrap();
        // 干扰项：yml 扩展名 / 非流程 yaml 名（project.yaml 已排除）
        std::fs::write(dir.join("readme.yml"), "name: x").unwrap();

        let found = discover_projects(&root);
        assert_eq!(found.len(), 1);
        let entry = &found[0];
        assert_eq!(
            entry.jobs.len(),
            2,
            "{:?}",
            entry
                .jobs
                .iter()
                .map(|j| j.job.name.clone())
                .collect::<Vec<_>>()
        );

        let build = entry.jobs.iter().find(|j| j.job.name == "build").unwrap();
        assert_eq!(
            build.job.description.as_deref(),
            Some("new-layout"),
            "同名以 jobs/ 为准"
        );
        assert!(build.path.ends_with("jobs/build.yaml"));
        let legacy = entry.jobs.iter().find(|j| j.job.name == "legacy").unwrap();
        assert!(
            legacy.path.ends_with("legacy.yaml"),
            "旧布局根目录流程仍可发现"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 两级布局：一级目录无 project.yaml 视为分类目录，子目录工程被识别，
    /// category 字段缺省时回填父目录名；平铺工程与分类工程可混存
    #[test]
    fn discovers_category_layout_and_backfills() {
        let root = std::env::temp_dir().join(format!("rbs_files_cat_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        // 分类目录下两个工程（category 留空，靠目录回填）
        let api = root.join("项目A").join("api工程");
        std::fs::create_dir_all(api.join("jobs")).unwrap();
        save_yaml(
            &api.join("project.yaml"),
            &crate::models::Project {
                name: "api工程".to_string(),
                local_path: String::new(),
                ..Default::default()
            },
        )
        .unwrap();
        save_yaml(
            &api.join("jobs").join("build.yaml"),
            &job_named("build", "api"),
        )
        .unwrap();
        // 同分类另一个工程显式写了 category：保留字段值，不被目录名覆盖
        let web = root.join("项目A").join("web工程");
        std::fs::create_dir_all(&web).unwrap();
        save_yaml(
            &web.join("project.yaml"),
            &crate::models::Project {
                name: "web工程".to_string(),
                category: Some("自定义分类".to_string()),
                local_path: String::new(),
                ..Default::default()
            },
        )
        .unwrap();
        // 平铺工程（无分类）混存
        let b = root.join("项目B");
        std::fs::create_dir_all(b.join("jobs")).unwrap();
        save_yaml(
            &b.join("project.yaml"),
            &crate::models::Project {
                name: "项目B".to_string(),
                local_path: String::new(),
                ..Default::default()
            },
        )
        .unwrap();

        let (found, errors) = discover_with_errors(&root);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(
            found.len(),
            3,
            "{:?}",
            found
                .iter()
                .map(|e| e.project.name.clone())
                .collect::<Vec<_>>()
        );

        let api_e = found.iter().find(|e| e.project.name == "api工程").unwrap();
        assert_eq!(
            api_e.project.category.as_deref(),
            Some("项目A"),
            "category 回填父目录名"
        );
        let web_e = found.iter().find(|e| e.project.name == "web工程").unwrap();
        assert_eq!(
            web_e.project.category.as_deref(),
            Some("自定义分类"),
            "显式 category 优先"
        );
        let b_e = found.iter().find(|e| e.project.name == "项目B").unwrap();
        assert_eq!(b_e.project.category, None, "平铺工程无分类");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 工程名全局唯一：跨分类重名要告警（CLI/历史/锁按名索引）
    #[test]
    fn warns_on_duplicate_project_names() {
        let root = std::env::temp_dir().join(format!("rbs_files_dup_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for parent in ["甲项目", "乙项目"] {
            let dir = root.join(parent).join("同名工程");
            std::fs::create_dir_all(&dir).unwrap();
            save_yaml(
                &dir.join("project.yaml"),
                &crate::models::Project {
                    name: "同名工程".to_string(),
                    local_path: String::new(),
                    ..Default::default()
                },
            )
            .unwrap();
        }

        let (found, errors) = discover_with_errors(&root);
        assert_eq!(found.len(), 2, "两条都保留可显示");
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].contains("同名工程"), "{errors:?}");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 空分类目录（没挪工程进去）不报错、不产生条目
    #[test]
    fn empty_category_dir_is_harmless() {
        let root = std::env::temp_dir().join(format!("rbs_files_empty_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("空项目").join("随便什么子目录")).unwrap();

        let (found, errors) = discover_with_errors(&root);
        assert!(found.is_empty());
        assert!(errors.is_empty(), "{errors:?}");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 任意深度：三层 a/b/工程 照样发现，category 回填 "a/b"；
    /// 工程目录内部的 project.yaml 不再下钻（工程内部的目录不是工程）
    #[test]
    fn discovers_arbitrary_depth_and_stops_at_project() {
        let root = std::env::temp_dir().join(format!("rbs_files_deep_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        // 三层容器里的工程
        let deep = root.join("a").join("b").join("c").join("d").join("深工程");
        std::fs::create_dir_all(&deep).unwrap();
        save_yaml(
            &deep.join("project.yaml"),
            &crate::models::Project {
                name: "深工程".to_string(),
                local_path: String::new(),
                ..Default::default()
            },
        )
        .unwrap();
        // 工程内部的"伪工程"：发现了 深工程 就该停，它不该成为独立条目
        let nested = deep.join("node_modules").join("内嵌伪工程");
        std::fs::create_dir_all(&nested).unwrap();
        save_yaml(
            &nested.join("project.yaml"),
            &crate::models::Project {
                name: "内嵌伪工程".to_string(),
                local_path: String::new(),
                ..Default::default()
            },
        )
        .unwrap();

        let (found, errors) = discover_with_errors(&root);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(found.len(), 1, "工程内部不重复下钻");
        assert_eq!(found[0].project.name, "深工程");
        assert_eq!(
            found[0].project.category.as_deref(),
            Some("a/b/c/d"),
            "category 回填完整父目录链"
        );

        let _ = std::fs::remove_dir_all(&root);
    }
}
