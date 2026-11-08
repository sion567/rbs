//! UI 快照测试（egui_kittest）：无头渲染全应用页面 → tests/snapshots/*.png。
//!
//! 用途 = **页面调整的比对基线 + 交互回归**：改 UI 前先跑一次留档，改完再跑一次，
//! 两次 PNG（或失败时自动生成的 *.diff.png）逐像素对比，肉眼比对从"凭记忆"
//! 变成"看图说话"。
//!
//! ```text
//! # 首次录制 / UI 改动后重录基线（录完测试直接通过）：
//! UPDATE_SNAPSHOTS=1 cargo test --bin remote-build ui_snapshots
//! # 日常回归（不改基线，对比失败则 panic 并落 *_diff.png / *_new.png）：
//! cargo test --bin remote-build ui_snapshots
//! ```
//!
//! 覆盖页面（main_screen_ui 渲染 MainState::show，一个入口带出全部页面）：
//! - 主屏全景 / 控制台折叠（点击）/ 项目树分组折叠（点击）
//! - ⚙ 设置 4 个 tab（点击 tab 行切换）
//! - 🎯 目标管理：SSH / k8s / 本机面板 / 窄窗 / DPI 1.5 / 测试失败态，
//!   以及输入流（点击列表、AccessKit SetValue 改文本、键盘输入密码、
//!   点重命名 → 断言未保存标记与流程引用同步）
//! - 帮助窗口（命令速查 / 变量语法两 tab）/ ＋新建流程 / ＋新建项目 / 删除确认 / 分组改名 / 人工确认门 / 首启引导
//!
//! 工程约束（踩过的坑）：
//! - WGPU_BACKEND=dx12：本机 Vulkan 驱动会延迟段错误（main.rs 同款）
//! - gpu_lock 串行 + 测试整体串行（.cargo/config.toml）：wgpu 满载并发会崩
//! - run() 需要 with_max_steps：本机信息 spinner 等后台任务让「跑稳」需要几十帧
//! - 输入用 AccessKit SetValue 替换文本：合成键盘 Ctrl+A 有焦点时序坑；
//!   type_text（键盘输入）发给焦点控件，点击后要先 run() 分帧等焦点生效
//! - egui TextEdit 天生无无障碍名称，查询用 角色 + 值（Role + value）定位
//!
//! 快照产物只进 git 基线 PNG；*.diff.png / *.new.png 已加 .gitignore。

use crate::app::{load_cjk_font, MainState};
use crate::models::{AuthMethod, Job, Step, Target, TargetType, TransportKind};
use crate::storage::files::JobEntry;
use accesskit::Role;
use eframe::egui;
use egui_kittest::kittest::{By, Queryable};
use std::collections::HashMap;
use std::sync::Arc;

/// 与真实 app 相同的字体（msyh 等 CJK），首帧装进无头 Context。
/// kittest 把缺字形策略设成了 Panic，真实 app 默认 Tofu——这里对齐真实行为；
/// 缺字形本身是 UI bug（如 🧰 U+1F9F0 无字形渲染成 □），由单独的检查发现而非
/// 快照测试替罪。
fn app_ui(ui: &mut egui::Ui, main: &mut MainState) {
    if !main.theme_applied {
        ui.ctx().set_missing_glyph_policy(egui::MissingGlyphPolicy::Tofu);
        load_cjk_font(ui.ctx());
        main.theme_applied = true;
    }
    crate::ui::targets::show(ui.ctx(), main);
}

/// 帮助窗口（主工具栏 ❓ 打开，命令速查 / 变量语法两 tab）：与 app_ui 同构，
/// 只换渲染目标；tab 页由用例预置 main.help_tab 决定
fn help_ui(ui: &mut egui::Ui, main: &mut MainState) {
    if !main.theme_applied {
        ui.ctx().set_missing_glyph_policy(egui::MissingGlyphPolicy::Tofu);
        load_cjk_font(ui.ctx());
        main.theme_applied = true;
    }
    main.show_help = true;
    crate::ui::job_editor::show_help_window(ui.ctx(), main);
}

/// 完整主屏（工具栏 + 项目树 + 流程编辑器 + 控制台 + 全部弹窗）：
/// MainState::show 一进一出，所有页面都从这里渲染
fn main_screen_ui(ui: &mut egui::Ui, main: &mut MainState) {
    if !main.theme_applied {
        ui.ctx().set_missing_glyph_policy(egui::MissingGlyphPolicy::Tofu);
        load_cjk_font(ui.ctx());
        main.theme_applied = true;
    }
    main.show(ui);
}

/// 演示流程：3 个步骤（构建 / 上传 / 重启），可选分组
fn demo_job(name: &str, group: Option<&str>) -> Job {
    let mut j = Job::default();
    j.name = name.to_string();
    j.project = "demo-project".to_string();
    j.target = Some("prod".to_string());
    j.environment = Some("prod".to_string());
    j.group = group.map(|g| g.to_string());
    j.steps = vec![
        Step {
            name: Some("本地构建".to_string()),
            tool: "maven".to_string(),
            command: "mvn -B clean package -DskipTests".to_string(),
            creates: Some("target/*.jar".to_string()),
            ..Default::default()
        },
        Step {
            name: Some("上传发布包".to_string()),
            command: "target/app.jar -> release/app.jar".to_string(),
            ..Default::default()
        },
        Step {
            name: Some("重启服务".to_string()),
            command: "systemctl restart app".to_string(),
            ..Default::default()
        },
    ];
    j
}

/// 带两个流程（一个属于「发布」分组）+ 已选中状态的主屏数据
fn test_main_state_with_jobs() -> MainState {
    let mut main = test_main_state();
    let dir = main.projects[0].dir.clone();
    let jobs = vec![
        JobEntry { path: dir.join("prod-deploy.yaml"), job: demo_job("prod-deploy", Some("发布")) },
        JobEntry { path: dir.join("rollback.yaml"), job: demo_job("rollback", None) },
    ];
    main.projects[0].jobs = jobs;
    main.projects[0].project.groups = vec!["发布".to_string()];
    main.selected_project = Some(0);
    main.selected_job = Some(0);
    main
}

/// 主屏 harness（不加锁：调用方按需持 gpu_lock——交互测试要跨多帧持锁）
fn main_harness(size: (f32, f32), main: MainState) -> egui_kittest::Harness<'static, MainState> {
    egui_kittest::Harness::builder()
        .with_accessibility_check(false)
        .with_size(egui::vec2(size.0, size.1))
        .with_pixels_per_point(1.0)
        // 默认 4 步不够本机信息采集 spinner 等后台任务跑稳
        .with_max_steps(200)
        .build_ui_state(main_screen_ui, main)
}

/// 本机 Intel 驱动 + wgpu 默认的 Vulkan 后端会延迟段错误（main.rs 同款问题，
/// 那里锁 DX12）。kittest 的渲染器不暴露后端配置，但 wgpu 的 request_adapter
/// 认 WGPU_BACKEND 环境变量——测试进程内自己设，免得每次跑测试都要带前缀。
fn force_dx12_backend() {
    #[cfg(windows)]
    std::env::set_var("WGPU_BACKEND", "dx12");
}

/// 串行化 wgpu 初始化：多测试并行各自建 DX12 实例时，本机驱动会段错误
/// （全量并行 cargo test 必崩，串行/加锁后 126 全过）。只锁 UI 测试自身，
/// 其余单元测试不受影响
fn gpu_lock() -> &'static std::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
}

/// 取锁并忽略毒化：某个 UI 测试 panic 后，其余测试仍照常串行执行，
/// 不该整批 PoisonError 连坐（毒化只说明有人持锁时 panic，锁本身仍可用）
fn gpu_lock_acquire() -> std::sync::MutexGuard<'static, ()> {
    gpu_lock().lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 建立无头 Harness、跑稳、落快照（锁内执行：wgpu 实例创建 + 渲染全程串行）
fn snapshot_harness(
    name: &str,
    size: (f32, f32),
    pixels_per_point: f32,
    main: MainState,
    ui_fn: impl FnMut(&mut egui::Ui, &mut MainState),
) {
    let _gpu = gpu_lock_acquire();
    let mut harness = egui_kittest::Harness::builder()
        .with_accessibility_check(false)
        .with_size(egui::vec2(size.0, size.1))
        .with_pixels_per_point(pixels_per_point)
        // 默认 4 步：本机信息采集 spinner / 后台任务会让「跑稳」需要几十帧
        .with_max_steps(200)
        .build_ui_state(ui_fn, main);
    let _ = harness.try_run();
    harness.snapshot(name);
}

/// 手工构造 MainState（不走 MainState::new：那会读真实用户目录 / 触发历史清理）
fn test_main_state() -> MainState {
    force_dx12_backend();
    let runtime = Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("测试 tokio runtime"),
    );
    let mut s = MainState {
        runtime,
        projects_root: std::env::temp_dir().join("rbs-ui-snapshots"),
        global: Default::default(),
        // 预置两个工具：工具管理列表 / 本机工具链面板按"非空列表"这一常态渲染
        // （putty = 探测自动生成的条目：套件目录配置在其专用表单里）
        tools: [
            ("java", "C:/Program Files/Java/jdk-17/bin/java.exe", "java version \"17.0.6\""),
            ("node", "D:/tools/fnm/node-versions/v22.22.2/node.exe", "v22.22.2"),
            ("putty", "C:/Program Files/PuTTY/pscp.exe", ""),
        ]
        .into_iter()
        .map(|(name, cmd, ver)| {
            (
                name.to_string(),
                crate::models::Tool {
                    name: name.to_string(),
                    command: cmd.to_string(),
                    version: Some(ver.to_string()),
                    version_check: Some(format!("{name} -v")),
                    ..Default::default()
                },
            )
        })
        .collect(),
        projects: vec![],
        validation: Default::default(),
        git_branches: Default::default(),
        dirty_baseline: None,
        job_dirty: false,
        project_edit: Default::default(),
        selected_project: None,
        selected_job: None,
        console: Default::default(),
        run: None,
        stop_requested: None,
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
        show_help: false,
        help_tab: Default::default(),
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
        // 预填空历史：设置页「运行历史」tab 见 Some 就直接渲染，不会去开真实 sqlite
        history_rows: Some(vec![]),
        run_step_durations: Vec::new(),
        run_step_retries: Vec::new(),
        last_group_failure: None,
        run_group_label: None,
        group_run_progress: None,
    };

    // 模拟项目：长项目名（测标题截断）+ 四种目标形态
    let mut project = crate::models::Project::default();
    project.name = "电商中台交付项目-spring-cloud-微服务发布系统".to_string();
    let mut targets = HashMap::new();
    targets.insert(
        "local".to_string(),
        Target {
            name: "local".to_string(),
            target_type: TargetType::Local,
            transport: None,
            work_root: None,
            env: Default::default(),
            cred_key: None,
        },
    );
    targets.insert(
        "prod".to_string(),
        Target {
            name: "prod".to_string(),
            target_type: TargetType::Ssh {
                host: "192.168.10.21".to_string(),
                port: 22,
                user: "deploy".to_string(),
                auth: AuthMethod::Password { encrypted: String::new() },
                host_key: None,
            },
            transport: Some(TransportKind::Russh),
            work_root: Some("/srv/app".to_string()),
            env: Default::default(),
            cred_key: None,
        },
    );
    targets.insert(
        "staging-backup-old-server-192.168.10.99".to_string(),
        Target {
            name: "staging-backup-old-server-192.168.10.99".to_string(),
            target_type: TargetType::Ssh {
                host: "192.168.10.99".to_string(),
                port: 2222,
                user: "root".to_string(),
                auth: AuthMethod::Key { path: "C:/keys/id_ed25519".to_string() },
                host_key: Some("SHA256:AbCdEf0123456789AbCdEf0123456789AbCdEf01234".to_string()),
            },
            transport: Some(TransportKind::Russh),
            work_root: None,
            env: Default::default(),
            cred_key: None,
        },
    );
    targets.insert(
        "k8s-prod-cluster".to_string(),
        Target {
            name: "k8s-prod-cluster".to_string(),
            target_type: TargetType::Kubernetes {
                kubeconfig: "C:/Users/demo/.kube/config".to_string(),
                context: Some("prod-context".to_string()),
                namespace: Some("app-prod".to_string()),
                registry: Some("registry.cn-hangzhou.aliyuncs.com".to_string()),
            },
            transport: None,
            work_root: None,
            env: Default::default(),
            cred_key: None,
        },
    );
    project.targets = targets;
    s.projects.push(crate::storage::files::ProjectEntry {
        dir: std::env::temp_dir().join("rbs-ui-snapshots").join("demo-project"),
        project,
        jobs: vec![],
    });
    s
}

/// 选中某目标并打开窗口（手工同步 load_buffers 要同步的表单缓冲，
/// 与真实路径 show() 的自动选中语义一致）
fn open_with_selected(mut main: MainState, name: &str) -> MainState {
    main.selected_project = Some(0);
    main.target_editor.open = true;
    main.target_editor.selected = Some(name.to_string());
    main.target_editor.name_buf = name.to_string();
    let t = main.projects[0].project.targets.get(name).cloned();
    match t.map(|t| t.target_type) {
        Some(TargetType::Ssh { auth, .. }) => {
            main.target_editor.kind_idx = 0;
            main.target_editor.auth_idx = match auth {
                AuthMethod::Key { .. } => 0,
                AuthMethod::Password { .. } => 1,
                AuthMethod::Agent => 2,
                AuthMethod::None => 3,
            };
            main.target_editor.transport_idx = 0;
        }
        Some(TargetType::Kubernetes { .. }) => main.target_editor.kind_idx = 1,
        _ => main.target_editor.kind_idx = 0,
    }
    main
}

#[test]
fn targets_ssh_form_default() {
    let main = open_with_selected(test_main_state(), "prod");
    snapshot_harness("targets_ssh_form_default", (1280.0, 800.0), 1.0, main, app_ui);
}

#[test]
fn targets_kubernetes_form_default() {
    let main = open_with_selected(test_main_state(), "k8s-prod-cluster");
    snapshot_harness("targets_kubernetes_form_default", (1280.0, 800.0), 1.0, main, app_ui);
}

#[test]
fn targets_ssh_form_narrow_window() {
    // 用户把窗口拖窄的场景：固定像素宽度最容易在这里出乱子
    let main = open_with_selected(test_main_state(), "prod");
    snapshot_harness("targets_ssh_form_narrow_window", (900.0, 640.0), 1.0, main, app_ui);
}

#[test]
fn targets_ssh_form_dpi_150() {
    // Windows 125%/150% 缩放是最常见的"整个乱掉"来源
    let main = open_with_selected(test_main_state(), "prod");
    snapshot_harness("targets_ssh_form_dpi_150", (1280.0, 800.0), 1.5, main, app_ui);
}

#[test]
fn targets_local_panel() {
    let mut main = open_with_selected(test_main_state(), "local");
    // 预置本机信息：面板会后台起线程采集（时序不定），预置后既稳定也不再起线程
    main.target_editor.local_info = Some(crate::env_detect::LocalSystemInfo {
        hostname: "BUILD-PC".to_string(),
        os: "Microsoft Windows [Version 10.0.26300.1]".to_string(),
        arch: "x86_64".to_string(),
        cpu_cores: 16,
        mem_total_mb: 32_768,
        mem_avail_mb: 18_432,
    });
    snapshot_harness("targets_local_panel", (1280.0, 800.0), 1.0, main, app_ui);
}

#[test]
fn help_window_cmd_tab() {
    let main = test_main_state();
    snapshot_harness("help_window_cmd_tab", (1280.0, 800.0), 1.0, main, help_ui);
}

#[test]
fn help_window_var_tab() {
    let mut main = test_main_state();
    main.help_tab = crate::ui::job_editor::HelpTab::Var;
    snapshot_harness("help_window_var_tab", (1280.0, 800.0), 1.0, main, help_ui);
}

/// 候选 emoji 的字形检查画廊：缺字形的会渲染成 □（豆腐块）。
/// UI 里用新图标前先跑一次看图挑选（缺字形=渲染成方块，一眼可辨）。
/// 另附折叠头图标对照区：默认三角 / 线段 chevron / 文本箭头——排查
/// 「项目树折叠头看不到三角」（egui 默认 icon 是 painter 三角，实测不渲染）。
#[test]
fn emoji_glyph_gallery() {
    let candidates = [
        "🔧", "🛠", "⚒", "🧰", "🧩", "📦", "⚙", "❓", "❔", "ℹ", "🖥", "💻",
        "❓ 变量语法", "🔧 工具管理", "🖥 本机", "💻 本机",
        // 折叠/展开箭头候选（msyh 字体链下逐一验证）
        "⌃ ⌄", "▲ ▼", "▴ ▾", "▶ ▶", "‹ ›", "⏵ ⏷", "^ v",
    ];
    let mut harness = egui_kittest::Harness::builder()
        .with_accessibility_check(false)
        .with_size(egui::vec2(420.0, 760.0))
        .build_ui_state(
            |ui: &mut egui::Ui, _s: &mut ()| {
                ui.ctx().set_missing_glyph_policy(egui::MissingGlyphPolicy::Tofu);
                load_cjk_font(ui.ctx());
                for c in &candidates {
                    ui.add(egui::Label::new(
                        egui::RichText::new(format!("{c}   <- 候选")).monospace().size(16.0),
                    ));
                }

                ui.add_space(8.0);
                ui.separator();
                ui.strong("折叠头 icon 对照（上=默认三角，下=线段 chevron）");
                for open in [true, false] {
                    egui::CollapsingHeader::new(format!("默认三角 open={open}"))
                        .default_open(open)
                        .show(ui, |ui| {
                            ui.weak("内容");
                        });
                    egui::CollapsingHeader::new(format!("线段chevron open={open}"))
                        .default_open(open)
                        .icon(|ui: &mut egui::Ui, openness: f32, response: &egui::Response| {
                            // 线段 chevron：两条 2px 线段，随 openness 旋转（字体无关，
                            // Path 形状在卡片描边等处已验证可渲染）
                            let rect = response.rect;
                            let c = rect.center();
                            let half = rect.height() * 0.25;
                            let spread = half * (1.0 - openness); // 展开时张开成 ^，收拢时贴近竖线
                            let stroke = egui::Stroke::new(2.0, ui.style().interact(response).fg_stroke.color);
                            ui.painter().line_segment(
                                [egui::pos2(c.x - half, c.y + spread * 0.0 + half * openness), egui::pos2(c.x, c.y - half * (0.5 + openness * 0.5))],
                                stroke,
                            );
                            ui.painter().line_segment(
                                [egui::pos2(c.x, c.y - half * (0.5 + openness * 0.5)), egui::pos2(c.x + half, c.y + half * openness)],
                                stroke,
                            );
                        })
                        .show(ui, |ui| {
                            ui.weak("内容");
                        });
                    // 文本字形折叠头：直接用能渲染的字符当 icon
                    egui::CollapsingHeader::new(format!("文本箭头 open={open}"))
                        .default_open(open)
                        .icon(|ui: &mut egui::Ui, _openness: f32, response: &egui::Response| {
                            let text = if response.rect.width() > 0.0 { "▶" } else { "▶" };
                            ui.painter().text(
                                response.rect.center(),
                                egui::Align2::CENTER_CENTER,
                                text,
                                egui::FontId::proportional(12.0),
                                ui.style().interact(response).fg_stroke.color,
                            );
                        })
                        .show(ui, |ui| {
                            ui.weak("内容");
                        });
                }
            },
            (),
        );
    let _ = harness.try_run();
    harness.snapshot("emoji_glyph_gallery");
}

// ---------------------------------------------------------------------------
// 全页面覆盖：主屏 + 各弹窗 + 交互（点击 / 键盘输入）
// ---------------------------------------------------------------------------

/// 主屏全景：工具栏 + 项目树（分组/流程）+ 步骤编辑器 + 控制台
#[test]
fn main_screen_overview() {
    let main = test_main_state_with_jobs();
    snapshot_harness("main_screen_overview", (1280.0, 800.0), 1.0, main, main_screen_ui);
}

/// 控制台折叠（点击标题左侧 chevron 图标）：与展开态对照，折叠后只剩标题行
#[test]
fn console_collapsed() {
    let _gpu = gpu_lock_acquire();
    let mut harness = main_harness((1280.0, 800.0), test_main_state_with_jobs());
    harness.run();
    harness.get_by_label("折叠控制台").click();
    harness.run();
    harness.snapshot("console_collapsed");
}

/// 项目树分组折叠（点击分组头 ▸）：树列收起后编辑器仍占满
#[test]
fn project_tree_group_collapsed() {
    let _gpu = gpu_lock_acquire();
    let mut harness = main_harness((1280.0, 800.0), test_main_state_with_jobs());
    harness.run();
    harness
        .query_by_label_contains("发布")
        .expect("分组头应可查询到")
        .click();
    harness.run();
    harness.snapshot("project_tree_group_collapsed");
}

/// ⚙ 设置 → 工具管理 → 选中 putty 条目：PuTTY 不再单开 tab，
/// 套件目录配置并入 putty 工具的专用表单（原 tab 内容同款）
#[test]
fn settings_window_putty_tool() {
    let _gpu = gpu_lock_acquire();
    let mut harness = main_harness((1280.0, 800.0), test_main_state_with_jobs());
    harness.run();
    harness.get_by_label("⚙ 设置").click();
    harness.run();
    harness.get_by_label("putty").click();
    harness.run();
    harness.snapshot("settings_window_putty_tool");
}

/// ⚙ 设置 → 全局配置 tab
#[test]
fn settings_window_global_tab() {
    let _gpu = gpu_lock_acquire();
    let mut harness = main_harness((1280.0, 800.0), test_main_state_with_jobs());
    harness.run();
    harness.get_by_label("⚙ 设置").click();
    harness.run();
    harness.get_by_label("⚙ 全局配置").click();
    harness.run();
    harness.snapshot("settings_window_global_tab");
}

/// ⚙ 设置 → 运行历史 tab（history_rows 预填空，不触真实 sqlite）
#[test]
fn settings_window_history_tab() {
    let _gpu = gpu_lock_acquire();
    let mut harness = main_harness((1280.0, 800.0), test_main_state_with_jobs());
    harness.run();
    harness.get_by_label("⚙ 设置").click();
    harness.run();
    harness.get_by_label("🕘 运行历史").click();
    harness.run();
    harness.snapshot("settings_window_history_tab");
}

/// ⚙ 设置 → 工具管理 tab（默认页）。标签与流程编辑器的「🔧 工具管理」按钮
/// 重名，按节点位置区分：设置窗口内的 tab 行在屏幕下半区（y > 200）
#[test]
fn settings_window_tools_tab() {
    let _gpu = gpu_lock_acquire();
    let mut harness = main_harness((1280.0, 800.0), test_main_state_with_jobs());
    harness.run();
    harness.get_by_label("⚙ 设置").click();
    harness.run();
    let tools_tab = harness
        .get_all_by_label("🔧 工具管理")
        .find(|n| n.rect().top() > 100.0)
        .expect("设置窗口的「🔧 工具管理」tab 应存在");
    tools_tab.click();
    harness.run();
    harness.snapshot("settings_window_tools_tab");
}

/// 目标管理输入流（全 UI 驱动）：工具栏开窗 → 点列表选中 prod →
/// 全选名称改「prod-eu」→ 点重命名 → 全选主机改 IP → 输入密码 →
/// 断言未保存标记与选中名，快照终态
#[test]
fn targets_input_flow() {
    let _gpu = gpu_lock_acquire();
    let mut harness = main_harness((1280.0, 800.0), test_main_state_with_jobs());
    harness.run();

    harness.get_by_label("🎯 目标").click();
    harness.run();
    harness.get_by_label("prod").click();
    harness.run();

    // 名称：AccessKit SetValue 全量替换（等价「全选后输入」；合成键盘的 Ctrl+A
    // 有焦点时序坑，a11y 动作是 egui 官方支持的文本替换通道）
    harness
        .get(By::new().role(Role::TextInput).value("prod"))
        .set_text_value_accesskit("prod-eu");
    harness.run();
    harness.get_by_label("重命名").click();
    harness.run();
    assert_eq!(
        harness.state().target_editor.selected.as_deref(),
        Some("prod-eu"),
        "重命名后选中项应同步"
    );

    // 主机：值唯一可定位 → 全量替换
    harness
        .get(By::new().role(Role::TextInput).value("192.168.10.21"))
        .set_text_value_accesskit("10.0.0.42");
    harness.run();

    // 密码：PasswordInput 角色唯一可定位（其余空输入框是 TextInput）；
    // 空字段上 type_text 键盘输入即全量
    harness.get_by_role(Role::PasswordInput).click();
    harness.run();
    harness.get_by_role(Role::PasswordInput).type_text("s3cret-pw");
    harness.run();

    assert!(harness.state().target_editor.unsaved, "输入后应有未保存标记");
    match &harness.state().projects[0].project.targets["prod-eu"].target_type {
        TargetType::Ssh { host, .. } => assert_eq!(host, "10.0.0.42", "主机输入应直接写入内存目标"),
        other => panic!("应是 SSH 目标，实际 {other:?}"),
    }
    harness.snapshot("targets_input_flow");
}

/// 连接测试失败态：❌ 结果 + 「📌 一键登记指纹」（真实 SSH 测试不做，状态直填）
#[test]
fn targets_test_result_state() {
    let mut main = open_with_selected(test_main_state_with_jobs(), "prod");
    main.target_editor.test_result = Some((false, "连接失败: host key verification failed".to_string()));
    main.target_editor.seen_fp =
        Some("SHA256:AbCdEf0123456789AbCdEf0123456789AbCdEf01234".to_string());
    snapshot_harness("targets_test_result_state", (1280.0, 800.0), 1.0, main, app_ui);
}

/// ＋ 新建流程弹窗
#[test]
fn new_job_window() {
    let mut main = test_main_state_with_jobs();
    main.new_job.open = true;
    snapshot_harness("new_job_window", (1280.0, 800.0), 1.0, main, main_screen_ui);
}

/// ＋ 新建项目弹窗（Git 模式默认表单）
#[test]
fn new_project_window() {
    let mut main = test_main_state_with_jobs();
    main.new_project.open = true;
    snapshot_harness("new_project_window", (1280.0, 800.0), 1.0, main, main_screen_ui);
}

/// 项目树删除确认弹窗（两段确认的第一段）
#[test]
fn tree_delete_confirm_dialog() {
    let mut main = test_main_state_with_jobs();
    main.tree_delete = Some(crate::ui::project_tree::TreeDeleteTarget::Job(0, 0));
    snapshot_harness("tree_delete_confirm_dialog", (1280.0, 800.0), 1.0, main, main_screen_ui);
}

/// 分组改名弹窗
#[test]
fn tree_rename_group_dialog() {
    let mut main = test_main_state_with_jobs();
    main.tree_rename = Some(crate::ui::project_tree::TreeRenameTarget::Group(0, "发布".to_string()));
    snapshot_harness("tree_rename_group_dialog", (1280.0, 800.0), 1.0, main, main_screen_ui);
}

/// 🔒 人工确认门（job.confirm）：输入确认词才放行运行
#[test]
fn confirm_gate_window() {
    let mut main = test_main_state_with_jobs();
    main.confirm_gate.open = true;
    main.confirm_gate.expected = "prod".to_string();
    main.confirm_gate.job_label = "demo-project/prod-deploy".to_string();
    snapshot_harness("confirm_gate_window", (1280.0, 800.0), 1.0, main, main_screen_ui);
}
/// 首启引导页（只在无配置时出现）。用 new_without_detection：不起后台探测
/// 线程（真实探测结果何时就绪不定，会让基线在 spinner / 结果间抖动）
/// 首帧的「探测中」状态是确定性的
#[test]
fn first_run_screen() {
    let _gpu = gpu_lock_acquire();
    let page = crate::ui::first_run::FirstRunPage::new_without_detection();
    let mut harness = egui_kittest::Harness::builder()
        .with_accessibility_check(false)
        .with_size(egui::vec2(1280.0, 800.0))
        .with_pixels_per_point(1.0)
        .build_ui_state(
            |ui: &mut egui::Ui, page: &mut crate::ui::first_run::FirstRunPage| {
                ui.ctx().set_missing_glyph_policy(egui::MissingGlyphPolicy::Tofu);
                load_cjk_font(ui.ctx());
                page.show(ui);
            },
            page,
        );
    harness.snapshot("first_run_screen");
}

/// 命令输入自动补全（CompletionPopup）端到端交互：
/// ① 命令模式：输入词 → 弹骨架表 → Tab 上屏替换输入词；
/// ② 变量模式：光标落在未闭合 `${{ ` 内 → 弹变量组变量 → 上屏自动闭合 ` }}`。
/// 布线用 SetValue 替换全文：egui 把光标放到替换文本末尾（词尾），确定性地触发弹层；
/// 弹层按键拦截在打开的下一帧生效，所以每次 key_press 前先 run() 两帧。
#[test]
fn command_completion_popup_flow() {
    let _gpu = gpu_lock_acquire();
    let mut main = test_main_state_with_jobs();
    // demo 项目没有变量组：补一个 prod 组（流程已选它），变量模式有东西可补
    main.projects[0].project.variables.insert(
        "prod".to_string(),
        crate::models::EnvironmentVariables {
            vars: [
                ("DEPLOY_DIR".to_string(), "/opt/yudao/prod".to_string()),
                ("REGISTRY".to_string(), "registry:5000".to_string()),
            ]
            .into_iter()
            .collect(),
        },
    );
    let mut harness = main_harness((1280.0, 800.0), main);
    harness.run();

    // Step 3（重启服务）的命令框：多行 TextEdit = MultilineTextInput 角色，
    // 按现有值定位（egui TextEdit 天生无无障碍名称）；卡片可能 below fold，先滚进来
    harness
        .get(By::new().role(Role::MultilineTextInput).value("systemctl restart app"))
        .scroll_to_me();
    harness.run();
    harness
        .get(By::new().role(Role::MultilineTextInput).value("systemctl restart app"))
        .click();
    harness.run();

    // ① 命令模式：全量替换为 "tail"（光标落词尾）→ 弹层列出 tail 骨架
    harness
        .get(By::new().role(Role::MultilineTextInput).value("systemctl restart app"))
        .set_text_value_accesskit("tail");
    harness.run();
    harness.run();
    harness.get(By::new().role(Role::Button).label_contains("tail -n 100"));
    harness.key_press(egui::Key::Tab);
    harness.run();
    let cmd = harness.state().projects[0].jobs[0].job.steps[2].command.clone();
    assert_eq!(cmd, "tail -n 100 ", "命令模式上屏：骨架应替换输入词");

    // ② 变量模式：光标处于未闭合 `${{ var` 内 → 列变量组变量（带现值）
    let field = harness
        .get_all_by_role(Role::MultilineTextInput)
        .find(|n| n.value().as_deref().is_some_and(|v| v.contains("tail")))
        .expect("上屏后应能按值找回命令框");
    field.set_text_value_accesskit("cp -a ${{ var");
    harness.run();
    harness.get(By::new().role(Role::Button).label_contains("variables.DEPLOY_DIR"));
    harness.key_press(egui::Key::Tab);
    harness.run();
    let cmd = harness.state().projects[0].jobs[0].job.steps[2].command.clone();
    assert_eq!(cmd, "cp -a ${{ variables.DEPLOY_DIR }}", "变量模式上屏应闭合 ` }}`");

    // ③ 参数级提示：行首词是已知命令（mysqldump）→ 空格后列常用参数；
    // ↓ 选中第 2 项 --single-transaction 后 Tab 上屏（顺带覆盖键盘导航）
    let field = harness
        .get_all_by_role(Role::MultilineTextInput)
        .find(|n| {
            n.value()
                .as_deref()
                .is_some_and(|v| v.contains("variables.DEPLOY_DIR"))
        })
        .expect("变量上屏后应能按值找回命令框");
    field.set_text_value_accesskit("mysqldump ");
    harness.run();
    harness.get(By::new().role(Role::Button).label_contains("--single-transaction"));
    harness.key_press(egui::Key::ArrowDown);
    harness.run();
    harness.key_press(egui::Key::Tab);
    harness.run();
    let cmd = harness.state().projects[0].jobs[0].job.steps[2].command.clone();
    assert_eq!(cmd, "mysqldump --single-transaction ", "参数提示：↓ 选中参数后 Tab 上屏");
}
