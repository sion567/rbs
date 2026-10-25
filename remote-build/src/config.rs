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
    let res = crate::storage::files::save_yaml(&config_path(), cfg);
    // putty.custom_path 可能变了，传输层的探测缓存需要失效重探
    crate::transport::putty::invalidate_detection_cache();
    res
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
    merge_detection(report)?;
    // config.yaml
    if !config_path().exists() {
        save_global(&default_global())?;
    }

    // 示范项目：四本合集演示各类步骤用法
    create_example_project()?;
    Ok(())
}

/// 把探测结果合并进 tools.yaml 并落盘（首启生成 / 目标管理的「重新探测」共用）：
/// 同名条目更新 command/version/来源（保留用户配的 env、类型）；新探测到的插入；
/// 用户手动添加的自定义工具不受影响。返回写入的工具数。
pub fn merge_detection(report: &DetectionReport) -> Result<usize> {
    let mut file = load_tools().unwrap_or_default();
    let mut count = 0usize;
    for dt in &report.tools {
        if !dt.ok {
            continue;
        }
        let entry = file
            .tools
            .entry(dt.name.clone())
            .or_insert_with(|| Tool {
                name: dt.name.clone(),
                tool_type: ToolType::System,
                command: String::new(),
                version: None,
                version_check: None,
                detected_from: None,
                env: HashMap::new(),
            });
        entry.command = dt.command.clone();
        entry.version = dt.version.clone();
        entry.version_check = dt.version_check.clone();
        entry.detected_from = Some(dt.source.clone());
        count += 1;
    }
    if report.putty.found {
        let entry = file
            .tools
            .entry("putty".to_string())
            .or_insert_with(|| Tool {
                name: "putty".to_string(),
                tool_type: ToolType::Custom,
                command: String::new(),
                version: None,
                version_check: None,
                detected_from: None,
                env: HashMap::new(),
            });
        entry.command = report.putty.pscp.to_string_lossy().to_string();
        entry.version = report.putty.version.clone();
        entry.detected_from = Some(report.putty.source.clone());
        count += 1;
    }
    save_tools(&file)?;
    Ok(count)
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
        tools_require: HashMap::new(),
        groups: Vec::new(),
    };
    crate::storage::files::save_yaml(&dir.join("project.yaml"), &project)?;

    let job = Job {
        name: "build".to_string(),
        project: name.to_string(),
        description: Some("起始作业，可在编辑器中修改".to_string()),
        target: Some("local".to_string()),
        environment: None,
        timeout: Some(60),
        confirm: None,
        preconditions: Vec::new(),
        handlers: Vec::new(),
        retry: None,
        on_failure: FailureAction::Stop,
        steps: starter_steps(),
        working_dir: None,
        group: None,
    };
    crate::storage::files::save_yaml(&dir.join("build.yaml"), &job)?;
    Ok(dir)
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

/// 示范步骤快捷构造（合集生成专用）
fn demo_step(name: &str, command: &str) -> Step {
    Step {
        name: Some(name.to_string()),
        command: command.to_string(),
        step_type: StepType::Exec,
        enabled: true,
        ..Default::default()
    }
}

/// 生成示范工程 example：四本合集（Git / 构建 / Linux 远端 / 发布），
/// 覆盖几乎全部步骤特性——目的不是执行，是当活文档：每本都能直接点
/// 「👁 预览」看变量解析 / 条件 / 守卫 / 上传清单的全貌。
/// 远端合集使用占位目标 demo-server（192.0.2.10 是文档保留地址，永不连通），
/// 在目标管理里把它改成真实服务器即可真正执行。
pub fn create_example_project() -> Result<()> {
    let dir = projects_dir().join("example");
    std::fs::create_dir_all(&dir)?;

    let mut targets = HashMap::new();
    targets.insert("local".to_string(), local_target());
    targets.insert(
        "demo-server".to_string(),
        Target {
            name: "demo-server".to_string(),
            target_type: TargetType::Ssh {
                // RFC 5737 文档保留地址：示范用，永远不会真的连上
                host: "192.0.2.10".to_string(),
                port: 22,
                user: "deploy".to_string(),
                auth: AuthMethod::None,
                host_key: None,
            },
            work_root: Some("/opt/demo".to_string()),
            env: HashMap::new(),
            cred_key: None,
        },
    );

    let project = Project {
        name: "example".to_string(),
        description: Some(
            "示范工程：四本合集（Git / 构建 / Linux 远端 / 发布）展示各类步骤用法。\n\
             看「👁 预览」即可，无需真的执行；demo-server 是占位目标，改成真实机器就能跑"
                .to_string(),
        ),
        local_path: dir.to_string_lossy().to_string(),
        variables: [
            (
                "staging".to_string(),
                EnvironmentVariables {
                    vars: [
                        ("REGISTRY".to_string(), "registry-staging.internal:5000".to_string()),
                        ("NAMESPACE".to_string(), "demo-staging".to_string()),
                        ("APP_NAME".to_string(), "demo-app".to_string()),
                    ]
                    .into_iter()
                    .collect(),
                },
            ),
            (
                "prod".to_string(),
                EnvironmentVariables {
                    vars: [
                        ("REGISTRY".to_string(), "registry.internal:5000".to_string()),
                        ("NAMESPACE".to_string(), "demo-prod".to_string()),
                        ("APP_NAME".to_string(), "demo-app".to_string()),
                    ]
                    .into_iter()
                    .collect(),
                },
            ),
        ]
        .into_iter()
        .collect(),
        tools: HashMap::new(),
        // mise 风格版本门禁演示：java 不满足要求时运行零步失败（预览里也会 ⚠）
        tools_require: [("java".to_string(), ">=17".to_string())].into_iter().collect(),
        jobs: vec![
            "① Git 合集".to_string(),
            "② 构建合集".to_string(),
            "③ Linux 远端合集".to_string(),
            "④ 发布合集".to_string(),
        ],
        // 分组演示：本地构建 / 远端发布 各放两本
        groups: vec!["本地构建".to_string(), "远端发布".to_string()],
        targets,
    };
    crate::storage::files::save_yaml(&dir.join("project.yaml"), &project)?;

    // ── ① Git 合集：变量 / 默认值 / creates / 条件 / 未启用 / outputs / 重试 ──
    let git_job = Job {
        name: "① Git 合集".to_string(),
        project: "example".to_string(),
        description: Some(
            "演示：变量拼 URL、|| 默认值、creates 重跑秒过、when 条件、未启用步骤、\n\
             outputs 捕获 → ${{ env.* }} 传递、步骤超时与重试"
                .to_string(),
        ),
        target: Some("local".to_string()),
        environment: Some("staging".to_string()),
        steps: vec![
            Step {
                creates: Some("demo/.git".to_string()),
                ..demo_step(
                    "克隆仓库（变量 + || 默认值；已克隆过则秒过）",
                    "git clone ${{ variables.REPO_URL || https://gitee.com/example/demo.git }} demo",
                )
            },
            Step {
                working_dir: Some("demo".to_string()),
                ..demo_step("拉取最新代码（working_dir 相对项目目录）", "git pull --ff-only")
            },
            Step {
                working_dir: Some("demo".to_string()),
                ..demo_step("查看工作区状态", "git status -s")
            },
            Step {
                working_dir: Some("demo".to_string()),
                condition: Some("${{ variables.NAMESPACE }} == demo-staging".to_string()),
                ..demo_step("切到测试分支（when 条件不满足则跳过）", "git switch develop")
            },
            Step {
                working_dir: Some("demo".to_string()),
                continue_on_error: true,
                ..demo_step(
                    "提交改动（continue_on_error：失败只黄灯不拦后续）",
                    "git add -A && git commit -m demo",
                )
            },
            Step {
                outputs: [("TAG".to_string(), r"TAG=(\S+)".to_string())]
                    .into_iter()
                    .collect(),
                ..demo_step("生成版本号（outputs：正则捕获 stdout）", "cmd /C echo TAG=v1.0.1")
            },
            demo_step("引用上一步捕获的变量", "echo 本次版本 = ${{ env.TAG }}"),
            Step {
                timeout: Some(60),
                retry: Some(RetryConfig { max_attempts: 3, backoff_seconds: 5 }),
                ..demo_step("推送（超时 60s + 失败重试 3 次）", "git push origin HEAD")
            },
            Step {
                enabled: false,
                ..demo_step("未启用状态演示（enabled: false）", "git switch -c feature/demo")
            },
        ],
        group: Some("本地构建".to_string()),
        ..Default::default()
    };

    // ── ② 构建合集：前置检查 / 大超时 / creates / 步骤 env / pack / handlers ──
    let build_job = Job {
        name: "② 构建合集".to_string(),
        project: "example".to_string(),
        description: Some(
            "演示：前置检查、大步骤超时、creates 守卫、条件路线选择、步骤 env、\n\
             pack 打包（--exclude）、notify → handlers（变更才收尾）"
                .to_string(),
        ),
        target: Some("local".to_string()),
        environment: Some("staging".to_string()),
        preconditions: vec![Precondition {
            check: "java -version".to_string(),
            message: "本机未找到 java（工具版本门禁见 project.yaml 的 tools_require）".to_string(),
        }],
        steps: vec![
            Step {
                timeout: Some(1800),
                creates: Some("target/*.jar".to_string()),
                notify: vec!["归档产物".to_string()],
                ..demo_step(
                    "Maven 构建（超时 30 分钟；产物已存在则跳过；完成后通知 handler）",
                    "mvn -B clean package -DskipTests",
                )
            },
            Step {
                condition: Some("${{ variables.BUILD_TOOL }} == gradle".to_string()),
                ..demo_step(
                    "Gradle 构建（BUILD_TOOL 未定义 → 条件不满足，演示跳过）",
                    "gradle clean bootJar",
                )
            },
            Step {
                timeout: Some(600),
                creates: Some("node_modules".to_string()),
                ..demo_step(
                    "前端安装依赖（工程声明 packageManager 时自动走 corepack）",
                    "pnpm install",
                )
            },
            Step {
                creates: Some("dist".to_string()),
                ..demo_step("前端构建", "pnpm build")
            },
            Step {
                env: [("BUILD_MODE".to_string(), "release".to_string())]
                    .into_iter()
                    .collect(),
                ..demo_step("步骤环境变量（步骤 env 注入）", "echo 构建模式 = %BUILD_MODE%")
            },
            Step {
                outputs: [("JAR".to_string(), r"JAR=(\S+\.jar)".to_string())]
                    .into_iter()
                    .collect(),
                ..demo_step(
                    "捕获产物文件名（outputs）",
                    r"cmd /C for %f in (target\*.jar) do @echo JAR=%f",
                )
            },
            demo_step("引用捕获的产物路径", "echo 产物 = ${{ env.JAR }}"),
            Step {
                step_type: StepType::Pack,
                ..demo_step(
                    "打包产物（pack 步骤；--exclude 排除 *.original）",
                    "target -> release/app-${{ now.date }}.zip --exclude *.original",
                )
            },
        ],
        handlers: vec![demo_step(
            "归档产物（handler：仅 Maven 真的构建了才执行）",
            r"cmd /C if not exist backup mkdir backup && copy target\*.jar backup\",
        )],
        group: Some("本地构建".to_string()),
        ..Default::default()
    };

    // ── ③ Linux 远端合集：远端命令 / 上传下载 / wait 三模式 / 危险命令提醒 ──
    let linux_job = Job {
        name: "③ Linux 远端合集".to_string(),
        project: "example".to_string(),
        description: Some(
            "演示：远端命令、targets.* 变量、upload（未变更自动跳过）、pack 后上传、\n\
             wait 三种探测（tcp / http / command）、download、危险命令 ⚠ 提醒。\n\
             demo-server 是占位目标（192.0.2.10）：先在 🎯 目标管理 里改成真实机器"
                .to_string(),
        ),
        target: Some("demo-server".to_string()),
        environment: Some("staging".to_string()),
        steps: vec![
            demo_step("远端系统信息", "uname -sr && nproc && free -h"),
            demo_step(
                "磁盘与发布目录准备（targets.* 变量取目标字段）",
                "df -h / && mkdir -p ${{ targets.demo-server.work_root }}/release",
            ),
            demo_step(
                "命令里用变量（staging 环境）",
                "echo 发布到 ${{ variables.NAMESPACE }} 的 ${{ variables.APP_NAME }}",
            ),
            Step {
                step_type: StepType::Upload,
                ..demo_step(
                    "上传单文件（sha256 未变更自动跳过传输）",
                    "target/app.jar -> release/app.jar",
                )
            },
            Step {
                step_type: StepType::Pack,
                ..demo_step(
                    "先打包目录（大量小文件先 pack 再传，快得多）",
                    "src -> upload/src.zip --exclude node_modules",
                )
            },
            Step {
                step_type: StepType::Upload,
                ..demo_step(
                    "上传压缩包（文件名带构建时间 ${{ now.date }}）",
                    "upload/src.zip -> release/src-${{ now.date }}.zip",
                )
            },
            demo_step(
                "远端解压",
                "cd ${{ targets.demo-server.work_root }}/release && unzip -o src-*.zip -d src",
            ),
            Step {
                step_type: StepType::Wait,
                wait: Some(WaitProbe {
                    mode: WaitMode::Tcp,
                    target: "${{ targets.demo-server.host }}:8080".to_string(),
                    interval_seconds: 5,
                }),
                ..demo_step("等待端口就绪（wait tcp，从发版机探测）", "")
            },
            Step {
                step_type: StepType::Wait,
                wait: Some(WaitProbe {
                    mode: WaitMode::Http,
                    target: "http://${{ targets.demo-server.host }}:8080/health".to_string(),
                    interval_seconds: 5,
                }),
                ..demo_step("HTTP 健康探测（wait http，2xx 即就绪）", "")
            },
            Step {
                step_type: StepType::Wait,
                wait: Some(WaitProbe {
                    mode: WaitMode::Command,
                    target: "curl -sf http://127.0.0.1:8080/health".to_string(),
                    interval_seconds: 10,
                }),
                ..demo_step("命令探测（wait command，在目标机上执行）", "")
            },
            Step {
                step_type: StepType::Download,
                ..demo_step(
                    "下载远端日志（download + sha256 校验）",
                    "logs/app.log -> downloaded/app.log",
                )
            },
            demo_step("危险命令演示（执行前与预览都会 ⚠ 提醒）", "rm -rf /tmp/demo-cache"),
        ],
        group: Some("远端发布".to_string()),
        ..Default::default()
    };

    // ── ④ 发布合集：确认门 / 前置检查 / 备份 / 重试 / notify / 等待健康 ──
    let deploy_job = Job {
        name: "④ 发布合集".to_string(),
        project: "example".to_string(),
        description: Some(
            "演示：人工确认门（运行前输入「发布」）、前置检查、作业级超时、\n\
             now 时间戳备份、continue_on_error 容错、上传真变更才触发 handler 清理"
                .to_string(),
        ),
        target: Some("demo-server".to_string()),
        environment: Some("prod".to_string()),
        timeout: Some(600),
        confirm: Some("发布".to_string()),
        preconditions: vec![
            Precondition {
                check: r"cmd /C if exist target\app.jar (exit 0) else (exit 1)".to_string(),
                message: "构建产物 target/app.jar 不存在——先跑「② 构建合集」".to_string(),
            },
            Precondition {
                check: "git --version".to_string(),
                message: "发版机缺少 git".to_string(),
            },
        ],
        steps: vec![
            demo_step(
                "备份当前版本（${{ now.full }} 做时间戳后缀）",
                "cp -a /opt/demo/app.jar /opt/demo/backup/app-${{ now.full }}.jar",
            ),
            Step {
                continue_on_error: true,
                ..demo_step(
                    "停止旧服务（首次部署没有服务也不拦）",
                    "systemctl stop demo-app",
                )
            },
            Step {
                step_type: StepType::Upload,
                notify: vec!["清理旧备份".to_string()],
                ..demo_step(
                    "上传新版本（内容未变更自动跳过 → 不触发 handler）",
                    "target/app.jar -> /opt/demo/app.jar",
                )
            },
            Step {
                timeout: Some(120),
                retry: Some(RetryConfig { max_attempts: 3, backoff_seconds: 5 }),
                ..demo_step("启动服务（失败重试 3 次）", "systemctl start demo-app")
            },
            Step {
                step_type: StepType::Wait,
                timeout: Some(120),
                wait: Some(WaitProbe {
                    mode: WaitMode::Tcp,
                    target: "${{ targets.demo-server.host }}:8080".to_string(),
                    interval_seconds: 5,
                }),
                ..demo_step("等待健康检查通过（总限 2 分钟）", "")
            },
            demo_step("验证运行版本", "curl -s http://127.0.0.1:8080/version"),
        ],
        handlers: vec![demo_step(
            "清理旧备份（只保留最近 5 份；仅上传真的传了新内容才执行）",
            "ls -t /opt/demo/backup/app-*.jar | tail -n +6 | xargs -r rm",
        )],
        group: Some("远端发布".to_string()),
        ..Default::default()
    };

    for (file, job) in [
        ("git.yaml", git_job),
        ("build.yaml", build_job),
        ("linux.yaml", linux_job),
        ("deploy.yaml", deploy_job),
    ] {
        crate::storage::files::save_yaml(&dir.join(file), &job)?;
    }
    Ok(())
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

        // ── 示范工程：四本合集生成、可解析、能被发现 ──
        create_example_project().unwrap();
        let ex_dir = projects_dir().join("example");
        let p: Project =
            crate::storage::files::load_yaml(&ex_dir.join("project.yaml")).unwrap();
        assert!(p.targets.contains_key("local"));
        assert!(p.targets.contains_key("demo-server"), "远端合集占位目标");
        let mut names = Vec::new();
        for f in ["git.yaml", "build.yaml", "linux.yaml", "deploy.yaml"] {
            let j: Job = crate::storage::files::load_yaml(&ex_dir.join(f)).unwrap();
            assert!(!j.steps.is_empty(), "{f} 应有示范步骤");
            names.push(j.name);
        }
        assert_eq!(p.jobs, names, "project.yaml 的 jobs 列表与合集文件一致");
        // 分组演示：git/build → 本地构建；linux/deploy → 远端发布（YAML 往返保真）
        assert_eq!(p.groups, vec!["本地构建".to_string(), "远端发布".to_string()]);
        let g: Job = crate::storage::files::load_yaml(&ex_dir.join("git.yaml")).unwrap();
        assert_eq!(g.group.as_deref(), Some("本地构建"));
        let d: Job = crate::storage::files::load_yaml(&ex_dir.join("deploy.yaml")).unwrap();
        assert_eq!(d.group.as_deref(), Some("远端发布"));

        // 干跑预览 Linux 合集：占位目标/变量应全部解析（无「未定义」警告），
        // upload 相对路径应拼上 demo-server 的 work_root /opt/demo
        let linux_job: Job =
            crate::storage::files::load_yaml(&ex_dir.join("linux.yaml")).unwrap();
        let input = crate::engine::RunInput {
            project: p.clone(),
            job: linux_job,
            tools: HashMap::new(),
            all_targets: effective_targets(&p),
            from_step: 0,
            only_step: None,
            run_id: String::new(),
            default_step_timeout: crate::engine::DEFAULT_STEP_TIMEOUT,
            cancel_tx: None,
        };
        let previews = crate::engine::preview_job(&input);
        for pv in &previews {
            for w in &pv.warnings {
                assert!(!w.contains("未定义"), "示范步骤不应有未定义引用: {w}");
            }
        }
        let upload = previews
            .iter()
            .flat_map(|pv| pv.uploads.iter())
            .find(|(l, _, _)| l.ends_with("app.jar"))
            .expect("应有 app.jar 上传示范");
        assert!(upload.1.starts_with("/opt/demo/"), "相对远端路径拼 work_root: {}", upload.1);

        let _ = std::fs::remove_dir_all(&tmp);
        std::env::remove_var("REMOTE_BUILD_HOME");
    }
}
