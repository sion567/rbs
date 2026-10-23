//! 执行引擎（设计文档 §4.3 主循环 / §12 并发与取消 / §13 错误处理与重试）。
//!
//! 流程：解析变量 → 构造 ExecutionContext → 逐步执行（enabled/condition 过滤、
//! 按 step_type 分发、超时/重试/取消包裹）→ LogEvent 流式输出 → on_failure 策略。

use crate::models::*;
use crate::transport::{self, local, LogHandle, RemoteTransport};
use crate::variables::{eval_condition, resolve, VarContext};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, mpsc};

/// 单步默认超时（秒，§3.3）
pub const DEFAULT_STEP_TIMEOUT: u64 = 300;

/// 一次运行的输入
#[derive(Default)]
pub struct RunInput {
    pub project: Project,
    pub job: Job,
    pub tools: HashMap<String, Tool>,
    /// 全部 targets（供 ${{ targets.x.y }} 解析）
    pub all_targets: HashMap<String, Target>,
    /// 从第几步开始执行（0 起；默认 0 = 全部）。
    /// 注意：跳过的前面步骤其产出变量（workspace/.env 约定）不可用。
    pub from_step: usize,
    /// 只执行这一步（调试用；None = 按顺序执行）
    pub only_step: Option<usize>,
    /// 本次运行唯一标识（与日志文件同名），供 ${{ run.id }}；空串 = 未设置
    pub run_id: String,
}

pub struct Engine;

impl Engine {
    pub async fn run_job(
        &self,
        input: RunInput,
        mut cancel_rx: broadcast::Receiver<()>,
        log_tx: mpsc::Sender<LogEvent>,
    ) -> JobStatus {
        // JobEnd 事件由外层包装任务统一发送（见 app.rs），引擎只返回最终状态

        // ── 解析变量（§4.3 第一步）────────────────────────────────────────
        let mut ctx = build_var_context(&input);

        // 工作目录解析（§4.5）
        let workspace = PathBuf::from(&input.project.local_path);
        let job_wd = resolve_job_working_dir(&input.job, &workspace);

        // 整个 job 的取消信号由本任务独享消费：步骤间也检查
        let mut cancelled = false;

        // §6.5：项目级 tools 覆盖全局 tools.yaml（键相同整体覆盖）
        let mut merged_tools = input.tools.clone();
        merged_tools.extend(input.project.tools.clone());

        // 前端工程：package.json 声明了 packageManager（钉版）时，
        // 本地步骤里的 pnpm/yarn 自动经 corepack 执行（用户无需知道 corepack 存在）
        let pinned_frontend = {
            let pj = workspace.join("package.json");
            std::fs::read_to_string(&pj)
                .map(|t| t.contains("\"packageManager\""))
                .unwrap_or(false)
        };
        let corepack_ok = pinned_frontend && corepack_available();

        for (index, step) in input.job.steps.iter().enumerate() {
            // 范围执行：跳过起始之前的步骤；仅此步模式跳过其他步骤（不产生事件）
            if index < input.from_step {
                continue;
            }
            if let Some(only) = input.only_step {
                if index != only {
                    continue;
                }
            }

            // 提前检查 job 级取消（上一步结束后收到信号）
            if matches!(cancel_rx.try_recv(), Ok(()) | Err(broadcast::error::TryRecvError::Closed)) {
                cancelled = true;
            }
            if cancelled {
                return JobStatus::Cancelled;
            }

            let step_name = step
                .name
                .clone()
                .unwrap_or_else(|| step.command.lines().next().unwrap_or("").to_string());

            if !step.enabled {
                let _ = log_tx
                    .send(LogEvent::StepStart {
                        step_index: index,
                        name: format!("{step_name}（跳过：未启用）"),
                        skipped: true,
                    })
                    .await;
                let _ = log_tx
                    .send(LogEvent::StepEnd { step_index: index, exit_code: 0, duration_ms: 0 })
                    .await;
                continue;
            }

            // condition 求值（§13.3，P0 只支持非空检查；无法解析时 fail-open + 告警）
            if let Some(cond) = &step.condition {
                match eval_condition(cond, &ctx) {
                    Some(false) => {
                        let _ = log_tx
                            .send(LogEvent::StepStart {
                                step_index: index,
                                name: format!("{step_name}（跳过：condition 不满足）"),
                                skipped: true,
                            })
                            .await;
                        let _ = log_tx
                            .send(LogEvent::StepEnd { step_index: index, exit_code: 0, duration_ms: 0 })
                            .await;
                        continue;
                    }
                    None => {
                        let _ = log_tx
                            .send(LogEvent::Error {
                                message: format!(
                                    "步骤 {index} condition 无法解析（P0 仅支持非空检查），按真值执行: {cond}"
                                ),
                            })
                            .await;
                    }
                    Some(true) => {}
                }
            }

            let _ = log_tx
                .send(LogEvent::StepStart { step_index: index, name: step_name, skipped: false })
                .await;

            // ── 有效目标（§3.3）：步骤级 target 覆盖流程级默认 ──
            // 混合流程的关键：下载/编译用 local，上传/发布用 prod，各步各选
            let step_ctx: Result<
                (Option<Target>, Arc<dyn transport::RemoteTransport>, String),
                String,
            > = (|| {
                let eff_name = step.target.clone().or_else(|| input.job.target.clone());
                let eff_target = match eff_name.as_deref() {
                    Some(n) => {
                        let t = input.all_targets.get(n).ok_or_else(|| {
                            format!(
                                "目标「{n}」未定义（请在 project.yaml 的 targets 表或 🎯 目标管理 中配置）"
                            )
                        })?;
                        let mut t = t.clone();
                        if t.name.is_empty() {
                            t.name = n.to_string();
                        }
                        Some(t)
                    }
                    None => None,
                };
                let transport = transport::make_transport(eff_target.as_ref())
                    .map_err(|e| e.to_string())?;
                // 远端根目录随有效目标走（上传相对路径 / 远端 cd 的基准）
                let remote_base = eff_target
                    .as_ref()
                    .and_then(|t| t.work_root.clone())
                    .unwrap_or_default();
                Ok((eff_target, transport, remote_base))
            })();

            let result = match step_ctx {
                Err(reason) => StepResult::Failed { exit_code: -1, duration_ms: 0, reason },
                Ok((eff_target, transport, remote_base)) => {
                    // 变量替换（§6.3 执行前一次性替换）：命令 / 工作目录 / 步骤 env
                    let mut command = resolve(&step.command, &ctx);

                    // 自动 corepack（§6.5）：仅本地步骤、仅命令以 pnpm/yarn 开头时包装
                    if pinned_frontend
                        && corepack_ok
                        && matches!(step.step_type, StepType::Exec | StepType::K8sApply)
                        && !is_remote(eff_target.as_ref())
                    {
                        let wrapped = auto_corepack_wrap(&command);
                        if wrapped != command {
                            let _ = log_tx
                                .send(LogEvent::Stdout {
                                    step_index: index,
                                    line: "[corepack] 检测到 package.json 的 packageManager 钉版，\
                                           pnpm/yarn 命令自动经 corepack 执行"
                                        .to_string(),
                                })
                                .await;
                            command = wrapped;
                        }
                    }
                    let step_local_wd = resolve_step_working_dir(step, &job_wd);
                    let remote_dir = match &step.working_dir {
                        Some(wd) if !remote_base.is_empty() => {
                            format!("{remote_base}/{}", wd.trim_matches('/'))
                        }
                        Some(wd) => wd.clone(),
                        None => remote_base,
                    };

                    // §6.1 优先级：步骤级 env > 工具 env > target env
                    let mut env_map: HashMap<String, String> = eff_target
                        .as_ref()
                        .map(|t| t.env.clone())
                        .unwrap_or_default();
                    if let Some(tool) = merged_tools.get(&step.tool) {
                        env_map.extend(tool.env.clone());
                    }
                    for (k, v) in &step.env {
                        env_map.insert(k.clone(), resolve(v, &ctx));
                    }

                    // 工具地址落地：tools.yaml / 项目级 tools 的 command 若是完整路径，
                    // 把其所在目录前置到步骤 PATH——命令里的裸名（mvn/node/git…）
                    // 优先解析到这份映射，而不是系统 PATH 里的任意版本（§6.5）
                    if let Some(tool) = merged_tools.get(&step.tool) {
                        let is_path_like =
                            tool.command.contains('\\') || tool.command.contains('/');
                        if is_path_like {
                            if let Some(parent) = Path::new(&tool.command).parent() {
                                if parent.exists() {
                                    env_map = crate::transport::with_path_prepended(
                                        env_map, parent,
                                    );
                                }
                            }
                        }
                    }

                    // 前端工程：node_modules/.bin 前置（vite/jest/tsc 等工程内命令零配置可用）；
                    // 放在工具目录之后，保证工程内命令优先于全局安装
                    let project_bin = workspace.join("node_modules").join(".bin");
                    if project_bin.exists() {
                        env_map = crate::transport::with_path_prepended(env_map, &project_bin);
                    }

                    // k8s target：注入 KUBECONFIG（§8.4 走本地 kubectl）
                    if let Some(TargetType::Kubernetes { kubeconfig, .. }) =
                        eff_target.as_ref().map(|t| &t.target_type)
                    {
                        env_map.insert("KUBECONFIG".to_string(), kubeconfig.clone());
                    }

                    let step_timeout =
                        Duration::from_secs(step.timeout.unwrap_or(DEFAULT_STEP_TIMEOUT));
                    run_step_with_retry(
                        &transport,
                        eff_target.as_ref(),
                        step,
                        &command,
                        &step_local_wd,
                        &remote_dir,
                        &env_map,
                        &workspace,
                        step_timeout,
                        &mut cancel_rx,
                        &log_tx,
                        index,
                    )
                    .await
                }
            };

            match result {
                StepResult::Success { exit_code, duration_ms, output_vars } => {
                    let _ = log_tx
                        .send(LogEvent::StepEnd { step_index: index, exit_code, duration_ms })
                        .await;
                    // §6.4 步骤间变量传递：P0 文件约定（workspace/.env）
                    ctx.env.extend(output_vars);
                }
                StepResult::Cancelled => return JobStatus::Cancelled,
                StepResult::Timeout => {
                    return JobStatus::Timeout;
                }
                StepResult::Failed { exit_code, duration_ms, reason } => {
                    let _ = log_tx
                        .send(LogEvent::StepEnd { step_index: index, exit_code, duration_ms })
                        .await;
                    if step.continue_on_error {
                        continue;
                    }
                    match &input.job.on_failure {
                        FailureAction::Continue => continue,
                        // job 级 retry 为 P1（§13.2），P0 按 stop 处理
                        FailureAction::Stop | FailureAction::Retry => {
                            return JobStatus::Failed { step_index: index, reason };
                        }
                    }
                }
            }
        }

        // JobEnd 事件由外层包装任务统一发送（见 app.rs），引擎只返回最终状态
        JobStatus::Success
    }
}

// ---------------------------------------------------------------------------
// 变量上下文（§4.1 ExecutionContext 的变量部分）
// ---------------------------------------------------------------------------

fn build_var_context(input: &RunInput) -> VarContext {
    let mut variables = HashMap::new();
    if let Some(env_name) = &input.job.environment {
        if let Some(evs) = input.project.variables.get(env_name) {
            variables.extend(evs.vars.clone());
        }
    }
    VarContext {
        variables,
        env: HashMap::new(),
        targets: input.all_targets.clone(),
        run_id: (!input.run_id.is_empty()).then(|| input.run_id.clone()),
        // 内置时间变量在此冻结：一次运行所有步骤同值（备份后缀等场景）
        now: crate::variables::BuiltinNow::generate(),
    }
}

pub fn resolve_job_working_dir(job: &Job, workspace: &Path) -> PathBuf {
    match &job.working_dir {
        Some(wd) => workspace.join(wd),
        None => workspace.to_path_buf(),
    }
}

pub fn resolve_step_working_dir(step: &Step, job_wd: &Path) -> PathBuf {
    match &step.working_dir {
        Some(wd) => job_wd.join(wd),
        None => job_wd.to_path_buf(),
    }
}

// ---------------------------------------------------------------------------
// 单步执行：retry + timeout + cancel 包裹（§4.3）
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
async fn run_step_with_retry(
    transport: &Arc<dyn RemoteTransport>,
    target: Option<&Target>,
    step: &Step,
    command: &str,
    local_wd: &Path,
    remote_dir: &str,
    env_map: &HashMap<String, String>,
    workspace: &Path,
    step_timeout: Duration,
    cancel_rx: &mut broadcast::Receiver<()>,
    log_tx: &mpsc::Sender<LogEvent>,
    index: usize,
) -> StepResult {
    let log = LogHandle::new(log_tx.clone(), index);
    // 重试计划：显式 retry 配置优先；upload 步骤无配置时默认重试 1 次（网络抖动兜底）
    let (attempts, backoff_secs) = step_retry_plan(step);
    let backoff = Duration::from_secs(backoff_secs);
    // 恢复执行时补发 StepStart 的名字（与主循环同一推导：优先自定义名，否则命令首行）
    let step_name = step
        .name
        .clone()
        .unwrap_or_else(|| step.command.lines().next().unwrap_or("").to_string());

    let deadline = tokio::time::Instant::now() + step_timeout;

    let mut attempt = 0;
    loop {
        // 取消语义：fut 内部（子进程 wait / 上传）select cancel 并 kill；
        // 这里只负责超时包裹与重试间隔。fut 独占借用 cancel_rx，故外层不可再 select。
        let fut = execute_step(
            transport,
            target,
            step,
            command,
            local_wd,
            remote_dir,
            env_map,
            workspace,
            &log,
            cancel_rx,
        );

        let result = {
            // deadline 是绝对时刻，tokio::time::timeout 需要 Duration
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            match tokio::time::timeout(remaining, fut).await {
                Ok(inner) => inner,
                Err(_) => return StepResult::Timeout,
            }
        };

        match result {
            ok @ (StepResult::Success { .. } | StepResult::Cancelled | StepResult::Timeout) => {
                return ok;
            }
            StepResult::Failed { exit_code, duration_ms, reason } => {
                attempt += 1;
                if attempt >= attempts {
                    return StepResult::Failed { exit_code, duration_ms, reason };
                }
                let _ = log_tx
                    .send(LogEvent::Error {
                        message: format!(
                            "步骤 {index} 第 {attempt} 次尝试失败，{backoff:?} 后重试: {reason}"
                        ),
                    })
                    .await;
                // 退避等待期：UI 把该步骤切到黄色等待态（恢复执行时由下方 StepStart 切回）
                let _ = log_tx
                    .send(LogEvent::StepRetryWait { step_index: index, attempt })
                    .await;
                tokio::select! {
                    _ = tokio::time::sleep(backoff) => {}
                    _ = cancel_rx.recv() => return StepResult::Cancelled,
                }
                let _ = log_tx
                    .send(LogEvent::StepStart {
                        step_index: index,
                        name: format!("{step_name}（第 {attempt} 次重试）"),
                        skipped: false,
                    })
                    .await;
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn execute_step(
    transport: &Arc<dyn RemoteTransport>,
    target: Option<&Target>,
    step: &Step,
    command: &str,
    local_wd: &Path,
    remote_dir: &str,
    env_map: &HashMap<String, String>,
    workspace: &Path,
    log: &LogHandle,
    cancel_rx: &mut broadcast::Receiver<()>,
) -> StepResult {
    let started = Instant::now();

    let outcome: anyhow::Result<transport::ExitStatus> = match step.step_type {
        StepType::Exec => {
            // 远端步骤使用远端目录，本地步骤使用本地解析目录
            let wd = if is_remote(target) { remote_dir.to_string() } else { local_wd.to_string_lossy().to_string() };
            transport.exec(target_dummy(target), command, &wd, env_map, log, cancel_rx).await
        }
        StepType::Wait => {
            // 轮询执行 check command 直到成功或超时（超时在外层包裹）
            let wd = if is_remote(target) { remote_dir.to_string() } else { local_wd.to_string_lossy().to_string() };
            let mut attempt = 0;
            loop {
                attempt += 1;
                log.line(
                    transport::Stream::Stdout,
                    format!("[wait] 第 {attempt} 次探测: {command}"),
                );
                match transport
                    .exec(target_dummy(target), command, &wd, env_map, log, cancel_rx)
                    .await
                {
                    Ok(st) if st.success => break Ok(st),
                    // 探测失败不算步骤失败，等待后重试；取消要透传
                    Ok(_) => {}
                    Err(e) if local::is_cancelled(&e) => break Err(e),
                    Err(e) => {
                        log.line(transport::Stream::Stderr, format!("[wait] 探测失败: {e}"));
                    }
                }
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(transport::WAIT_POLL_SECONDS)) => {}
                    _ = cancel_rx.recv() => {
                        break Err(anyhow::anyhow!("__cancelled__"));
                    }
                }
            }
        }
        StepType::Upload => {
            // 命令格式约定：<本地路径或目录> -> <远端路径>；成功统一走下方 Ok 分支
            upload_step(transport, target, command, workspace, remote_dir, log, cancel_rx).await
        }
        StepType::Pack => {
            // 命令格式约定：<目录或通配> -> <目标.zip>（本地操作，§5.8）
            pack_step(command, workspace, log).await
        }
        StepType::K8sApply => {
            // §8.3 P0 简化：执行 kubectl 命令（本地 + KUBECONFIG 注入在引擎层完成）；
            // k8s_watch 轮询为 P1.5
            if step.k8s_watch.is_some() {
                log.line(
                    transport::Stream::Stderr,
                    "[k8s] k8s_watch 轮询为 P1.5 功能，P0 仅执行 apply 命令",
                );
            }
            transport
                .exec(target_dummy(target), command, remote_dir, env_map, log, cancel_rx)
                .await
        }
    };

    let duration_ms = started.elapsed().as_millis() as u64;

    match outcome {
        Ok(status) if status.success => {
            let output_vars = capture_env_file(workspace);
            StepResult::Success { exit_code: status.code, duration_ms, output_vars }
        }
        Ok(status) => StepResult::Failed {
            exit_code: status.code,
            duration_ms,
            reason: format!("退出码 {}", status.code),
        },
        Err(e) => {
            if local::is_cancelled(&e) {
                StepResult::Cancelled
            } else {
                StepResult::Failed { exit_code: -1, duration_ms, reason: e.to_string() }
            }
        }
    }
}

fn is_remote(target: Option<&Target>) -> bool {
    matches!(target.map(|t| &t.target_type), Some(TargetType::Ssh { .. }))
}

/// 步骤重试计划（次数, 退避秒）：
/// 显式 retry 配置优先；未配置时 upload 默认 2 次 / 3s（网络抖动兜底），其余 1 次 / 0s。
fn step_retry_plan(step: &Step) -> (u32, u64) {
    match &step.retry {
        Some(r) => (r.max_attempts.max(1), r.backoff_seconds),
        None if step.step_type == StepType::Upload => (2, 3),
        None => (1, 0),
    }
}

/// 人类可读的文件大小
fn human_size(bytes: u64) -> String {
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.2} GB", bytes as f64 / 1024.0 / 1024.0 / 1024.0)
    } else if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / 1024.0 / 1024.0)
    } else {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    }
}

/// 前端工程自动 corepack（§6.5）：命令里以 pnpm / yarn 开头的行自动加 corepack 前缀，
/// 其余行原样保留（保留原始缩进）。pnpm.cmd / yarn.cmd 同样识别；
/// 用户已手写 corepack 前缀的行不会二次包装。
fn auto_corepack_wrap(command: &str) -> String {
    command
        .lines()
        .map(|line| {
            let trimmed = line.trim_start();
            let first = trimmed.split_whitespace().next().unwrap_or("");
            let bare = first.strip_suffix(".cmd").unwrap_or(first);
            if matches!(bare, "pnpm" | "yarn") {
                let indent = &line[..line.len() - trimmed.len()];
                format!("{indent}corepack {trimmed}")
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// corepack 是否可用（每个 job 只探测一次；node 16.10+ 自带）
fn corepack_available() -> bool {
    let mut cmd = std::process::Command::new("corepack");
    cmd.arg("--version")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    cmd.spawn()
        .and_then(|c| c.wait_with_output())
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// pack 步骤：`<目录或通配> -> <目标.zip>`，路径相对 workspace 解析
async fn pack_step(
    command: &str,
    workspace: &Path,
    log: &LogHandle,
) -> anyhow::Result<transport::ExitStatus> {
    let (src_part, dst_part) = command.split_once("->").ok_or_else(|| {
        anyhow::anyhow!("pack 步骤命令格式应为: <目录或通配> -> <目标.zip>，实际: {command}")
    })?;
    let resolve = |p: &str| {
        let path = Path::new(p);
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            workspace.join(path)
        }
    };
    let src = resolve(src_part.trim());
    let dst = resolve(dst_part.trim());
    if !dst.to_string_lossy().to_ascii_lowercase().ends_with(".zip") {
        anyhow::bail!("pack 目标必须是 .zip 文件: {}", dst.display());
    }

    log.line(
        transport::Stream::Stdout,
        format!("[pack] {} -> {}", src.display(), dst.display()),
    );
    let (count, bytes) = crate::pack::pack(&src, &dst)?;
    let size = human_size(bytes);
    log.line(
        transport::Stream::Stdout,
        format!("[pack] 完成：{count} 个文件，{size}"),
    );
    Ok(transport::ExitStatus::from_code(0))
}

/// trait 要求 &Target；None 时给本地占位 target
fn target_dummy(target: Option<&Target>) -> &Target {
    static LOCAL: std::sync::OnceLock<Target> = std::sync::OnceLock::new();
    target.unwrap_or_else(|| LOCAL.get_or_init(crate::config::local_target))
}

/// 解析 upload 命令：每行一条映射 `<本地路径> -> <远端路径>`（空行与 # 注释忽略）。
/// 微服务多 jar 场景在一个步骤里写多条映射即可。
fn parse_upload_mappings(command: &str) -> anyhow::Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    for line in command
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
    {
        let Some((l, r)) = line.split_once("->") else {
            anyhow::bail!("upload 映射格式应为: <本地路径> -> <远端路径>，实际行: {line}");
        };
        let (l, r) = (l.trim(), r.trim());
        // 空映射行（UI 编辑器加行的 "-> " 占位）与空白行一样跳过
        if l.is_empty() && r.is_empty() {
            continue;
        }
        out.push((l.to_string(), r.to_string()));
    }
    if out.is_empty() {
        anyhow::bail!("upload 命令为空；每行一条映射：<本地路径> -> <远端路径>");
    }
    Ok(out)
}

async fn upload_step(
    transport: &Arc<dyn RemoteTransport>,
    target: Option<&Target>,
    command: &str,
    workspace: &Path,
    remote_dir: &str,
    log: &LogHandle,
    cancel_rx: &mut broadcast::Receiver<()>,
) -> anyhow::Result<transport::ExitStatus> {
    if !is_remote(target) {
        anyhow::bail!("step_type: upload 需要 SSH target，当前 target 不支持上传");
    }
    let target = target.expect("checked above");
    let mappings = parse_upload_mappings(command)?;
    let total = mappings.len();

    for (i, (local_part, remote_part)) in mappings.iter().enumerate() {
        let local_path = {
            let p = Path::new(local_part);
            if p.is_absolute() {
                p.to_path_buf()
            } else {
                workspace.join(p)
            }
        };
        let remote_path = if remote_part.starts_with('/') {
            remote_part.clone()
        } else if remote_dir.is_empty() {
            remote_part.clone()
        } else {
            format!("{}/{}", remote_dir.trim_end_matches('/'), remote_part)
        };
        if !local_path.exists() {
            anyhow::bail!("上传源不存在: {}", local_path.display());
        }
        log.line(
            transport::Stream::Stdout,
            format!("[upload {}/{}] {} -> {}", i + 1, total, local_path.display(), remote_path),
        );
        if local_path.is_file() {
            let bytes = std::fs::metadata(&local_path).map(|m| m.len()).unwrap_or(0);
            log.line(
                transport::Stream::Stdout,
                format!("[upload {}/{}] 文件大小 {}", i + 1, total, human_size(bytes)),
            );
        }
        transport
            .upload(target, &local_path, &remote_path, log, cancel_rx)
            .await?;

        // 上传完整性校验（§5.9）：单文件比对本地/远端 sha256；
        // 目录递归上传跳过（建议先用 pack 打成单文件）。校验失败 → 步骤失败 → 按重试配置重传。
        if local_path.is_file() {
            log.line(
                transport::Stream::Stdout,
                format!("[verify] 校验远端 sha256: {}", remote_path),
            );
            let local_hash = crate::transport::putty::sha256_file(&local_path)?;
            let remote_hash = crate::transport::putty::remote_sha256(target, &remote_path)?;
            if remote_hash != local_hash {
                anyhow::bail!(
                    "上传校验失败：sha256 不一致（本地 {local_hash} / 远端 {remote_hash}）"
                );
            }
            log.line(
                transport::Stream::Stdout,
                format!("[verify] sha256 一致 ✓ ({})", &local_hash[..12]),
            );
        } else {
            log.line(
                transport::Stream::Stderr,
                "[verify] 目录上传跳过校验（建议先用 pack 打成单文件再上传）",
            );
        }
    }
    Ok(transport::ExitStatus::from_code(0))
}

/// §6.4 P0 变量传递约定：步骤成功后读取 workspace/.env 的 KEY=VALUE 行
fn capture_env_file(workspace: &Path) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let path = workspace.join(".env");
    let Ok(text) = std::fs::read_to_string(path) else {
        return out;
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            out.insert(k.trim().to_string(), v.trim().to_string());
        }
    }
    out
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::EnvironmentVariables;

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Runtime::new().unwrap()
    }

    fn simple_job(steps: Vec<Step>) -> RunInput {
        RunInput {
            project: Project {
                name: "p".into(),
                local_path: std::env::temp_dir().to_string_lossy().to_string(),
                variables: [(
                    "staging".to_string(),
                    EnvironmentVariables {
                        vars: [("GREETING".to_string(), "hi".to_string())].into_iter().collect(),
                    },
                )]
                .into_iter()
                .collect(),
                ..Default::default()
            },
            job: Job {
                name: "j".into(),
                project: "p".into(),
                environment: Some("staging".into()),
                steps,
                ..Default::default()
            },
            tools: HashMap::new(),
            all_targets: HashMap::new(),
            from_step: 0,
            only_step: None,
            run_id: "test-run".into(),
        }
    }

    fn exec_step(command: &str) -> Step {
        Step {
            name: Some(command.to_string()),
            tool: String::new(),
            command: command.to_string(),
            step_type: StepType::Exec,
            enabled: true,
            ..Default::default()
        }
    }

    #[test]
    fn upload_mapping_parser_multi_line() {
        // 微服务多 jar：一个 upload 步骤多行映射；空行与 # 注释忽略
        let m = parse_upload_mappings(
            "target/user.jar -> /opt/app/user.jar\n\n# 注释行\ntarget/order.jar -> /opt/app/order.jar",
        )
        .unwrap();
        assert_eq!(m.len(), 2);
        assert_eq!(m[0], ("target/user.jar".to_string(), "/opt/app/user.jar".to_string()));
        assert_eq!(m[1], ("target/order.jar".to_string(), "/opt/app/order.jar".to_string()));

        // 缺箭头 → 明确报错
        assert!(parse_upload_mappings("target/a.jar").is_err());
        // 全空 → 明确报错
        assert!(parse_upload_mappings("  \n# only comment\n").is_err());
    }

    fn collect(status: JobStatus, rx: &mut mpsc::Receiver<LogEvent>) -> Vec<LogEvent> {
        let mut events = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            events.push(ev);
        }
        assert!(status.ok(), "job 应成功: {status:?}");
        events
    }

    #[test]
    fn runs_local_echo_job() {
        let rt = runtime();
        let (tx, mut rx) = mpsc::channel(1024);
        let (_cancel_tx, cancel_rx) = broadcast::channel(4);
        let engine = Engine;

        let input = simple_job(vec![exec_step("echo rbs_engine_test_ok")]);
        let status = rt.block_on(engine.run_job(input, cancel_rx, tx));
        let events = collect(status, &mut rx);

        assert!(events.iter().any(|e| matches!(
            e,
            LogEvent::Stdout { line, .. } if line.contains("rbs_engine_test_ok")
        )));
    }

    #[test]
    fn variable_substitution_reaches_command() {
        let rt = runtime();
        let (tx, mut rx) = mpsc::channel(1024);
        let (_cancel_tx, cancel_rx) = broadcast::channel(4);
        let engine = Engine;

        let input = simple_job(vec![exec_step("echo VAR_IS_${{ variables.GREETING }}")]);
        let status = rt.block_on(engine.run_job(input, cancel_rx, tx));
        let events = collect(status, &mut rx);

        assert!(events.iter().any(|e| matches!(
            e,
            LogEvent::Stdout { line, .. } if line.contains("VAR_IS_hi")
        )));
    }

    #[test]
    fn failing_step_stops_job() {
        let rt = runtime();
        let (tx, mut rx) = mpsc::channel(1024);
        let (_cancel_tx, cancel_rx) = broadcast::channel(4);
        let engine = Engine;

        let input = simple_job(vec![
            exec_step("cmd /C exit 3"),
            exec_step("echo should_not_run"),
        ]);
        let status = rt.block_on(engine.run_job(input, cancel_rx, tx));
        assert!(matches!(status, JobStatus::Failed { step_index: 0, .. }));

        let mut events = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            events.push(ev);
        }
        assert!(!events.iter().any(|e| matches!(
            e,
            LogEvent::Stdout { line, .. } if line.contains("should_not_run")
        )));
    }

    #[test]
    fn disabled_step_is_skipped() {
        let rt = runtime();
        let (tx, mut rx) = mpsc::channel(1024);
        let (_cancel_tx, cancel_rx) = broadcast::channel(4);
        let engine = Engine;

        let input = simple_job(vec![
            Step { enabled: false, ..exec_step("echo disabled_out") },
            exec_step("echo enabled_out"),
        ]);
        let status = rt.block_on(engine.run_job(input, cancel_rx, tx));
        let events = collect(status, &mut rx);

        assert!(!events.iter().any(|e| matches!(
            e,
            LogEvent::Stdout { line, .. } if line.contains("disabled_out")
        )));
        assert!(events.iter().any(|e| matches!(
            e,
            LogEvent::Stdout { line, .. } if line.contains("enabled_out")
        )));
    }

    #[test]
    fn env_file_convention_passes_vars() {
        // §6.4 P0：前一步写 workspace/.env，引擎捕获，后一步 ${{ env.X }} 可用
        let dir = std::env::temp_dir().join(format!("rbs_env_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let rt = runtime();
        let (tx, mut rx) = mpsc::channel(1024);
        let (_cancel_tx, cancel_rx) = broadcast::channel(4);
        let engine = Engine;

        let mut input = simple_job(vec![
            // `1>` 显式 stdout 重定向，避免 cmd 把值末尾数字当成句柄前缀
            exec_step("cmd /C echo VERSION=9.9.9 1> .env"),
            exec_step("echo GOT_${{ env.VERSION }}"),
        ]);
        input.project.local_path = dir.to_string_lossy().to_string();

        let status = rt.block_on(engine.run_job(input, cancel_rx, tx));
        let events = collect(status, &mut rx);

        assert!(events.iter().any(|e| matches!(
            e,
            LogEvent::Stdout { line, .. } if line.contains("GOT_9.9.9")
        )));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn project_tools_override_global_env() {
        // §6.5：项目级 tools 注入的工具 env 对步骤命令进程可见
        let rt = runtime();
        let (tx, mut rx) = mpsc::channel(1024);
        let (_cancel_tx, cancel_rx) = broadcast::channel(4);
        let engine = Engine;

        let step = Step {
            tool: "mytool".to_string(),
            command: "echo TOOL_%FOO%".to_string(),
            step_type: StepType::Exec,
            enabled: true,
            ..Default::default()
        };
        let mut input = simple_job(vec![step]);
        input.project.tools.insert(
            "mytool".to_string(),
            Tool {
                name: "mytool".to_string(),
                tool_type: ToolType::System,
                env: [("FOO".to_string(), "bar".to_string())].into_iter().collect(),
                ..Default::default()
            },
        );

        let status = rt.block_on(engine.run_job(input, cancel_rx, tx));
        let events = collect(status, &mut rx);
        assert!(events.iter().any(|e| matches!(
            e,
            LogEvent::Stdout { line, .. } if line.contains("TOOL_bar")
        )));
    }

    #[test]
    fn range_execution_from_and_only() {
        // 从此步：跳过之前的步骤（不产生输出），之后顺序执行
        {
            let rt = runtime();
            let (tx, mut rx) = mpsc::channel(1024);
            let (_cancel_tx, cancel_rx) = broadcast::channel(4);
            let engine = Engine;
            let mut input = simple_job(vec![
                exec_step("echo step0_out"),
                exec_step("echo step1_out"),
                exec_step("echo step2_out"),
            ]);
            input.from_step = 1;
            let status = rt.block_on(engine.run_job(input, cancel_rx, tx));
            let events = collect(status, &mut rx);
            assert!(!events.iter().any(|e| matches!(
                e, LogEvent::Stdout { line, .. } if line.contains("step0_out")
            )));
            assert!(events.iter().any(|e| matches!(
                e, LogEvent::Stdout { line, .. } if line.contains("step1_out")
            )));
            assert!(events.iter().any(|e| matches!(
                e, LogEvent::Stdout { line, .. } if line.contains("step2_out")
            )));
        }
        // 仅此步：只执行指定步骤
        {
            let rt = runtime();
            let (tx, mut rx) = mpsc::channel(1024);
            let (_cancel_tx, cancel_rx) = broadcast::channel(4);
            let engine = Engine;
            let mut input = simple_job(vec![
                exec_step("echo step0_out"),
                exec_step("echo step1_out"),
                exec_step("echo step2_out"),
            ]);
            input.only_step = Some(1);
            let status = rt.block_on(engine.run_job(input, cancel_rx, tx));
            let events = collect(status, &mut rx);
            assert!(events.iter().any(|e| matches!(
                e, LogEvent::Stdout { line, .. } if line.contains("step1_out")
            )));
            assert!(!events.iter().any(|e| matches!(
                e, LogEvent::Stdout { line, .. } if line.contains("step0_out") || line.contains("step2_out")
            )));
        }
    }

    #[test]
    fn step_target_overrides_job_target() {
        // §3.3 步骤级目标：未设置 → 跟随流程；设置 → 覆盖；未定义 → 步骤失败
        let rt = runtime();
        let (_cancel_tx, cancel_rx) = broadcast::channel(4);
        let engine = Engine;

        let mut all_targets = HashMap::new();
        all_targets.insert(
            "remote".to_string(),
            Target {
                name: "remote".to_string(),
                target_type: TargetType::Ssh {
                    host: "10.9.9.9".to_string(),
                    port: 22,
                    user: "u".to_string(),
                    auth: crate::models::AuthMethod::None,
                    host_key: None,
                },
                ..Default::default()
            },
        );

        let mut input = simple_job(vec![
            exec_step("echo S0_local"),
            {
                let mut s = exec_step("echo S1_local2");
                s.target = Some("local2".to_string());
                s
            },
            {
                let mut s = exec_step("echo S2_never");
                s.target = Some("no-such-target".to_string());
                s
            },
            exec_step("echo S3_never"),
        ]);
        input.all_targets.insert(
            "local2".to_string(),
            Target {
                name: "local2".to_string(),
                target_type: TargetType::Local,
                ..Default::default()
            },
        );

        let (tx, mut rx) = mpsc::channel(1024);
        let status = rt.block_on(engine.run_job(input, cancel_rx, tx));
        // 第 3 步目标未定义 → on_failure=stop → 整个 job 失败于 step 2（0 起）
        assert!(matches!(status, JobStatus::Failed { step_index: 2, .. }));

        // 单独验证前两步的执行：只跑到第 2 步（from_step/only 无关，直接裁剪步骤）
        let mut input2 = simple_job(vec![
            exec_step("echo S0_local"),
            {
                let mut s = exec_step("echo S1_local2");
                s.target = Some("local2".to_string());
                s
            },
        ]);
        all_targets.insert(
            "local2".to_string(),
            Target {
                name: "local2".to_string(),
                target_type: TargetType::Local,
                ..Default::default()
            },
        );
        input2.all_targets = all_targets;
        let (tx2, mut rx2) = mpsc::channel(1024);
        let (_c2, c2) = broadcast::channel(4);
        let status2 = rt.block_on(engine.run_job(input2, c2, tx2));
        assert!(status2.ok(), "status2: {status2:?}");
        let mut events = Vec::new();
        while let Ok(ev) = rx2.try_recv() {
            events.push(ev);
        }
        assert!(events.iter().any(|e| matches!(
            e,
            LogEvent::Stdout { line, .. } if line.contains("S1_local2")
        )));
    }

    #[test]
    fn step_retry_plan_defaults() {
        // exec 未配置 → 不重试
        let mut s = exec_step("echo x");
        assert_eq!(step_retry_plan(&s), (1, 0));
        // upload 未配置 → 默认重试 1 次（2 次尝试）/ 3s 退避（网络抖动兜底）
        s.step_type = StepType::Upload;
        assert_eq!(step_retry_plan(&s), (2, 3));
        // 显式配置优先
        s.retry = Some(RetryConfig { max_attempts: 5, backoff_seconds: 10 });
        assert_eq!(step_retry_plan(&s), (5, 10));
    }

    #[test]
    fn auto_corepack_wrap_rules() {
        // 钉版项目：pnpm/yarn 行加前缀，其余行不动
        assert_eq!(
            auto_corepack_wrap("pnpm install"),
            "corepack pnpm install"
        );
        assert_eq!(
            auto_corepack_wrap("pnpm install\necho done\nyarn build"),
            "corepack pnpm install\necho done\ncorepack yarn build"
        );
        // .cmd 后缀（Windows 习惯写法）
        assert_eq!(auto_corepack_wrap("pnpm.cmd -v"), "corepack pnpm.cmd -v");
        // 用户已手写 corepack → 不二次包装
        assert_eq!(auto_corepack_wrap("corepack pnpm install"), "corepack pnpm install");
        // 非 pnpm/yarn 命令原样
        assert_eq!(auto_corepack_wrap("mvn clean package"), "mvn clean package");
        // 保留原始缩进
        assert_eq!(
            auto_corepack_wrap("  pnpm run build"),
            "  corepack pnpm run build"
        );
    }

    #[test]
    fn auto_corepack_end_to_end() {
        // 工程 package.json 声明 packageManager → 步骤写 pnpm -v 即可（无需手写 corepack）
        let workspace = std::env::temp_dir().join(format!("rbs_cpk_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&workspace);
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(
            workspace.join("package.json"),
            r#"{ "name": "t", "packageManager": "pnpm@10.10.0" }"#,
        )
        .unwrap();

        let rt = runtime();
        let (tx, mut rx) = mpsc::channel(1024);
        let (_cancel_tx, cancel_rx) = broadcast::channel(4);
        let engine = Engine;

        let mut input = simple_job(vec![exec_step("pnpm -v")]);
        input.project.local_path = workspace.to_string_lossy().to_string();

        let status = rt.block_on(engine.run_job(input, cancel_rx, tx));
        let events = collect(status, &mut rx);
        assert!(events.iter().any(|e| matches!(
            e,
            LogEvent::Stdout { line, .. } if !line.is_empty() && line.chars().next().unwrap().is_ascii_digit()
        )));
        let _ = std::fs::remove_dir_all(&workspace);
    }

    #[test]
    fn tool_command_path_prepended_to_step_path() {
        // tools.yaml / 项目级 tools 的 command 是完整路径时，
        // 所在目录前置到 PATH → 命令里的裸名解析到这份映射
        let dir = std::env::temp_dir().join(format!("rbs_tool_path_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("rbs_fake_tool.cmd");
        std::fs::write(&script, "@echo toolhit").unwrap();

        let rt = runtime();
        let (tx, mut rx) = mpsc::channel(1024);
        let (_cancel_tx, cancel_rx) = broadcast::channel(4);
        let engine = Engine;

        let step = Step {
            tool: "fake".to_string(),
            command: "rbs_fake_tool".to_string(),
            step_type: StepType::Exec,
            enabled: true,
            ..Default::default()
        };
        let mut input = simple_job(vec![step]);
        input.project.tools.insert(
            "fake".to_string(),
            Tool {
                name: "fake".to_string(),
                command: script.to_string_lossy().to_string(),
                tool_type: ToolType::Custom,
                ..Default::default()
            },
        );

        let status = rt.block_on(engine.run_job(input, cancel_rx, tx));
        let events = collect(status, &mut rx);
        assert!(events.iter().any(|e| matches!(
            e,
            LogEvent::Stdout { line, .. } if line.contains("toolhit")
        )));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn project_node_modules_bin_on_path() {
        // 前端工程：node_modules/.bin 自动前置，工程内命令零配置可用
        let workspace = std::env::temp_dir().join(format!("rbs_nmbin_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&workspace);
        std::fs::create_dir_all(workspace.join("node_modules").join(".bin")).unwrap();
        std::fs::write(
            workspace.join("node_modules").join(".bin").join("rbs_local_cmd.cmd"),
            "@echo localhit",
        )
        .unwrap();

        let rt = runtime();
        let (tx, mut rx) = mpsc::channel(1024);
        let (_cancel_tx, cancel_rx) = broadcast::channel(4);
        let engine = Engine;

        let mut input = simple_job(vec![exec_step("rbs_local_cmd")]);
        input.project.local_path = workspace.to_string_lossy().to_string();

        let status = rt.block_on(engine.run_job(input, cancel_rx, tx));
        let events = collect(status, &mut rx);
        assert!(events.iter().any(|e| matches!(
            e,
            LogEvent::Stdout { line, .. } if line.contains("localhit")
        )));
        let _ = std::fs::remove_dir_all(&workspace);
    }

    #[test]
    fn pack_step_zips_dir_and_glob() {
        // pack 步骤：目录递归打包 + 通配打包，zip 内容回读校验
        let workspace = std::env::temp_dir().join(format!("rbs_pack_e2e_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&workspace);
        std::fs::create_dir_all(workspace.join("src/sub")).unwrap();
        std::fs::create_dir_all(workspace.join("target")).unwrap();
        std::fs::write(workspace.join("src/a.txt"), "A").unwrap();
        std::fs::write(workspace.join("src/sub/b.txt"), "B").unwrap();
        std::fs::write(workspace.join("target/x.jar"), "X").unwrap();
        std::fs::write(workspace.join("target/y.jar"), "Y").unwrap();

        let rt = runtime();
        let (tx, mut rx) = mpsc::channel(1024);
        let (_cancel_tx, cancel_rx) = broadcast::channel(4);
        let engine = Engine;

        let pack = |command: &str| Step {
            name: Some(command.to_string()),
            command: command.to_string(),
            step_type: StepType::Pack,
            enabled: true,
            ..Default::default()
        };
        let mut input = simple_job(vec![
            pack("src -> out/app.zip"),
            pack("target/*.jar -> out/jars.zip"),
        ]);
        input.project.local_path = workspace.to_string_lossy().to_string();

        let status = rt.block_on(engine.run_job(input, cancel_rx, tx));
        let events = collect(status, &mut rx);
        assert!(events.iter().any(|e| matches!(
            e,
            LogEvent::Stdout { line, .. } if line.contains("[pack] 完成")
        )));

        let app_zip = zip_names(&workspace.join("out/app.zip"));
        assert!(app_zip.contains(&"src/a.txt".to_string()), "{app_zip:?}");
        assert!(app_zip.contains(&"src/sub/b.txt".to_string()), "{app_zip:?}");

        let jars_zip = zip_names(&workspace.join("out/jars.zip"));
        assert!(jars_zip.contains(&"x.jar".to_string()), "{jars_zip:?}");
        assert!(jars_zip.contains(&"y.jar".to_string()), "{jars_zip:?}");

        let _ = std::fs::remove_dir_all(&workspace);
    }

    fn zip_names(path: &Path) -> Vec<String> {
        let f = std::fs::File::open(path).unwrap();
        let mut z = zip::ZipArchive::new(f).unwrap();
        (0..z.len())
            .map(|i| z.by_index(i).unwrap().name().to_string())
            .collect()
    }
}
