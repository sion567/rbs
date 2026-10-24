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
#[derive(Default, Clone)]
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
    /// 步骤未显式配置 timeout 时的默认超时秒数（来自 config.yaml default_timeout；
    /// 0 视为未设置，回退 DEFAULT_STEP_TIMEOUT）
    pub default_step_timeout: u64,
    /// GHA concurrency 开关：传入本运行的取消发送端即启用「同项目运行互斥」
    /// （新运行接管锁文件 → 旧运行 watcher 自杀）。None = 不启用（单测/预览）。
    pub cancel_tx: Option<broadcast::Sender<()>>,
}

pub struct Engine;

impl Engine {
    pub async fn run_job(
        &self,
        input: RunInput,
        cancel_rx: broadcast::Receiver<()>,
        log_tx: mpsc::Sender<LogEvent>,
    ) -> JobStatus {
        // ── GHA concurrency：同项目运行互斥 ─────────────────────────────
        // 锁文件直接接管（cancel-in-progress）；旧运行的 watcher 发现锁被夺走
        // 会向自己的取消信道发信号，经引擎现有取消机制整树终止后返回 Cancelled。
        let lock_enabled = input.cancel_tx.is_some() && !input.run_id.is_empty();
        let _lock = if lock_enabled {
            let dir = crate::config::app_root().join("runs");
            match crate::runlock::acquire(&dir, &input.project.name, &input.run_id, &input.job.name)
            {
                Ok((guard, warning)) => {
                    if let Some(w) = &warning {
                        let _ = log_tx.send(LogEvent::Error { message: w.clone() }).await;
                    }
                    let watcher = crate::runlock::spawn_watcher(
                        input.cancel_tx.clone().expect("lock_enabled implies Some"),
                        crate::runlock::lock_path(&dir, &input.project.name),
                        input.run_id.clone(),
                    );
                    Some((guard, watcher))
                }
                Err(e) => {
                    let reason = format!("运行锁获取失败: {e:#}");
                    let _ = log_tx.send(LogEvent::Error { message: reason.clone() }).await;
                    return JobStatus::Failed { step_index: 0, reason };
                }
            }
        } else {
            None
        };

        // ── job 级整体重试（§13.2 retry，on_failure: retry 时生效）────────
        // 整个 job 从头重跑（creates/when 守卫让重跑只补失败部分），同一 run_id、
        // 同一把运行锁；重试间隔可取消。前置检查/确认门在重试时不重复拦（引擎层无门）。
        let job_retry = match input.job.on_failure {
            FailureAction::Retry => input
                .job
                .retry
                .as_ref()
                .map(|r| (r.max_attempts.max(1), r.backoff_seconds))
                .unwrap_or((2, 5)),
            _ => (1, 0),
        };

        let mut attempt = 0;
        let mut first_rx = Some(cancel_rx);
        loop {
            attempt += 1;
            // 首次用调用方给的 receiver；重试从 cancel_tx 重新订阅（receiver 已被上次消费）
            let rx = first_rx.take().unwrap_or_else(|| {
                input
                    .cancel_tx
                    .as_ref()
                    .map(|tx| tx.subscribe())
                    .unwrap_or_else(|| broadcast::channel(1).1)
            });

            let status = self.run_job_inner(input.clone(), rx, log_tx.clone()).await;

            let retriable = matches!(&status, JobStatus::Failed { .. })
                && input.job.on_failure == FailureAction::Retry;
            if !retriable || attempt >= job_retry.0 {
                if let Some((_, watcher)) = &_lock {
                    watcher.abort();
                }
                return status;
            }
            let backoff = Duration::from_secs(job_retry.1);
            let _ = log_tx
                .send(LogEvent::Error {
                    message: format!(
                        "作业失败，{backoff:?} 后第 {}/{} 次整体重试（on_failure: retry）",
                        attempt, job_retry.0
                    ),
                })
                .await;
            // 退避期间响应取消（用新订阅收信号）
            let mut wait_rx = input
                .cancel_tx
                .as_ref()
                .map(|tx| tx.subscribe())
                .unwrap_or_else(|| broadcast::channel(1).1);
            tokio::select! {
                _ = tokio::time::sleep(backoff) => {}
                _ = wait_rx.recv() => {
                    if let Some((_, watcher)) = &_lock {
                        watcher.abort();
                    }
                    return JobStatus::Cancelled;
                }
            }
        }
    }

    async fn run_job_inner(
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

        // 作业级总超时（job.timeout 接线）：到达后剩余步骤不再启动；
        // 运行中的步骤由 run_step_with_retry 取 min(步骤截止, 作业截止) 提前掐断
        let job_deadline: Option<tokio::time::Instant> = input
            .job
            .timeout
            .filter(|s| *s > 0)
            .map(|s| tokio::time::Instant::now() + Duration::from_secs(s));
        // 步骤默认超时：config.yaml default_timeout（0/未传回退内置默认）
        let default_step_timeout = if input.default_step_timeout == 0 {
            DEFAULT_STEP_TIMEOUT
        } else {
            input.default_step_timeout
        };

        // 前端工程：package.json 声明了 packageManager（钉版）时，
        // 本地步骤里的 pnpm/yarn 自动经 corepack 执行（用户无需知道 corepack 存在）
        let pinned_frontend = {
            let pj = workspace.join("package.json");
            std::fs::read_to_string(&pj)
                .map(|t| t.contains("\"packageManager\""))
                .unwrap_or(false)
        };
        let corepack_ok = pinned_frontend && corepack_available();

        // ── 工具版本要求（mise 风格）：偏差零步失败 ────────────────────
        // 无进程开销（对比 tools.yaml 探测结果），放在前置检查之前先拦配置级问题
        {
            let mut reqs: Vec<(&String, &String)> = input.project.tools_require.iter().collect();
            reqs.sort();
            for (name, req) in reqs {
                let outcome: Result<String, String> = (|| {
                    let Some(tool) = merged_tools.get(name) else {
                        return Err(format!(
                            "工具「{name}」未定义（tools.yaml / 项目级 tools 或 ⚙ 设置里添加）"
                        ));
                    };
                    let detected = tool.version.as_deref().unwrap_or("");
                    match crate::version::version_matches(req, detected) {
                        Ok(true) => {
                            Ok(format!("[tools] {name} {detected} 满足 {req} ✓"))
                        }
                        Ok(false) => Err(format!(
                            "工具版本偏差: {name} 需要 {req}，探测到 {detected}\n\
                             （在 ⚙ 设置里重新探测；版本确实太旧请升级工具）"
                        )),
                        Err(e) => Err(format!("工具版本要求无法校验: {name} {req}: {e}")),
                    }
                })();
                match outcome {
                    Ok(line) => {
                        let _ = log_tx
                            .send(LogEvent::Stdout {
                                step_index: JobStatus::PRECONDITION_STEP,
                                line,
                            })
                            .await;
                    }
                    Err(reason) => {
                        let _ = log_tx.send(LogEvent::Error { message: reason.clone() }).await;
                        return JobStatus::Failed {
                            step_index: JobStatus::PRECONDITION_STEP,
                            reason,
                        };
                    }
                }
            }
        }

        // ── 前置检查（Task preconditions 语义）──────────────────────────
        // 发版机本地逐条执行，任一退出码非 0 → 人话报错，整个 job 零秒失败
        let pre_total = input.job.preconditions.len();
        for (pi, pre) in input.job.preconditions.iter().enumerate() {
            let cmd = resolve(&pre.check, &ctx);
            let _ = log_tx
                .send(LogEvent::Stdout {
                    step_index: JobStatus::PRECONDITION_STEP,
                    line: format!("[preflight {}/{}] {}", pi + 1, pre_total, cmd),
                })
                .await;
            if let Err(detail) = run_local_check(&cmd, &workspace).await {
                let reason = if pre.message.trim().is_empty() {
                    format!("前置检查未通过: {cmd}（{detail}）")
                } else {
                    format!("前置检查未通过: {}（{detail}）", pre.message.trim())
                };
                let _ = log_tx.send(LogEvent::Error { message: reason.clone() }).await;
                return JobStatus::Failed { step_index: JobStatus::PRECONDITION_STEP, reason };
            }
        }

        // GHA concurrency 兜底：锁被新运行接管（watcher 失效时）→ 取消本次运行
        let lock_enabled = input.cancel_tx.is_some() && !input.run_id.is_empty();
        let lock_file = crate::runlock::lock_path(
            &crate::config::app_root().join("runs"),
            &input.project.name,
        );

        // direnv 信任模型提醒：仓库自带的 .env 会随步骤成功并入 ${{ env.X }}
        if workspace.join(".env").exists() {
            let _ = log_tx
                .send(LogEvent::Stdout {
                    step_index: JobStatus::PRECONDITION_STEP,
                    line: "⚠ 检测到项目目录已有 .env：步骤成功后其 KEY=VALUE 将并入 ${{ env.X }}；非预期请删除或改名".to_string(),
                })
                .await;
        }

        // notify 登记（Ansible handlers 语义）：变更步骤 → 待执行 handler 名
        let mut notified: std::collections::HashSet<String> = std::collections::HashSet::new();
        // GHA 黄灯：continue_on_error / on_failure=continue 跳过的失败步骤
        let mut failed_skipped: Vec<usize> = Vec::new();

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

            // 作业级总超时：步与步之间先判，避免超时后还启动新步骤
            if let Some(dl) = job_deadline {
                if tokio::time::Instant::now() >= dl {
                    let _ = log_tx
                        .send(LogEvent::Error {
                            message: format!(
                                "作业级超时（job.timeout = {}s）已到，剩余步骤不再执行",
                                input.job.timeout.unwrap_or(0)
                            ),
                        })
                        .await;
                    return JobStatus::Timeout;
                }
            }

            // GHA concurrency：锁被新运行接管 → 本次运行取消（watcher 失效时的兜底）
            if lock_enabled && !crate::runlock::is_mine(&lock_file, &input.run_id) {
                let _ = log_tx
                    .send(LogEvent::Error {
                        message: "检测到同项目新运行已接管，本次运行取消".to_string(),
                    })
                    .await;
                return JobStatus::Cancelled;
            }

            let result = run_single_step(
                &input,
                &merged_tools,
                pinned_frontend,
                corepack_ok,
                &workspace,
                &job_wd,
                job_deadline,
                default_step_timeout,
                &mut ctx,
                &mut cancel_rx,
                &log_tx,
                index,
                step,
            )
            .await;

            match result {
                StepResult::Success { exit_code, duration_ms, changed, .. } => {
                    let _ = log_tx
                        .send(LogEvent::StepEnd { step_index: index, exit_code, duration_ms })
                        .await;
                    // notify 登记（Ansible 语义）：只有实际产生变更的步骤才触发 handler
                    if changed {
                        for name in &step.notify {
                            if !name.trim().is_empty() {
                                notified.insert(name.trim().to_string());
                            }
                        }
                    }
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
                        failed_skipped.push(index);
                        continue;
                    }
                    match &input.job.on_failure {
                        FailureAction::Continue => {
                            failed_skipped.push(index);
                            continue;
                        }
                        // job 级 retry 为 P1（§13.2），P0 按 stop 处理
                        FailureAction::Stop | FailureAction::Retry => {
                            return JobStatus::Failed { step_index: index, reason };
                        }
                    }
                }
            }
        }

        // ── handlers（Ansible handlers + notify）：变更才触发的收尾步骤 ──
        // 主流程成功收尾（含黄灯）才执行；只跑被通知到的；按定义顺序；失败 = job 失败
        if !input.job.handlers.is_empty() && !notified.is_empty() {
            let _ = log_tx
                .send(LogEvent::Stdout {
                    step_index: JobStatus::PRECONDITION_STEP,
                    line: format!(
                        "[handlers] 变更通知待执行：{} 个（共 {} 个 handler 定义）",
                        notified.len(),
                        input.job.handlers.len()
                    ),
                })
                .await;
            let handler_base = input.job.steps.len();
            for (hi, handler) in input.job.handlers.iter().enumerate() {
                let Some(hname) =
                    handler.name.as_deref().map(str::trim).filter(|s| !s.is_empty())
                else {
                    let _ = log_tx
                        .send(LogEvent::Error {
                            message: format!("handler #{} 未命名，跳过", hi + 1),
                        })
                        .await;
                    continue;
                };
                if !notified.contains(hname) {
                    continue;
                }
                // handler 之间也响应取消
                if matches!(
                    cancel_rx.try_recv(),
                    Ok(()) | Err(broadcast::error::TryRecvError::Closed)
                ) {
                    return JobStatus::Cancelled;
                }
                // 事件 step_index 接在主流程之后（UI 的步骤光圈只覆盖主流程，忽略之）
                let index = handler_base + hi;
                let _ = log_tx
                    .send(LogEvent::Stdout {
                        step_index: index,
                        line: format!("[handler] 执行「{hname}」"),
                    })
                    .await;
                let result = run_single_step(
                    &input,
                    &merged_tools,
                    pinned_frontend,
                    corepack_ok,
                    &workspace,
                    &job_wd,
                    job_deadline,
                    default_step_timeout,
                    &mut ctx,
                    &mut cancel_rx,
                    &log_tx,
                    index,
                    handler,
                )
                .await;
                match result {
                    StepResult::Success { exit_code, duration_ms, .. } => {
                        let _ = log_tx
                            .send(LogEvent::StepEnd { step_index: index, exit_code, duration_ms })
                            .await;
                    }
                    StepResult::Cancelled => return JobStatus::Cancelled,
                    StepResult::Timeout => return JobStatus::Timeout,
                    StepResult::Failed { exit_code, duration_ms, reason } => {
                        let _ = log_tx
                            .send(LogEvent::StepEnd { step_index: index, exit_code, duration_ms })
                            .await;
                        return JobStatus::Failed {
                            step_index: index,
                            reason: format!("handler「{hname}」失败: {reason}"),
                        };
                    }
                }
            }
        }

        // JobEnd 事件由外层包装任务统一发送（见 app.rs），引擎只返回最终状态；
        // 有被跳过的失败步骤 → 黄灯（成功结论 + 汇总标注）
        if failed_skipped.is_empty() {
            JobStatus::Success
        } else {
            JobStatus::SucceededWithFailures { failed_steps: failed_skipped }
        }
    }
}

// ---------------------------------------------------------------------------
// 单步完整执行（主流程与 handlers 共用）：跳过判定（enabled / when / creates）
// + 变量替换 + 有效目标 + 执行。发出 StepStart（含跳过标记），StepEnd 由调用方
// 统一补发；输出变量在此并入 ctx.env。
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
async fn run_single_step(
    input: &RunInput,
    merged_tools: &HashMap<String, Tool>,
    pinned_frontend: bool,
    corepack_ok: bool,
    workspace: &Path,
    job_wd: &Path,
    job_deadline: Option<tokio::time::Instant>,
    default_step_timeout: u64,
    ctx: &mut VarContext,
    cancel_rx: &mut broadcast::Receiver<()>,
    log_tx: &mpsc::Sender<LogEvent>,
    index: usize,
    step: &Step,
) -> StepResult {
    // 跳过路径的统一返回值（exit 0 / 无产出 / 无变更；StepEnd 由调用方补发）
    let skipped = || {
        StepResult::Success {
            exit_code: 0,
            duration_ms: 0,
            output_vars: HashMap::new(),
            changed: false,
        }
    };

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
        return skipped();
    }

    // when / condition 求值（§13.3；支持空判断与 ==/!= 值比较；无法解析时 fail-open + 告警）
    if let Some(cond) = &step.condition {
        match eval_condition(cond, ctx) {
            Some(false) => {
                let _ = log_tx
                    .send(LogEvent::StepStart {
                        step_index: index,
                        name: format!("{step_name}（跳过：条件不满足）"),
                        skipped: true,
                    })
                    .await;
                return skipped();
            }
            None => {
                let _ = log_tx
                    .send(LogEvent::Error {
                        message: format!(
                            "步骤 {index} 条件无法解析（支持 ==/!= 比较与空判断），按真值执行: {cond}"
                        ),
                    })
                    .await;
            }
            Some(true) => {}
        }
    }

    // ── 有效目标（§3.3）：步骤级 target 覆盖流程级默认 ──
    // 混合流程的关键：下载/编译用 local，上传/发布用 prod，各步各选
    let step_ctx: Result<(Option<Target>, Arc<dyn transport::RemoteTransport>, String), String> =
        (|| {
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
            let transport =
                transport::make_transport(eff_target.as_ref()).map_err(|e| e.to_string())?;
            // 远端根目录随有效目标走（上传相对路径 / 远端 cd 的基准）
            let remote_base = eff_target
                .as_ref()
                .and_then(|t| t.work_root.clone())
                .unwrap_or_default();
            Ok((eff_target, transport, remote_base))
        })();

    // creates 守卫（Ansible creates 语义）：产物已存在 → 跳过本步，
    // 流程重跑时「拉依赖 / 编译」等已完成步骤秒过。检查失败 fail-open 照常执行。
    if let Ok((eff_target, transport, remote_base)) = &step_ctx {
        if let Some(creates) = &step.creates {
            let resolved = resolve(creates, ctx);
            let remote_dir = resolve_remote_step_dir(step, remote_base);
            let log = LogHandle::new(log_tx.clone(), index);
            match check_creates_exists(
                &resolved,
                eff_target.as_ref(),
                transport,
                &remote_dir,
                workspace,
                &log,
                cancel_rx,
            )
            .await
            {
                Err(e) if local::is_cancelled(&e) => return StepResult::Cancelled,
                Err(e) => {
                    let _ = log_tx
                        .send(LogEvent::Error {
                            message: format!(
                                "步骤 {index} creates 检查失败，按不存在继续执行: {e}"
                            ),
                        })
                        .await;
                }
                Ok(true) => {
                    let _ = log_tx
                        .send(LogEvent::StepStart {
                            step_index: index,
                            name: format!("{step_name}（跳过：creates 已存在 {resolved}）"),
                            skipped: true,
                        })
                        .await;
                    return skipped();
                }
                Ok(false) => {}
            }
        }
    }

    let _ = log_tx
        .send(LogEvent::StepStart { step_index: index, name: step_name, skipped: false })
        .await;

    let result = match step_ctx {
        Err(reason) => StepResult::Failed { exit_code: -1, duration_ms: 0, reason },
        Ok((eff_target, transport, remote_base)) => {
            // 变量替换（§6.3 执行前一次性替换）：命令 / 工作目录 / 步骤 env
            let mut command = resolve(&step.command, ctx);

            // 危险命令静态检查（§14）：不拦截，执行前醒目提醒一次（预览里也有 ⚠）
            if matches!(step.step_type, StepType::Exec | StepType::K8sApply) {
                let hits = dangerous_command_hits(&command);
                if !hits.is_empty() {
                    let _ = log_tx
                        .send(LogEvent::Error {
                            message: format!(
                                "⚠ 步骤 {index} 命令含危险模式 {}，请确认是有意为之",
                                hits.join(" / ")
                            ),
                        })
                        .await;
                }
            }

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
            let step_local_wd = resolve_step_working_dir(step, job_wd);
            let remote_dir = resolve_remote_step_dir(step, &remote_base);

            // §6.1 优先级：步骤级 env > 工具 env > target env
            let mut env_map: HashMap<String, String> = eff_target
                .as_ref()
                .map(|t| t.env.clone())
                .unwrap_or_default();
            if let Some(tool) = merged_tools.get(&step.tool) {
                env_map.extend(tool.env.clone());
            }
            for (k, v) in &step.env {
                env_map.insert(k.clone(), resolve(v, ctx));
            }

            // 工具地址落地：tools.yaml / 项目级 tools 的 command 若是完整路径，
            // 把其所在目录前置到步骤 PATH——命令里的裸名（mvn/node/git…）
            // 优先解析到这份映射，而不是系统 PATH 里的任意版本（§6.5）
            if let Some(tool) = merged_tools.get(&step.tool) {
                let is_path_like = tool.command.contains('\\') || tool.command.contains('/');
                if is_path_like {
                    if let Some(parent) = Path::new(&tool.command).parent() {
                        if parent.exists() {
                            env_map = crate::transport::with_path_prepended(env_map, parent);
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

            // step outputs：声明了输出捕获才建 stdout 缓冲
            let capture = (!step.outputs.is_empty()).then(|| {
                std::sync::Arc::new(std::sync::Mutex::new(String::new()))
            });

            let step_timeout =
                Duration::from_secs(step.timeout.unwrap_or(default_step_timeout));
            let result = run_step_with_retry(
                &transport,
                eff_target.as_ref(),
                step,
                &command,
                &step_local_wd,
                &remote_dir,
                &env_map,
                workspace,
                step_timeout,
                job_deadline,
                capture.as_ref(),
                cancel_rx,
                log_tx,
                index,
            )
            .await;
            // §6.4 步骤间变量传递：产出变量并入 ctx（.env 约定 + step outputs 捕获）
            if let StepResult::Success { output_vars, .. } = &result {
                ctx.env.extend(output_vars.clone());
            }
            result
        }
    };
    result
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
    job_deadline: Option<tokio::time::Instant>,
    capture: Option<&std::sync::Arc<std::sync::Mutex<String>>>,
    cancel_rx: &mut broadcast::Receiver<()>,
    log_tx: &mpsc::Sender<LogEvent>,
    index: usize,
) -> StepResult {
    let log = match capture {
        Some(buf) => LogHandle::with_capture(log_tx.clone(), index, std::sync::Arc::clone(buf)),
        None => LogHandle::new(log_tx.clone(), index),
    };
    // 重试计划：显式 retry 配置优先；upload 步骤无配置时默认重试 1 次（网络抖动兜底）
    let (attempts, backoff_secs) = step_retry_plan(step);
    let backoff = Duration::from_secs(backoff_secs);
    // 恢复执行时补发 StepStart 的名字（与主循环同一推导：优先自定义名，否则命令首行）
    let step_name = step
        .name
        .clone()
        .unwrap_or_else(|| step.command.lines().next().unwrap_or("").to_string());

    // 步骤级 deadline；作业级 deadline（job.timeout）更早到则取更早者
    let mut deadline = tokio::time::Instant::now() + step_timeout;
    if let Some(jd) = job_deadline {
        if jd < deadline {
            deadline = jd;
        }
    }

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
            capture,
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
    capture: Option<&std::sync::Arc<std::sync::Mutex<String>>>,
    cancel_rx: &mut broadcast::Receiver<()>,
) -> StepResult {
    let started = Instant::now();
    // 本步骤是否实际产生变更（notify 登记依据）：upload = 真的传了内容；
    // exec / wait / pack / k8s 真执行了 = 是（creates/when 跳过根本不会走到这）
    let mut changed = !matches!(step.step_type, StepType::Upload);

    let outcome: anyhow::Result<transport::ExitStatus> = match step.step_type {
        StepType::Exec => {
            // 远端步骤使用远端目录，本地步骤使用本地解析目录
            let wd = if is_remote(target) { remote_dir.to_string() } else { local_wd.to_string_lossy().to_string() };
            transport.exec(target_dummy(target), command, &wd, env_map, log, cancel_rx).await
        }
        StepType::Wait => {
            // 轮询探测直到就绪或超时（总时长由外层包裹）：三种模式（Compose healthcheck 同款）
            let wd = if is_remote(target) { remote_dir.to_string() } else { local_wd.to_string_lossy().to_string() };
            let probe = step.wait.clone().unwrap_or_default();
            let interval = if probe.interval_seconds == 0 {
                transport::WAIT_POLL_SECONDS
            } else {
                probe.interval_seconds.max(1)
            };
            let mut attempt = 0;
            loop {
                attempt += 1;
                let hit: Option<transport::ExitStatus> = match probe.mode {
                    WaitMode::Tcp => {
                        log.line(
                            transport::Stream::Stdout,
                            format!("[wait] 第 {attempt} 次探测 tcp {}", probe.target),
                        );
                        if probe_tcp(&probe.target).await {
                            Some(transport::ExitStatus::from_code(0))
                        } else {
                            None
                        }
                    }
                    WaitMode::Http => {
                        log.line(
                            transport::Stream::Stdout,
                            format!("[wait] 第 {attempt} 次探测 http {}", probe.target),
                        );
                        match probe_http(&probe.target).await {
                            Ok(ok) => {
                                if ok {
                                    Some(transport::ExitStatus::from_code(0))
                                } else {
                                    None
                                }
                            }
                            Err(e) => {
                                log.line(transport::Stream::Stderr, format!("[wait] {e}"));
                                None
                            }
                        }
                    }
                    WaitMode::Command => {
                        // 兼容旧配置：probe.target 空 = 沿用 step.command
                        let cmd = if probe.target.trim().is_empty() {
                            command
                        } else {
                            probe.target.trim()
                        };
                        log.line(
                            transport::Stream::Stdout,
                            format!("[wait] 第 {attempt} 次探测: {cmd}"),
                        );
                        match transport
                            .exec(target_dummy(target), cmd, &wd, env_map, log, cancel_rx)
                            .await
                        {
                            Ok(st) if st.success => Some(st),
                            // 探测失败不算步骤失败，等待后重试；取消要透传
                            Ok(_) => None,
                            Err(e) if local::is_cancelled(&e) => break Err(e),
                            Err(e) => {
                                log.line(transport::Stream::Stderr, format!("[wait] 探测失败: {e}"));
                                None
                            }
                        }
                    }
                };
                if let Some(st) = hit {
                    break Ok(st);
                }
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(interval)) => {}
                    _ = cancel_rx.recv() => {
                        break Err(anyhow::anyhow!("__cancelled__"));
                    }
                }
            }
        }
        StepType::Upload => {
            // 命令格式约定：<本地路径或目录> -> <远端路径>；返回是否真传了内容（变更判断）
            let res =
                upload_step(transport, target, command, workspace, remote_dir, log, cancel_rx).await;
            if let Ok((_, uploaded)) = &res {
                changed = *uploaded;
            }
            res.map(|(st, _)| st)
        }
        StepType::Download => {
            // 命令格式约定：<远端路径> -> <本地路径>（方向与 upload 相反）；
            // 本地路径相对 workspace 解析，父目录自动创建，单文件下载后 sha256 校验
            download_step(transport, target, command, workspace, remote_dir, log, cancel_rx).await
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
            let mut output_vars = capture_env_file(workspace);
            // step outputs（GHA 语义）：对捕获的 stdout 按声明正则提取运行时变量
            if !step.outputs.is_empty() {
                if let Some(buf) = capture {
                    let stdout = buf.lock().map(|b| b.clone()).unwrap_or_default();
                    output_vars.extend(apply_step_outputs(&step.outputs, &stdout, log));
                }
            }
            StepResult::Success { exit_code: status.code, duration_ms, output_vars, changed }
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

/// wait tcp 探测：从发版机直连 host:port（5 秒连接超时）
async fn probe_tcp(target: &str) -> bool {
    tokio::time::timeout(Duration::from_secs(5), tokio::net::TcpStream::connect(target))
        .await
        .map(|r| r.is_ok())
        .unwrap_or(false)
}

/// wait http 探测：GET 请求状态码 2xx = 就绪（HTTP/1.0 直连，仅 http://；https 用命令模式 curl）
async fn probe_http(url: &str) -> anyhow::Result<bool> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| anyhow::anyhow!("http 模式仅支持 http:// URL（https 请用命令模式 curl -k）"))?;
    let (hostport, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    if hostport.is_empty() {
        anyhow::bail!("http URL 缺少主机: {url}");
    }
    let authority = if hostport.contains(':') {
        hostport.to_string()
    } else {
        format!("{hostport}:80")
    };
    let fut = async {
        let stream = tokio::net::TcpStream::connect(&authority).await?;
        let mut reader = tokio::io::BufReader::new(stream);
        reader
            .write_all(
                format!("GET {path} HTTP/1.0\r\nHost: {hostport}\r\nConnection: close\r\n\r\n")
                    .as_bytes(),
            )
            .await?;
        // 状态行在响应最前面（"HTTP/1.x 200 OK"），读到首行即可
        let mut line = String::new();
        reader.read_line(&mut line).await?;
        let status = line
            .split_whitespace()
            .nth(1)
            .and_then(|c| c.parse::<u16>().ok())
            .unwrap_or(0);
        Ok((200..300).contains(&status))
    };
    tokio::time::timeout(Duration::from_secs(5), fut)
        .await
        .map_err(|_| anyhow::anyhow!("http 探测超时（5s）"))?
}

/// step outputs：每个键对 stdout 取首个正则匹配（有捕获组取组 1，无则整行匹配）。
/// 未匹配的键留空并告警（GHA：未声明的输出 = 空）。
fn apply_step_outputs(
    outputs: &HashMap<String, String>,
    stdout: &str,
    log: &LogHandle,
) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for (key, pattern) in outputs {
        let value = regex::Regex::new(pattern)
            .ok()
            .and_then(|re| re.captures(stdout))
            .and_then(|caps| caps.get(1).or_else(|| caps.get(0)))
            .map(|m| m.as_str().trim().to_string())
            // cmd 嵌套调用（命令里手写 cmd /C …）常见的引号尾巴清理
            .map(|v| v.trim_matches('"').to_string());
        match value {
            Some(v) => {
                log.line(
                    transport::Stream::Stdout,
                    format!("[outputs] {key} = {v}"),
                );
                out.insert(key.clone(), v);
            }
            None => {
                log.line(
                    transport::Stream::Stderr,
                    format!("[outputs] 未捕获到 {key}（正则无匹配）: {pattern}"),
                );
            }
        }
    }
    out
}

fn is_remote(target: Option<&Target>) -> bool {
    matches!(target.map(|t| &t.target_type), Some(TargetType::Ssh { .. }))
}

/// 远端步骤工作目录：work_root 基准 + 步骤 working_dir（上传相对路径 / 远端 cd 的基准）
fn resolve_remote_step_dir(step: &Step, remote_base: &str) -> String {
    match &step.working_dir {
        Some(wd) if !remote_base.is_empty() => {
            format!("{remote_base}/{}", wd.trim_matches('/'))
        }
        Some(wd) => wd.clone(),
        None => remote_base.to_string(),
    }
}

/// creates 守卫的存在性检查（TODO「Ansible 参考 #1」）：
/// - 本地步骤：相对 workspace 解析，支持 `*` `?` 通配（任一匹配存在即算）
/// - SSH 步骤：远端 `test -e` 退出码判定（相对路径随 work_root / working_dir 展开）
///
/// 检查失败（如连不上远端）返回 Err，调用方 fail-open 照常执行步骤。
async fn check_creates_exists(
    pattern: &str,
    target: Option<&Target>,
    transport: &Arc<dyn RemoteTransport>,
    remote_dir: &str,
    workspace: &Path,
    log: &LogHandle,
    cancel_rx: &mut broadcast::Receiver<()>,
) -> anyhow::Result<bool> {
    if is_remote(target) {
        let path = if pattern.starts_with('/') || remote_dir.is_empty() {
            pattern.to_string()
        } else {
            format!("{}/{}", remote_dir.trim_end_matches('/'), pattern)
        };
        log.line(
            transport::Stream::Stdout,
            format!("[creates] 检查远端路径: {path}"),
        );
        let cmd = format!(
            "test -e '{}'",
            crate::transport::putty::escape_posix_single_quote(&path)
        );
        let st = transport
            .exec(target_dummy(target), &cmd, "", &HashMap::new(), log, cancel_rx)
            .await?;
        Ok(st.success)
    } else {
        Ok(local_creates_exists(workspace, pattern))
    }
}

/// 本地 creates 检查：字面量路径直接 exists；含通配符的展开后任一匹配存在即算
fn local_creates_exists(workspace: &Path, pattern: &str) -> bool {
    let p = Path::new(pattern);
    let absolute = if p.is_absolute() { p.to_path_buf() } else { workspace.join(p) };
    if absolute.exists() {
        return true;
    }
    if pattern.contains('*') || pattern.contains('?') {
        // glob 模式统一用 / 分隔，避免 Windows 反斜杠被当转义符
        let norm = absolute.to_string_lossy().replace('\\', "/");
        if let Ok(paths) = glob::glob(&norm) {
            return paths.filter_map(Result::ok).any(|p| p.exists());
        }
    }
    false
}

/// 前置检查执行：发版机本地 cmd /C，退出码 0 = 通过（30 秒超时；失败取输出末尾几行）
async fn run_local_check(command: &str, working_dir: &Path) -> Result<(), String> {
    use std::process::Stdio;
    let mut cmd = tokio::process::Command::new("cmd");
    cmd.arg("/C")
        .arg(command)
        .current_dir(working_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW（tokio Command 自带该方法）
    let output = tokio::time::timeout(Duration::from_secs(30), cmd.output())
        .await
        .map_err(|_| "执行超时（30s）".to_string())?
        .map_err(|e| format!("执行失败: {e}"))?;
    if output.status.success() {
        return Ok(());
    }
    // GBK / UTF-8 输出统一走控制台解码，末尾 2 行进错误（命令横幅太长）
    let mut text = crate::transport::decode_console_bytes(&output.stdout);
    let err_text = crate::transport::decode_console_bytes(&output.stderr);
    if !err_text.trim().is_empty() {
        text.push_str(&err_text);
    }
    let tail: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .rev()
        .take(2)
        .collect();
    let tail = tail.into_iter().rev().collect::<Vec<_>>().join(" | ");
    Err(if tail.is_empty() {
        format!("退出码 {}", output.status.code().unwrap_or(-1))
    } else {
        tail
    })
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

/// pack 步骤：`<目录或通配> -> <目标.zip> [--exclude <glob>]…`，路径相对 workspace 解析
async fn pack_step(
    command: &str,
    workspace: &Path,
    log: &LogHandle,
) -> anyhow::Result<transport::ExitStatus> {
    let (src_part, dst_part) = command.split_once("->").ok_or_else(|| {
        anyhow::anyhow!("pack 步骤命令格式应为: <目录或通配> -> <目标.zip>，实际: {command}")
    })?;
    // dst 段可带 --exclude <模式>…（rsync 语义）：字面量 = 目录/文件名分量，* ? = 通配
    let mut dst_tokens = dst_part.split("--exclude");
    let dst_str = dst_tokens.next().unwrap_or("").trim();
    let excludes: Vec<String> = dst_tokens
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    let resolve = |p: &str| {
        let path = Path::new(p);
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            workspace.join(path)
        }
    };
    let src = resolve(src_part.trim());
    let dst = resolve(dst_str);
    if !dst.to_string_lossy().to_ascii_lowercase().ends_with(".zip") {
        anyhow::bail!("pack 目标必须是 .zip 文件: {}", dst.display());
    }

    log.line(
        transport::Stream::Stdout,
        format!("[pack] {} -> {}", src.display(), dst.display()),
    );
    if !excludes.is_empty() {
        log.line(
            transport::Stream::Stdout,
            format!("[pack] 排除: {}", excludes.join(", ")),
        );
    }
    let (count, bytes) = crate::pack::pack(&src, &dst, &excludes)?;
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
) -> anyhow::Result<(transport::ExitStatus, bool)> {
    // 返回值第二项 = 是否真的传了内容（变更判断；hash 未变更跳过的映射不计）
    if !is_remote(target) {
        anyhow::bail!("step_type: upload 需要 SSH target，当前 target 不支持上传");
    }
    let target = target.expect("checked above");
    let mappings = parse_upload_mappings(command)?;
    let total = mappings.len();
    let mut uploaded_any = false;

    for (i, (local_part, remote_part)) in mappings.iter().enumerate() {
        let local_path = {
            let p = Path::new(local_part);
            if p.is_absolute() {
                p.to_path_buf()
            } else {
                workspace.join(p)
            }
        };
        let remote_path = if remote_part.starts_with('/') || remote_dir.is_empty() {
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

        // 单文件：先做内容变更判断（Ansible handlers/notify 的地基）——
        // 本地 sha256 与远端一致 → 整个传输跳过；不一致 / 远端不可查 → 正常上传
        let mut local_hash: Option<String> = None;
        if local_path.is_file() {
            let bytes = std::fs::metadata(&local_path).map(|m| m.len()).unwrap_or(0);
            log.line(
                transport::Stream::Stdout,
                format!("[upload {}/{}] 文件大小 {}", i + 1, total, human_size(bytes)),
            );
            let hash = crate::transport::putty::sha256_file(&local_path)?;
            if let Ok(remote_hash) = crate::transport::putty::remote_sha256(target, &remote_path) {
                if remote_hash == hash {
                    log.line(
                        transport::Stream::Stdout,
                        format!(
                            "[upload {}/{}] 内容未变更（sha256 {}），跳过传输",
                            i + 1,
                            total,
                            &hash[..12]
                        ),
                    );
                    continue;
                }
                log.line(
                    transport::Stream::Stdout,
                    format!(
                        "[upload {}/{}] 远端内容不同（sha256 {} != {}），开始传输",
                        i + 1,
                        total,
                        &hash[..12],
                        &remote_hash[..remote_hash.len().min(12)]
                    ),
                );
            }
            local_hash = Some(hash);
        }
        transport
            .upload(target, &local_path, &remote_path, log, cancel_rx)
            .await?;
        uploaded_any = true;

        // 上传完整性校验（§5.9）：单文件比对本地/远端 sha256；
        // 目录递归上传跳过（建议先用 pack 打成单文件）。校验失败 → 步骤失败 → 按重试配置重传。
        if let Some(local_hash) = &local_hash {
            log.line(
                transport::Stream::Stdout,
                format!("[verify] 校验远端 sha256: {}", remote_path),
            );
            let remote_hash = crate::transport::putty::remote_sha256(target, &remote_path)?;
            if &remote_hash != local_hash {
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
    Ok((transport::ExitStatus::from_code(0), uploaded_any))
}

/// download 步骤：`<远端路径> -> <本地路径>` 每行一条（复用 upload 的映射解析，
/// 方向相反）。本地路径相对 workspace 解析、父目录自动创建；单文件下载后 sha256 校验。
async fn download_step(
    transport: &Arc<dyn RemoteTransport>,
    target: Option<&Target>,
    command: &str,
    workspace: &Path,
    remote_dir: &str,
    log: &LogHandle,
    cancel_rx: &mut broadcast::Receiver<()>,
) -> anyhow::Result<transport::ExitStatus> {
    if !is_remote(target) {
        anyhow::bail!("step_type: download 需要 SSH target，当前 target 不支持下载");
    }
    let target = target.expect("checked above");
    let mappings = parse_upload_mappings(command)?;
    let total = mappings.len();
    for (i, (remote_part, local_part)) in mappings.iter().enumerate() {
        let remote_path = if remote_part.starts_with('/') || remote_dir.is_empty() {
            remote_part.clone()
        } else {
            format!("{}/{}", remote_dir.trim_end_matches('/'), remote_part)
        };
        let local_path = {
            let p = Path::new(local_part);
            if p.is_absolute() { p.to_path_buf() } else { workspace.join(p) }
        };
        if let Some(parent) = local_path.parent() {
            if !parent.exists() {
                std::fs::create_dir_all(parent)?;
            }
        }
        log.line(
            transport::Stream::Stdout,
            format!("[download {}/{}] {} -> {}", i + 1, total, remote_path, local_path.display()),
        );
        transport
            .download(target, &remote_path, &local_path, log, cancel_rx)
            .await?;
        // 下载完整性校验：单文件比对远端/本地 sha256（目录跳过）
        if local_path.is_file() {
            let remote_hash = crate::transport::putty::remote_sha256(target, &remote_path)?;
            let local_hash = crate::transport::putty::sha256_file(&local_path)?;
            if remote_hash != local_hash {
                anyhow::bail!(
                    "下载校验失败：sha256 不一致（远端 {remote_hash} / 本地 {local_hash}）"
                );
            }
            log.line(
                transport::Stream::Stdout,
                format!("[verify] sha256 一致 ✓ ({})", &local_hash[..12]),
            );
        }
    }
    Ok(transport::ExitStatus::from_code(0))
}

/// 危险命令模式（§14 P1 静态检查）：预览 ⚠ + 执行前日志提醒（不拦截，操作者自己判断）
pub fn dangerous_command_hits(command: &str) -> Vec<&'static str> {
    const PATTERNS: &[&str] = &[
        "rm -rf /",
        "rm -fr /",
        "rm -rf ~",
        "rm -rf /*",
        "chmod -r 777",
        "chmod 777 /",
        "mkfs",
        "dd if=/dev/",
        "del /f /s /q",
        "format c:",
        "shutdown -r",
        "shutdown /r",
    ];
    let lower = command.to_lowercase();
    PATTERNS
        .iter()
        .copied()
        .filter(|p| lower.contains(p))
        .collect()
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
// 干跑预览（Ansible --check 思路）：变量全量解析 + when/creates 判定，不执行
// 任何命令、不发起任何网络请求。顺带当变量拼错检查器用：未解析的占位符原样
// 保留，预览里以 ⚠ 标出。
// ---------------------------------------------------------------------------

/// 单个步骤的预览结果（preview_job 产出；UI 控制台 / CLI 直接渲染）
#[derive(Debug, Clone)]
pub struct StepPreview {
    pub index: usize,
    pub name: String,
    pub skipped: bool,
    /// 跳过原因 / 备注（未启用 / 条件不满足 / creates 已存在 / 远端 creates 未校验）
    pub note: Option<String>,
    /// ⚠ 警告（条件无法解析、变量未解析、上传源缺失、目标未定义…）
    pub warnings: Vec<String>,
    /// 有效目标名（None = 本机）
    pub target: Option<String>,
    /// 变量解析后的最终命令（含 corepack 自动包装后的形态）
    pub command: String,
    /// 解析后的本地工作目录
    pub working_dir: PathBuf,
    /// upload 步骤：解析后的 (本地路径, 远端路径, 本地是否存在)
    pub uploads: Vec<(String, String, bool)>,
    /// pack 步骤：解析后的 (源, 目标 zip)
    pub pack: Option<(String, String)>,
    /// step outputs：捕获声明（键, 正则）
    pub outputs: Vec<(String, String)>,
    /// notify 的 handler 名
    pub notify: Vec<String>,
    /// wait 探测描述（模式 目标 间隔）
    pub wait_note: Option<String>,
}

/// 干跑预览：按真实执行路径解析每个步骤（变量替换、when 条件、creates 守卫、
/// upload/pack 命令格式），但不执行。同步无副作用，可在 UI 线程直接调用。
pub fn preview_job(input: &RunInput) -> Vec<StepPreview> {
    let ctx = build_var_context(input);
    let workspace = PathBuf::from(&input.project.local_path);
    let job_wd = resolve_job_working_dir(&input.job, &workspace);

    // 与 run_job 同一判定：前端工程 corepack 钉版（预览展示包装后的最终命令）
    let pinned_frontend = {
        let pj = workspace.join("package.json");
        std::fs::read_to_string(&pj)
            .map(|t| t.contains("\"packageManager\""))
            .unwrap_or(false)
    };
    let corepack_ok = pinned_frontend && corepack_available();

    let mut out = Vec::new();
    for (index, step) in input.job.steps.iter().enumerate() {
        let step_name = step
            .name
            .clone()
            .unwrap_or_else(|| step.command.lines().next().unwrap_or("").to_string());
        let mut p = StepPreview {
            index,
            name: step_name,
            skipped: false,
            note: None,
            warnings: Vec::new(),
            target: None,
            command: String::new(),
            working_dir: resolve_step_working_dir(step, &job_wd),
            uploads: Vec::new(),
            pack: None,
            outputs: step.outputs.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            notify: step.notify.clone(),
            wait_note: step.wait.as_ref().map(|wp| {
                let interval = if wp.interval_seconds == 0 {
                    transport::WAIT_POLL_SECONDS
                } else {
                    wp.interval_seconds.max(1)
                };
                format!("{} {} 每 {interval}s", wp.mode.as_str(), wp.target)
            }),
        };

        if !step.enabled {
            p.skipped = true;
            p.note = Some("未启用".to_string());
            out.push(p);
            continue;
        }

        // when / condition
        if let Some(cond) = &step.condition {
            match eval_condition(cond, &ctx) {
                Some(false) => {
                    p.skipped = true;
                    p.note = Some(format!("条件不满足: {cond}"));
                    out.push(p);
                    continue;
                }
                None => p.warnings.push(format!("条件无法解析，将按真值执行: {cond}")),
                Some(true) => {}
            }
        }

        // 有效目标（§3.3）
        let eff_name = step.target.clone().or_else(|| input.job.target.clone());
        let eff_target = eff_name.as_deref().and_then(|n| {
            let t = input.all_targets.get(n)?;
            let mut t = t.clone();
            if t.name.is_empty() {
                t.name = n.to_string();
            }
            Some(t)
        });
        p.target = eff_name.clone();
        if let Some(n) = &eff_name {
            if eff_target.is_none() {
                p.warnings
                    .push(format!("目标「{n}」未定义（project.yaml 的 targets 表）"));
            }
        }
        let remote = is_remote(eff_target.as_ref());
        let remote_base = eff_target
            .as_ref()
            .and_then(|t| t.work_root.clone())
            .unwrap_or_default();
        let remote_dir = resolve_remote_step_dir(step, &remote_base);

        // creates：本地实际校验；远端路径干跑不发起 SSH 往返，仅标注
        if let Some(creates) = &step.creates {
            let resolved = resolve(creates, &ctx);
            if remote {
                p.note =
                    Some(format!("creates={resolved}（远端路径，干跑不校验，按执行处理）"));
            } else if local_creates_exists(&workspace, &resolved) {
                p.skipped = true;
                p.note = Some(format!("creates 已存在: {resolved}"));
                out.push(p);
                continue;
            }
        }

        // 命令解析（与 run_job 同一替换与 corepack 包装）
        let mut command = resolve(&step.command, &ctx);
        if pinned_frontend
            && corepack_ok
            && matches!(step.step_type, StepType::Exec | StepType::K8sApply)
            && !remote
        {
            command = auto_corepack_wrap(&command);
        }
        if command.contains("${{") || command.contains("${") {
            p.warnings
                .push("命令仍含未解析的变量占位符（检查变量名拼写 / 变量组）".to_string());
        }
        // 危险命令静态检查（§14）：预览 ⚠，防手滑（执行前引擎也会再提醒一次）
        if matches!(step.step_type, StepType::Exec | StepType::K8sApply) {
            let hits = dangerous_command_hits(&command);
            if !hits.is_empty() {
                p.warnings
                    .push(format!("⚠ 危险命令模式: {}（请确认是有意为之）", hits.join(" / ")));
            }
        }
        p.command = command.clone();

        match step.step_type {
            StepType::Upload if remote => {
                match parse_upload_mappings(&command) {
                    Ok(mappings) => {
                        for (local_part, remote_part) in mappings {
                            let local_path = {
                                let lp = Path::new(&local_part);
                                if lp.is_absolute() {
                                    lp.to_path_buf()
                                } else {
                                    workspace.join(lp)
                                }
                            };
                            let exists = local_path.exists();
                            if !exists {
                                p.warnings
                                    .push(format!("上传源不存在: {}", local_path.display()));
                            }
                            let remote_path = if remote_part.starts_with('/')
                                || remote_dir.is_empty()
                            {
                                remote_part.clone()
                            } else {
                                format!("{}/{}", remote_dir.trim_end_matches('/'), remote_part)
                            };
                            p.uploads
                                .push((local_path.to_string_lossy().to_string(), remote_path, exists));
                        }
                    }
                    Err(e) => p.warnings.push(e.to_string()),
                }
            }
            StepType::Upload => {
                p.warnings
                    .push("upload 需要 SSH 目标，当前目标不支持上传".to_string());
            }
            StepType::Download if remote => {
                match parse_upload_mappings(&command) {
                    Ok(mappings) => {
                        for (remote_part, local_part) in mappings {
                            let remote_path = if remote_part.starts_with('/')
                                || remote_dir.is_empty()
                            {
                                remote_part.clone()
                            } else {
                                format!("{}/{}", remote_dir.trim_end_matches('/'), remote_part)
                            };
                            let local_path = {
                                let lp = Path::new(&local_part);
                                if lp.is_absolute() { lp.to_path_buf() } else { workspace.join(lp) }
                            };
                            p.uploads.push((
                                remote_path,
                                local_path.to_string_lossy().to_string(),
                                true,
                            ));
                        }
                    }
                    Err(e) => p.warnings.push(e.to_string()),
                }
            }
            StepType::Download => {
                p.warnings
                    .push("download 需要 SSH 目标，当前目标不支持下载".to_string());
            }
            StepType::Pack => match command.split_once("->") {
                Some((s, d)) => {
                    let rp = |v: &str| {
                        let pp = Path::new(v.trim());
                        if pp.is_absolute() {
                            pp.to_path_buf()
                        } else {
                            workspace.join(pp)
                        }
                    };
                    let src = rp(s);
                    let dst = rp(d);
                    if !dst.to_string_lossy().to_ascii_lowercase().ends_with(".zip") {
                        p.warnings.push("pack 目标必须是 .zip 文件".to_string());
                    }
                    p.pack =
                        Some((src.to_string_lossy().to_string(), dst.to_string_lossy().to_string()));
                }
                None => p
                    .warnings
                    .push("pack 命令格式应为: <目录或通配> -> <目标.zip>".to_string()),
            },
            _ => {}
        }

        out.push(p);
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
            default_step_timeout: DEFAULT_STEP_TIMEOUT,
            cancel_tx: None,
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
    fn preconditions_gate_the_job() {
        // Task preconditions：任一前置检查失败 → 零秒失败，步骤不执行；
        // 全部通过 → 正常执行
        let rt = runtime();
        let engine = Engine;

        // 失败路径：检查失败 + 人话消息
        {
            let (tx, mut rx) = mpsc::channel(1024);
            let (_c, cancel_rx) = broadcast::channel(4);
            let pre = crate::models::Precondition {
                check: "cmd /C exit 7".to_string(),
                message: "环境未就绪".to_string(),
            };
            let mut input = simple_job(vec![exec_step("echo NEVER_RAN")]);
            input.job.preconditions = vec![pre];
            let status = rt.block_on(engine.run_job(input, cancel_rx, tx));
            assert!(
                matches!(
                    &status,
                    JobStatus::Failed { step_index, reason }
                        if *step_index == JobStatus::PRECONDITION_STEP
                            && reason.contains("环境未就绪")
                ),
                "got: {status:?}"
            );
            let mut events = Vec::new();
            while let Ok(ev) = rx.try_recv() {
                events.push(ev);
            }
            assert!(!events.iter().any(|e| matches!(
                e, LogEvent::Stdout { line, .. } if line.contains("NEVER_RAN")
            )));
        }

        // 通过路径：exit 0 → 步骤照常执行
        {
            let (tx, mut rx) = mpsc::channel(1024);
            let (_c, cancel_rx) = broadcast::channel(4);
            let pre = crate::models::Precondition {
                check: "cmd /C echo ok".to_string(),
                message: String::new(),
            };
            let mut input = simple_job(vec![exec_step("echo PREFLIGHT_PASSED")]);
            input.job.preconditions = vec![pre];
            let status = rt.block_on(engine.run_job(input, cancel_rx, tx));
            let events = collect(status, &mut rx);
            assert!(events.iter().any(|e| matches!(
                e, LogEvent::Stdout { line, .. } if line.contains("PREFLIGHT_PASSED")
            )));
        }
    }

    #[test]
    fn step_outputs_captured_and_shared() {
        // GHA step outputs：正则捕获 stdout → 运行时变量 → 后续步骤 ${{ env.X }} 引用
        let rt = runtime();
        let (tx, mut rx) = mpsc::channel(1024);
        let (_cancel_tx, cancel_rx) = broadcast::channel(4);
        let engine = Engine;

        let mut build = exec_step("cmd /C echo VERSION=9.9.9");
        build.outputs = [("VER".to_string(), r"VERSION=(\S+)".to_string())]
            .into_iter()
            .collect();
        let use_it = exec_step("echo GOT_${{ env.VER }}");

        let input = simple_job(vec![build, use_it]);
        let status = rt.block_on(engine.run_job(input, cancel_rx, tx));
        let events = collect(status, &mut rx);

        assert!(events.iter().any(|e| matches!(
            e, LogEvent::Stdout { line, .. } if line.contains("GOT_9.9.9")
        )));
        assert!(events.iter().any(|e| matches!(
            e, LogEvent::Stdout { line, .. } if line.contains("[outputs] VER = 9.9.9")
        )));
    }

    #[test]
    fn handlers_run_only_when_notified() {
        let rt = runtime();
        let engine = Engine;

        // 触发路径：步骤真执行了 → notify 登记 → job 收尾执行 handler
        {
            let (tx, mut rx) = mpsc::channel(1024);
            let (_c, cancel_rx) = broadcast::channel(4);
            let mut work = exec_step("echo WORK_DONE");
            work.notify = vec!["清理".to_string()];
            let mut cleanup = exec_step("echo HANDLER_RAN");
            cleanup.name = Some("清理".to_string());
            let mut input = simple_job(vec![work]);
            input.job.handlers = vec![cleanup];
            let status = rt.block_on(engine.run_job(input, cancel_rx, tx));
            let events = collect(status, &mut rx);
            assert!(events.iter().any(|e| matches!(
                e, LogEvent::Stdout { line, .. } if line.contains("HANDLER_RAN")
            )));
            assert!(events.iter().any(|e| matches!(
                e, LogEvent::Stdout { line, .. } if line.contains("[handler] 执行「清理」")
            )));
        }

        // 不触发路径：creates 跳过的步骤不算变更 → handler 不执行
        {
            let dir = std::env::temp_dir().join(format!("rbs_notify_{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("marker.txt"), "x").unwrap();

            let (tx, mut rx) = mpsc::channel(1024);
            let (_c, cancel_rx) = broadcast::channel(4);
            let mut skipped_step = exec_step("echo SKIPPED_WORK");
            skipped_step.creates = Some("marker.txt".to_string());
            skipped_step.notify = vec!["清理".to_string()];
            let mut cleanup = exec_step("echo HANDLER_RAN");
            cleanup.name = Some("清理".to_string());
            let mut input = simple_job(vec![skipped_step]);
            input.job.handlers = vec![cleanup];
            input.project.local_path = dir.to_string_lossy().to_string();
            let status = rt.block_on(engine.run_job(input, cancel_rx, tx));
            let events = collect(status, &mut rx);
            assert!(!events.iter().any(|e| matches!(
                e, LogEvent::Stdout { line, .. } if line.contains("HANDLER_RAN")
            )), "creates 跳过不应触发 handler: {events:?}");
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn wait_tcp_probe_succeeds() {
        // wait tcp 模式：监听一个真实端口，探测应立即就绪
        let (tx_port, rx_port) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            tx_port.send(port).unwrap();
            for stream in listener.incoming() {
                drop(stream);
            }
        });
        let port = rx_port.recv().unwrap();

        let rt = runtime();
        let (tx, mut rx) = mpsc::channel(1024);
        let (_c, cancel_rx) = broadcast::channel(4);
        let engine = Engine;

        let mut wait = exec_step("echo unused");
        wait.step_type = StepType::Wait;
        wait.wait = Some(crate::models::WaitProbe {
            mode: crate::models::WaitMode::Tcp,
            target: format!("127.0.0.1:{port}"),
            interval_seconds: 0,
        });
        let after = exec_step("echo AFTER_WAIT");

        let input = simple_job(vec![wait, after]);
        let status = rt.block_on(engine.run_job(input, cancel_rx, tx));
        let events = collect(status, &mut rx);
        assert!(events.iter().any(|e| matches!(
            e, LogEvent::Stdout { line, .. } if line.contains("AFTER_WAIT")
        )));
        assert!(events.iter().any(|e| matches!(
            e, LogEvent::Stdout { line, .. } if line.contains("tcp 127.0.0.1")
        )));
    }

    #[test]
    fn partial_success_reports_yellow() {
        // GHA 黄灯：continue_on_error 步骤失败被跳过 → 成功结论 + 汇总
        let rt = runtime();
        let (tx, mut rx) = mpsc::channel(1024);
        let (_c, cancel_rx) = broadcast::channel(4);
        let engine = Engine;

        let mut s0 = exec_step("cmd /C exit 3");
        s0.continue_on_error = true;
        let s1 = exec_step("echo AFTER_FAIL");

        let input = simple_job(vec![s0, s1]);
        let status = rt.block_on(engine.run_job(input, cancel_rx, tx));
        assert!(
            matches!(
                &status,
                JobStatus::SucceededWithFailures { failed_steps } if failed_steps == &vec![0]
            ),
            "got: {status:?}"
        );
        assert!(status.ok(), "黄灯结论仍是成功: {status:?}");
        assert!(status.short().contains("失败被跳过"), "{}", status.short());

        let events = collect(status, &mut rx);
        assert!(events.iter().any(|e| matches!(
            e, LogEvent::Stdout { line, .. } if line.contains("AFTER_FAIL")
        )));
    }

    #[test]
    fn tools_require_gate_the_run() {
        // mise 风格：tools_require 偏差 → 零步失败（人话说明）；满足 → 正常执行
        let rt = runtime();
        let engine = Engine;
        let maven = |ver: &str| crate::models::Tool {
            name: "maven".into(),
            tool_type: crate::models::ToolType::System,
            command: "mvn".into(),
            version: Some(ver.into()),
            ..Default::default()
        };

        // 失败路径：要求 >=99，探测 21.0.3
        {
            let (tx, mut rx) = mpsc::channel(1024);
            let (_c, cancel_rx) = broadcast::channel(4);
            let mut input = simple_job(vec![exec_step("echo NEVER_RAN")]);
            input.project.tools_require.insert("maven".into(), ">=99".into());
            input.tools.insert("maven".to_string(), maven("21.0.3"));
            let status = rt.block_on(engine.run_job(input, cancel_rx, tx));
            assert!(
                matches!(
                    &status,
                    JobStatus::Failed { step_index, reason }
                        if *step_index == JobStatus::PRECONDITION_STEP
                            && reason.contains("工具版本偏差")
                ),
                "got: {status:?}"
            );
            let mut events = Vec::new();
            while let Ok(ev) = rx.try_recv() {
                events.push(ev);
            }
            assert!(!events.iter().any(|e| matches!(
                e, LogEvent::Stdout { line, .. } if line.contains("NEVER_RAN")
            )));
        }

        // 通过路径：>=21 满足 → 步骤照常执行
        {
            let (tx, mut rx) = mpsc::channel(1024);
            let (_c, cancel_rx) = broadcast::channel(4);
            let mut input = simple_job(vec![exec_step("echo TOOL_OK")]);
            input.project.tools_require.insert("maven".into(), ">=21".into());
            input.tools.insert("maven".to_string(), maven("21.0.3"));
            let status = rt.block_on(engine.run_job(input, cancel_rx, tx));
            let events = collect(status, &mut rx);
            assert!(events.iter().any(|e| matches!(
                e, LogEvent::Stdout { line, .. } if line.contains("TOOL_OK")
            )));
            assert!(events.iter().any(|e| matches!(
                e, LogEvent::Stdout { line, .. } if line.contains("满足 >=21")
            )));
        }

        // 未定义工具 → 明确报错（而非静默放过）
        {
            let (tx, _rx) = mpsc::channel(1024);
            let (_c, cancel_rx) = broadcast::channel(4);
            let mut input = simple_job(vec![]);
            input.project.tools_require.insert("nope".into(), ">=1".into());
            let status = rt.block_on(engine.run_job(input, cancel_rx, tx));
            assert!(
                matches!(&status, JobStatus::Failed { reason, .. } if reason.contains("未定义")),
                "got: {status:?}"
            );
        }
    }

    #[test]
    fn when_condition_value_comparison_skips() {
        // when（condition 别名）：等值比较，条件不满足 → 跳过该步、继续后面步骤
        let rt = runtime();
        let (tx, mut rx) = mpsc::channel(1024);
        let (_cancel_tx, cancel_rx) = broadcast::channel(4);
        let engine = Engine;

        let mut s_run = exec_step("echo WHEN_RAN");
        s_run.condition = Some("${{ variables.GREETING }} == hi".into());
        let mut s_skip = exec_step("echo WHEN_NOT_RAN");
        s_skip.condition = Some("${{ variables.GREETING }} == prod".into());

        let input = simple_job(vec![s_run, s_skip]);
        let status = rt.block_on(engine.run_job(input, cancel_rx, tx));
        let events = collect(status, &mut rx);

        assert!(events.iter().any(|e| matches!(
            e, LogEvent::Stdout { line, .. } if line.contains("WHEN_RAN")
        )));
        assert!(!events.iter().any(|e| matches!(
            e, LogEvent::Stdout { line, .. } if line.contains("WHEN_NOT_RAN")
        )));
        assert!(events.iter().any(|e| matches!(e, LogEvent::StepStart { skipped: true, .. })));
    }

    #[test]
    fn creates_guard_skips_second_run() {
        // creates 守卫：产物已存在的重跑直接跳过（重跑秒过已完成步骤）
        let dir = std::env::temp_dir().join(format!("rbs_creates_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let rt = runtime();
        let engine = Engine;
        let step = Step {
            name: Some("建目录".into()),
            command: "cmd /C mkdir rbs_creates_dir".to_string(),
            step_type: StepType::Exec,
            enabled: true,
            creates: Some("rbs_creates_dir".to_string()),
            ..Default::default()
        };
        let mk_input = || {
            let mut input = simple_job(vec![step.clone()]);
            input.project.local_path = dir.to_string_lossy().to_string();
            input
        };

        // 第一次：目录不存在 → 执行 → 目录被创建
        {
            let (tx, mut rx) = mpsc::channel(1024);
            let (_c, cancel_rx) = broadcast::channel(4);
            let status = rt.block_on(engine.run_job(mk_input(), cancel_rx, tx));
            let events = collect(status, &mut rx);
            assert!(events.iter().any(|e| matches!(e, LogEvent::StepStart { skipped: false, .. })));
        }
        assert!(dir.join("rbs_creates_dir").exists(), "第一次运行应创建目录");

        // 第二次：creates 已存在 → 跳过
        {
            let (tx, mut rx) = mpsc::channel(1024);
            let (_c, cancel_rx) = broadcast::channel(4);
            let status = rt.block_on(engine.run_job(mk_input(), cancel_rx, tx));
            let events = collect(status, &mut rx);
            assert!(
                events.iter().any(|e| matches!(
                    e,
                    LogEvent::StepStart { name, skipped: true, .. } if name.contains("creates")
                )),
                "第二次运行应显示 creates 跳过: {events:?}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn local_creates_glob() {
        // creates 通配：target/*.jar 任一匹配存在即算
        let dir = std::env::temp_dir().join(format!("rbs_creates_glob_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("target")).unwrap();
        assert!(!local_creates_exists(&dir, "target/*.jar"));
        std::fs::write(dir.join("target").join("a.jar"), "x").unwrap();
        assert!(local_creates_exists(&dir, "target/*.jar"));
        // 字面量路径
        assert!(local_creates_exists(&dir, "target"));
        assert!(!local_creates_exists(&dir, "no_such_thing"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn job_level_timeout_stops_remaining() {
        // job.timeout 接线：作业级超时 → JobStatus::Timeout，剩余步骤不启动
        let rt = runtime();
        let (tx, mut rx) = mpsc::channel(1024);
        let (_cancel_tx, cancel_rx) = broadcast::channel(4);
        let engine = Engine;

        let mut s = exec_step("ping -n 6 127.0.0.1 > nul");
        s.timeout = Some(30); // 步骤级给足，超时必须来自 job 级
        let mut input = simple_job(vec![s, exec_step("echo NEVER_RAN")]);
        input.job.timeout = Some(1);
        let status = rt.block_on(engine.run_job(input, cancel_rx, tx));
        assert!(matches!(status, JobStatus::Timeout), "got: {status:?}");

        let mut events = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            events.push(ev);
        }
        assert!(!events.iter().any(|e| matches!(
            e, LogEvent::Stdout { line, .. } if line.contains("NEVER_RAN")
        )));
    }

    #[test]
    fn default_step_timeout_wiring() {
        // config default_timeout 接线：RunInput.default_step_timeout 作为未配置步骤的兜底
        let rt = runtime();
        let (tx, _rx) = mpsc::channel(1024);
        let (_cancel_tx, cancel_rx) = broadcast::channel(4);
        let engine = Engine;

        let mut input = simple_job(vec![exec_step("ping -n 6 127.0.0.1 > nul")]);
        input.default_step_timeout = 1;
        let status = rt.block_on(engine.run_job(input, cancel_rx, tx));
        assert!(matches!(status, JobStatus::Timeout), "got: {status:?}");
    }

    #[test]
    fn cancel_kills_running_step_quickly() {
        // 取消路径（问题 1）：运行中的长步骤收到取消信号应立即终止整棵进程树，
        // 而不是等步骤自然结束
        let rt = runtime();
        let (tx, _rx) = mpsc::channel(1024);
        let (cancel_tx, cancel_rx) = broadcast::channel(4);
        let engine = Engine;

        let input = simple_job(vec![exec_step("ping -n 30 127.0.0.1 > nul")]);
        let sender = rt.spawn(async move {
            tokio::time::sleep(Duration::from_millis(500)).await;
            let _ = cancel_tx.send(());
        });
        let started = std::time::Instant::now();
        let status = rt.block_on(engine.run_job(input, cancel_rx, tx));
        let elapsed = started.elapsed();
        let _ = rt.block_on(sender);
        assert!(matches!(status, JobStatus::Cancelled), "got: {status:?}");
        assert!(
            elapsed < Duration::from_secs(5),
            "取消应立即生效而非等步骤自然结束: {elapsed:?}"
        );
    }

    #[test]
    fn preview_flags_unresolved_vars_missing_sources_and_creates() {
        // 干跑预览：变量拼错 ⚠ / 上传源缺失 ⚠ / creates 已存在 → 跳过；不发任何网络请求
        let dir = std::env::temp_dir().join(format!("rbs_preview_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("exists_marker.txt"), "x").unwrap();

        let upload = Step {
            name: Some("上传".into()),
            command: "missing/nope.jar -> app.jar".to_string(),
            step_type: StepType::Upload,
            enabled: true,
            target: Some("prod".into()),
            ..Default::default()
        };
        let typo = exec_step("echo ${{ variables.TYPO }}");
        let done = Step {
            name: Some("已完成步骤".into()),
            command: "echo done".to_string(),
            step_type: StepType::Exec,
            enabled: true,
            creates: Some("exists_marker.txt".to_string()),
            ..Default::default()
        };

        let mut input = simple_job(vec![upload, typo, done]);
        input.project.local_path = dir.to_string_lossy().to_string();
        input.all_targets.insert(
            "prod".to_string(),
            Target {
                name: "prod".to_string(),
                target_type: TargetType::Ssh {
                    host: "1.2.3.4".to_string(),
                    port: 22,
                    user: "u".to_string(),
                    auth: crate::models::AuthMethod::None,
                    host_key: None,
                },
                work_root: Some("/srv".to_string()),
                ..Default::default()
            },
        );

        let previews = preview_job(&input);
        assert_eq!(previews.len(), 3);
        // 上传源缺失 → 警告；相对远端路径应拼上 work_root
        assert!(
            previews[0].warnings.iter().any(|w| w.contains("上传源不存在")),
            "{:?}",
            previews[0].warnings
        );
        assert_eq!(previews[0].uploads.len(), 1);
        assert_eq!(previews[0].uploads[0].1, "/srv/app.jar");
        // 变量拼写错误 → 警告，占位符原样展示
        assert!(
            previews[1].warnings.iter().any(|w| w.contains("未解析")),
            "{:?}",
            previews[1].warnings
        );
        assert_eq!(previews[1].command, "echo ${{ variables.TYPO }}");
        // creates 已存在 → 跳过
        assert!(previews[2].skipped, "{:?}", previews[2]);
        let _ = std::fs::remove_dir_all(&dir);
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
