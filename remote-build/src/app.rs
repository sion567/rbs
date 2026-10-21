//! egui App 主体与屏幕路由（设计文档 §2 / §16 app.rs）。
//!
//! 桥接模型：tokio 引擎在后台 runtime 运行，LogEvent 经 tokio mpsc →
//! 转发任务（追加日志文件）→ std mpsc，UI 每帧 try_recv 排空后渲染（§4.2）。

use crate::models::{JobStatus, LogEvent};
use crate::storage::files::{self, ProjectEntry};
use crate::ui;
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
        load_cjk_font(&cc.egui_ctx);
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
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        match &mut self.page {
            Page::FirstRun(page) => {
                page.show(ctx);
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
            Page::Main(main) => main.show(ctx),
        }
    }
}

/// 中文字体：egui 默认字体无 CJK 字形，加载微软雅黑（缺失时回退默认）
fn load_cjk_font(ctx: &egui::Context) {
    const CANDIDATES: &[&str] = &[
        "C:/Windows/Fonts/msyh.ttc",
        "C:/Windows/Fonts/msyh.ttf",
        "C:/Windows/Fonts/simhei.ttf",
        "C:/Windows/Fonts/simsun.ttc",
    ];
    for path in CANDIDATES {
        if let Ok(bytes) = std::fs::read(path) {
            let mut fonts = egui::FontDefinitions::default();
            fonts.font_data.insert("cjk".to_string(), egui::FontData::from_owned(bytes));
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
    pub targets: std::collections::HashMap<String, crate::models::Target>,
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
    pub target_editor: ui::targets::TargetEditorState,
    /// 设置窗口当前分区 tab
    pub settings_tab: ui::settings::SettingsTab,
    /// 「？ 变量语法」帮助窗口
    pub show_var_help: bool,
    /// 主题是否已在首帧应用（egui ctx 需要在 show 阶段才能设置）
    pub theme_applied: bool,
    pub status_msg: Option<String>,
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
            targets: Default::default(),
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
            target_editor: Default::default(),
            settings_tab: Default::default(),
            show_var_help: false,
            theme_applied: false,
            status_msg: None,
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
        // 配置解析失败不再静默：直接提示，避免"工具/目标莫名消失"
        let mut warnings = Vec::new();
        self.tools = match crate::config::load_tools() {
            Ok(t) => t.tools,
            Err(e) => {
                warnings.push(format!("tools.yaml 解析失败: {e:#}"));
                Default::default()
            }
        };
        self.targets = match crate::config::load_targets() {
            Ok(t) => t.targets,
            Err(e) => {
                warnings.push(format!("targets.yaml 解析失败: {e:#}"));
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

    pub fn show(&mut self, ctx: &egui::Context) {
        // 主题首帧应用（此后设置页切换时即时应用）
        if !self.theme_applied {
            apply_theme(ctx, &self.global.theme);
            self.theme_applied = true;
        }
        self.drain_events();

        // ── 工具栏 ──────────────────────────────────────────────────
        egui::TopBottomPanel::top("toolbar").show(ctx, |ui| {
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
                if ui.button("💾 Save").clicked() {
                    self.save_selected();
                }
                if ui.button("⟳ Reload").clicked() {
                    self.reload();
                }

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("⚙ 设置").clicked() {
                        self.show_settings = !self.show_settings;
                    }
                    if ui.button("🎯 目标").clicked() {
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
        ui::console::show(&mut self.console, ctx, running);

        // ── 左侧项目树 ──────────────────────────────────────────────
        egui::SidePanel::left("project_tree")
            .resizable(true)
            .default_width(220.0)
            .show(ctx, |ui| {
                ui.add_space(8.0);
                ui::project_tree::show(self, ui);
            });

        // ── 中央步骤编辑器 ──────────────────────────────────────────
        egui::CentralPanel::default().show(ctx, |ui| {
            ui::job_editor::show(self, ui);
        });

        if self.show_settings {
            ui::settings::show(ctx, self);
        }
        if self.target_editor.open {
            ui::targets::show(ctx, self);
        }

        ui::new_project::show(ctx, self);
        ui::job_editor::show_new_job_window(ctx, self);
        ui::job_editor::show_var_help_window(ctx, self);

        // 运行中保持帧率，让日志事件流畅上屏
        if running {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }

    pub(crate) fn run_active(&self) -> bool {
        match &self.run {
            None => false,
            Some(r) => r.finished.is_none() && !r.join.is_finished(),
        }
    }

    fn drain_events(&mut self) {
        let Some(run) = &mut self.run else { return };
        let mut events = Vec::new();
        while let Ok(ev) = run.events.try_recv() {
            events.push(ev);
        }
        for ev in &events {
            if let LogEvent::JobEnd { status, .. } = ev {
                run.finished = Some(status.clone());
            }
            self.console.push_event(ev);
        }
        // 包装任务异常退出但没发 JobEnd 的兜底
        if run.finished.is_none() && run.join.is_finished() {
            run.finished = Some(JobStatus::Failed {
                step_index: 0,
                reason: "引擎任务异常终止".to_string(),
            });
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
        // §3.5：项目级目标覆盖全局 targets.yaml（键相同项目优先），并回填凭据键
        let effective = crate::config::effective_targets(&project, &self.targets);
        let target = job.target.as_ref().and_then(|n| effective.get(n).cloned());
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
            target,
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
                    let _ = writeln!(f, "{}", format_event(&ev));
                }
                if std_tx.send(ev).is_err() {
                    break; // UI 已关闭
                }
            }
        });

        self.console.push_line(
            format!("==== 启动作业 {label} ===="),
            crate::ui::console::palette::INFO,
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

/// 日志文件的行格式（控制台渲染走 ConsoleLine 带色，文件存纯文本）
/// 应用主题（明亮/暗黑；System 视同暗黑——egui 无法直接感知系统主题）
pub(crate) fn apply_theme(ctx: &egui::Context, theme: &crate::models::Theme) {
    match theme {
        crate::models::Theme::Light => ctx.set_visuals(egui::Visuals::light()),
        _ => ctx.set_visuals(egui::Visuals::dark()),
    }
}
fn format_event(ev: &LogEvent) -> String {
    match ev {
        LogEvent::Stdout { line, .. } => line.clone(),
        LogEvent::Stderr { line, .. } => format!("[stderr] {line}"),
        LogEvent::StepStart { step_index, name } => {
            format!("── Step {} ── {name}", step_index + 1)
        }
        LogEvent::StepEnd { step_index, exit_code, duration_ms } => {
            format!("── Step {} 退出码 {exit_code} ({duration_ms}ms)", step_index + 1)
        }
        LogEvent::JobEnd { status, total_duration_ms, .. } => {
            format!("==== {} ({total_duration_ms}ms) ====", status.short())
        }
        LogEvent::Error { message } => format!("[error] {message}"),
    }
}
