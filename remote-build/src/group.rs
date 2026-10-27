//! 组运行（第五批，Spinnaker/Octopus 参照讨论的落点）：
//! 分组内流程按项目树顺序**串行**执行——失败即停（金丝雀"任一批失败不碰剩余机器"），
//! 锁由引擎逐流程获取/释放（串行天然安全；并行才会锁互杀，故组内只做串行），
//! 确认门在启动前统一收口（取组内最严确认词，GUI/CLI 各自实现提示与输入）。
//!
//! GUI 与 CLI 共用本模块的执行循环：调用方只组装 GroupSpec 与 GroupSink
//! （事件出口），调度、历史入库、日志落盘、取消传播都在 run_group_serial。

use crate::engine::{Engine, RunInput};
use crate::models::{Job, JobStatus, LogEvent, Project, Target, Tool};
use async_trait::async_trait;
use std::collections::HashMap;
use tokio::sync::broadcast;

/// 组内一个待执行流程（顺序 = 项目树顺序，即 jobs 列表过滤后的相对顺序）
#[derive(Clone)]
pub struct GroupJob {
    pub name: String,
    pub job: Job,
}

/// 组运行参数（GUI / CLI 各自组装）
pub struct GroupSpec {
    pub project: Project,
    pub group: String,
    pub jobs: Vec<GroupJob>,
    pub tools: HashMap<String, Tool>,
    pub all_targets: HashMap<String, Target>,
    pub default_step_timeout: u64,
    /// 组运行唯一标识（{项目}-grp-时间戳）：成员流程 run_id = {group_id}-{序号}，
    /// 历史里同前缀 = 同一次组发布
    pub group_id: String,
    /// 取消发送端：GUI Stop / CLI 生命周期；锁被接管时引擎也用它自杀——
    /// 取消当前流程即停组，后续流程不再启动
    pub cancel_tx: Option<broadcast::Sender<()>>,
    /// 从第几个流程开始（续跑：上次组运行失败处）
    pub from_job: usize,
    pub save_history: bool,
    /// GUI 落日志文件的目录（{run_id}.log）；None = 不落（CLI 现状）
    pub log_dir: Option<std::path::PathBuf>,
    /// 历史入库的触发来源（RunMeta.trigger：manual / watch / cli / group）
    pub trigger: &'static str,
}

/// 执行结果：首个非成功流程即停
#[derive(Debug, Clone)]
pub struct GroupOutcome {
    /// (流程名, 状态)，按执行先后
    pub results: Vec<(String, JobStatus)>,
    /// 首个未通过流程的下标（含取消：续跑应从它重试）；全部通过 = None
    pub failed: Option<usize>,
    /// 运行被取消（Stop / 锁被接管）
    pub cancelled: bool,
    pub total_duration_ms: u64,
}

impl GroupOutcome {
    pub fn ok(&self) -> bool {
        self.failed.is_none() && !self.cancelled
    }

    /// 组级聚合状态：整个组结束时发 JobEnd 用（UI finished / CLI 退出码）
    pub fn status(&self) -> JobStatus {
        if self.cancelled {
            JobStatus::Cancelled
        } else if let Some(i) = self.failed {
            let (job, st) = &self.results[i];
            JobStatus::Failed {
                step_index: 0,
                reason: format!("组运行在流程「{job}」中止（{}）", st.short()),
            }
        } else {
            JobStatus::Success
        }
    }
}

/// 事件出口：GUI = 转发 UI 通道；CLI = 控制台打印；测试 = 收集
#[async_trait]
pub trait GroupSink: Send + Sync {
    async fn on_event(&self, ev: LogEvent);
}

/// 组运行执行循环：按序逐个执行，任一流程未通过（或被取消）即停。
/// 锁、重试、超时、确认语义全部复用引擎单流程行为——本循环只做调度与边界事件。
pub async fn run_group_serial(spec: GroupSpec, sink: &dyn GroupSink) -> GroupOutcome {
    let started = std::time::Instant::now();
    let total = spec.jobs.len();
    let mut results: Vec<(String, JobStatus)> = Vec::new();
    let mut failed = None;
    let mut cancelled = false;
    let mut cancel_rx = spec.cancel_tx.as_ref().map(|tx| tx.subscribe());

    for (i, gj) in spec.jobs.iter().enumerate().skip(spec.from_job) {
        // 上一流程结束后检查取消信号（Stop / 锁被接管）：后续流程不再启动
        if let Some(rx) = &mut cancel_rx {
            match rx.try_recv() {
                Ok(_) => {
                    cancelled = true;
                    break;
                }
                // Closed = 发送端已 drop（运行被整体丢弃）；Lagged = 取消爆发漏读，同样视为取消
                Err(broadcast::error::TryRecvError::Closed)
                | Err(broadcast::error::TryRecvError::Lagged(_)) => {
                    cancelled = true;
                    break;
                }
                Err(broadcast::error::TryRecvError::Empty) => {}
            }
        }

        let run_id = format!("{}-{:02}", spec.group_id, i + 1);
        let snapshot = yaml_serde::to_string(&gj.job).unwrap_or_default();
        #[cfg(feature = "sqlite")]
        let meta = crate::storage::sqlite::RunMeta {
            run_id: run_id.clone(),
            project: spec.project.name.clone(),
            job: gj.job.name.clone(),
            target: gj.job.target.clone(),
            trigger: spec.trigger,
            log_path: spec
                .log_dir
                .as_ref()
                .map(|d| d.join(format!("{run_id}.log")).to_string_lossy().to_string()),
        };
        let _ = sink
            .on_event(LogEvent::GroupJobBegin {
                run_id: run_id.clone(),
                group: spec.group.clone(),
                project: spec.project.name.clone(),
                job: gj.job.name.clone(),
                index: i,
                total,
            })
            .await;

        // 历史入库 + 日志落盘：与单流程运行同一套路径（GUI 落文件，CLI 不落）
        #[cfg(feature = "sqlite")]
        let recorder = if spec.save_history {
            crate::storage::sqlite::RunRecorder::start(&meta, &snapshot, gj.job.steps.len()).await
        } else {
            None
        };
        let mut file = spec.log_dir.as_ref().and_then(|d| {
            let _ = std::fs::create_dir_all(d);
            std::fs::File::create(d.join(format!("{run_id}.log"))).ok()
        });

        let (tok_tx, mut tok_rx) = tokio::sync::mpsc::channel(8192);
        let input = RunInput {
            project: spec.project.clone(),
            job: gj.job.clone(),
            tools: spec.tools.clone(),
            all_targets: spec.all_targets.clone(),
            from_step: 0,
            only_step: None,
            run_id: run_id.clone(),
            default_step_timeout: spec.default_step_timeout,
            cancel_tx: spec.cancel_tx.clone(),
        };
        let job_started = std::time::Instant::now();
        let rx_for_engine = match &mut cancel_rx {
            Some(rx) => rx.resubscribe(),
            None => crate::engine::detached_cancel_rx(),
        };
        let handle = tokio::spawn(async move {
            // Engine 零尺寸类型在任务内构造（future 必须 'static，不能借用局部）
            Engine.run_job(input, rx_for_engine, tok_tx).await
        });
        while let Some(ev) = tok_rx.recv().await {
            #[cfg(feature = "sqlite")]
            if let Some(rec) = &recorder {
                rec.on_event(&ev).await;
            }
            if let Some(f) = file.as_mut() {
                if let Some(text) = format_event(&ev) {
                    use std::io::Write;
                    let _ = writeln!(f, "{text}");
                }
            }
            sink.on_event(ev).await;
        }
        let status = handle
            .await
            .unwrap_or(JobStatus::Failed { step_index: 0, reason: "引擎任务异常".to_string() });
        let duration_ms = job_started.elapsed().as_millis() as u64;

        let _ = sink
            .on_event(LogEvent::GroupJobEnd {
                run_id,
                group: spec.group.clone(),
                project: spec.project.name.clone(),
                job: gj.job.name.clone(),
                index: i,
                total,
                status: status.clone(),
                duration_ms,
            })
            .await;
        // 历史收尾：JobEnd 闭合 runs 行后关录制器
        #[cfg(feature = "sqlite")]
        if let Some(rec) = recorder {
            rec.on_event(&LogEvent::JobEnd {
                run_id: meta.run_id,
                status: status.clone(),
                total_duration_ms: duration_ms,
            })
            .await;
            rec.close().await;
        }

        results.push((gj.job.name.clone(), status));
        if !results.last().map(|(_, st)| st.ok()).unwrap_or(false) {
            if matches!(results.last().map(|(_, st)| st), Some(JobStatus::Cancelled)) {
                cancelled = true;
            } else {
                failed = Some(i);
            }
            break;
        }
    }

    GroupOutcome {
        results,
        failed,
        cancelled,
        total_duration_ms: started.elapsed().as_millis() as u64,
    }
}

/// 组级预览（确认门前展示）：各流程干跑连展的紧凑文本行。
/// 同步无副作用（preview_job 不执行命令、不发网络请求）。
/// GUI 确认门窗口与 CLI run-group 共用；续跑场景传 `&jobs[from..]` 只展剩余。
pub fn plan_lines(
    project: &Project,
    jobs: &[GroupJob],
    tools: &HashMap<String, Tool>,
    all_targets: &HashMap<String, Target>,
    default_step_timeout: u64,
) -> Vec<String> {
    let total = jobs.len();
    let mut lines = Vec::new();
    for (i, gj) in jobs.iter().enumerate() {
        let input = RunInput {
            project: project.clone(),
            job: gj.job.clone(),
            tools: tools.clone(),
            all_targets: all_targets.clone(),
            from_step: 0,
            only_step: None,
            run_id: String::new(),
            default_step_timeout,
            cancel_tx: None,
        };
        let previews = crate::engine::preview_job(&input);
        let target = gj
            .job
            .target
            .clone()
            .unwrap_or_else(|| "local".to_string());
        lines.push(format!(
            "【{}/{total}】{}（目标 {target}，{} 步）",
            i + 1,
            gj.name,
            previews.len()
        ));
        if let Some(c) = gj.job.confirm.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            lines.push(format!("  🔒 确认门: 「{c}」"));
        }
        for pre in &gj.job.preconditions {
            let msg = if pre.message.trim().is_empty() {
                String::new()
            } else {
                format!("（失败: {}）", pre.message.trim())
            };
            lines.push(format!("  前置检查: {}{msg}", pre.check));
        }
        for p in &previews {
            if p.skipped {
                lines.push(format!("  [{}] {} ⏭ 跳过：{}", p.index + 1, p.name, p.note.as_deref().unwrap_or("")));
            } else {
                let tgt = p.target.clone().unwrap_or_else(|| "local".to_string());
                lines.push(format!("  [{}] {} → {tgt}", p.index + 1, p.name));
                if !p.command.is_empty() {
                    lines.push(format!("      $ {}", p.command));
                }
                for (l, r, exists) in &p.uploads {
                    lines.push(format!(
                        "      ↑ {l} -> {r}{}",
                        if *exists { "" } else { "（本地缺失 ⚠）" }
                    ));
                }
            }
            for w in &p.warnings {
                lines.push(format!("      ⚠ {w}"));
            }
        }
    }
    lines
}

/// 组运行事件行格式化（日志文件落盘用）。单流程事件与 GUI 的格式一致；
/// 组边界事件补分隔行。app.rs 的单流程转发也用本函数（单一出处）。
pub(crate) fn format_event(ev: &LogEvent) -> Option<String> {
    match ev {
        LogEvent::Stdout { line, .. } => Some(line.clone()),
        LogEvent::Stderr { line, .. } => Some(format!("[stderr] {line}")),
        LogEvent::StepStart { step_index, name, .. } => {
            Some(format!("── Step {} ── {name}", step_index + 1))
        }
        LogEvent::StepEnd { step_index, exit_code, duration_ms } => {
            Some(format!("── Step {} 退出码 {exit_code} ({duration_ms}ms)", step_index + 1))
        }
        // 退避等待期已有"第 N 次尝试失败…后重试"告警行，不重复落盘
        LogEvent::StepRetryWait { .. } => None,
        LogEvent::JobEnd { status, total_duration_ms, .. } => {
            Some(format!("==== {} ({total_duration_ms}ms) ====", status.short()))
        }
        LogEvent::Error { message } => Some(format!("[error] {message}")),
        LogEvent::GroupJobBegin { job, index, total, .. } => {
            Some(format!("──── 组运行 [{index}/{total}] 启动流程 {job} ────"))
        }
        LogEvent::GroupJobEnd { job, status, duration_ms, .. } => {
            Some(format!("──── 组流程 {job}: {} ({duration_ms}ms) ────", status.short()))
        }
    }
}

/// 确认门放行判定：输入必须**包含全部**确认词（按空白分词）。
/// 单确认词退化为相等判断的超集（多打了字不放行——必须恰好包含期望词），
/// 组多确认词场景（如打包 prod + 发布 prod 两个流程都设门）一次输入全部通过。
pub fn confirm_words_match(input: &str, expected_words: &[String]) -> bool {
    let tokens: Vec<&str> = input.split_whitespace().collect();
    !expected_words.is_empty()
        && expected_words.iter().all(|w| tokens.contains(&w.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{Step, StepType};
    use std::sync::{Arc, Mutex};

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Runtime::new().unwrap()
    }

    #[derive(Default, Clone)]
    struct VecSink(Arc<Mutex<Vec<LogEvent>>>);

    #[async_trait]
    impl GroupSink for VecSink {
        async fn on_event(&self, ev: LogEvent) {
            self.0.lock().unwrap().push(ev);
        }
    }

    fn group_job(name: &str, command: &str) -> GroupJob {
        GroupJob {
            name: name.to_string(),
            job: Job {
                name: name.to_string(),
                project: "p".to_string(),
                steps: vec![Step {
                    name: Some("s".to_string()),
                    command: command.to_string(),
                    step_type: StepType::Exec,
                    enabled: true,
                    ..Default::default()
                }],
                ..Default::default()
            },
        }
    }

    fn spec(dir: &std::path::Path, jobs: Vec<GroupJob>, from: usize) -> GroupSpec {
        GroupSpec {
            project: Project {
                name: "p".to_string(),
                local_path: dir.to_string_lossy().to_string(),
                ..Default::default()
            },
            group: "g".to_string(),
            jobs,
            tools: Default::default(),
            all_targets: Default::default(),
            default_step_timeout: crate::engine::DEFAULT_STEP_TIMEOUT,
            group_id: "p-grp-test".to_string(),
            cancel_tx: None,
            from_job: from,
            save_history: false,
            log_dir: None,
            trigger: "manual",
        }
    }

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("rbs-group-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn group_runs_in_order_and_stops_on_failure() {
        // 金丝雀分批语义：任一流程失败 → 后续不再启动（三号流程的产物不应出现）
        let dir = temp_dir("fail-stop");
        let rt = rt();
        let sink = VecSink::default();
        let jobs = vec![
            group_job("一", "echo done> m1.txt"),
            group_job("二", "cmd /C exit 5"),
            group_job("三", "echo done> m3.txt"),
        ];
        let outcome = rt.block_on(run_group_serial(spec(&dir, jobs, 0), &sink));
        assert_eq!(outcome.results.len(), 2, "失败即停: {:?}", outcome.results);
        assert_eq!(outcome.failed, Some(1));
        assert!(!outcome.cancelled);
        assert!(!outcome.ok());
        assert!(matches!(outcome.status(), JobStatus::Failed { reason, .. } if reason.contains("二")));
        assert!(dir.join("m1.txt").exists(), "第一个流程应已执行");
        assert!(!dir.join("m3.txt").exists(), "失败后的流程不应执行");

        let evs = sink.0.lock().unwrap();
        assert_eq!(evs.iter().filter(|e| matches!(e, LogEvent::GroupJobBegin { .. })).count(), 2);
        assert_eq!(evs.iter().filter(|e| matches!(e, LogEvent::GroupJobEnd { .. })).count(), 2);
        assert!(
            !evs.iter().any(|e| matches!(e, LogEvent::JobEnd { .. })),
            "聚合 JobEnd 由调用方发，循环内只发组边界事件"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn group_resume_skips_leading_jobs() {
        // 续跑：from_job=1 → 第一个流程不执行，其余正常走完
        let dir = temp_dir("resume");
        let rt = rt();
        let sink = VecSink::default();
        let jobs = vec![
            group_job("一", "echo done> m1.txt"),
            group_job("二", "echo done> m2.txt"),
            group_job("三", "echo done> m3.txt"),
        ];
        let outcome = rt.block_on(run_group_serial(spec(&dir, jobs, 1), &sink));
        assert!(outcome.ok(), "got: {:?}", outcome.results);
        assert_eq!(outcome.results.len(), 2);
        assert!(!dir.join("m1.txt").exists());
        assert!(dir.join("m2.txt").exists() && dir.join("m3.txt").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn group_continue_on_error_job_does_not_stop_group() {
        // 黄灯流程（continue_on_error）不算组失败，后续照常
        let dir = temp_dir("yellow");
        let rt = rt();
        let sink = VecSink::default();
        let mut yellow = group_job("黄", "cmd /C exit 3");
        yellow.job.steps[0].continue_on_error = true;
        let jobs = vec![yellow, group_job("三", "echo done> m3.txt")];
        let outcome = rt.block_on(run_group_serial(spec(&dir, jobs, 0), &sink));
        assert!(outcome.ok(), "黄灯不应停组: {:?}", outcome.results);
        assert_eq!(outcome.failed, None);
        assert!(matches!(outcome.status(), JobStatus::Success));
        assert!(dir.join("m3.txt").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn confirm_words_match_requires_all_words() {
        let words = vec!["prod".to_string(), "发布".to_string()];
        assert!(confirm_words_match("prod 发布", &words));
        assert!(confirm_words_match("  发布   prod  ", &words), "顺序无关");
        assert!(!confirm_words_match("prod", &words), "缺一个都不放行");
        assert!(!confirm_words_match("", &words));
        let single = vec!["prod".to_string()];
        assert!(confirm_words_match("prod", &single));
        assert!(!confirm_words_match("product", &single), "整词匹配不是子串匹配");
        assert!(!confirm_words_match("anything", &[]), "无确认词时应走旧路径而非本函数");
    }
}
