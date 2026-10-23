//! egui App 主体与屏幕路由（设计文档 §2 / §16 app.rs）。
//!
//! 桥接模型：tokio 引擎在后台 runtime 运行，LogEvent 经 tokio mpsc →
//! 转发任务（追加日志文件）→ std mpsc，UI 每帧 try_recv 排空后渲染（§4.2）。

use crate::models::{JobStatus, LogEvent, StepType};
use crate::storage::files::{self, ProjectEntry};
use crate::ui;
use crate::ui::status::StepVisualState;
use eframe::egui;
use std::sync::mpsc as std_mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, mpsc};

// ---------------------------------------------------------------------------
// 顶层 App：首次启动引导 / 主界面 两个屏幕
// ---------------------------------------------------------------------------

pub struct RemoteBuildApp {
    pub runtime: Arc<tokio::runtime::Runtime>,
    pub page: Page,
}

pub enum Page {
    FirstRun(ui::first_run::FirstRunPage),
    Main(MainState),
}

impl RemoteBuildApp {
    pub fn new(cc: &eframe::CreationContext<'_>, runtime: Arc<tokio::runtime::Runtime>) -> Self {
        // 排查开关：RBS_NO_CJK=1 跳过 CJK 字体加载（隔离字体/渲染层问题用）
        if std::env::var("RBS_NO_CJK").unwrap_or_default() != "1" {
            load_cjk_font(&cc.egui_ctx);
        }
        if crate::config::is_first_run() {
            let page = Page::FirstRun(ui::first_run::FirstRunPage::new());
            Self { runtime, page }
        } else {
            let main = MainState::new(runtime.clone());
            Self { runtime, page: Page::Main(main) }
        }
    }
}

impl eframe::App for RemoteBuildApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        match &mut self.page {
            Page::FirstRun(page) => {
                page.show(ui);
                if page.confirmed() {
                    if let Some(report) = page.report.clone() {
                        match crate::config::first_run_generate(&report) {
                            Ok(()) => {
                                self.page = Page::Main(MainState::new(self.runtime.clone()));
                            }
                            Err(e) => page.error = Some(e.to_string()),
                        }
                    }
                }
            }
            Page::Main(main) => main.show(ui),
        }
    }
}

/// 中文字体：egui 默认字体无 CJK 字形，加载微软雅黑（缺失时回退默认）
fn load_cjk_font(ctx: &egui::Context) {
    // 非 TTC 的 .ttf 优先：egui 0.37 新字体引擎解析 msyh.ttc 等 TrueType Collection
    // 会直接段错误（升级实测），TTC 仅作最后兜底
    const CANDIDATES: &[&str] = &[
        "C:/Windows/Fonts/msyh.ttf",
        "C:/Windows/Fonts/Deng.ttf",   // 等线，Win10/11 标配
        "C:/Windows/Fonts/simhei.ttf",
        "C:/Windows/Fonts/DENG.TTF",
        "C:/Windows/Fonts/msyh.ttc",
        "C:/Windows/Fonts/simsun.ttc",
    ];
    for path in CANDIDATES {
        if let Ok(bytes) = std::fs::read(path) {
            let mut fonts = egui::FontDefinitions::default();
            fonts.font_data.insert("cjk".to_string(), Arc::new(egui::FontData::from_owned(bytes)));
            fonts
                .families
                .entry(egui::FontFamily::Proportional)
                .or_default()
                .insert(0, "cjk".to_string());
            fonts
                .families
                .entry(egui::FontFamily::Monospace)
                .or_default()
                .push("cjk".to_string());
            ctx.set_fonts(fonts);
            return;
        }
    }
}

// ---------------------------------------------------------------------------
// 主界面状态
// ---------------------------------------------------------------------------

pub struct MainState {
    pub runtime: Arc<tokio::runtime::Runtime>,
    pub workspace_root: std::path::PathBuf,
    pub global: crate::models::GlobalConfig,
    pub tools: std::collections::HashMap<String, crate::models::Tool>,
    pub projects: Vec<ProjectEntry>,
    pub selected_project: Option<usize>,
    pub selected_job: Option<usize>,
    pub console: ui::console::ConsoleState,
    pub run: Option<RunHandle>,
    pub show_settings: bool,
    pub tool_editor: ui::settings::ToolEditorState,
    pub new_project: ui::new_project::NewProjectState,
    pub new_job: ui::job_editor::NewJobState,
    /// 删除流程的两段确认状态
    pub delete_job_armed: bool,
    /// 项目树右键菜单：待确认的删除目标（弹窗确认后才执行）
    pub tree_delete: Option<ui::project_tree::TreeDeleteTarget>,
    /// 项目树右键菜单：改名目标 + 输入缓冲
    pub tree_rename: Option<ui::project_tree::TreeRenameTarget>,
    pub tree_rename_buf: String,
    pub target_editor: ui::targets::TargetEditorState,
    /// 设置窗口当前分区 tab
    pub settings_tab: ui::settings::SettingsTab,
    /// 「？ 变量语法」帮助窗口
    pub show_var_help: bool,
    /// 主题是否已在首帧应用（egui ctx 需要在 show 阶段才能设置）
    pub theme_applied: bool,
    pub status_msg: Option<String>,
    /// 步骤运行状态可视化（ui::status）：drain_events 按事件驱动，start_run 复位
    pub run_steps: Vec<StepVisualState>,
    /// 正在运行的作业（项目名, 流程名）——光圈/圆点只对它显示，
    /// 防止运行中在树上切换到别的流程时把状态错挂到别的作业上
    pub run_job: Option<(String, String)>,
    /// 运行作业各步骤是否为 wait 类型（轮询期间该步骤显示黄色等待态）
    pub run_step_wait: Vec<bool>,
}

pub struct RunHandle {
    /// UI Stop 按钮向它发送取消信号（§12.2 broadcast 模型）
    pub cancel_tx: broadcast::Sender<()>,
    /// 引擎事件（转发任务从 tokio mpsc 搬运而来）
    pub events: std_mpsc::Receiver<LogEvent>,
    pub join: tokio::task::JoinHandle<()>,
    pub finished: Option<JobStatus>,
}

impl MainState {
    pub fn new(runtime: Arc<tokio::runtime::Runtime>) -> Self {
        let mut s = Self {
            runtime,
            workspace_root: crate::config::projects_dir(),
            global: Default::default(),
            tools: Default::default(),
            projects: Vec::new(),
            selected_project: None,
            selected_job: None,
            console: Default::default(),
            run: None,
            show_settings: false,
            tool_editor: Default::default(),
            new_project: Default::default(),
            new_job: Default::default(),
            delete_job_armed: false,
            tree_delete: None,
            tree_rename: None,
            tree_rename_buf: String::new(),
            target_editor: Default::default(),
            settings_tab: Default::default(),
            show_var_help: false,
            theme_applied: false,
            status_msg: None,
            run_steps: Vec::new(),
            run_job: None,
            run_step_wait: Vec::new(),
        };
        s.reload();
        s
    }

    pub fn reload(&mut self) {
        self.global = crate::config::load_global().unwrap_or_default();
        let root = if self.global.workspace_root.is_empty() {
            crate::config::projects_dir()
        } else {
            std::path::PathBuf::from(&self.global.workspace_root)
        };
        self.workspace_root = root;
        // 配置解析失败不再静默：直接提示，避免"工具莫名消失"
        let mut warnings = Vec::new();
        self.tools = match crate::config::load_tools() {
            Ok(t) => t.tools,
            Err(e) => {
                warnings.push(format!("tools.yaml 解析失败: {e:#}"));
                Default::default()
            }
        };
        self.projects = files::discover_projects(&self.workspace_root);

        // 收缩选择到有效范围
        if self.projects.is_empty() {
            self.selected_project = None;
            self.selected_job = None;
        } else {
            let pi = self.selected_project.unwrap_or(0).min(self.projects.len() - 1);
            let ji = self
                .selected_job
                .unwrap_or(0)
                .min(self.projects[pi].jobs.len().saturating_sub(1));
            self.selected_project = Some(pi);
            self.selected_job = Some(ji);
        }
        if warnings.is_empty() {
            self.status_msg = Some(format!("已加载 {} 个项目", self.projects.len()));
        } else {
            self.status_msg = Some(format!(
                "⚠ {}（已加载 {} 个项目）",
                warnings.join("；"),
                self.projects.len()
            ));
        }
    }

    /// 按项目名选中（新建项目后自动定位）
    pub fn select_project_by_name(&mut self, name: &str) {
        let Some(idx) = self.projects.iter().position(|p| p.project.name == name) else {
            return;
        };
        self.selected_project = Some(idx);
        self.selected_job = if self.projects[idx].jobs.is_empty() {
            None
        } else {
            Some(0)
        };
    }

    pub fn show(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        // 主题首帧应用（此后设置页切换时即时应用）
        if !self.theme_applied {
            apply_theme(&ctx, &self.global.theme);
            self.theme_applied = true;
        }
        self.drain_events();

        // ── 工具栏 ──────────────────────────────────────────────────
        egui::Panel::top("toolbar").show(ui, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                let running = self.run_active();
                let has_selection = self.selected_project.is_some() && self.selected_job.is_some();
                if ui
                    .add_enabled(has_selection && !running, egui::Button::new("▶ 运行全部"))
                    .on_hover_text("按顺序执行所有启用的步骤（单步调试请用步骤卡片上的「▶ 从此 / ▶ 仅此」）")
                    .clicked()
                {
                    self.start_run(0, None);
                }
                if ui.add_enabled(running, egui::Button::new("⏹ Stop")).clicked() {
                    if let Some(run) = &self.run {
                        let _ = run.cancel_tx.send(());
                    }
                    self.status_msg = Some("已发送取消信号…".to_string());
                }
                // 运行中禁用一切会改盘/改配置的入口（引擎正按路径读写、状态可视化依赖步骤索引稳定）
                if ui.add_enabled(!running, egui::Button::new("💾 Save")).clicked() {
                    self.save_selected();
                }
                if ui.add_enabled(!running, egui::Button::new("⟳ Reload")).clicked() {
                    self.reload();
                }

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.add_enabled(!running, egui::Button::new("⚙ 设置")).clicked() {
                        self.show_settings = !self.show_settings;
                    }
                    if ui.add_enabled(!running, egui::Button::new("🎯 目标")).clicked() {
                        self.target_editor.open = !self.target_editor.open;
                    }
                    if let Some(msg) = &self.status_msg {
                        ui.weak(msg);
                    }
                });
            });
            ui.add_space(2.0);
        });

        // ── 控制台（底部）────────────────────────────────────────────
        let running = self.run_active();
        let console_dark = !matches!(self.global.theme, crate::models::Theme::Light);
        ui::console::show(&mut self.console, ui, running, console_dark);

        // ── 左侧项目树 ──────────────────────────────────────────────
        egui::Panel::left("project_tree")
            .resizable(true)
            .default_size(220.0)
            .show(ui, |ui| {
                ui.add_space(8.0);
                ui::project_tree::show(self, ui);
            });

        // ── 中央步骤编辑器 ──────────────────────────────────────────
        egui::CentralPanel::default().show(ui, |ui| {
            ui::job_editor::show(self, ui);
        });

        if self.show_settings {
            ui::settings::show(&ctx, self);
        }
        if self.target_editor.open {
            ui::targets::show(&ctx, self);
        }

        ui::new_project::show(&ctx, self);
        ui::job_editor::show_new_job_window(&ctx, self);
        ui::job_editor::show_var_help_window(&ctx, self);

        // 运行中保持帧率：日志流畅上屏 + 蓝黄光圈 1Hz 呼吸动画
        if running {
            ctx.request_repaint_after(Duration::from_millis(50));
        }
    }

    pub(crate) fn run_active(&self) -> bool {
        match &self.run {
            None => false,
            Some(r) => r.finished.is_none() && !r.join.is_finished(),
        }
    }

    /// 树上作业行的状态圆点（ui::status）：仅运行中/最近完成的作业有。
    /// 运行中：有步骤在等待（wait 轮询 / 重试退避）= 黄，否则 = 蓝；
    /// 已结束：按最终状态显示（成功绿 / 失败红 / 取消灰），直到下一次 start_run 复位。
    pub fn job_dot_state(&self, project_name: &str, job_name: &str) -> Option<StepVisualState> {
        let (run_proj, run_job_name) = self.run_job.as_ref()?;
        if run_proj != project_name || run_job_name != job_name {
            return None;
        }
        let run = self.run.as_ref()?;
        if let Some(status) = &run.finished {
            return Some(match status {
                JobStatus::Success => StepVisualState::Success,
                JobStatus::Failed { .. } => StepVisualState::Failed,
                JobStatus::Cancelled | JobStatus::Timeout => StepVisualState::Cancelled,
                JobStatus::Running => StepVisualState::Running,
            });
        }
        if self.run_steps.contains(&StepVisualState::Waiting) {
            return Some(StepVisualState::Waiting);
        }
        if self.run_steps.contains(&StepVisualState::Running) {
            return Some(StepVisualState::Running);
        }
        None
    }

    fn drain_events(&mut self) {
        let Some(run) = &mut self.run else { return };
        let mut events = Vec::new();
        while let Ok(ev) = run.events.try_recv() {
            events.push(ev);
        }
        for ev in &events {
            // 步骤状态机（run / run_steps / console 为不同字段，可拆分借用）
            match ev {
                LogEvent::StepStart { step_index, skipped, .. } => {
                    if let Some(st) = self.run_steps.get_mut(*step_index) {
                        *st = if *skipped {
                            StepVisualState::Skipped
                        } else if self.run_step_wait.get(*step_index).copied().unwrap_or(false) {
                            // wait 类型步骤整个执行期都在轮询等待，直接进入黄色等待态
                            StepVisualState::Waiting
                        } else {
                            StepVisualState::Running
                        };
                    }
                }
                LogEvent::StepEnd { step_index, exit_code, .. } => {
                    if let Some(st) = self.run_steps.get_mut(*step_index) {
                        if *st != StepVisualState::Skipped {
                            *st = if *exit_code == 0 {
                                StepVisualState::Success
                            } else {
                                StepVisualState::Failed
                            };
                        }
                    }
                }
                LogEvent::StepRetryWait { step_index, .. } => {
                    if let Some(st) = self.run_steps.get_mut(*step_index) {
                        *st = StepVisualState::Waiting;
                    }
                }
                LogEvent::JobEnd { status, .. } => {
                    run.finished = Some(status.clone());
                    // 取消/超时：被中断的步骤等不到 StepEnd，兜底落终态，避免永远闪蓝
                    if matches!(status, JobStatus::Cancelled | JobStatus::Timeout) {
                        mark_interrupted_cancelled(&mut self.run_steps);
                    }
                }
                _ => {}
            }
            self.console.push_event(ev);
        }
        // 包装任务异常退出但没发 JobEnd 的兜底
        if run.finished.is_none() && run.join.is_finished() {
            run.finished = Some(JobStatus::Failed {
                step_index: 0,
                reason: "引擎任务异常终止".to_string(),
            });
            mark_interrupted_cancelled(&mut self.run_steps);
        }
    }

    pub(crate) fn start_run(&mut self, from_step: usize, only_step: Option<usize>) {
        if self.run_active() {
            return;
        }
        let (pi, ji) = match (self.selected_project, self.selected_job) {
            (Some(a), Some(b)) => (a, b),
            _ => return,
        };
        if self.projects.get(pi).and_then(|p| p.jobs.get(ji)).is_none() {
            return;
        }

        let project = self.projects[pi].project.clone();
        let job = self.projects[pi].jobs[ji].job.clone();

        // 状态可视化复位：新一次运行所有步骤的光圈回 Idle，并记录运行对象与 wait 步骤
        self.run_steps = vec![StepVisualState::Idle; job.steps.len()];
        self.run_step_wait = job
            .steps
            .iter()
            .map(|s| matches!(s.step_type, StepType::Wait))
            .collect();
        self.run_job = Some((project.name.clone(), job.name.clone()));

        // §3.5：项目级目标覆盖全局 targets.yaml（键相同项目优先），并回填凭据键
        let effective = crate::config::effective_targets(&project);
        let mut label = format!("{}/{}", project.name, job.name);
        if only_step.is_some() {
            label.push_str(&format!(" · 仅 Step {}", from_step + 1));
        } else if from_step > 0 {
            label.push_str(&format!(" · 从 Step {}", from_step + 1));
        }

        // §12.2：每个 job 一个 broadcast sender
        let (cancel_tx, cancel_rx) = broadcast::channel(4);
        let (tok_tx, mut tok_rx) = mpsc::channel(8192);
        let (std_tx, std_rx) = std_mpsc::channel();

        let engine = crate::engine::Engine;
        let input = crate::engine::RunInput {
            project: project.clone(),
            job,
            tools: self.tools.clone(),
            all_targets: effective,
            from_step,
            only_step,
        };

        // 日志文件（§11：日志原文落文件，SQLite 只存路径——P1）
        let log_dir = crate::config::logs_dir().join(&project.name);
        let _ = std::fs::create_dir_all(&log_dir);
        let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
        let run_id = format!("{}-{}", project.name, stamp);
        let log_path = log_dir.join(format!("{run_id}.log"));

        let started = Instant::now();
        let inner = {
            let tok_tx = tok_tx.clone();
            self.runtime.spawn(async move { engine.run_job(input, cancel_rx, tok_tx).await })
        };
        let join = self.runtime.spawn(async move {
            let status = inner
                .await
                .unwrap_or(JobStatus::Failed { step_index: 0, reason: "引擎任务异常".to_string() });
            let _ = tok_tx
                .send(LogEvent::JobEnd {
                    run_id,
                    status: status.clone(),
                    total_duration_ms: started.elapsed().as_millis() as u64,
                })
                .await;
        });

        // 转发任务：tokio mpsc → std mpsc（UI）+ 追加日志文件
        self.runtime.spawn(async move {
            let mut file = std::fs::File::create(&log_path).ok();
            while let Some(ev) = tok_rx.recv().await {
                if let Some(f) = file.as_mut() {
                    use std::io::Write;
                    if let Some(text) = format_event(&ev) {
                        let _ = writeln!(f, "{text}");
                    }
                }
                if std_tx.send(ev).is_err() {
                    break; // UI 已关闭
                }
            }
        });

        self.console.push_line(
            format!("==== 启动作业 {label} ===="),
            crate::ui::console::Role::Info,
        );
        self.status_msg = Some(format!("运行中: {label}"));
        self.run = Some(RunHandle { cancel_tx, events: std_rx, join, finished: None });
    }

    fn save_selected(&mut self) {
        let (pi, ji) = match (self.selected_project, self.selected_job) {
            (Some(a), Some(b)) => (a, b),
            _ => return,
        };
        let Some(entry) = self.projects.get_mut(pi) else { return };

        let project_result = files::save_yaml(&entry.dir.join("project.yaml"), &entry.project);
        let job_result = match entry.jobs.get(ji) {
            Some(je) => files::save_yaml(&je.path, &je.job),
            None => Ok(()),
        };

        self.status_msg = Some(match (project_result, job_result) {
            (Ok(()), Ok(())) => "已保存 ✅".to_string(),
            (Err(e), _) | (_, Err(e)) => format!("保存失败: {e}"),
        });
    }
}

/// 取消/超时/异常退出时，仍在运行的步骤等不到 StepEnd，兜底落中性终态（避免永远闪蓝）
fn mark_interrupted_cancelled(steps: &mut [StepVisualState]) {
    for st in steps.iter_mut() {
        if st.in_progress() {
            *st = StepVisualState::Cancelled;
        }
    }
}

/// 日志文件的行格式（控制台渲染走 ConsoleLine 带色，文件存纯文本）
/// 应用主题（明亮/暗黑；System 视同暗黑——egui 无法直接感知系统主题）
pub(crate) fn apply_theme(ctx: &egui::Context, theme: &crate::models::Theme) {
    match theme {
        crate::models::Theme::Light => ctx.set_visuals(egui::Visuals::light()),
        _ => ctx.set_visuals(egui::Visuals::dark()),
    }
}
fn format_event(ev: &LogEvent) -> Option<String> {
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
    }
}
