//! 配置管理（设计文档 §3.6 / §7 / §10.1）。
//!
//! 目录布局：
//! ~/.remote-build/
//! ├── config.yaml      全局设置
//! ├── tools.yaml       工具链（首次探测生成）
//! ├── targets.yaml     SSH / k8s 目标
//! ├── projects/        构建配置仓库（用户 git clone 到这里）
//! └── logs/            运行日志

use crate::env_detect::DetectionReport;
use crate::models::*;
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// 应用根目录；测试可通过 REMOTE_BUILD_HOME 环境变量重定向
pub fn app_root() -> PathBuf {
    if let Ok(p) = std::env::var("REMOTE_BUILD_HOME") {
        return PathBuf::from(p);
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".remote-build")
}

pub fn config_path() -> PathBuf { app_root().join("config.yaml") }
pub fn tools_path() -> PathBuf { app_root().join("tools.yaml") }
pub fn projects_dir() -> PathBuf { app_root().join("projects") }
pub fn logs_dir() -> PathBuf { app_root().join("logs") }

/// 创建目录骨架（§7.1 首次启动第一步）
pub fn ensure_layout() -> std::io::Result<()> {
    std::fs::create_dir_all(projects_dir())?;
    std::fs::create_dir_all(logs_dir())?;
    Ok(())
}

pub fn is_first_run() -> bool {
    !config_path().exists()
}

pub fn default_global() -> GlobalConfig {
    let mut g = GlobalConfig::default();
    g.workspace_root = projects_dir().to_string_lossy().to_string();
    g
}

// ---------------------------------------------------------------------------
// 读写
// ---------------------------------------------------------------------------

pub fn load_global() -> Result<GlobalConfig> {
    let p = config_path();
    if !p.exists() {
        return Ok(default_global());
    }
    crate::storage::files::load_yaml(&p)
}

pub fn save_global(cfg: &GlobalConfig) -> Result<()> {
    crate::storage::files::save_yaml(&config_path(), cfg)
}

pub fn load_tools() -> Result<ToolsFile> {
    let p = tools_path();
    if !p.exists() {
        return Ok(ToolsFile::default());
    }
    crate::storage::files::load_yaml(&p)
}

pub fn save_tools(tools: &ToolsFile) -> Result<()> {
    crate::storage::files::save_yaml(&tools_path(), tools)
}

// ---------------------------------------------------------------------------
// 首次启动生成（§7.1 / §17.3）
// ---------------------------------------------------------------------------

/// 根据探测结果生成 tools.yaml / config.yaml / 示例项目
/// （v0.4 调整：目标随项目走，不再生成全局 targets.yaml）
pub fn first_run_generate(report: &DetectionReport) -> Result<()> {
    ensure_layout().context("创建目录结构失败")?;

    // tools.yaml：探测成功的工具 + putty 条目（仅供 UI 展示，见 §3.4 v0.4 澄清）
    let mut tools = ToolsFile::default();
    for dt in &report.tools {
        if !dt.ok {
            continue;
        }
        tools.tools.insert(
            dt.name.clone(),
            Tool {
                name: dt.name.clone(),
                tool_type: ToolType::System,
                command: dt.command.clone(),
                install_path: None,
                version: dt.version.clone(),
                version_check: dt.version_check.clone(),
                detected_from: Some(dt.source.clone()),
                install: None,
                env: HashMap::new(),
            },
        );
    }
    if report.putty.found {
        tools.tools.insert(
            "putty".to_string(),
            Tool {
                name: "putty".to_string(),
                tool_type: ToolType::Custom,
                command: report.putty.pscp.to_string_lossy().to_string(),
                install_path: Some(report.putty.dir.to_string_lossy().to_string()),
                version: report.putty.version.clone(),
                version_check: None,
                detected_from: Some(report.putty.source.clone()),
                install: None,
                env: HashMap::new(),
            },
        );
    }
    save_tools(&tools)?;
    // config.yaml
    if !config_path().exists() {
        save_global(&default_global())?;
    }

    // 示例项目：演示本地执行 + 变量替换
    create_example_project()?;
    Ok(())
}

pub fn local_target() -> Target {
    Target {
        name: "local".to_string(),
        target_type: TargetType::Local,
        work_root: None,
        env: HashMap::new(),
        cred_key: None,
    }
}

/// 项目级目标的规范化（§3.5）：回填 Target.name = 键名、凭据键 = {项目}/{目标}——
/// 不同项目的同名目标（如各自的 prod）密码互不干扰（§14）。
/// v0.4 调整：目标仅定义在 project.yaml，不再有全局 targets.yaml。
pub fn effective_targets(project: &Project) -> HashMap<String, Target> {
    let mut out = HashMap::new();
    for (k, t) in &project.targets {
        let mut t = t.clone();
        t.name = k.clone();
        t.cred_key = Some(format!("{}/{k}", project.name));
        out.insert(k.clone(), t);
    }
    out
}

/// 本地目标的单条映射（项目 targets 初始化用）
pub fn local_targets_map() -> HashMap<String, Target> {
    [local_target()].into_iter().map(|t| (t.name.clone(), t)).collect()
}

fn create_example_project() -> Result<()> {
    let dir = projects_dir().join("example");
    std::fs::create_dir_all(&dir)?;

    let project = Project {
        name: "example".to_string(),
        description: Some("示例项目：演示本地执行与变量替换".to_string()),
        local_path: dir.to_string_lossy().to_string(),
        variables: [(
            "staging".to_string(),
            EnvironmentVariables {
                vars: [("GREETING".to_string(), "hello-staging".to_string())]
                    .into_iter()
                    .collect(),
            },
        )]
        .into_iter()
        .collect(),
        tools: HashMap::new(),
        jobs: vec!["build".to_string()],
        targets: crate::config::local_targets_map(),
        env_file: None,
    };
    crate::storage::files::save_yaml(&dir.join("project.yaml"), &project)?;

    let job = Job {
        name: "build".to_string(),
        project: "example".to_string(),
        description: Some("示例：两条本地 echo".to_string()),
        target: Some("local".to_string()),
        environment: Some("staging".to_string()),
        timeout: Some(60),
        on_failure: FailureAction::Stop,
        steps: vec![
            Step {
                name: Some("问候".to_string()),
                tool: String::new(),
                command: "echo ${{ variables.GREETING }}".to_string(),
                step_type: StepType::Exec,
                enabled: true,
                ..Default::default()
            },
            Step {
                name: Some("再来一条".to_string()),
                tool: String::new(),
                command: "echo done".to_string(),
                step_type: StepType::Exec,
                enabled: true,
                ..Default::default()
            },
        ],
        working_dir: None,
    };
    crate::storage::files::save_yaml(&dir.join("build.yaml"), &job)?;
    Ok(())
}

/// 默认新建项目目录：~/.remote-build/projects/<name>（Git clone 目标 / 本地模式的配置目录）
pub fn default_project_dir(name: &str) -> PathBuf {
    projects_dir().join(name)
}

/// Windows 文件名非法字符替换为下划线。
/// 流程名（如 dev:h5）进 YAML 的 name 字段保留原名，仅文件名净化。
pub fn sanitize_filename(s: &str) -> String {
    s.chars()
        .map(|c| if r#"\/:*?"<>|"#.contains(c) { '_' } else { c })
        .collect()
}

/// 流程（job）文件路径：projects/<项目>/<流程名>.yaml，文件名经净化
pub fn job_file_path(project_dir: &Path, job_name: &str) -> PathBuf {
    project_dir.join(format!("{}.yaml", sanitize_filename(job_name)))
}

/// 新流程的起始步骤模板
pub fn starter_steps() -> Vec<Step> {
    vec![Step {
        name: Some("新步骤".to_string()),
        tool: String::new(),
        command: "echo 新流程，开始编辑你的步骤".to_string(),
        step_type: StepType::Exec,
        enabled: true,
        ..Default::default()
    }]
}

/// 生成 project.yaml + 起始 build.yaml（Git clone 完成后 / 本地目录模式创建时调用）。
/// 返回项目配置目录（projects/<name>）。
pub fn create_project_entry(name: &str, local_path: &Path) -> Result<PathBuf> {
    let dir = projects_dir().join(name);
    std::fs::create_dir_all(&dir)?;

    let project = Project {
        name: name.to_string(),
        description: None,
        local_path: local_path.to_string_lossy().to_string(),
        variables: HashMap::new(),
        tools: HashMap::new(),
        jobs: vec!["build".to_string()],
        targets: crate::config::local_targets_map(),
        env_file: None,
    };
    crate::storage::files::save_yaml(&dir.join("project.yaml"), &project)?;

    let job = Job {
        name: "build".to_string(),
        project: name.to_string(),
        description: Some("起始作业，可在编辑器中修改".to_string()),
        target: Some("local".to_string()),
        environment: None,
        timeout: Some(60),
        on_failure: FailureAction::Stop,
        steps: vec![Step {
            name: Some("项目就绪".to_string()),
            tool: String::new(),
            command: "echo 项目已创建，开始编写你的构建步骤".to_string(),
            step_type: StepType::Exec,
            enabled: true,
            ..Default::default()
        }],
        working_dir: None,
    };
    crate::storage::files::save_yaml(&dir.join("build.yaml"), &job)?;
    Ok(dir)
}

// ---------------------------------------------------------------------------
// 项目校验（§9.4）
// ---------------------------------------------------------------------------

pub fn validate_project(project: &Project) -> Vec<ValidationIssue> {
    let mut issues = Vec::new();
    let path = Path::new(&project.local_path);

    if !path.exists() {
        issues.push(ValidationIssue::Error {
            field: "local_path".to_string(),
            message: format!("目录不存在: {}，请检查路径或先 git clone", project.local_path),
        });
    } else if !path.join(".git").exists() {
        issues.push(ValidationIssue::Warning {
            field: "local_path".to_string(),
            message: "目录不是 git 仓库，部分步骤可能异常".to_string(),
        });
    }
    issues
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 两个用例都依赖 REMOTE_BUILD_HOME 环境变量，合并为单个测试顺序执行，
    /// 避免并行测试互相改环境变量。
    #[test]
    fn project_entry_and_job_name_roundtrip() {
        let tmp = std::env::temp_dir().join(format!("rbs_proj_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::env::set_var("REMOTE_BUILD_HOME", &tmp);

        // ── create_project_entry 生成 project.yaml + build.yaml ──
        let code_dir = tmp.join("code");
        std::fs::create_dir_all(&code_dir).unwrap();
        let dir = create_project_entry("demo", &code_dir).unwrap();

        assert!(dir.join("project.yaml").exists());
        assert!(dir.join("build.yaml").exists());
        let p: Project =
            crate::storage::files::load_yaml(&dir.join("project.yaml")).unwrap();
        assert_eq!(p.name, "demo");
        assert_eq!(p.local_path, code_dir.to_string_lossy().to_string());
        let j: Job = crate::storage::files::load_yaml(&dir.join("build.yaml")).unwrap();
        assert_eq!(j.steps.len(), 1);
        assert!(j.steps[0].enabled);

        // ── dev:h5 这类自由命名：文件名净化为 dev_h5.yaml，name 保留冒号 ──
        let job = Job {
            name: "deploy-dev:h5".to_string(),
            project: "demo".to_string(),
            steps: starter_steps(),
            ..Default::default()
        };
        crate::storage::files::save_yaml(&job_file_path(&dir, "deploy-dev:h5"), &job).unwrap();

        let found = crate::storage::files::discover_projects(&projects_dir());
        let entry = found.iter().find(|p| p.project.name == "demo").unwrap();
        let je = entry
            .jobs
            .iter()
            .find(|j| j.job.name == "deploy-dev:h5")
            .expect("应能按原名发现流程");
        assert_eq!(je.path.file_name().unwrap(), "deploy-dev_h5.yaml");

        let _ = std::fs::remove_dir_all(&tmp);
        std::env::remove_var("REMOTE_BUILD_HOME");
    }
}
