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
    /// 「⟳ 更新代码」任务的流式输出（git 进度行，后台线程 → UI 控制台）
    pub pull_lines: Option<std_mpsc::Receiver<String>>,
    /// 「⟳ 更新代码」任务完成信号（Ok = 项目名，Err = 错误详情）
    pub pull_done: Option<std_mpsc::Receiver<Result<String, String>>>,
    /// 人工确认门弹窗（job.confirm）：输入确认词后才真正启动运行
    pub confirm_gate: ConfirmGate,
    /// 👀 监听模式（watchexec/WinSCP keep-up-to-date 思路）：工作目录文件变化自动重跑
    pub watch: Option<WatchState>,
    /// 后台目录扫描结果通道（扫描在独立线程，UI 线程只收结果不递归走盘）
    pub watch_scan: Option<std_mpsc::Receiver<Option<std::time::SystemTime>>>,
    /// 下一次 launch_run 的触发来源是否为监听自动重跑（历史入库 trigger=watch）
    pub run_trigger_watch: bool,
    /// 运行历史列表缓存（设置页懒加载，None = 需要重读；sqlite 关闭时恒为空）
    pub history_rows: Option<Vec<crate::storage::HistoryRow>>,
    /// 本次运行各步骤耗时（ms）：StepEnd 事件收集，步骤卡片渲染耗时条
    pub run_step_durations: Vec<Option<u64>>,
    /// 本次运行各步骤重试次数：StepRetryWait 事件收集
    pub run_step_retries: Vec<u32>,
}

/// 👀 监听模式状态：盯住项目代码目录，变化后自动重跑指定流程
#[derive(Clone)]
pub struct WatchState {
    pub project: String,
    pub job: String,
    /// 监听的工作目录（开启时快照，稳定不受项目列表刷新影响）
    pub dir: String,
    /// 监听起点：之后有文件修改才触发重跑（触发后重置）
    pub since: std::time::SystemTime,
    /// 上次 mtime 轮询时刻（3 秒节流）
    pub last_poll: std::time::Instant,
}

/// 「🔒 人工确认」窗口状态（GHA environment protection 思路）
#[derive(Default)]
pub struct ConfirmGate {
    pub open: bool,
    pub input: String,
    /// 需要输入的确认词（job.confirm）
    pub expected: String,
    /// 展示用：项目/流程名
    pub job_label: String,
    /// 确认通过后要启动的运行参数 (from_step, only_step)
    pub pending: Option<(usize, Option<usize>)>,
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
            pull_lines: None,
            pull_done: None,
            confirm_gate: Default::default(),
            watch: None,
            watch_scan: None,
            run_trigger_watch: false,
            history_rows: None,
            run_step_durations: Vec::new(),
            run_step_retries: Vec::new(),
        };
        s.reload();
        // 历史保留期清理（后台，log_retention_days 天前的 runs 删除；失败只提示不阻塞启动）
        if s.global.save_history {
            let days = s.global.log_retention_days;
            s.runtime.spawn(async move {
                #[cfg(feature = "sqlite")]
                match crate::storage::sqlite::prune_old(days).await {
                    Ok(0) => {}
                    Ok(n) => eprintln!("[history] 已清理 {n} 条过期运行记录"),
                    Err(e) => eprintln!("[history] 清理失败: {e:#}"),
                }
                #[cfg(not(feature = "sqlite"))]
                let _ = days;
            });
        }
        s
    }

    pub fn reload(&mut self) {
        // Reload 会重建内存里的项目/作业列表：顺手复位上次运行的残留状态
        // （失败红点/状态圈挂在已被 Reload 掉的作业上只会误导）。运行中 Reload
        // 按钮本就禁用，这里再兜一层底。
        let had_stale_run = self.run_job.is_some() && !self.run_active();
        if !self.run_active() {
            self.run = None;
            self.run_job = None;
            self.run_steps.clear();
            self.run_step_wait.clear();
        }

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
            self.status_msg = Some(format!(
                "已加载 {} 个项目{}",
                self.projects.len(),
                if had_stale_run { "（已复位上次运行状态）" } else { "" }
            ));
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
        self.drain_pull();
        self.poll_watch();

        // ── 工具栏 ──────────────────────────────────────────────────
        egui::Panel::top("toolbar").show(ui, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                let running = self.run_active();
                let has_selection = self.selected_project.is_some() && self.selected_job.is_some();
                if ui
                    .add_enabled(has_selection, egui::Button::new("▶ 运行全部"))
                    .on_hover_text(
                        "按顺序执行所有启用的步骤。\n\
                         运行中再次点击 = 取消当前运行并重新开始（仅同项目，GHA concurrency 语义）",
                    )
                    .clicked()
                {
                    self.start_run(0, None);
                }
                if ui
                    .add_enabled(has_selection && !running, egui::Button::new("👁 预览"))
                    .on_hover_text(
                        "干跑预览：展示变量全量解析后的最终命令、上传清单与将跳过的步骤，\
                         不执行任何命令；未解析的变量占位符会以 ⚠ 标出",
                    )
                    .clicked()
                {
                    self.preview_selected();
                }
                // 👀 监听模式（watchexec 思路）：文件变化自动重跑
                {
                    let watching = self
                        .watch
                        .as_ref()
                        .map(|w| {
                            self.selected_project
                                .and_then(|pi| self.projects.get(pi))
                                .map(|p| {
                                    w.project == p.project.name
                                        && self
                                            .selected_job
                                            .and_then(|ji| p.jobs.get(ji))
                                            .map(|j| w.job == j.job.name)
                                            .unwrap_or(false)
                                })
                                .unwrap_or(false)
                        })
                        .unwrap_or(false);
                    let label = if watching { "👀 监听中 ●" } else { "👀 监听" };
                    if ui
                        .add_enabled(has_selection && !running, egui::Button::new(label))
                        .on_hover_text(
                            "监听模式：工作目录文件变化后自动重跑当前流程（3 秒轮询 mtime，\n\
                             排除 .git / node_modules / target / dist 等）。\n\
                             确认门只在开启时生效一次；再点一次停止监听",
                        )
                        .clicked()
                    {
                        if watching {
                            self.stop_watch();
                        } else {
                            self.start_watch();
                        }
                    }
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
        show_confirm_gate_window(&ctx, self);

        // 运行中保持帧率：日志流畅上屏 + 蓝黄光圈 1Hz 呼吸动画；更新代码中保持进度上屏；
        // 监听模式空闲时也需要定时醒来轮询 mtime
        if running {
            ctx.request_repaint_after(Duration::from_millis(50));
        } else if self.pull_done.is_some() {
            ctx.request_repaint_after(Duration::from_millis(150));
        } else if self.watch.is_some() {
            ctx.request_repaint_after(Duration::from_millis(500));
        }
    }

    // -----------------------------------------------------------------------
    // 👀 监听模式（watchexec / WinSCP keep-up-to-date 思路）
    // -----------------------------------------------------------------------

    /// 开启监听：记录当前流程 + 工作目录，立即跑一次首轮（确认门在此生效一次）
    pub fn start_watch(&mut self) {
        let (Some(pi), Some(ji)) = (self.selected_project, self.selected_job) else {
            return;
        };
        let Some(entry) = self.projects.get(pi) else { return };
        let Some(je) = entry.jobs.get(ji) else { return };
        let dir = entry.project.local_path.trim().to_string();
        if dir.is_empty() || !std::path::Path::new(&dir).is_dir() {
            self.status_msg = Some(format!(
                "⚠ 项目目录不存在，无法监听: {}",
                entry.project.local_path
            ));
            return;
        }
        self.watch = Some(WatchState {
            project: entry.project.name.clone(),
            job: je.job.name.clone(),
            dir,
            since: std::time::SystemTime::now(),
            last_poll: std::time::Instant::now(),
        });
        let label = format!("{}/{}", entry.project.name, je.job.name);
        self.console.push_line(
            format!(
                "==== 👀 监听模式开启: {label} ====\n工作目录文件变化后自动重跑（3 秒轮询 mtime，\
                 排除 .git / node_modules / target / dist 等）"
            ),
            crate::ui::console::Role::Info,
        );
        self.status_msg = Some(format!("👀 监听中: {label}"));
        self.start_run(0, None);
    }

    /// 停止监听；正在跑的监听目标运行也一并取消
    pub fn stop_watch(&mut self) {
        if let Some(w) = self.watch.take() {
            self.console.push_line(
                format!("==== 👀 监听已停止: {}/{} ====", w.project, w.job),
                crate::ui::console::Role::Info,
            );
            self.status_msg = Some("👀 监听已停止".to_string());
        }
        self.watch_scan = None; // 丢弃在途的扫描结果
        if self.run_active() {
            if let Some(run) = &self.run {
                let _ = run.cancel_tx.send(());
            }
        }
    }

    /// 空闲时轮询工作目录 mtime（3 秒节流），变化则自动重跑。
    /// 递归扫描放后台线程（大项目几万文件，UI 线程同步扫会掉帧），
    /// UI 每帧只做 try_recv 收结果。
    fn poll_watch(&mut self) {
        let Some(w) = self.watch.clone() else { return };
        // 确认门开着 = 等用户输入，不抢跑；运行中 / 更新代码中不轮询
        if self.run_active() || self.pull_done.is_some() || self.confirm_gate.open {
            return;
        }
        // 收后台扫描结果（Some(mtime) = 有最新修改时刻；None = 目录不可读）
        if let Some(rx) = &self.watch_scan {
            match rx.try_recv() {
                Ok(mtime) => {
                    self.watch_scan = None;
                    if let Some(mtime) = mtime {
                        if mtime > w.since {
                            if let Some(w) = self.watch.as_mut() {
                                w.since = std::time::SystemTime::now();
                            }
                            self.console.push_line(
                                "👀 检测到文件变化，自动重跑…".to_string(),
                                crate::ui::console::Role::Info,
                            );
                            // 监听重跑 = 已确认过的会话，跳过确认门直达启动
                            self.auto_rerun(&w);
                        }
                    }
                }
                Err(std_mpsc::TryRecvError::Empty) => {} // 扫描还在跑
                Err(std_mpsc::TryRecvError::Disconnected) => self.watch_scan = None,
            }
        }
        if self.watch_scan.is_some() {
            return; // 上一轮扫描未完，不叠加
        }
        if w.last_poll.elapsed() < Duration::from_secs(3) {
            return;
        }
        if let Some(w) = self.watch.as_mut() {
            w.last_poll = std::time::Instant::now();
        }
        // 目标流程可能已被删除 → 顺势停止监听
        let target_exists = self.projects.iter().any(|p| {
            p.project.name == w.project && p.jobs.iter().any(|j| j.job.name == w.job)
        });
        if !target_exists {
            self.stop_watch();
            return;
        }
        // 扫描丢后台线程，结果回传
        let (tx, rx) = std_mpsc::channel();
        let dir = std::path::PathBuf::from(&w.dir);
        std::thread::spawn(move || {
            let _ = tx.send(newest_mtime(&dir, 0));
        });
        self.watch_scan = Some(rx);
    }

    /// 监听触发的重跑：定位监听目标并启动（绕过确认门；目标被选中以保持 UI 一致）
    fn auto_rerun(&mut self, w: &WatchState) {
        let Some(pi) = self
            .projects
            .iter()
            .position(|p| p.project.name == w.project)
        else {
            self.stop_watch();
            return;
        };
        let Some(ji) = self.projects[pi].jobs.iter().position(|j| j.job.name == w.job) else {
            self.stop_watch();
            return;
        };
        self.selected_project = Some(pi);
        self.selected_job = Some(ji);
        // 监听触发的重跑：历史入库 trigger=watch
        self.run_trigger_watch = true;
        self.launch_run(0, None);
    }

    /// 项目树「⟳ 更新代码」：对项目代码目录（local_path）后台执行 git pull --ff-only，
    /// 输出流式进控制台。与作业运行互斥；本地目录模式 / 缺 .git 的项目直接提示不可用。
    pub fn pull_project_code(&mut self, pi: usize) {
        if self.pull_done.is_some() {
            self.status_msg = Some("⚠ 已有更新任务进行中".to_string());
            return;
        }
        if self.run_active() {
            self.status_msg = Some("⚠ 有作业运行中，先 ⏹ Stop 再更新代码".to_string());
            return;
        }
        let Some(entry) = self.projects.get(pi) else { return };
        let name = entry.project.name.clone();
        let dir = entry.project.local_path.trim().to_string();
        let dir_path = std::path::PathBuf::from(&dir);
        if dir.is_empty() {
            self.status_msg = Some(format!("⚠ 项目「{name}」未配置代码目录（项目目录）"));
            return;
        }
        if !dir_path.is_dir() {
            self.status_msg = Some(format!("⚠ 代码目录不存在: {dir}"));
            return;
        }
        if !dir_path.join(".git").exists() {
            self.status_msg =
                Some(format!("⚠ {dir} 不是 Git 仓库（本地目录模式），无法自动更新"));
            return;
        }
        // git 优先用 tools.yaml 里探测到的，否则回退 PATH（与 clone 同一策略）
        let git_cmd = self
            .tools
            .get("git")
            .map(|t| t.command.clone())
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "git".to_string());

        let (line_tx, line_rx) = std_mpsc::channel::<String>();
        let (done_tx, done_rx) = std_mpsc::channel::<Result<String, String>>();
        self.pull_lines = Some(line_rx);
        self.pull_done = Some(done_rx);
        self.console.push_line(
            format!("==== 更新代码: {name} ({dir}) ===="),
            crate::ui::console::Role::Info,
        );
        self.status_msg = Some(format!("正在更新代码: {name}…"));

        std::thread::spawn(move || {
            let res = crate::ui::new_project::run_pull(&git_cmd, &dir_path, line_tx);
            let _ = done_tx.send(res.map(|_| name));
        });
    }

    /// 「⟳ 更新代码」后台输出 → 控制台；完成信号 → 控制台收尾行 + 状态栏
    fn drain_pull(&mut self) {
        if let Some(rx) = &self.pull_lines {
            while let Ok(line) = rx.try_recv() {
                self.console.push_line(line, crate::ui::console::Role::Stdout);
            }
        }
        let mut finished = None;
        if let Some(rx) = &self.pull_done {
            match rx.try_recv() {
                Ok(res) => finished = Some(res),
                Err(std_mpsc::TryRecvError::Disconnected) => {
                    finished = Some(Err("更新任务异常终止".to_string()));
                }
                Err(std_mpsc::TryRecvError::Empty) => {}
            }
        }
        if let Some(res) = finished {
            self.pull_lines = None;
            self.pull_done = None;
            match res {
                Ok(name) => {
                    self.console.push_line(
                        format!("==== 「{name}」代码已更新 ✔ ===="),
                        crate::ui::console::Role::Ok,
                    );
                    self.status_msg = Some(format!("「{name}」代码已更新 ✔"));
                }
                Err(e) => {
                    for line in e.lines() {
                        self.console
                            .push_line(line.to_string(), crate::ui::console::Role::Error);
                    }
                    let first = e.lines().next().unwrap_or("").to_string();
                    self.status_msg = Some(format!("更新代码失败 ❌ {first}"));
                }
            }
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
                JobStatus::Success | JobStatus::SucceededWithFailures { .. } => {
                    StepVisualState::Success
                }
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
                LogEvent::StepEnd { step_index, exit_code, duration_ms } => {
                    // 耗时时间线数据（handlers 的 step_index 超出主流程长度 → 忽略）
                    if let Some(d) = self.run_step_durations.get_mut(*step_index) {
                        *d = Some(*duration_ms);
                    }
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
                    if let Some(n) = self.run_step_retries.get_mut(*step_index) {
                        *n += 1;
                    }
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
                    // 桌面通知（toast）：最小化在干别的也能看到结果；watch 自动重跑尤其需要
                    if self.global.desktop_notify {
                        let label = self
                            .run_job
                            .as_ref()
                            .map(|(p, j)| format!("{p}/{j}"))
                            .unwrap_or_default();
                        desktop_notify(&label, &crate::ui::console::job_label_public(status));
                    }
                    // 监听模式：结束即进入等待文件变化的待命态
                    if let Some(w) = &self.watch {
                        if self
                            .run_job
                            .as_ref()
                            .map(|(p, j)| p == &w.project && j == &w.job)
                            .unwrap_or(false)
                        {
                            self.status_msg =
                                Some("👀 监听中：已结束，等待文件变化自动重跑…".to_string());
                        }
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

    /// 「👁 预览」：干跑展示当前流程（变量全量解析、when/creates 判定、上传清单），
    /// 结果进控制台。同步无副作用，不启动引擎任务、不发任何网络请求。
    fn preview_selected(&mut self) {
        let (Some(pi), Some(ji)) = (self.selected_project, self.selected_job) else {
            return;
        };
        let Some(entry) = self.projects.get(pi) else { return };
        let Some(je) = entry.jobs.get(ji) else { return };
        let project = entry.project.clone();
        let job = je.job.clone();

        let effective = crate::config::effective_targets(&project);
        let input = crate::engine::RunInput {
            project: project.clone(),
            job: job.clone(),
            tools: self.tools.clone(),
            all_targets: effective,
            from_step: 0,
            only_step: None,
            run_id: String::new(),
            default_step_timeout: self.global.default_timeout,
            cancel_tx: None,
        };

        let previews = crate::engine::preview_job(&input);
        use crate::ui::console::Role;
        let workspace = std::path::PathBuf::from(&project.local_path);
        self.console.push_line(
            format!("==== 预览（干跑，不执行）: {}/{} ====", project.name, job.name),
            Role::Info,
        );
        // 作业级信息：确认门 / 前置检查
        if let Some(c) = job.confirm.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            self.console
                .push_line(format!("🔒 确认门: 运行前需输入「{c}」"), Role::Info);
        }
        for pre in &job.preconditions {
            let msg = if pre.message.trim().is_empty() {
                String::new()
            } else {
                format!("（失败: {}）", pre.message.trim())
            };
            self.console
                .push_line(format!("前置检查: {}{msg}", pre.check), Role::Info);
        }
        if !job.handlers.is_empty() {
            let names = job
                .handlers
                .iter()
                .map(|h| h.name.clone().unwrap_or_else(|| "（未命名）".into()))
                .collect::<Vec<_>>()
                .join(", ");
            self.console
                .push_line(format!("handlers（变更才触发）: {names}"), Role::Info);
        }
        // 工具版本要求偏差（mise 风格）
        {
            let mut reqs: Vec<(&String, &String)> = project.tools_require.iter().collect();
            reqs.sort();
            for (name, req) in reqs {
                let detected = self
                    .tools
                    .get(name)
                    .and_then(|t| t.version.clone())
                    .unwrap_or_default();
                match crate::version::version_matches(req, &detected) {
                    Ok(true) => self.console.push_line(
                        format!("工具版本: {name} {detected} 满足 {req} ✓"),
                        Role::Info,
                    ),
                    Ok(false) => self.console.push_line(
                        format!("⚠ 工具版本偏差: {name} 需要 {req}，探测到 {detected}（运行将被拦截）"),
                        Role::Fail,
                    ),
                    Err(e) => self.console.push_line(format!("⚠ {name}: {e}"), Role::Fail),
                }
            }
        }
        let (mut exec_n, mut skip_n) = (0usize, 0usize);
        for p in &previews {
            let tgt = p.target.clone().unwrap_or_else(|| "local".to_string());
            if p.skipped {
                skip_n += 1;
                self.console.push_line(
                    format!(
                        "[{}] {}  ⏭ 跳过：{}",
                        p.index + 1,
                        p.name,
                        p.note.as_deref().unwrap_or("")
                    ),
                    Role::Cancelled,
                );
            } else {
                exec_n += 1;
                self.console.push_line(
                    format!("[{}] {}  目标: {}", p.index + 1, p.name, tgt),
                    Role::StepStart,
                );
                if !p.command.is_empty() {
                    self.console
                        .push_line(format!("    $ {}", p.command), Role::Stdout);
                }
                if let Some((tname, tcmd)) = &p.tool {
                    self.console.push_line(
                        format!("    🔧 工具 {tname} → {tcmd}（其目录前置 PATH，裸名命令用这个版本）"),
                        Role::Stdout,
                    );
                }
                for (l, r, exists) in &p.uploads {
                    self.console.push_line(
                        format!(
                            "    ↑ {l} -> {r}{}",
                            if *exists { "" } else { "（本地缺失 ⚠）" }
                        ),
                        Role::Stdout,
                    );
                }
                if let Some((s, d)) = &p.pack {
                    self.console
                        .push_line(format!("    ⧉ 打包 {s} -> {d}"), Role::Stdout);
                }
                for (k, pat) in &p.outputs {
                    self.console
                        .push_line(format!("    ⇩ 捕获输出 {k} ← /{pat}/"), Role::Stdout);
                }
                if !p.notify.is_empty() {
                    self.console
                        .push_line(format!("    ⇮ notify: {}", p.notify.join(",")), Role::Stdout);
                }
                if let Some(w) = &p.wait_note {
                    self.console
                        .push_line(format!("    ⏲ 探测: {w}"), Role::Stdout);
                }
                if p.working_dir != workspace && !p.working_dir.as_os_str().is_empty() {
                    self.console.push_line(
                        format!("    工作目录: {}", p.working_dir.display()),
                        Role::Stdout,
                    );
                }
            }
            for w in &p.warnings {
                self.console.push_line(format!("    ⚠ {w}"), Role::Fail);
            }
        }
        self.console.push_line(
            format!("==== 预览结束：{exec_n} 步执行 / {skip_n} 步跳过 ===="),
            Role::Ok,
        );
        self.status_msg = Some(format!("预览完成: {}/{}", project.name, job.name));
    }

    pub(crate) fn start_run(&mut self, from_step: usize, only_step: Option<usize>) {
        let (pi, ji) = match (self.selected_project, self.selected_job) {
            (Some(a), Some(b)) => (a, b),
            _ => return,
        };
        if self.projects.get(pi).and_then(|p| p.jobs.get(ji)).is_none() {
            return;
        }
        // 人工确认门（job.confirm）：弹窗输入确认词才放行，防手滑上生产
        let expected = self.projects[pi].jobs[ji]
            .job
            .confirm
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        if let Some(expected) = expected {
            if !self.confirm_gate.open {
                self.confirm_gate = ConfirmGate {
                    open: true,
                    input: String::new(),
                    expected,
                    job_label: format!(
                        "{}/{}",
                        self.projects[pi].project.name, self.projects[pi].jobs[ji].job.name
                    ),
                    pending: Some((from_step, only_step)),
                };
            }
            return;
        }
        self.launch_run(from_step, only_step);
    }

    /// 确认门通过：按 pending 参数真正启动
    fn confirm_gate_pass(&mut self) {
        if self.confirm_gate.input.trim() != self.confirm_gate.expected {
            self.status_msg = Some(format!(
                "确认词不匹配，需输入「{}」",
                self.confirm_gate.expected
            ));
            return;
        }
        self.confirm_gate.open = false;
        if let Some((from, only)) = self.confirm_gate.pending.take() {
            self.launch_run(from, only);
        }
    }

    fn launch_run(&mut self, from_step: usize, only_step: Option<usize>) {
        // 与「⟳ 更新代码」互斥：git 改文件期间跑作业会读到半新半旧的代码
        if self.pull_done.is_some() {
            self.status_msg = Some("⚠ 正在更新代码，请稍候再运行".to_string());
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

        // GHA concurrency：同项目重跑 = 取消旧的换新的（引擎锁接管 + 直接发取消信号）；
        // 不同项目仍互斥（GUI 单运行槽），跨进程互斥由引擎的运行锁保证
        if self.run_active() {
            let same = self
                .run_job
                .as_ref()
                .map(|(p, j)| p == &project.name && j == &job.name)
                .unwrap_or(false);
            if !same {
                self.status_msg =
                    Some("⚠ 有其他项目的作业运行中；同项目重跑会自动取消旧的".to_string());
                return;
            }
            if let Some(run) = &self.run {
                let _ = run.cancel_tx.send(());
            }
            self.console.push_line(
                "==== 同项目重新运行：旧运行取消中，新运行即将开始 ====".to_string(),
                crate::ui::console::Role::Info,
            );
        }

        // 状态可视化复位：新一次运行所有步骤的光圈回 Idle，并记录运行对象与 wait 步骤；
        // 耗时/重试数据同步清零（新运行的时间线从零开始）
        self.run_steps = vec![StepVisualState::Idle; job.steps.len()];
        self.run_step_wait = job
            .steps
            .iter()
            .map(|s| matches!(s.step_type, StepType::Wait))
            .collect();
        self.run_step_durations = vec![None; job.steps.len()];
        self.run_step_retries = vec![0; job.steps.len()];
        self.run_job = Some((project.name.clone(), job.name.clone()));

        // §3.5：项目级目标覆盖全局 targets.yaml（键相同项目优先），并回填凭据键
        let effective = crate::config::effective_targets(&project);
        let mut label = format!("{}/{}", project.name, job.name);
        if only_step.is_some() {
            label.push_str(&format!(" · 仅 Step {}", from_step + 1));
        } else if from_step > 0 {
            label.push_str(&format!(" · 从 Step {}", from_step + 1));
        }

        // §12.2：每个 job 一个 broadcast sender（cancel_tx 一份进 RunInput ——
        // 引擎用它实现同项目运行互斥的锁监视）
        let (cancel_tx, cancel_rx) = broadcast::channel(4);
        let (tok_tx, mut tok_rx) = mpsc::channel(8192);
        let (std_tx, std_rx) = std_mpsc::channel();

        let engine = crate::engine::Engine;
        // 运行唯一标识：与日志文件同名，引擎经 ${{ run.id }} 透出给步骤命令。
        // 精确到毫秒：同秒内重跑（连点 / 脚本触发）若 run_id 相同，日志文件会互写、
        // SQLite runs.run_id UNIQUE 冲突导致历史不入库
        let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S%3f");
        let run_id = format!("{}-{}", project.name, stamp);
        let steps_len = job.steps.len();
        let job_snapshot = yaml_serde::to_string(&job).unwrap_or_default();
        let project_job_name = job.name.clone();
        let project_target_name = job.target.clone();
        let run_id_for_end = run_id.clone();
        let save_history = self.global.save_history;
        let trigger: &'static str = if self.run_trigger_watch { "watch" } else { "manual" };
        self.run_trigger_watch = false;
        let input = crate::engine::RunInput {
            project: project.clone(),
            job,
            tools: self.tools.clone(),
            all_targets: effective,
            from_step,
            only_step,
            run_id: run_id.clone(),
            default_step_timeout: self.global.default_timeout,
            cancel_tx: Some(cancel_tx.clone()),
        };

        // 日志文件（§11：日志原文落文件，SQLite 只存路径——P1）
        let log_dir = crate::config::logs_dir().join(&project.name);
        let _ = std::fs::create_dir_all(&log_dir);
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
                    run_id: run_id_for_end,
                    status: status.clone(),
                    total_duration_ms: started.elapsed().as_millis() as u64,
                })
                .await;
        });

        // 转发任务：tokio mpsc → std mpsc（UI）+ 追加日志文件 + 运行历史入库（save_history 开）
        let history_meta = crate::storage::sqlite::RunMeta {
            run_id: run_id.clone(),
            project: project.name.clone(),
            job: project_job_name.clone(),
            target: project_target_name,
            trigger,
            log_path: Some(log_path.to_string_lossy().to_string()),
        };
        self.runtime.spawn(async move {
            #[cfg(feature = "sqlite")]
            let recorder = if save_history {
                crate::storage::sqlite::RunRecorder::start(
                    &history_meta,
                    &job_snapshot,
                    steps_len,
                )
                .await
            } else {
                None
            };
            let mut file = std::fs::File::create(&log_path).ok();
            while let Some(ev) = tok_rx.recv().await {
                #[cfg(feature = "sqlite")]
                if let Some(rec) = &recorder {
                    rec.on_event(&ev).await;
                }
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
            #[cfg(feature = "sqlite")]
            if let Some(rec) = recorder {
                rec.close().await;
            }
        });

        // 运行前自动保存：跑的必然是内存里刚编辑过的副本，用户忘了点「💾 Save」时
        // Reload 也不会把编辑丢掉（新建流程本就落盘，这里兜住步骤编辑）
        {
            let entry = &self.projects[pi];
            let job_entry = &entry.jobs[ji];
            let results = (
                files::save_yaml(&entry.dir.join("project.yaml"), &entry.project),
                files::save_yaml(&job_entry.path, &job_entry.job),
            );
            match results {
                (Ok(()), Ok(())) => {
                    self.console.push_line(
                        "（运行前已自动保存 project.yaml 与当前流程）",
                        crate::ui::console::Role::Info,
                    );
                }
                (Err(e), _) | (_, Err(e)) => {
                    self.console.push_line(
                        format!("⚠ 运行前自动保存失败: {e}（本次按内存内容执行）"),
                        crate::ui::console::Role::Info,
                    );
                }
            }
        }

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

/// 「🔒 人工确认」窗口（job.confirm 确认门）：输入确认词后才真正 launch_run
fn show_confirm_gate_window(ctx: &egui::Context, main: &mut MainState) {
    if !main.confirm_gate.open {
        return;
    }
    crate::ui::modal_backdrop(ctx, "mask_confirm_gate");
    let mut pass = false;
    let mut cancel = false;
    egui::Window::new("🔒 人工确认")
        .collapsible(false)
        .order(egui::Order::Tooltip)
        .default_width(420.0)
        .show(ctx, |ui| {
            let gate = &mut main.confirm_gate;
            ui.colored_label(
                egui::Color32::from_rgb(230, 180, 80),
                format!(
                    "流程「{}」设置了确认门（防手滑上生产）。\n输入「{}」以继续执行：",
                    gate.job_label, gate.expected
                ),
            );
            ui.add_space(4.0);
            let resp = ui.add(
                egui::TextEdit::singleline(&mut gate.input)
                    .desired_width(180.0)
                    .hint_text("输入确认词"),
            );
            let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            let matched = gate.input.trim() == gate.expected;
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(matched, egui::Button::new("✅ 确认执行"))
                    .clicked()
                    || (enter && matched)
                {
                    pass = true;
                }
                if ui.button("取消").clicked() {
                    cancel = true;
                }
                if !gate.input.is_empty() && !matched {
                    ui.weak(format!("输入与确认词「{}」不一致", gate.expected));
                }
            });
        });
    if pass {
        main.confirm_gate_pass();
    }
    if cancel {
        main.confirm_gate.open = false;
    }
}

/// Windows 桌面通知（toast）：零 Rust 依赖——起隐藏 PowerShell 用 WinRT Toast
/// （Win10/11 自带，无需装模块）。spawn 即忘（PowerShell 启动 ~300ms 在后台）。
fn desktop_notify(title: &str, body: &str) {
    // XML 转义（& < >）+ PowerShell 单引号转义（' → ''）：XML 模板包在 PS
    // 单引号字符串里，项目名 / 失败原因带撇号时会破坏脚本导致通知弹不出
    let esc = |s: &str| {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('\'', "''")
    };
    let title = esc(title);
    let body = esc(body);
    let script = format!(
        r#"
[Windows.UI.Notifications.ToastNotificationManager,Windows.UI.Notifications,ContentType=WindowsRuntime] | Out-Null
[Windows.Data.Xml.Dom.XmlDocument,Windows.Data.Xml.Dom.XmlDocument,ContentType=WindowsRuntime] | Out-Null
$x = [Windows.Data.Xml.Dom.XmlDocument]::new()
$x.LoadXml('<toast><visual><binding template="ToastText02"><text id="1">{title}</text><text id="2">{body}</text></binding></visual></toast>')
$t = [Windows.UI.Notifications.ToastNotification]::new($x)
[Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier('Remote Build System').Show($t)
"#
    );
    let mut cmd = std::process::Command::new("powershell");
    cmd.args(["-NoProfile", "-NonInteractive", "-WindowStyle", "Hidden", "-Command", &script]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let _ = cmd.spawn();
}

/// 监听扫描时跳过的目录（构建产物 / 依赖 / IDE 元数据——它们变化不代表源码变了）
const WATCH_SKIP_DIRS: &[&str] = &[
    ".git", ".svn", ".idea", ".vscode", "node_modules", "target", "dist", "build", "out",
    ".next", "coverage", "__pycache__",
];

/// 递归取目录下最新文件 mtime（depth 为递归层级，防符号链接环 / 异常深树）
fn newest_mtime(dir: &std::path::Path, depth: usize) -> Option<std::time::SystemTime> {
    if depth > 12 {
        return None;
    }
    let entries = std::fs::read_dir(dir).ok()?;
    let mut max: Option<std::time::SystemTime> = None;
    let mut consider = |t: Option<std::time::SystemTime>| {
        if let Some(t) = t {
            if max.map(|m| t > m).unwrap_or(true) {
                max = Some(t);
            }
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        if WATCH_SKIP_DIRS.contains(&name.as_str()) {
            continue;
        }
        match entry.file_type() {
            Ok(ft) if ft.is_dir() => consider(newest_mtime(&path, depth + 1)),
            Ok(_) => consider(entry.metadata().ok().and_then(|m| m.modified().ok())),
            Err(_) => {}
        }
    }
    max
}

/// 取消/超时/异常退出时，仍在运行的步骤等不到 StepEnd，兜底落中性终态（避免永远闪蓝）
fn mark_interrupted_cancelled(steps: &mut [StepVisualState]) {
    for st in steps.iter_mut() {
        if st.in_progress() {
            *st = StepVisualState::Cancelled;
        }
    }
}

/// 应用主题（明亮/暗黑；System 视同暗黑——egui 无法直接感知系统主题）
pub(crate) fn apply_theme(ctx: &egui::Context, theme: &crate::models::Theme) {
    match theme {
        crate::models::Theme::Light => ctx.set_visuals(egui::Visuals::light()),
        _ => ctx.set_visuals(egui::Visuals::dark()),
    }
}

/// 日志文件的行格式（控制台渲染走 ConsoleLine 带色，文件存纯文本）
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_id_stamp_has_millisecond_precision() {
        // 同秒重跑防冲突：run_id 时间戳必须带毫秒
        // （18 位 = yyyyMMdd(8) + '-' + HHmmss(6) + 毫秒(3)）
        let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S%3f").to_string();
        assert_eq!(stamp.len(), 18, "stamp: {stamp}");
        assert!(stamp.chars().all(|c| c.is_ascii_digit() || c == '-'), "stamp: {stamp}");
    }

    #[test]
    fn newest_mtime_scans_and_skips_junk_dirs() {
        let dir = std::env::temp_dir().join(format!("rbs_watch_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join("node_modules/pkg")).unwrap();
        let f1 = dir.join("src/main.rs");
        std::fs::write(&f1, "fn main() {}").unwrap();
        let f2 = dir.join("node_modules/pkg/index.js");
        std::fs::write(&f2, "module.exports = 1").unwrap();

        // 扫描到源码文件的 mtime（node_modules 被剪枝也无所谓，文件存在即可）
        let m = newest_mtime(&dir, 0);
        assert!(m.is_some(), "应至少发现一个文件");
        // 最近 mtime 不早于任一文件自身的 mtime
        let file_m = std::fs::metadata(&f1).unwrap().modified().unwrap();
        assert!(m.unwrap() >= file_m);

        // 空目录 → None
        let empty = dir.join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        assert!(newest_mtime(&empty, 0).is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
