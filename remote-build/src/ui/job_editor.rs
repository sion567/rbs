//! 中间步骤编辑器（设计文档 §9.1 Job Editor）。

use crate::app::MainState;
use crate::models::{FailureAction, Job, Step, StepType, Tool};
use crate::ui::status::{self, StepVisualState};
use eframe::egui;
use egui::Color32;
use std::collections::HashMap;

/// 项目级编辑缓冲（变量表 / 工具版本要求）：跨帧保存正在输入的文本。
/// 直接绑 HashMap 键编辑会让行序随迭代乱跳、键名逐键变形——一律
/// 「缓冲输入 → changed 时收集动作 → 渲染后统一校验应用」
#[derive(Default)]
pub struct ProjectEditBufs {
    /// 新建变量组名输入缓冲
    pub vars_new_group: String,
    /// 组名 → 新变量名 / 新变量值（添加行缓冲）
    pub var_new_key: HashMap<String, String>,
    pub var_new_val: HashMap<String, String>,
    /// 组名 → 改组名缓冲（✓ 确认才生效）
    pub var_group_rename: HashMap<String, String>,
    /// 两段删除确认：待删变量组名
    pub var_del_armed: Option<String>,
    /// 工具版本要求：新条目的工具名 / 版本表达式
    pub tr_new_name: String,
    pub tr_new_expr: String,
}

/// 组名变化（改名/删除）后清掉该组的全部输入缓冲，避免残留旧名键
fn bufs_cleanup_group(bufs: &mut ProjectEditBufs, gname: &str) {
    bufs.var_new_key.remove(gname);
    bufs.var_new_val.remove(gname);
    bufs.var_group_rename.remove(gname);
    if bufs.var_del_armed.as_deref() == Some(gname) {
        bufs.var_del_armed = None;
    }
}
/// 步骤「工具」下拉的菜单数据：项目级 / 全局分组 + 合并解析表
pub struct ToolMenu {
    /// 当前项目 project.yaml tools 的键（含对全局的覆盖），已排序
    pub project: Vec<String>,
    /// 全局 tools.yaml 的键（排除已被项目覆盖的），已排序
    pub global: Vec<String>,
    /// 合并后的解析表（项目覆盖全局），用于徽标与悬停显示实际地址
    pub merged: std::collections::HashMap<String, Tool>,
}

impl ToolMenu {
    pub fn build(main: &MainState) -> Self {
        let proj_keys: std::collections::BTreeSet<String> = main
            .selected_project
            .and_then(|pi| main.projects.get(pi))
            .map(|p| p.project.tools.keys().cloned().collect())
            .unwrap_or_default();
        let project: Vec<String> = proj_keys.iter().cloned().collect();
        let mut global: Vec<String> = main
            .tools
            .keys()
            .filter(|k| !proj_keys.contains(*k))
            .cloned()
            .collect();
        global.sort();
        let mut merged = main.tools.clone();
        if let Some(pi) = main.selected_project {
            if let Some(p) = main.projects.get(pi) {
                merged.extend(p.project.tools.clone());
            }
        }
        Self { project, global, merged }
    }
}

/// 类型下拉收起态用短标签（收起按钮 90px 即可容纳，长说明在弹层里）：
/// 第一行此前塞不下 checkbox+Step号+名称+工具+类型+超时+5 个按钮（自然宽
/// 956px，布局回归测试实测），工具/超时移入「执行参数」行、类型用短标签收窄
const STEP_TYPES_SHORT: [&str; 6] = ["exec", "wait", "upload", "download", "pack", "k8s"];

const STEP_TYPES: [&str; 6] = [
    "exec（执行命令）",
    "wait（轮询等待）",
    "upload（上传文件）",
    "download（下载文件）",
    "pack（打包 zip）",
    "k8s-apply（kubectl 应用）",
];

fn step_type_index(st: &StepType) -> usize {
    match st {
        StepType::Exec => 0,
        StepType::Wait => 1,
        StepType::Upload => 2,
        StepType::Download => 3,
        StepType::Pack => 4,
        StepType::K8sApply => 5,
    }
}

fn step_type_from_index(i: usize) -> StepType {
    match i {
        0 => StepType::Exec,
        1 => StepType::Wait,
        2 => StepType::Upload,
        3 => StepType::Download,
        4 => StepType::Pack,
        _ => StepType::K8sApply,
    }
}

const WAIT_MODES: [&str; 3] = ["命令", "tcp", "http"];

fn wait_mode_index(m: crate::models::WaitMode) -> usize {
    match m {
        crate::models::WaitMode::Command => 0,
        crate::models::WaitMode::Tcp => 1,
        crate::models::WaitMode::Http => 2,
    }
}

fn wait_mode_from_index(i: usize) -> crate::models::WaitMode {
    match i {
        0 => crate::models::WaitMode::Command,
        1 => crate::models::WaitMode::Tcp,
        _ => crate::models::WaitMode::Http,
    }
}

/// 耗时展示：< 1s 用毫秒，否则秒（1 位小数），≥ 60s 用「x分y秒」
fn human_duration(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        let total = ms / 1000;
        format!("{}分{}秒", total / 60, total % 60)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StepAction {
    None,
    Delete,
    MoveUp,
    MoveDown,
    /// 从该步骤执行到流程结束
    RunFrom,
    /// 只执行该步骤（调试）
    RunOnly,
}

pub fn show(main: &mut MainState, ui: &mut egui::Ui) {
    let Some(pi) = main.selected_project else {
        ui.vertical_centered(|ui| {
            ui.add_space(40.0);
            ui.label("← 从左侧选择一个流程开始");
        });
        return;
    };
    let Some(ji) = main.selected_job else {
        ui.vertical_centered(|ui| {
            ui.add_space(40.0);
            ui.label("← 从左侧选择一个流程开始");
        });
        return;
    };
    if main.projects.get(pi).is_none() || main.projects[pi].jobs.get(ji).is_none() {
        ui.vertical_centered(|ui| ui.label("流程不存在，请点 ⟳ 重新加载"));
        return;
    }

    // 校验提示（§9.4）——读缓存（reload 时算一次）；先取 owned，避免与下面的可变借用冲突
    let issues = main
        .validation
        .get(&main.projects[pi].project.name)
        .cloned()
        .unwrap_or_default();
    for issue in &issues {
        match issue {
            crate::models::ValidationIssue::Error { message, .. } => {
                ui.colored_label(Color32::from_rgb(220, 100, 90), format!("⚠ {message}"));
            }
            crate::models::ValidationIssue::Warning { message, .. } => {
                ui.colored_label(Color32::from_rgb(230, 180, 80), format!("⚠ {message}"));
            }
        }
    }

    // 是否有作业在跑（在 entry 借用之前取，避免借用冲突）
    let running = main.run_active();
    // 工具菜单（项目级 / 全局分组 + 合并解析表）
    let tool_menu = ToolMenu::build(main);

    // 状态可视化：光圈颜色按主题取色；状态快照为 owned，避免与下面的可变借用冲突
    let dark = !matches!(main.global.theme, crate::models::Theme::Light);
    let (run_proj, run_job_name) = main.run_job.clone().unwrap_or_default();
    let step_states = main.run_steps.clone();
    // 耗时时间线 / 重试标注（GHA 式）：仅「正在显示 = 正在运行」的作业展示
    let step_durations = main.run_step_durations.clone();
    let step_retries = main.run_step_retries.clone();

    // 同一 ProjectEntry 的两个字段分别借用（字段级借用拆分）
    let entry = &mut main.projects[pi];
    // 变量组的引用计数（排除当前流程）：改名/删除前检查用。必须在 job_entry
    // 的可变借用占住 entry.jobs 之前收集——之后整个 show 期间不能再遍历 jobs
    let env_refs_other: HashMap<String, usize> = {
        let mut m: HashMap<String, usize> = HashMap::new();
        for (idx, je) in entry.jobs.iter().enumerate() {
            if idx != ji {
                if let Some(g) = &je.job.environment {
                    *m.entry(g.clone()).or_default() += 1;
                }
            }
        }
        m
    };
    let project = &mut entry.project;
    let Some(job_entry) = entry.jobs.get_mut(ji) else {
        return;
    };
    let job = &mut job_entry.job;

    // ── 运行中禁用编辑输入 ──
    // 注意不能 disable 整个面板 Ui：egui 的滚轮滚动会检查所在 Ui 的 enabled
    // （scroll_area.rs 的 mouse_wheel 门），禁用容器会连"翻看步骤"一起锁死。
    // 因此分块禁用：第 1/2/3 行各包 add_enabled_ui；步骤列表只 disable 内容，
    // ScrollArea 本身保持 enabled，滚轮/拖动翻看不受影响（⏹ Stop 在工具栏）。
    // 顺带保证运行中步骤列表不被增删，光圈与 step_index 不会错位。

    let env_names: Vec<String> = project.variables.keys().cloned().collect();
    // 目标下拉 = 本项目 project.yaml 的 targets 表（§3.5 目标随项目走）
    let mut target_names: Vec<String> = project.targets.keys().cloned().collect();
    target_names.sort();
    // 命令补全数据源：变量组的 (组名, 键, 值)（排序保证弹层稳定）。流程选了
    // 变量组只提示该组；未选组提示全部组（desc 标注组名 + ⚠ 运行时不替换）
    let mut var_names: Vec<(String, String, String)> = project
        .variables
        .iter()
        .flat_map(|(g, group)| {
            group
                .vars
                .iter()
                .map(move |(k, v)| (g.clone(), k.clone(), v.clone()))
        })
        .collect();
    var_names.sort();
    let group_selected = job.environment.is_some();
    if let Some(g) = &job.environment {
        var_names.retain(|(vg, _, _)| vg == g);
    }
    // 流程默认目标的展示文本（步骤「跟随流程」时显示它）
    let job_target_display = job.target.clone().unwrap_or_else(|| "无".to_string());

    // ── 第 1 行：流程名 + 项目目录（占满剩余宽度）──
    // ── 第 2 行：变量组 / 目标 / 失败策略（悬停可见说明）──
    ui.add_enabled_ui(!running, |ui| {
        ui.horizontal(|ui| {
            // 长流程名截断 + 悬停看全：heading 不截断，会把整行撑宽、
            // 把项目目录输入框和浏览按钮推出可视区
            let shown_name = crate::ui::truncate_label(&job.name, 24);
            ui.add(
                egui::Label::new(egui::RichText::new(format!("📄 {shown_name}")).heading())
                    .truncate(),
            )
            .on_hover_text(&job.name);
            ui.separator();
            ui.label("项目目录:");
            let dir_width = (ui.available_width() - 90.0).max(280.0);
            ui.add(
                egui::TextEdit::singleline(&mut project.local_path)
                    .desired_width(dir_width)
                    .hint_text("D:\\workspace\\my-project"),
            );
            if ui.button("📂 浏览…").clicked() {
                // 起点 = 输入框当前路径（所有浏览按钮统一约定）
                if let Some(dir) = crate::ui::pick_folder(&project.local_path, "选择项目本地目录") {
                    project.local_path = dir.to_string_lossy().to_string();
                }
            }
        });
        ui.horizontal(|ui| {
            ui.label("变量组:").on_hover_text(
                "决定 ${{ variables.X }} 从哪张变量表取值（project.yaml 的 variables.<组名>）。\n\
                 例：变量组=prod 时 ${{ variables.DEPLOY_DIR }} 取 variables.prod.DEPLOY_DIR，\n\
                 同一条流程换变量组即换一套参数（私服地址 / 部署目录等）。\n\
                 注意：它不是 Spring profile 那样的运行环境；执行机器由「目标」决定。",
            );
            egui::ComboBox::from_id_salt("env_combo")
                .truncate()
                .selected_text(job.environment.clone().unwrap_or_else(|| "无".to_string()))
                .width(140.0)
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut job.environment, None, "无");
                    for name in &env_names {
                        ui.selectable_value(&mut job.environment, Some(name.clone()), name);
                    }
                });

            ui.separator();
            ui.label("默认目标:").on_hover_text(
                "流程的默认执行机器（取自本项目 project.yaml 的 targets 表）。\n\
                 每个步骤可用自己的「目标」覆盖它——混合流程示例：\n\
                 下载(local) → 编译(local) → 上传(prod) → 发布(prod)。\n\
                 local = 本机 cmd 执行；SSH 目标 = 远端 Linux（exec 经 plink、upload 经 pscp）。",
            );
            egui::ComboBox::from_id_salt("target_combo")
                .truncate()
                .selected_text(job.target.clone().unwrap_or_else(|| "无".to_string()))
                .width(160.0)
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut job.target, None, "无");
                    for name in &target_names {
                        ui.selectable_value(&mut job.target, Some(name.clone()), name);
                    }
                });

            ui.separator();
            ui.label("on_failure:").on_hover_text(
                "某步失败且该步未勾选「失败继续」时的处理：\n\
                 stop = 立即终止整个流程（默认）；continue = 跳过该步继续后面的步骤；\n\
                 retry = 整个流程重试（P1，当前等同 stop）。",
            );
            let on_failure_label = match &job.on_failure {
                FailureAction::Stop => "stop",
                FailureAction::Continue => "continue",
                FailureAction::Retry => "retry",
            };
            egui::ComboBox::from_id_salt("on_failure_combo")
                .selected_text(on_failure_label)
                .width(110.0)
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut job.on_failure, FailureAction::Stop, "stop");
                    ui.selectable_value(&mut job.on_failure, FailureAction::Continue, "continue");
                    ui.selectable_value(&mut job.on_failure, FailureAction::Retry, "retry");
                });
            // job 级整体重试参数（on_failure: retry 时生效）
            // 定宽：DragValue 缺省会按内容自适应变宽，输入位数一多把同行后续控件挤出窗口
            if matches!(job.on_failure, FailureAction::Retry) {
                let r = job.retry.get_or_insert(crate::models::RetryConfig {
                    max_attempts: 2,
                    backoff_seconds: 5,
                });
                ui.weak("重试:");
                ui.add(
                    egui::DragValue::new(&mut r.max_attempts)
                        .suffix("次")
                        .clip_text(true)
                        .min_size(egui::vec2(52.0, 0.0))
                        .range(1..=10),
                );
                ui.add(
                    egui::DragValue::new(&mut r.backoff_seconds)
                        .suffix("s")
                        .clip_text(true)
                        .min_size(egui::vec2(56.0, 0.0))
                        .range(0..=600),
                );
            }
        });
    });

    // ── 第 3 行：流程管理按钮 ──（确认门也放这里：第 2 行在 on_failure=retry
    // 展开重试参数时横向空间不够，会把确认门挤出可视区）
    let mut open_tool_overrides = false;
    let mut create_job_requested = false;
    let mut delete_job_confirmed = false;
    ui.add_space(4.0);
    ui.add_enabled_ui(!running, |ui| {
        ui.horizontal(|ui| {
            // 人工确认门（GHA environment protection）：防手滑上生产
            match &mut job.confirm {
                Some(c) => {
                    ui.label("确认门:").on_hover_text(
                        "运行前弹窗要求输入该确认词（如 prod）才放行，防手滑上生产。\n\
                         CLI 对应 remote-build run ... [--yes] 跳过",
                    );
                    ui.add(
                        egui::TextEdit::singleline(c)
                            .desired_width(70.0)
                            .hint_text("prod"),
                    );
                    if ui.small_button("🗑").clicked() {
                        job.confirm = None;
                    }
                    ui.separator();
                }
                None => {
                    if ui
                        .small_button("＋确认门")
                        .on_hover_text(
                            "运行前要求输入确认词（如 prod）才放行，防手滑上生产",
                        )
                        .clicked()
                    {
                        job.confirm = Some("prod".to_string());
                    }
                    ui.separator();
                }
            }
            // 流程级总闸（job.timeout）：编译构建类流程一般不需要；
            // 部署类可设兜底（如 1800）防失控（起步模板不再强加 60s 的教训）
            match &mut job.timeout {
                Some(v) => {
                    ui.label("总超时:").on_hover_text(
                        "流程级总闸（秒）：到点后剩余步骤不再启动，运行中的步骤被提前掐断。\n\
                         填 0 = 不限时；🗑 清除 = 不限时（字段从 yaml 移除）。\n\
                         单步超时在每张步骤卡片上配；没配的步骤走 ⚙ 设置的全局默认超时",
                    );
                    ui.add(
                        egui::DragValue::new(v)
                            .suffix("s")
                            .speed(10.0)
                            .clip_text(true)
                            .min_size(egui::vec2(64.0, 0.0))
                            // 无符号数拖到 0 以下会回绕成 u64::MAX（真实踩坑），30 天封顶
                            .range(0..=2_592_000),
                    );
                    if *v == 0 {
                        ui.weak("（不限时）");
                    }
                    if ui.small_button("🗑").clicked() {
                        job.timeout = None;
                    }
                }
                None => {
                    if ui
                        .small_button("⏱ 总超时")
                        .on_hover_text(
                            "加流程级总闸（job.timeout，默认 3600s）：到点后剩余步骤不再启动。\n\
                             编译构建类流程一般不需要；部署类流程可设兜底防失控",
                        )
                        .clicked()
                    {
                        job.timeout = Some(3600);
                    }
                }
            }
            ui.separator();
            // 与 ⚙ 设置窗口的「🔧 工具管理」页同名：这里是一键直达本项目的工具覆盖页
            if ui
                .button("🔧 工具管理")
                .on_hover_text("管理本项目的工具覆盖（写入 project.yaml 的 tools 段）；\n机器级工具在 ⚙ 设置 → 🔧 工具管理 → 🌐 全局")
                .clicked()
            {
                open_tool_overrides = true;
            }
            // 「变量语法」入口已上移到主工具栏（❓，⚙ 设置 / 🎯 目标旁）：
            // 语法速查是全局参考，不该只挂在流程编辑器里
            ui.separator();
            if ui.button("＋ 新建流程").clicked() {
                create_job_requested = true;
                main.new_job.message = None;
                main.new_job.name.clear();
            }
            // 删除流程：两段确认，避免误删
            let job_name = job.name.clone();
            if !main.delete_job_armed {
                if ui.button("🗑 删除流程").clicked() {
                    main.delete_job_armed = true;
                }
            } else {
                ui.colored_label(Color32::from_rgb(230, 120, 90), format!("删除「{job_name}」？"));
                if ui.small_button("确认").clicked() {
                    delete_job_confirmed = true;
                    main.delete_job_armed = false;
                }
                if ui.small_button("取消").clicked() {
                    main.delete_job_armed = false;
                }
            }
        });
    });

    ui.add_space(8.0);
    ui.separator();

    // ── 前置检查（Task preconditions）：执行前本地断言，失败零秒退出 ──
    // 折叠区设计：条目非空才渲染头部（空时零占位）；有条目默认展开，
    // 用户手动收起后 egui 记住选择（id_salt 固定，增删条目不重置状态）——
    // 前置一多不再把下面步骤列表的可视区挤没
    let mut pre_del: Option<usize> = None;
    let mut pre_add = false;
    ui.add_enabled_ui(!running, |ui| {
        ui.horizontal(|ui| {
            if ui
                .small_button("＋前置检查")
                .on_hover_text(
                    "执行前在发版机本地逐条运行，任一退出码非 0 → 整个流程零秒失败并展示说明。\n\
                     例：java -version（失败说明「JDK 未安装」）；支持 ${{ variables.X }} 替换\n\
                     ⚠ 只放无副作用的检查命令：成功时输出不显示、固定 30s 超时。\n\
                     拉代码 / 构建这类动作请加为普通步骤（输出实时打印、可配超时重试）",
                )
                .clicked()
            {
                pre_add = true;
            }
            if job.preconditions.is_empty() {
                ui.weak("（无前置检查——需要发版前断言环境时点左侧添加）");
            }
        });
    });
    if !job.preconditions.is_empty() {
        egui::CollapsingHeader::new(egui::RichText::new(format!(
            "⚖ 前置检查（{} 条，任一失败整个流程不执行）",
            job.preconditions.len()
        )))
        .icon(crate::ui::chevron_icon)
        .id_salt("job_editor_preconditions")
        .default_open(true)
        .show(ui, |ui| {
            ui.add_enabled_ui(!running, |ui| {
                for (i, pre) in job.preconditions.iter_mut().enumerate() {
                    // 检查命令与人话说明上下两行：并排两个 LONG 定宽输入框在窄窗口
                    // 会把右半截推出可视区（横向溢出，× 都看不到）
                    ui.horizontal(|ui| {
                        if ui.small_button("🗑").clicked() {
                            pre_del = Some(i);
                        }
                        ui.monospace(format!("{}", i + 1));
                        let check_w = (ui.available_width() - 30.0).max(160.0);
                        ui.add(
                            egui::TextEdit::singleline(&mut pre.check)
                                .desired_width(check_w)
                                .hint_text("本地命令，如 java -version"),
                        );
                    });
                    ui.indent("pre_msg_indent", |ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut pre.message)
                                .desired_width(ui.available_width())
                                .hint_text("失败时的人话说明（为什么失败 / 该怎么办）"),
                        );
                    });
                }
            });
        });
    }
    if let Some(i) = pre_del {
        if i < job.preconditions.len() {
            job.preconditions.remove(i);
        }
    }
    if pre_add {
        job.preconditions.push(crate::models::Precondition::default());
    }

    // ── 变量表（project.yaml variables）──────────────────────────
    // 此前只能手写 yaml（用户反馈：最常用、手写最容易错）。缓冲输入 →
    // changed 收集动作 → 渲染后统一校验应用（键名逐键改会乱跳，见 ProjectEditBufs）
    let mut var_add_group = false;
    let mut var_del_group: Option<String> = None;
    let mut var_rename_group: Option<(String, String)> = None; // (旧组名, 新组名)
    let mut var_add: Option<String> = None; // 组名（键值从缓冲取）
    let mut var_del: Option<(String, String)> = None; // (组名, 键)
    let mut var_key_rename: Option<(String, String, String)> = None; // (组名, 旧键, 新键)
    {
        let bufs = &mut main.project_edit;
        let total_vars: usize = project.variables.values().map(|g| g.vars.len()).sum();
        let header = egui::CollapsingHeader::new(egui::RichText::new(format!(
            "⛁ 变量表（{} 个变量组 / {} 个变量）",
            project.variables.len(),
            total_vars
        )))
        .icon(crate::ui::chevron_icon)
        .id_salt("job_editor_variables")
        .default_open(false);
        ui.add_enabled_ui(!running, |ui| {
            header.show(ui, |ui| {
                ui.weak("${{ variables.键 }} 按当前流程的「变量组」从这里取值；改动随 💾 Save 写入 project.yaml。");
                let mut gnames: Vec<String> = project.variables.keys().cloned().collect();
                gnames.sort();
                for gname in &gnames {
                    let Some(group) = project.variables.get_mut(gname) else {
                        continue;
                    };
                    ui.add_space(2.0);
                    ui.horizontal(|ui| {
                        ui.strong(gname);
                        // 改组名：缓冲 + ✓ 确认（被其他流程引用时拒绝）
                        let ren = bufs.var_group_rename.entry(gname.clone()).or_default();
                        let resp = ui.add(
                            egui::TextEdit::singleline(ren)
                                .desired_width(110.0)
                                .hint_text("新组名"),
                        );
                        if resp.changed()
                            && !ren.trim().is_empty()
                            && ren.trim() != gname
                            && env_refs_other.get(gname).copied().unwrap_or(0) > 0
                        {
                            main.status_msg = Some(format!(
                                "⚠ 变量组「{gname}」被其他 {} 个流程引用，先在那些流程里换组再改名",
                                env_refs_other[gname]
                            ));
                            ren.clear();
                        } else if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter))
                            || ui.small_button("✓").clicked()
                        {
                            let new_name = ren.trim().to_string();
                            if !new_name.is_empty() && &new_name != gname {
                                var_rename_group = Some((gname.clone(), new_name));
                            }
                        }
                        if bufs.var_del_armed.as_deref() == Some(gname.as_str()) {
                            ui.colored_label(
                                Color32::from_rgb(230, 120, 90),
                                format!("删除「{gname}」？"),
                            );
                            if ui.small_button("确认").clicked() {
                                var_del_group = Some(gname.clone());
                            }
                            if ui.small_button("取消").clicked() {
                                bufs.var_del_armed = None;
                            }
                        } else if ui.small_button("🗑").clicked() {
                            bufs.var_del_armed = Some(gname.clone());
                        }
                    });
                    // 键值行：键名排序展示（HashMap 迭代序每帧乱跳，行会闪）
                    let mut keys: Vec<String> = group.vars.keys().cloned().collect();
                    keys.sort();
                    for k in &keys {
                        ui.horizontal(|ui| {
                            let Some(val) = group.vars.get_mut(k) else {
                                return;
                            };
                            // 改键名 = 收集动作（渲染后校验重名再换键）
                            let mut kb = k.clone();
                            let k_resp = ui.add(
                                egui::TextEdit::singleline(&mut kb)
                                    .desired_width(160.0)
                                    .hint_text("键，如 REGISTRY"),
                            );
                            if k_resp.changed() && kb != *k && !kb.trim().is_empty() {
                                var_key_rename = Some((gname.clone(), k.clone(), kb.trim().to_string()));
                            }
                            ui.add(
                                egui::TextEdit::singleline(val)
                                    .desired_width(ui.available_width() - 40.0)
                                    .hint_text("值"),
                            );
                            if ui.small_button("🗑").clicked() {
                                var_del = Some((gname.clone(), k.clone()));
                            }
                        });
                    }
                    // ＋变量行：键值缓冲（组名键控）
                    let nk = bufs.var_new_key.entry(gname.clone()).or_default();
                    let nv = bufs.var_new_val.entry(gname.clone()).or_default();
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(nk)
                                .desired_width(160.0)
                                .hint_text("新键名"),
                        );
                        ui.add(
                            egui::TextEdit::singleline(nv)
                                .desired_width(200.0)
                                .hint_text("值（可含 ${{ … }} 占位符）"),
                        );
                        if ui.small_button("＋变量").clicked() && !nk.trim().is_empty() {
                            var_add = Some(gname.clone());
                        }
                    });
                }
                ui.add_space(4.0);
                ui.separator();
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut bufs.vars_new_group)
                            .desired_width(160.0)
                            .hint_text("新变量组名，如 prod"),
                    );
                    if ui
                        .small_button("＋新建变量组")
                        .on_hover_text("同一流程换组即换一套参数（私服地址 / 部署目录等）；建好后在流程第 2 行「变量组」下拉选用")
                        .clicked()
                        && !bufs.vars_new_group.trim().is_empty()
                    {
                        var_add_group = true;
                    }
                });
            });
        });
    }
    // 应用变量表动作（渲染期收集，统一校验——重名/空名在此拦截）
    if var_add_group {
        let name = main.project_edit.vars_new_group.trim().to_string();
        if !name.is_empty() && !project.variables.contains_key(&name) {
            project.variables.insert(name, Default::default());
        }
        main.project_edit.vars_new_group.clear();
    }
    if let Some((old, new)) = var_rename_group {
        if !project.variables.contains_key(&new) {
            if let Some(g) = project.variables.remove(&old) {
                project.variables.insert(new.clone(), g);
            }
            // 当前流程正用着旧组名 → 同步换过来；别的流程在收集期已拦截
            if job.environment.as_deref() == Some(old.as_str()) {
                job.environment = Some(new.clone());
            }
            bufs_cleanup_group(&mut main.project_edit, &old);
            main.status_msg = Some(format!("变量组已改名: {old} → {new}（💾 Save 落盘）"));
        } else {
            main.status_msg = Some(format!("⚠ 变量组「{new}」已存在"));
        }
    }
    if let Some(g) = var_del_group {
        let referenced_by_current = job.environment.as_deref() == Some(g.as_str());
        if referenced_by_current || env_refs_other.contains_key(&g) {
            main.status_msg = Some(format!(
                "⚠ 变量组「{g}」仍被流程引用（{} 个其他流程{}），先换掉引用再删",
                env_refs_other.get(&g).copied().unwrap_or(0),
                if referenced_by_current { "，含当前流程" } else { "" }
            ));
        } else {
            project.variables.remove(&g);
            bufs_cleanup_group(&mut main.project_edit, &g);
            main.status_msg = Some(format!("变量组「{g}」已删除（💾 Save 落盘）"));
        }
        main.project_edit.var_del_armed = None;
    }
    if let Some((g, k)) = var_del {
        if let Some(group) = project.variables.get_mut(&g) {
            group.vars.remove(&k);
        }
    }
    if let Some((g, old_k, new_k)) = var_key_rename {
        if let Some(group) = project.variables.get_mut(&g) {
            if !group.vars.contains_key(&new_k) {
                if let Some(v) = group.vars.remove(&old_k) {
                    group.vars.insert(new_k, v);
                }
            }
        }
    }
    if let Some(g) = var_add {
        let key = main.project_edit.var_new_key.get(&g).cloned().unwrap_or_default();
        let val = main.project_edit.var_new_val.get(&g).cloned().unwrap_or_default();
        if let Some(group) = project.variables.get_mut(&g) {
            if group.vars.contains_key(key.trim()) {
                main.status_msg = Some(format!("⚠ 变量「{}」已存在", key.trim()));
            } else {
                group.vars.insert(key.trim().to_string(), val);
                main.project_edit.var_new_key.insert(g.clone(), String::new());
                main.project_edit.var_new_val.insert(g, String::new());
            }
        }
    }

    // ── 工具版本要求（project.yaml tools_require，mise 风格）──────────
    // 运行前逐项断言（>= / > / <= / < 段比较或前缀匹配），不满足零步失败。
    // 此前只能手写 yaml
    let mut tr_del: Option<String> = None;
    let mut tr_add = false;
    {
        let bufs = &mut main.project_edit;
        let header = egui::CollapsingHeader::new(egui::RichText::new(format!(
            "🛠 工具版本要求（{} 项，运行前断言不满足即失败）",
            project.tools_require.len()
        )))
        .icon(crate::ui::chevron_icon)
        .id_salt("job_editor_tools_require")
        .default_open(false);
        ui.add_enabled_ui(!running, |ui| {
            header.show(ui, |ui| {
                ui.weak("表达式：\">=21\" 段比较、\"22\" 前缀匹配 22.x、\"22.1.3\" 精确；工具名须在 ⚙ 设置的工具清单里。");
                let mut names: Vec<String> = project.tools_require.keys().cloned().collect();
                names.sort();
                for name in &names {
                    ui.horizontal(|ui| {
                        ui.monospace(name);
                        let Some(expr) = project.tools_require.get_mut(name) else {
                            return;
                        };
                        ui.add(
                            egui::TextEdit::singleline(expr)
                                .desired_width(160.0)
                                .hint_text("\">=21\""),
                        );
                        if ui.small_button("🗑").clicked() {
                            tr_del = Some(name.clone());
                        }
                    });
                }
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut bufs.tr_new_name)
                            .desired_width(160.0)
                            .hint_text("工具名，如 java"),
                    );
                    ui.add(
                        egui::TextEdit::singleline(&mut bufs.tr_new_expr)
                            .desired_width(160.0)
                            .hint_text("\">=21\""),
                    );
                    if ui.small_button("＋要求").clicked() && !bufs.tr_new_name.trim().is_empty() {
                        tr_add = true;
                    }
                });
            });
        });
    }
    if let Some(name) = tr_del {
        project.tools_require.remove(&name);
    }
    if tr_add {
        let name = main.project_edit.tr_new_name.trim().to_string();
        let expr = main.project_edit.tr_new_expr.trim().to_string();
        if project.tools_require.contains_key(&name) {
            main.status_msg = Some(format!("⚠ 工具「{name}」已有版本要求，先删旧再加"));
        } else if expr.is_empty() {
            main.status_msg = Some("⚠ 版本表达式不能为空".to_string());
        } else {
            project.tools_require.insert(name, expr);
            main.project_edit.tr_new_name.clear();
            main.project_edit.tr_new_expr.clear();
        }
    }

    ui.add_space(4.0);

    // ── 步骤列表 ─────────────────────────────────────────────────────
    let mut actions: Vec<StepAction> = Vec::new();
    egui::ScrollArea::vertical()
        .id_salt("steps_scroll")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            // 运行中只禁内容、不禁 ScrollArea 所在容器：滚轮翻看仍可用
            if running {
                ui.disable();
            }
            for si in 0..job.steps.len() {
                // 光圈/耗时条只挂在「正在显示的作业 = 正在运行的作业」上
                let is_active_job = run_proj == project.name && run_job_name == job.name;
                let state = if is_active_job {
                    step_states.get(si).copied().unwrap_or_default()
                } else {
                    StepVisualState::Idle
                };
                // 耗时时间线：(耗时 ms, 相对最慢步骤的占比, 重试次数)，由 show 统一算好
                let timeline = if is_active_job {
                    step_durations.get(si).copied().flatten().map(|ms| {
                        let max = step_durations
                            .iter()
                            .flatten()
                            .copied()
                            .max()
                            .unwrap_or(ms)
                            .max(1);
                        (
                            ms,
                            (ms as f32 / max as f32).clamp(0.03, 1.0),
                            step_retries.get(si).copied().unwrap_or(0),
                        )
                    })
                } else {
                    None
                };
                actions.push(edit_step(
                    ui,
                    job,
                    si,
                    &tool_menu,
                    running,
                    &target_names,
                    &var_names,
                    group_selected,
                    job_target_display.as_str(),
                    main.global.default_timeout,
                    state,
                    dark,
                    timeline,
                ));
            }

            ui.add_space(4.0);
            if ui.button("＋ 添加步骤").clicked() {
                job.steps.push(Step {
                    name: None,
                    tool: String::new(),
                    command: String::new(),
                    step_type: StepType::Exec,
                    enabled: true,
                    ..Default::default()
                });
            }
        });

    // 从后往前应用删除/上移，避免索引失效（一帧至多一个点击动作，方向混排安全）
    for (si, action) in actions.iter().enumerate().rev() {
        match action {
            StepAction::Delete => {
                if si < job.steps.len() {
                    job.steps.remove(si);
                }
            }
            StepAction::MoveUp => {
                if si > 0 && si < job.steps.len() {
                    job.steps.swap(si - 1, si);
                }
            }
            StepAction::MoveDown => {
                if si + 1 < job.steps.len() {
                    job.steps.swap(si, si + 1);
                }
            }
            StepAction::RunFrom | StepAction::RunOnly => {}
            StepAction::None => {}
        }
    }

    // 范围执行（推迟到所有借用结束后再动 main）
    let run_from = actions.iter().position(|a| *a == StepAction::RunFrom);
    let run_only = actions.iter().position(|a| *a == StepAction::RunOnly);
    if let Some(si) = run_only {
        main.start_run(si, Some(si));
    } else if let Some(si) = run_from {
        main.start_run(si, None);
    }

    // 打开「本项目」工具覆盖（推迟到所有借用结束后再动 main）
    if open_tool_overrides {
        crate::ui::settings::open_project_scope(main);
    }
    if create_job_requested {
        main.new_job.open = true;
        main.new_job.preset_group = None; // 编辑器入口默认不分组（分组入口会带预选）
    }
    if delete_job_confirmed {
        delete_current_job(main);
    }
}

// ---------------------------------------------------------------------------
// 新建 / 删除流程（子 job 自由命名，不走下拉枚举）
// ---------------------------------------------------------------------------

/// 新建流程窗口状态：名字随便起（local / test / qa / deploy-dev:h5…），
/// 一个流程一个 yaml 文件，目录里有什么树里就显示什么
#[derive(Default)]
pub struct NewJobState {
    pub open: bool,
    pub name: String,
    /// 创建时复制当前选中流程的步骤（否则用起始模板）
    pub clone_from: bool,
    /// 预选分组（从分组右键「＋ 添加任务」打开时带入；窗口内可改/清除）
    pub preset_group: Option<String>,
    pub message: Option<(bool, String)>,
}

pub fn show_new_job_window(ctx: &egui::Context, main: &mut MainState) {
    let mut open = main.new_job.open;
    if open {
        crate::ui::modal_backdrop(ctx, "mask_new_job");
    }
    crate::ui::centered_window(egui::Window::new("＋ 新建流程"), ctx)
    .collapsible(false)
        .open(&mut open)
        .order(egui::Order::Foreground)
        .default_width(520.0)
        .show(ctx, |ui| {
            let Some(pi) = main.selected_project else {
                ui.weak("请先在左侧选择一个项目");
                return;
            };
            ui.weak(format!(
                "项目：{}（配置目录 {}）",
                main.projects[pi].project.name,
                main.projects[pi].dir.display()
            ));
            ui.add_space(6.0);

            ui.horizontal(|ui| {
                ui.label("流程名:");
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut main.new_job.name)
                        .desired_width(240.0)
                        .hint_text("local / test / qa / deploy-dev:h5 …"),
                );
                let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if enter {
                    create_job(main);
                }
            });
            ui.weak("自由命名不走枚举；文件名自动净化（dev:h5 → dev_h5.yaml），树上仍显示原名");
            // 分组：从分组右键打开时预选，这里可改
            {
                let groups = &main.projects[pi].project.groups;
                if !groups.is_empty() || main.new_job.preset_group.is_some() {
                    ui.horizontal(|ui| {
                        ui.label("分组:");
                        let selected =
                            main.new_job.preset_group.clone().unwrap_or_else(|| "（不分组）".to_string());
                        egui::ComboBox::from_id_salt("new_job_group")
                            .truncate()
                            .selected_text(selected)
                            .width(180.0)
                            .show_ui(ui, |ui| {
                                ui.selectable_value(
                                    &mut main.new_job.preset_group,
                                    None,
                                    "（不分组）",
                                );
                                for g in groups {
                                    ui.selectable_value(
                                        &mut main.new_job.preset_group,
                                        Some(g.clone()),
                                        g,
                                    );
                                }
                            });
                    });
                }
            }
            ui.add_space(4.0);
            ui.checkbox(
                &mut main.new_job.clone_from,
                "复制当前选中流程的步骤作为起点",
            );
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("创建").clicked() {
                    create_job(main);
                }
                if let Some((ok, msg)) = &main.new_job.message {
                    let color = if *ok {
                        Color32::from_rgb(120, 200, 120)
                    } else {
                        Color32::from_rgb(230, 110, 110)
                    };
                    ui.colored_label(color, msg);
                }
            });
        });
    main.new_job.open = open;
}

fn create_job(main: &mut MainState) {
    let Some(pi) = main.selected_project else { return };
    let display = main.new_job.name.trim().to_string();
    if display.is_empty() {
        main.new_job.message = Some((false, "请输入流程名".to_string()));
        return;
    }
    let project_name = main.projects[pi].project.name.clone();
    let dir = main.projects[pi].dir.clone();
    let duplicate = main.projects[pi]
        .jobs
        .iter()
        .any(|j| j.job.name == display);
    if duplicate {
        main.new_job.message =
            Some((false, format!("流程「{display}」已存在（每个项目内流程名唯一）")));
        return;
    }
    let path = crate::config::job_file_path(&dir, &display);
    if path.exists() {
        main.new_job.message = Some((false, format!(
            "文件已存在: {}（该文件不属于当前任何流程，多半是旧版改名留下的孤儿文件——\
             确认无用后手动删除，或把文件里 name 字段改成要的流程名即可复用）",
            path.display()
        )));
        return;
    }

    // 步骤来源：复制当前流程 / 起始模板
    let steps = if main.new_job.clone_from {
        main.selected_job
            .and_then(|ji| main.projects[pi].jobs.get(ji))
            .map(|j| j.job.steps.clone())
            .unwrap_or_else(crate::config::starter_steps)
    } else {
        crate::config::starter_steps()
    };

    let job = Job {
        name: display.clone(),
        project: project_name.clone(),
        description: None,
        target: None,
        environment: None,
        timeout: Some(60),
        confirm: None,
        preconditions: Vec::new(),
        handlers: Vec::new(),
        retry: None,
        on_failure: FailureAction::Stop,
        steps,
        working_dir: None,
        group: main.new_job.preset_group.clone(),
    };
    if let Err(e) = crate::storage::files::save_yaml(&path, &job) {
        main.new_job.message = Some((false, format!("写入失败: {e}")));
        return;
    }

    // 同步 project.yaml 的 jobs 列表并保存
    main.projects[pi].project.jobs.push(display.clone());
    let entry = &main.projects[pi];
    if let Err(e) =
        crate::storage::files::save_yaml(&entry.dir.join("project.yaml"), &entry.project)
    {
        main.status_msg = Some(format!("project.yaml 同步失败: {e}"));
    }

    main.new_job.message = None;
    main.new_job.name.clear();
    main.new_job.preset_group = None;
    main.new_job.open = false;
    main.reload();
    main.select_project_by_name(&project_name);
    if let Some(pi2) = main.selected_project {
        if let Some(ji) = main.projects[pi2]
            .jobs
            .iter()
            .position(|j| j.job.name == display)
        {
            main.selected_job = Some(ji);
        }
    }
    main.status_msg = Some(format!("流程「{display}」已创建 ✅"));
}

// ---------------------------------------------------------------------------
// 速查窗口共用排版（变量语法速查 / 命令速查）
// ---------------------------------------------------------------------------

/// 折叠章头：默认展开（与旧版平铺等价），点标题收起——章节多（变量 9 节 / 命令 17 章）
/// 时滚动定位快，长窗口不再一眼望不到底
fn help_section(ui: &mut egui::Ui, title: &str, body: impl FnOnce(&mut egui::Ui)) {
    egui::CollapsingHeader::new(title)
        .default_open(true)
        .show(ui, body);
}

/// 速查里的等宽行渲染：行内「空白 # …」注释拆到下一行灰字缩进展示。
/// 此前注释与命令靠手工空格在同一行对齐，中文与等宽字体混排宽度对不齐，
/// 窗口窄时折行更是全乱——结构性分行后排版不再依赖对齐，折行也不破坏结构。
/// 整行注释（行首 #，示例里的步骤编号行）保持等宽灰字原样。
fn help_mono(ui: &mut egui::Ui, line: &str) {
    if line.trim_start().starts_with('#') {
        ui.monospace(egui::RichText::new(line).weak());
        return;
    }
    match split_inline_comment(line) {
        Some((cmd, comment)) => {
            ui.monospace(cmd.trim_end());
            ui.indent("help_inline_cmt", |ui| {
                ui.weak(egui::RichText::new(comment).small());
            });
        }
        None => {
            ui.monospace(line);
        }
    }
}

/// 行内注释切分：第一个前面是空白的 `#` 起算；无则 None。
/// 速查数据（变量速查各节 / cmd_help_lines）里 # 只以注释形态出现，不存在裸 # 命令语法
fn split_inline_comment(line: &str) -> Option<(&str, &str)> {
    let bytes = line.as_bytes();
    for i in 1..bytes.len() {
        if bytes[i] == b'#' && (bytes[i - 1] == b' ' || bytes[i - 1] == b'\t') {
            return Some((&line[..i], &line[i..]));
        }
    }
    None
}

/// 帮助窗口当前 tab（❓ 帮助 → 一窗两 tab：命令速查 / 变量语法）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HelpTab {
    /// 命令骨架 / 参数速查（与 docs/命令速查.md 同源）
    #[default]
    Cmd,
    /// 变量占位符 / 条件 / outputs 语法（与 docs/变量速查.md 同源）
    Var,
}

/// 「❓ 帮助」窗口（主工具栏；原「变量语法」「命令速查」两窗合一）：
/// tab 行切换两页，状态记在 MainState.help_tab——开关窗口后还停在上次看的页
pub fn show_help_window(ctx: &egui::Context, main: &mut MainState) {
    let mut open = main.show_help;
    if open {
        crate::ui::modal_backdrop(ctx, "mask_help");
    }
    crate::ui::centered_window(egui::Window::new("？ 帮助"), ctx)
        .collapsible(false)
        .open(&mut open)
        .order(egui::Order::Foreground)
        .default_width(840.0)
        .default_height(560.0)
        // 两个 tab 内容都超过一屏（变量语法的 wait / 长任务节、命令速查的 17 章），
        // 内容超窗高时滚动可达
        .vscroll(true)
        .show(ctx, |ui| {
            let tab_idx = match main.help_tab {
                HelpTab::Cmd => 0,
                HelpTab::Var => 1,
            };
            let tabs = vec!["命令速查".to_string(), "变量语法".to_string()];
            if let Some(i) = crate::ui::settings::tab_row(ui, &tabs, tab_idx, &[true, true]) {
                main.help_tab = match i {
                    1 => HelpTab::Var,
                    _ => HelpTab::Cmd,
                };
            }
            ui.add_space(6.0);
            match main.help_tab {
                HelpTab::Cmd => help_tab_cmd(ui),
                HelpTab::Var => help_tab_var(ui),
            }
        });
    main.show_help = open;
}

/// 变量语法 tab 内容（速查表，与 docs/变量速查.md 内容一致）
fn help_tab_var(ui: &mut egui::Ui) {
    ui.weak("可用于步骤命令、步骤 env、工作目录、条件字段、wait 探测目标。执行前一次性替换；未解析的占位符原样保留。");
    ui.add_space(6.0);

    help_section(ui, "一、取值形态", |ui| {
        // 不用 Grid：窗口按内容自适应宽度时，Grid 末列宽度 = 剩余空间（受 max_col_width
        // 封顶），与窗口宽度互相反馈会塌缩成几个字符宽。改为「写法一行 + 来源/示例缩进
        // 两行」的纵向堆叠，宽度只受窗口约束，永不互相挤压
        let rows: Vec<(String, &str, &str)> = vec![
            (
                "${{ variables.X }}".into(),
                "project.yaml 的 variables.<组名>（「变量组」下拉选中的表；YAML 字段名仍为 environment）",
                "选 prod 组时 ${{ variables.DEPLOY_DIR }} → /opt/yudao/prod",
            ),
            (
                "${{ 取值表达式 || 默认值 }}".into(),
                "左侧未定义时取默认值——|| 适用于所有取值形态（variables / env / targets.* / now* / run.id）；\
                 默认值可带引号；左侧已定义但为空串仍算已定义（用空串，不取默认）",
                "${{ variables.PORT || 8080 }} / ${{ run.id || manual-run }}",
            ),
            (
                "${{ env.X }}".into(),
                "前序步骤产出（往 workspace/.env 写 KEY=VALUE，步骤成功后自动捕获；与 outputs 捕获同命名空间）",
                "前一步 echo VERSION=1.2.0>.env，本步 docker build -t app:${{ env.VERSION }}",
            ),
            (
                "${{ targets.名.字段 }}".into(),
                "本项目目标（project.yaml targets 表）的字段：host / port / user / kubeconfig /\ncontext / namespace / registry / work_root / name",
                "${{ targets.prod.host }} → 192.168.40.198",
            ),
            (
                "${{ now }}".into(),
                "运行启动时刻（引擎侧生成，一次运行所有步骤同值，备份后缀场景）",
                "cp app.jar app.jar.${{ now }} → app.jar.20261023153045",
            ),
            (
                "${{ now.date }} / ${{ now.time }} / ${{ now.iso }}".into(),
                "同上的日期 / 时间 / 可读格式",
                "20261023 · 153045 · 2026-10-23 15:30:45",
            ),
            (
                "${{ run.id }}".into(),
                "本次运行唯一标识（与控制台日志文件同名）",
                "echo ${{ run.id }} >> /opt/release.log",
            ),
            (
                "${VAR}".into(),
                "步骤产出 env → 环境变量表 → 系统环境变量（依次查找）",
                "${JAVA_HOME}",
            ),
        ];
        for (syn, src, ex) in rows {
            ui.add(egui::Label::new(egui::RichText::new(&syn).monospace()));
            ui.indent("var_help_indent", |ui| {
                ui.add(egui::Label::new(egui::RichText::new(src).weak()).wrap());
                ui.add(egui::Label::new(egui::RichText::new(ex).monospace()).wrap());
            });
            ui.add_space(4.0);
        }
        ui.weak(
            "⏱ 时区：now 系列 = 发版机本地时间；要以服务器时间为准，远端（SSH）步骤里直接写 \
             $(date +%Y%m%d%H%M%S)——由远端 shell 求值。跨步骤一致性用 ${{ now }}，服务器侧实时值用 $(date …)。",
        );

    });

    help_section(ui, "二、条件与状态守卫（步骤的「＋条件」/「＋creates」字段；YAML 里 when: 与 condition: 等价）", |ui| {
        help_mono(ui, "${{ env.VERSION != '' }}        # 非空才执行本步；为空/未产出/拼错 → 跳过");
        help_mono(ui, "${{ env.VERSION == '' }}         # 为空才执行（首次构建场景：还没版本号时）");
        help_mono(ui, "${{ variables.ENV }} == prod    # 与字面量等值比较");
        help_mono(ui, "${{ variables.ENV }} == 'prod'  # 字面量可带引号（单/双引号成对去除）");
        help_mono(ui, "${{ variables.ENV }} != prod    # 不等比较；左侧未定义按空串参与");
        help_mono(ui, "variables.A == variables.B     # 两侧都可为取值表达式（支持 || 默认值）");
        help_mono(ui, "${{ targets.prod.port }} == 2222 # 数字按文本精确匹配（== 只做字符串比较）");
        help_mono(ui, "creates: node_modules         # 产物路径已存在 → 跳过本步（重跑秒过）");
        ui.weak("条件两侧支持 env. / variables. / targets.名.字段 / now / run.id；解析失败按真值执行并警告（fail-open）。");
        ui.weak("⚠ 仅支持 == / !=，不支持 > < >= <= 数值比较——需要时在命令里用 shell 判断（如 [ \"$N\" -gt 3 ] && exit 1）。");
        ui.weak("比较右侧的取值表达式未定义时，会按字面量文本参与比较（通常判不等）——两侧都可能未定义时，改用 != '' 判空。");
        ui.weak("creates 路径：本地步骤相对项目目录（支持 * ? 通配）；SSH 步骤为远端路径（相对 = work_root），远端用 test -e 判定。");

    });

    help_section(ui, "三、关键行为", |ui| {
        ui.weak("• 执行前一次性替换：不是运行时动态读。命令可预览、可日志、可审计。");
        ui.weak("• 未解析的占位符原样保留：${{ variables.TYPO }} 拼错会原样出现在命令里，一眼可见（预览会 ⚠）。");
        ui.weak("• 步骤间传递：前一步往 workspace/.env 追加 KEY=VALUE，或声明 outputs 正则捕获 stdout；后续步骤 ${{ env.KEY }} 引用。");
        ui.weak("• 注意：本地命令已由引擎包 cmd /C 执行，直接写 echo VERSION=… 即可；手写 cmd /C 嵌套会引入引号残留。");

    });

    help_section(ui, "四、env 注入优先级（后者被前者覆盖）", |ui| {
        help_mono(ui, "步骤 env > 工具 env > target env > 系统环境变量");
        ui.weak("JAVA_HOME 这类注入只影响该步骤的子进程，不改系统设置；env.X 特指前序步骤产出，全局配置请走 variables.X。");

    });

    help_section(ui, "五、流程级开关（build.yaml）", |ui| {
        help_mono(ui, "confirm: prod            # 确认门：运行前弹窗输入 prod 才放行");
        help_mono(ui, "preconditions:           # 前置检查：发版机本地断言，失败零秒退出（只放检查，别放拉代码/构建等动作）");
        help_mono(ui, "  - check: java -version");
        help_mono(ui, "    message: JDK 未安装");
        help_mono(ui, "timeout: 600             # 流程级总超时（秒；0/不填 = 不限时）；步骤级 timeout: 0 同为不限时");
        help_mono(ui, "working_dir: sub/dir     # 项目级默认工作目录（相对项目目录）");
        help_mono(ui, "tools_require:           # 工具版本要求（project.yaml，mise 风格）");
        help_mono(ui, "  java: \">=21\"           # >= > <= < 数字段比较；\"22\" 前缀匹配 22.x；\"22.1.3\" 精确");
        ui.weak("运行期自动自检：引擎启动时对本次用到的工具逐个跑 version_check 现算版本，\n未配 version_check 的工具降级为检查 command 是否存在；失败零步失败 + 人话报错。\n同项目运行互斥（GHA concurrency）：新运行自动取消旧运行，无需配置。");

    });

    help_section(ui, "六、运行时变量与变更触发（outputs / notify / handlers / wait）", |ui| {
        help_mono(ui, "outputs:                 # 步骤 stdout 按正则捕获 → ${{ env.键 }}");
        help_mono(ui, "  VER: 'VERSION=(\\S+)'   # 有捕获组取组 1，无则取整行匹配；未匹配留空并告警");
        help_mono(ui, "notify: [重启服务]       # 本步骤真的执行/真传了新内容 → 登记通知");
        help_mono(ui, "handlers:                # 流程成功收尾后按定义顺序执行被通知到的");
        help_mono(ui, "  - name: 重启服务");
        help_mono(ui, "    command: systemctl restart app");
        ui.weak("creates / when 跳过、上传 hash 未变更 → 不算变更，handler 不执行（Ansible handlers 语义）。");
        help_mono(ui, "secret_env:               # 凭据注入：环境变量名 → 凭据管理器键");
        help_mono(ui, "  MYSQL_PWD: db/prod-mysql # 运行时取密码注入子进程 env；先 set-password 存密码");
        ui.weak("数据库备份（mysqldump / pg_dump）配合 tool 绑定使用——密码不进命令行、不落 YAML。");
        help_mono(ui, "wait: { mode: tcp, target: 1.2.3.4:8080 }");
        ui.weak("wait 探测三模式：命令 / tcp 可连 / http 2xx（部署后验证可达，从发版机直连）；interval_seconds 缺省 5。");
        help_mono(ui, "wait: { mode: command, target: 探测命令, fail_exit_code: 2 }");
        ui.weak(
            "fail_exit_code：探测命令返回该退出码 = 任务已死，立即失败不再傻等\n\
             （长任务配方：远端 nohup 起任务写 status 文件，探测读它，failed 以约定码退出）",
        );

    });

    help_section(ui, "七、实战示例", |ui| {
        ui.strong("① 微服务发布最小骨架");
        help_mono(ui, "# 1. 本地构建（creates 守卫：产物在就跳过）");
        help_mono(ui, "mvn -B clean package -DskipTests     creates: target/*.jar");
        help_mono(ui, "# 2. 上传（多 jar 多行映射；内容未变更自动跳过）");
        help_mono(ui, "target/user.jar -> release/user.jar");
        help_mono(ui, "target/order.jar -> release/order.jar    notify: [重启服务]");
        help_mono(ui, "# 3. 远端重启（handler：只有真的传了新包才执行）");
        help_mono(ui, "systemctl restart app && ${{ now.iso }} >> /opt/release.log");
        help_mono(ui, "# 4. 等健康（wait tcp 从发版机探测，2 分钟总限）");
        help_mono(ui, "wait: { mode: tcp, target: ${{ targets.prod.host }}:8080 }   timeout: 120");

        ui.add_space(6.0);
        ui.strong("② 条件实战（when / condition）");
        help_mono(ui, "command: systemctl restart app    condition: ${{ variables.ENV }} == prod");
        help_mono(ui, "#  ↑ 生产才重启；测试环境跑同一流程时本步自动跳过");
        help_mono(ui, "command: curl -s .../debug/stats  condition: ${{ variables.ENV }} != prod");
        help_mono(ui, "#  ↑ 非生产才采集调试信息");
        help_mono(ui, "command: mvn deploy               condition: ${{ env.VERSION != '' }}");
        help_mono(ui, "#  ↑ 上一步没产出版本号（构建失败/被跳过）→ 部署步也跳过");

        ui.add_space(6.0);
        ui.strong("③ 默认值实战（|| 适用于所有取值形态）");
        help_mono(ui, "docker push ${{ variables.REGISTRY || registry.local:5000 }}/app:${{ env.TAG }}");
        help_mono(ui, "#  ↑ 变量组没配 REGISTRY 时兜底本地仓库");
        help_mono(ui, "echo 手动触发（run_id=${{ run.id || manual }}）>> /opt/audit.log");
        help_mono(ui, "#  ↑ run.id 缺省（占位）时兜底固定标记");

        ui.add_space(6.0);
        ui.strong("④ outputs 捕获 → 后续步骤引用（运行时变量）");
        help_mono(ui, "# 步骤 A：捕获版本号（正则取 stdout，有捕获组取组 1）");
        help_mono(ui, "command: mvn help:evaluate -Dexpression=project.version -q -DforceStdout");
        help_mono(ui, "outputs: { VER: '(\\S+)$' }");
        help_mono(ui, "# 步骤 B：直接引用（构建镜像并推送）");
        help_mono(ui, "command: docker build -t registry/app:${{ env.VER }} .");
        help_mono(ui, "command: docker push registry/app:${{ env.VER }}");

        ui.add_space(6.0);
        ui.strong("⑤ targets 字段拼装（连接信息不写死在命令里）");
        help_mono(ui, "echo ssh -p ${{ targets.prod.port }} ${{ targets.prod.user }}@${{ targets.prod.host }}");
        help_mono(ui, "rsync -av release/ ${{ targets.prod.user }}@${{ targets.prod.host }}:${{ targets.prod.work_root }}/");

        ui.add_space(6.0);
        ui.strong("⑥ ${VAR} 系统变量兜底 + run.id 台账");
        help_mono(ui, "echo JAVA_HOME=${JAVA_HOME}     # 步骤产出 env → 变量表 → 系统环境变量");
        help_mono(ui, "echo ${{ now.iso }} run=${{ run.id }} >> ${{ targets.prod.work_root }}/release.log");

    });

    help_section(ui, "八、pack 打包格式与排除（映射行的目标扩展名决定格式）", |ui| {
        help_mono(ui, "target/ -> dist/app.tar.gz --exclude node_modules --exclude *.map");
        ui.weak(".zip → zip；.tar.gz / .tgz → tar+gzip（tar 保留 +x 权限，Windows 打 zip 到 Linux 解压会丢）；\n\
                 排除 = rsync --exclude 语义：字面量按目录/文件名分量整棵排除，* ? 通配相对路径与文件名。");
        help_mono(ui, "dist -> app.tar.gz        # 目录源带目录名根（里面是 dist/...，同 tar -czf / zip -r 习惯）");
        help_mono(ui, "dist/. -> app.tar.gz      # 源以 /. 结尾 = tar -C dist . 语义：条目不带目录根，解包直接铺文件");
        ui.weak("单文件 / 通配源条目为裸文件名。");

    });

    help_section(ui, "九、常用命令速查（构建 / 部署高频命令；逐参数说明见 docs/命令速查.md）", |ui| {
        help_mono(ui, "mvn -B clean package -DskipTests -T 1C   # -B 非交互 -q 静默 -P prod 选 profile -U 刷 SNAPSHOT");
        help_mono(ui, "mvn -pl 模块 -am clean package           # 只构建指定模块及依赖；-o 离线；timeout 建议 1800");
        help_mono(ui, "mvn help:evaluate -Dexpression=project.version -q -DforceStdout   # 配 outputs 捕获版本号");
        help_mono(ui, "gradlew.bat bootJar --no-daemon -x test -Penv=prod   # Windows 本地必须 .bat；远端 Linux 写 ./gradlew");
        help_mono(ui, "pnpm install --frozen-lockfile && pnpm run build     # CI 标准装依赖（不一致即失败）；creates: node_modules");
        help_mono(ui, "docker build -t reg/app:${{ env.VER }} . && docker push reg/app:${{ env.VER }}");
        help_mono(ui, "docker save -o app.tar reg/app:tag       # 离线包走 upload → 目标机 docker load -i（无 registry 链路）");
        help_mono(ui, "kubectl apply -f deploy.yaml -n ${{ variables.NAMESPACE }}   # k8s 目标自动注入 KUBECONFIG，无需手动指定");
        help_mono(ui, "kubectl rollout status deployment/app --timeout=120s   # restart / undo 回滚 / logs -f --tail=100");
        help_mono(ui, "mysqldump -h 主机 -P 3306 -u root --single-transaction 库名 > backup.sql   # 大写 -P；不锁表；密码走 secret_env");
        help_mono(ui, "mysql -h 主机 -P 3306 -u root -D 库 -e \"SQL\"   # -e 单条 SQL；导入恢复：mysql 库 < dump.sql");
        help_mono(ui, "pg_dump -h 主机 -p 5432 -U postgres -f backup.sql 库名   # 小写 -p（跟 mysql 相反）；密码 PGPASSWORD");
        help_mono(ui, "systemctl restart app && systemctl is-active --quiet app   # is-active 退出码 = 判活，可接前置检查");
        help_mono(ui, "journalctl -u app -n 100 --since \"10 min ago\"   # -f 跟踪日志 -p err 只看错误");
        help_mono(ui, "tar -xzf app.tar.gz -C /opt/app          # 解包铺目录；-czf 建 --exclude 排除 -tzf 只看不解");
        help_mono(ui, "find /opt/backup -name \"app-*.jar\" -mtime +7 -delete   # 清 7 天前备份；du -sh /opt/* 看占用");
        help_mono(ui, "java -Xms512m -Xmx2g -jar app.jar --spring.profiles.active=prod   # JVM 参数在 -jar 前，-- 程序参数在后");
        help_mono(ui, "netstat -ano | findstr :8080 & taskkill /PID 1234 /F   # Windows 本地查端口占用并强杀（sc query 查服务）");
        help_mono(ui, "robocopy dist release /MIR /XD node_modules   # Windows 镜像同步；退出码 0-7 都算成功，需接 if %ERRORLEVEL% GEQ 8 判失败");
        help_mono(ui, "nginx -t && nginx -s reload              # 测配置语法后热加载；502 先看 /var/log/nginx/error.log");
        help_mono(ui, "sha256sum app.tar.gz                     # 传后校验；Windows 本地 certutil -hashfile 文件 SHA256");
        ui.weak("【本地】步骤 = 发版机 Windows（cmd 语法：&& | > 可用，$VAR 不可用）；【远端】SSH 步骤 = Linux 全语法。\n\
                 Shell 片段（判断 / $( ) / xargs / wait 探测模板）、Windows cmd 全套（robocopy / mklink / certutil…）、\n\
                 Java 运行时与排障（jps / jstack / keytool）、Nginx、apt / yum、7z、redis-cli 等\n\
                 完整逐参数说明：本窗口「命令速查」tab（docs/命令速查.md）。");
    });
}

/// 「？ 命令速查」窗口内容行（与 docs/命令速查.md 同源）：
/// H = 小节标题、M = 命令/示例/参数表行（monospace）、W = 说明 / 注意点（weak，自动换行）
enum CmdHelpLine {
    H(&'static str),
    M(&'static str),
    W(&'static str),
}

fn cmd_help_lines() -> &'static [CmdHelpLine] {
    use CmdHelpLine::{H, M, W};
    &[
        W("【本地】= 发版机 Windows（cmd /C 执行）：&& | > >> 可用；变量写 %VAR%（cmd）或 ${{ }}（RBS 替换）；单引号、$VAR、$() 不可用"),
        W("【远端】= SSH 步骤发到远端 Linux：bash 语法全量可用（$VAR、$()、单引号等）。cmd 会自动补扩展名（mvn → mvn.cmd），但无扩展名的 sh 脚本不行——Windows 本地跑 Gradle 写 gradlew.bat，远端写 ./gradlew。"),
        W("命令找不到 / 版本不对 → tool: 绑定（tools.yaml 登记路径）+ tools_require 版本门禁；装包类命令放一次性初始化或 preconditions，别放发版主流程。"),

        H("一、Linux 常用命令【远端】"),
        M("ls -lah /opt/app                              # 长格式 + 隐藏文件 + 人读大小"),
        M("cp -a app.jar backup/app-${{ now }}.jar       # 归档复制（保留权限/时间戳，递归）"),
        M("mkdir -p /opt/app/backup                      # 多级创建，已存在不报错"),
        M("ln -sfn /opt/releases/app-1.2.3 /opt/app      # 软链指向新版本（原子切换惯用）"),
        M("find /opt/backup -name \"app-*.jar\" -mtime +7 -delete   # 删 7 天前的备份"),
        M("chmod +x deploy.sh && chown -R app:app /opt/app"),
        W("cp -a 保留权限/属主/时间戳（备份首选）；ln -sfn 原子切换软链；find 支持 -name / -mtime +N / -size +N M / -delete / -exec；rm -rf 属危险命令：预览与执行前会 ⚠，路径写全别用变量拼根。"),
        M("tail -n 100 app.log / tail -f app.log         # 末 100 行 / 跟踪新增"),
        M("grep -in \"timeout\" app.log                    # 忽略大小写 + 行号"),
        M("ps -ef | grep java | grep -v grep             # 查进程（-v 剔除 grep 自身）"),
        M("sed -i 's/8080/8081/g' config.yml             # 就地替换（g = 全局；不加 -i 只打印）"),
        M("awk -F: '{print $1}' /etc/passwd              # 按冒号切列取第 1 列（$NF = 末列）"),
        M("du -sh /opt/* | sort -rh | head -5            # 各目录占用 Top5（df -h 看磁盘余量）"),
        M("pgrep -f app.jar / pkill -f app.jar           # 按命令行片段查 / 杀 PID"),
        M("nohup /opt/app/start.sh > app.out 2>&1 &      # 后台起飞，SSH 断开不连带（长任务配方）"),
        M("ss -tlnp | grep 8080 / lsof -i:8080           # 谁在监听 / 谁占着端口"),
        M("curl -sf http://127.0.0.1:8080/health         # 健康检查（-f：非 2xx 返回非零）"),
        M("curl -s -X POST -H \"Content-Type: application/json\" -d '{\"k\":\"v\"}' http://host/api"),
        M("ssh -p 2222 user@host / scp -P 2222 文件 user@host:/dst   # scp 端口是大写 -P"),
        W("grep：-i 忽略大小写 -n 行号 -v 反选 -c 计数 -r 递归 -E 扩展正则 -A2/-B2/-C2 上下文。curl：-s 静默 -o 写文件 -m 10 总超时；wait 的 http 模式不支持 https，探测用命令模式 curl -skf。RBS 的 upload / download 已覆盖主链路，ssh/scp 用于远端侧中转或手工调试。"),
        M("rsync -avz dist/ user@host:/opt/app/          # 增量同步（-avn 先干跑看会传什么）"),
        M("rsync -avz --delete dist/ /opt/app/           # 镜像同步：目标多出的删掉（想清楚再 --delete）"),
        W("rsync：-a 递归保权限 -z 压缩 --exclude 排除；尾随斜杠有语义：dist/ 传内容不带 dist 根，dist 会多套一层——与 pack 的「源/.」语义对应。"),
        M("firewall-cmd --permanent --add-port=8080/tcp && firewall-cmd --reload   # firewalld 开端口（--list-ports 查）"),
        M("ufw allow 8080/tcp && ufw status              # Ubuntu 系"),
        M("ssh-keygen -t ed25519 / ssh-copy-id user@host   # 公钥免密（RBS 目标 key 认证的前置）"),
        W("防火墙：--permanent 不加 reload 后丢失，加了还要 reload 才生效；本机通了仍不通 → 云安全组（排查顺序：ss 监听 → 本机防火墙 → 安全组）。免密：chmod 700 ~/.ssh && chmod 600 authorized_keys（权限不对 sshd 拒用）；russh 私钥须 OpenSSH 格式（PPK 先 puttygen 转）。"),
        M("crontab -e                                    # 定时任务：分 时 日 月 周"),
        M("(crontab -l; echo \"0 2 * * * /opt/app/backup.sh >> /var/log/backup.log 2>&1\") | crontab -"),
        W("| crontab - 是整体覆盖——追加要 (crontab -l; echo 新任务) | crontab -，直接 echo 会清掉原有任务。"),
        M("which mvn java / env | grep -i mysql          # 命令实际路径 / 验证 secret_env 注入结果"),
        W("SSH 步骤每一步都是新 shell：上一步 export 的变量下一步不存在——跨步骤传值用 outputs / .env 约定，不要指望 export。"),

        H("二、Shell 片段与管道【远端】"),
        M("[ -f /opt/app/app.jar ] && echo 在 || echo 缺   # 判断（-d 目录 -e 存在 -z 空串 -n 非空）"),
        M("VER=${TAG:-v0.0.0}                            # 变量默认值展开（未定义/空都取默认）"),
        M("NOW=$(date +%Y%m%d%H%M%S)                     # 命令替换：拿命令输出当值用"),
        M("if [ $? -eq 0 ]; then echo 上条成功; fi        # 上一条命令的退出码"),
        M("ls -t /opt/backup/app-*.jar | tail -n +6 | xargs -r rm   # 只留最近 5 份（示范工程④同款）"),
        M("echo \"$(date '+%F %T') 发版\" | tee -a /opt/release.log   # 打屏同时落文件"),
        M("for f in /opt/backup/app-*.jar; do sha256sum \"$f\"; done"),
        W("判断符别混：字符串 = !=、空/非空 -z -n、文件 -f -d -e、数字 -eq -ne -gt -lt（数字比较不是 > <——那是重定向/文本比较）。xargs -r：输入为空就不执行；$? 与 && || 组合是脚本判断成败的全部。"),
        W("wait command 探测模板（fail_exit_code: 2）：读状态文件 → success exit 0 / failed exit 2 / 还在跑 exit 1（配方见 docs/长任务配方.md）。"),

        H("三、systemctl 与日志【远端】"),
        M("systemctl restart app                         # start / stop / status 同型"),
        M("systemctl enable --now app                    # 开机自启 + 立即启动"),
        M("systemctl is-active --quiet app && echo 存活   # 脚本判活（--quiet 只看退出码）"),
        M("systemctl daemon-reload                       # 改过 *.service 文件后先执行"),
        M("journalctl -u app -f --since \"10 min ago\"     # 跟日志（-n 100 最近 N 行 -p err 只看错误）"),
        W("重启步骤常配 retry + wait 探活；首次部署服务不存在时 systemctl stop 加 continue_on_error: true 只黄灯不拦（示范工程 ④）。"),

        H("四、包管理【远端】"),
        M("apt update && apt install -y curl jq          # Debian/Ubuntu（-y 免交互必须带）"),
        M("apt list --installed 2>/dev/null | grep mysql"),
        M("yum install -y curl jq                        # CentOS/RHEL（新系统首选 dnf，同型）"),
        M("rpm -q mysql-server                           # 查单个包版本"),
        W("SSH 非交互终端没有确认机会，-y 必带否则卡死到超时；放流程里配 creates:（如 /usr/bin/jq）守卫，装过秒过。"),

        H("五、压缩与解包"),
        M("tar -czf app.tar.gz -C dist .                 # 打包（-C 进目录再打，条目不带 dist/ 前缀）"),
        M("tar -xzf app.tar.gz -C /opt/app               # 解包到指定目录（目录先 mkdir -p）"),
        M("tar -tzf app.tar.gz | head                    # 只看包里有什么，不解"),
        M("unzip -o app.zip -d /opt/app                  # -o 覆盖不问（非交互必须）"),
        M("7z a -t7z backup/db.7z backup/*.sql           # 压缩（tool: 7z 绑定；-mx=9 最高压缩率 -p 加密）"),
        M("7z x backup/db.7z -o/opt/restore              # 解压（-o 后紧跟目录不加空格）"),
        W("权限注意：Windows 侧打 zip 传 Linux 解压会丢可执行位——传带 .sh / 二进制的包用 tar.gz（RBS pack 的 tar.gz 保留 +x），或解压后补 chmod +x。"),

        H("六、Windows 本地常用命令【本地 cmd】"),
        M("dir /B target\\*.jar                           # 列文件名（/B 裸名，配 outputs 捕获产物名）"),
        M("copy /Y target\\app.jar backup\\                # /Y 覆盖不问"),
        M("xcopy dist\\* release\\ /E /I /Y                # 递归复制（/E 含空目录 /I 目标是目录）"),
        M("robocopy dist release /MIR /XD node_modules   # 镜像同步（/XD 排除目录 /XF 排除文件）"),
        M("mklink /J app D:\\releases\\app-1.2.3           # 目录 junction（免管理员；/D 真软链要管理员）"),
        M("tasklist | findstr java                       # 查进程（findstr ≈ grep）"),
        M("taskkill /PID 1234 /F                         # 强杀（/IM java.exe /F 按映像名）"),
        M("netstat -ano | findstr :8080                  # 查端口占用（末列 = PID）"),
        M("sc query demo-app                             # 查 Windows 服务（stop / start 同型，需管理员）"),
        M("netsh advfirewall firewall add rule name=\"app-8080\" dir=in action=allow protocol=TCP localport=8080   # 防火墙开端口（需管理员）"),
        M("schtasks /Create /SC DAILY /ST 02:00 /TN 备份 /TR D:\\app\\backup.bat   # 计划任务（crontab 对应物；/Run /TN 手动触发）"),
        M("where mvn                                     # 命令实际路径（cmd 版 which）"),
        M("curl.exe -sf http://127.0.0.1:8080/health     # Win10 1803+ 自带真 curl（写 .exe 最稳）"),
        M("certutil -hashfile app.zip SHA256             # 算哈希（对 upload 的 sha256 校验排障）"),
        M("powershell -Command \"Get-Content app.log -Tail 100\"   # 逃生舱：复杂逻辑借 PowerShell"),
        W("⚠ robocopy 退出码 0-7 都算成功（1 = 复制了文件），直接用会被判失败——收尾接 & if %ERRORLEVEL% GEQ 8 (exit /B 1) else (exit /B 0)。"),
        W("cmd 变量是 %VAR%（$VAR 不可用）；mklink /J 不需要管理员且跨卷，是 Linux 软链切换的 Windows 对应物；复杂逻辑别硬写一行串，借 powershell -Command 或放 脚本.bat 调用。"),

        H("七、git【本地 workspace】"),
        M("git clone --depth 1 https://gitee.com/x/demo.git demo   # 浅克隆（只要最新一层）"),
        M("git pull --ff-only                            # 只快进合并（有分叉直接失败，不悄悄合）"),
        M("git status -s / git log --oneline -5          # 短状态 / 最近 5 条"),
        M("git tag -a v1.2.0 -m \"发版\""),
        W("克隆步配 creates: demo/.git 守卫（克隆过秒过）；工作目录用步骤 working_dir: demo（示范工程 ①）。"),

        H("八、Maven【本地】"),
        M("mvn -B clean package -DskipTests"),
        M("mvn -T 1C clean package -P prod -DskipTests   # 并行 + 指定 profile"),
        M("mvn -pl yudao-server -am clean package -DskipTests   # 只构建指定模块及其依赖"),
        M("mvn help:evaluate -Dexpression=project.version -q -DforceStdout   # 配 outputs 捕版本号"),
        M("mvn dependency:tree -Dincludes=com.mysql      # 查依赖来源"),
        M("mvn clean deploy -DskipTests                  # 发私服"),
        M("  -B              批处理模式：关进度条彩色输出，日志干净（CI 标配）"),
        M("  -q              只输出 ERROR；配 outputs 捕获 stdout 时用"),
        M("  -T 1C           每核 1 线程并行构建模块"),
        M("  -P 名           激活 pom 里的 profile（dev / prod）"),
        M("  -D键=值         传系统属性：-DskipTests 编译测试类但不跑；-Dmaven.test.skip=true 连编译都跳"),
        M("  -U              强制刷新 SNAPSHOT 依赖"),
        M("  -o              离线模式：只吃本地仓库"),
        M("  -pl 模块 -am    只构建该模块；-am 连上游依赖一起建"),
        M("  -s 路径         指定 settings.xml（多私服 / 镜像源隔离）"),
        M("  -Dmaven.repo.local=路径   指定本地仓库（多 JDK / 挪 C 盘）"),
        W("RBS 配合：timeout: 1800（首次拉依赖慢，默认超时会误杀）、creates: target/*.jar（产物在就跳过，配作业级 retry 只补失败步骤）、tools_require: { java: \">=21\" }。"),

        H("九、Gradle【本地 / 远端】"),
        M("gradlew.bat clean bootJar --no-daemon         # Windows 本地必须带 .bat；远端写 ./gradlew"),
        M("gradlew.bat build -x test                     # 跳过 test 任务"),
        M("gradlew.bat bootJar -Penv=prod                # 传项目属性（build.gradle 里 project.env）"),
        M("gradlew.bat --refresh-dependencies build      # 强刷依赖"),
        W("gradlew（wrapper）按工程钉死的版本跑，永远优先于全局 gradle；bootJar = Spring Boot 可执行 jar（build 含 test，发版通常 clean bootJar -x test）；-x 排除任务、-P 传构建属性（区别 -D 的 JVM 属性）、--no-daemon 不驻留后台进程（CI 类环境推荐）。"),

        H("十、前端 Node（pnpm / npm）【本地】"),
        M("corepack enable && corepack prepare pnpm@latest --activate   # 按 packageManager 字段钉版本"),
        M("pnpm install --frozen-lockfile                # CI 标准装依赖（lock 不一致直接失败不自动改）"),
        M("pnpm run build / npm run build"),
        M("npm ci                                        # npm 干净安装：严格按 lock 装、绝不改 lockfile"),
        W("装依赖步 creates: node_modules（装过跳过）；构建步 creates: dist。corepack：工程没写 packageManager 字段时才需要手动装 pnpm。"),

        H("十一、Java 运行时与排障【本地 / 远端】"),
        M("java -Xms512m -Xmx2g -jar app.jar --spring.profiles.active=prod --server.port=8080"),
        M("java -Dfile.encoding=UTF-8 -Duser.timezone=Asia/Shanghai -jar app.jar"),
        M("jps -l                                        # 列 java 进程（cmd 同款可用）"),
        M("jstack <pid> > /tmp/stack.txt                 # 线程栈快照（卡死 / 死锁排查）"),
        M("jmap -histo:live <pid> | head -20             # 对象直方图（内存涨排查）"),
        M("jar tf app.jar | head                         # 看 jar 内容不解包"),
        M("keytool -importcert -alias ca -file ca.cer -keystore trust.jks -storepass changeit -noprompt"),
        W("参数位置别错：JVM 参数（-Xmx、-Dxxx）必须在 -jar 前面；jar 名后面的 --xxx 是程序参数（Spring Boot 解析）——写反了不生效还找不到原因。-Xms/-Xmx 生产设同值；-Dfile.encoding 治日志乱码；keytool 默认库口令 changeit。"),

        H("十二、Docker【本地】"),
        M("docker build -t registry.internal:5000/app:${{ env.VER }} ."),
        M("docker build -f Dockerfile.prod --build-arg APP_ENV=prod -t app:${{ env.VER }} ."),
        M("docker push registry.internal:5000/app:${{ env.VER }}"),
        M("docker save -o app.tar registry/app:tag       # 导出离线包 → upload 传目标机 docker load -i"),
        M("docker logs -f --tail=100 容器名"),
        M("docker login registry.internal:5000 -u user   # 密码交互输入，别进命令行"),
        W("-t 名:tag、-f 指定 Dockerfile、--build-arg 传 ARG 构建参数；层缓存按顺序命中：常变的源码 COPY 放 Dockerfile 靠后，依赖安装放前面。版本号用上游 outputs 捕获（${{ env.VER }}），不要手写 latest。"),

        H("十三、Kubernetes（kubectl）【本地 · k8s 目标】"),
        M("kubectl apply -f deploy.yaml                  # k8s-apply 步骤；-n 指命名空间"),
        M("kubectl apply -f k8s/ -n ${{ variables.NAMESPACE }}   # 目录全量应用（幂等：存在即更新）"),
        M("kubectl get pods -n demo-prod -o wide         # 列表（-w 持续 watch）"),
        M("kubectl logs -f --tail=100 pod名 -c 容器名    # 多容器要 -c"),
        M("kubectl describe pod pod名                    # 排查拉不起 / 镜像失败 / 挂载错误"),
        M("kubectl rollout restart deployment/app        # 滚动重启（改了 ConfigMap 后）"),
        M("kubectl rollout status deployment/app --timeout=120s   # 等滚动完成"),
        M("kubectl rollout undo deployment/app           # 回滚上一版（history 看版本）"),
        M("kubectl set image deployment/app app=registry/app:${{ env.VER }}"),
        M("kubectl scale deployment/app --replicas=3"),
        M("kubectl exec -it pod名 -- sh / kubectl cp pod名:/path ./file"),
        W("RBS 配合：目标类型选 k8s（kubeconfig / context / namespace 存目标，步骤里 ${{ targets.名.* }} 引用）；k8s-apply 步骤在发版机跑 kubectl，KUBECONFIG 由引擎自动注入——命令里不用手动指定。"),

        H("十四、Nginx【远端】"),
        M("nginx -t && nginx -s reload                   # 测语法后热加载（不断现有连接）"),
        M("nginx -s quit / nginx -s reopen               # 优雅停 / 重开日志（切割用）"),
        M("systemctl reload nginx                        # apt/yum 装的走 systemd，别与 -s 混用"),
        M("tail -f /var/log/nginx/error.log              # 502/504 第一现场"),
        W("reload 前必 -t：配置有语法错 reload 失败且保持旧配置在跑；-s 信号要找得到 pid 文件（编译安装），包管理器装的一律 systemctl。502 排查：error.log → 后端进程活着吗（ss -tlnp）→ SELinux / 防火墙。"),

        H("十五、MySQL（mysql / mysqldump）【本地客户端 → 远程库】"),
        M("mysql -h ${{ variables.DB_HOST }} -P 3306 -u root -D app_db -e \"SELECT COUNT(*) FROM t_order;\""),
        M("mysql -h 主机 -P 3306 -u root app_db < backup/mysql-20261031.sql   # 导入恢复"),
        M("mysqldump -h ${{ variables.DB_HOST }} -P ${{ variables.DB_PORT }} -u ${{ variables.DB_USER }} \\"),
        M("  --single-transaction --routines --triggers --events \\"),
        M("  ${{ variables.DB_NAME }} > backup/mysql-${{ now.date }}.sql   # 全量备份（不锁表）"),
        M("mysqldump ... --no-data 库名 > schema.sql     # 只导表结构"),
        M("mysqldump ... --where=\"created_at >= '2026-10-01'\" 库 t_order   # 条件导出单表"),
        M("  -h / -P         主机 / 端口——MySQL 端口是大写 -P（pg 是小写 -p）"),
        M("  -u / -p         用户 / 密码；-p 单独写 = 交互；严禁 -p密码 连写（进程列表可见）→ secret_env 注入 MYSQL_PWD"),
        M("  -D 库名         默认数据库（-e 里不用写库前缀）；-t 表格化输出"),
        M("  --single-transaction   InnoDB 一致性快照不锁表，生产必备"),
        M("  --routines --triggers --events   存储过程 / 触发器 / 事件（默认不含 routines 与 events）"),
        M("  --databases db1 db2     备多库且带 CREATE DATABASE；不带则只备表"),
        M("  --no-data / --no-create-info   只结构 / 只数据"),
        M("  --set-gtid-purged=OFF   GTID 库备份导到非 GTID 库报错时加"),
        M("  --max-allowed-packet=512M   超大行报包超限时加"),
        M("  --login-path=名  mysql_config_editor 存过的混淆凭据路径"),
        W("RBS 配合：tool: mysqldump + secret_env: { MYSQL_PWD: db/prod-mysql }（先 remote-build set-password 存密码），连接信息走变量组——示范工程 ⑤ 即完整示例。"),

        H("十六、PostgreSQL（psql / pg_dump）【本地客户端 → 远程库】"),
        M("psql -h ${{ variables.DB_HOST }} -p 5432 -U postgres -d app_db -c \"\\dt\""),
        M("pg_dump -h 主机 -p 5432 -U postgres -f backup/pg-${{ now.date }}.sql 库名"),
        M("pg_dump -Fc -f backup/pg.dump 库名            # 自定义压缩格式"),
        M("pg_restore -h 主机 -U postgres -d app_db backup/pg.dump   # -Fc 格式用它恢复（-j 4 并行）"),
        W("端口是小写 -p（MySQL 是大写 -P，两库连用最容易踩）；密码走 PGPASSWORD = pg 官方推荐 → secret_env 注入；-s 只结构 / -a 只数据、-t 表 / -n schema 范围导出。"),

        H("十七、redis-cli【本地客户端 → 远程库】"),
        M("redis-cli -h 主机 -p 6379 ping                # PONG = 通"),
        M("redis-cli -h host info memory | grep used_memory_human"),
        M("redis-cli -h host --rdb backup/dump-${{ now.date }}.rdb   # 远程拉 RDB 备份"),
        W("密码建议 REDISCLI_AUTH 环境变量（比 -a 命令行安全，同样可走 secret_env）；-n N 选 db。"),

        W("完整逐参数说明与更多命令见 docs/命令速查.md；变量替换语法见「❓ 变量语法」窗口。"),
    ]
}

/// 命令速查 tab 内容（与 docs/命令速查.md 内容一致）：内容量大（17 章），
/// 用行数组数据驱动渲染（cmd_help_lines），避免几百行重复 UI 调用
fn help_tab_cmd(ui: &mut egui::Ui) {
    ui.weak("构建 / 部署高频命令速查：每条实例 + 参数说明，与 docs/命令速查.md 同源。先看执行位置：【本地】= 发版机 Windows（cmd），【远端】= SSH 步骤发到远端 Linux。");
    ui.add_space(6.0);
    // 分章：H 行切成折叠章头（默认展开），M/W 归入前一个 H；
    // 首个 H 之前的 W 是全局导读（执行位置 / 工具绑定），留在窗外顶部
    let mut sections: Vec<(&str, Vec<&CmdHelpLine>)> = Vec::new();
    for line in cmd_help_lines() {
        match line {
            CmdHelpLine::H(t) => sections.push((t, Vec::new())),
            _ => match sections.last_mut() {
                Some((_, body)) => body.push(line),
                None => {
                    if let CmdHelpLine::W(t) = line {
                        ui.weak(*t);
                    }
                }
            },
        }
    }
    for (title, body) in &sections {
        help_section(ui, title, |ui| {
            ui.add_space(2.0);
            for &line in body {
                match line {
                    CmdHelpLine::M(t) => help_mono(ui, *t),
                    CmdHelpLine::W(t) => {
                        ui.weak(*t);
                    }
                    CmdHelpLine::H(_) => unreachable!("章头已在分章阶段消费"),
                }
            }
        });
    }
}

// ── 命令输入自动补全（egui 0.37 内置 CompletionPopup）────────────────────────
// 键盘交互弹层自带：↑↓ 选 / Enter·Tab 上屏 / Esc 关（词变后再现）/ 点击接受，
// 焦点全程留在输入框；弹层是独立 Area 层，不影响卡片布局。

/// 变量占位符模式：光标落在未闭合的 `${{ …`（Double）或 `${…`（Single）内
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VarMode {
    Double,
    Single,
}

/// 光标前文本是否处于未闭合的取值占位符内（变量解析 §6）：找最后一个 `${`，
/// 双括号查 `}}` 闭合、单括号查 `}` 闭合，未闭合即在补全变量。
/// 已闭合的占位符不影响判定（`${{ a }} tail` → None，回到命令模式）。
fn unclosed_var(before: &str) -> Option<VarMode> {
    let after = &before[before.rfind("${")? + 2..];
    if after.starts_with('{') {
        (!after[1..].contains("}}")).then_some(VarMode::Double)
    } else {
        (!after.contains('}')).then_some(VarMode::Single)
    }
}

/// egui 光标偏移是字符序号（CharIndex），字符串切片要字节偏移——换算
/// （含中文等多字节字符：取第 n 个字符的起始字节位）
fn char_idx_to_byte(text: &str, char_idx: usize) -> usize {
    text.char_indices()
        .nth(char_idx)
        .map_or(text.len(), |(b, _)| b)
}

/// 命令骨架表：(上屏文本, 中文说明)。骨架带常用参数、路径占位留给用户补；
/// 匹配 = 骨架首词对输入词不区分大小写前缀。提炼自 docs/命令速查.md
/// （❓ 命令速查窗口同源），逐参数完整说明在那边。
const CMD_SUGGESTIONS: &[(&str, &str)] = &[
    // Linux 远端：文件与目录
    ("ls -lah ", "列目录：长格式 + 隐藏文件 + 人读大小"),
    ("cp -a ", "归档复制：保权限/属主/时间戳（备份首选）"),
    ("mkdir -p ", "多级创建目录，已存在不报错"),
    ("ln -sfn ", "软链原子切换：ln -sfn 新目标 链接名"),
    ("find ", "找文件：find 路径 -name 通配 -mtime +N -delete（空格后出参数提示）"),
    ("chmod +x ", "加可执行权限"),
    ("chown -R app:app ", "递归改属主（app:组 路径）"),
    ("rm -rf ", "⚠ 危险删除：路径写全，别用变量拼根目录"),
    // Linux 远端：查看与文本
    ("tail -n 100 ", "看文件末 100 行"),
    ("tail -f ", "跟踪日志新增（Ctrl+C 退出）"),
    ("grep -in ", "搜文本：忽略大小写 + 行号"),
    ("ps -ef | grep java | grep -v grep", "查进程（-v 剔除 grep 自身）"),
    ("sed -i 's/旧/新/g' ", "就地替换（g=全局；不加 -i 只打印）"),
    ("awk -F: '{print $1}' ", "按分隔符取列（$NF = 末列）"),
    ("du -sh ", "目录占用汇总（人读；df -h 看磁盘）"),
    ("df -h ", "磁盘余量"),
    ("free -h ", "内存余量"),
    ("wc -l ", "统计行数"),
    ("which ", "命令实际路径"),
    // Linux 远端：进程
    ("pgrep -f ", "按命令行片段查 PID"),
    ("pkill -f ", "按命令行片段杀进程"),
    ("kill -9 ", "强杀指定 PID"),
    ("nohup ", "后台起飞：nohup 命令 > 输出 2>&1 &（长任务）"),
    // Linux 远端：网络与探测
    ("ss -tlnp | grep ", "谁在监听端口"),
    ("lsof -i:", "谁占着端口（含进程命令行；需 lsof 包）"),
    ("curl -sf ", "健康检查：非 2xx 返回码非零"),
    ("curl -s -X POST -H 'Content-Type: application/json' -d ", "POST 请求（引号里放 URL 与 JSON 体）"),
    ("wget -q -O ", "下载到指定文件"),
    ("ping -c 3 ", "发 3 个包测连通"),
    ("rsync -avz ", "增量同步（尾随斜杠有语义）"),
    ("rsync -avn ", "干跑预演：先看会传什么再真跑"),
    ("firewall-cmd --permanent --add-port=8080/tcp && firewall-cmd --reload", "firewalld 开端口（必须 reload 才生效）"),
    ("ufw allow ", "Ubuntu 防火墙开端口"),
    // Linux 远端：压缩
    ("tar -czf ", "打包 tar.gz：tar -czf x.tar.gz -C 目录 ."),
    ("tar -xzf ", "解包 tar.gz（-C 指定目录，先 mkdir -p）"),
    ("unzip -o ", "解压覆盖不问（非交互必须 -o）"),
    ("7z a -t7z ", "压缩（tool: 7z 绑定；-mx=9 最高压缩率）"),
    ("7z x ", "解压（-o 后紧跟目录，不加空格）"),
    // Linux 远端：systemctl / 日志
    ("systemctl restart ", "重启服务（start / stop / status 同型）"),
    ("systemctl status ", "看服务状态"),
    ("systemctl enable --now ", "开机自启 + 立即启动"),
    ("systemctl is-active --quiet ", "脚本判活（只看退出码）"),
    ("systemctl daemon-reload", "改过 *.service 文件后先执行"),
    ("journalctl -u ", "跟服务日志：-f 跟踪 / -n 100 最近行"),
    ("nginx -t && nginx -s reload", "测语法后热加载（reload 前必 -t）"),
    // Linux 远端：包管理 / 定时 / 免密
    ("apt install -y ", "Debian/Ubuntu 装包（-y 免交互必须）"),
    ("apt list --installed 2>/dev/null | grep ", "查已装包"),
    ("yum install -y ", "CentOS/RHEL 装包（新系统用 dnf）"),
    ("crontab -e", "编辑定时任务（分 时 日 月 周）"),
    ("(crontab -l; echo \"0 2 * * * 命令 >> 日志 2>&1\") | crontab -", "追加定时任务（| crontab - 是整体覆盖，追加要这么写）"),
    ("ssh-keygen -t ed25519", "生成 ed25519 公钥（免密前置）"),
    ("ssh-copy-id ", "推公钥免密"),
    ("ssh -p 2222 用户@主机 ", "远程登录（scp 的端口是大写 -P）"),
    ("scp -P 2222 文件 用户@host:/dst ", "传文件（端口是大写 -P；upload/download 步骤已覆盖主链路）"),
    // Windows 本地 cmd
    ("dir /B ", "列文件名（/B 裸名，配 outputs 捕获产物名）"),
    ("copy /Y ", "复制覆盖不问"),
    ("xcopy ", "递归复制：/E 含空目录 /I 目标是目录 /Y 覆盖"),
    ("robocopy ", "镜像同步：/MIR 镜像 /XD 排除目录；退出码 0-7 都算成功"),
    ("mklink /J ", "目录 junction（免管理员；跨卷软链对应物）"),
    ("tasklist | findstr ", "查进程（findstr ≈ grep）"),
    ("taskkill /PID ", "强杀进程（/IM 名 /F 按映像名）"),
    ("taskkill /IM 进程名.exe /F ", "按映像名强杀"),
    ("netstat -ano | findstr ", "查端口占用（末列 = PID）"),
    ("sc query ", "查 Windows 服务（stop / start 需管理员）"),
    ("schtasks /Create /SC DAILY /ST 02:00 /TN ", "计划任务（/TR 命令；/Run /TN 手动触发）"),
    ("where ", "命令实际路径（cmd 版 which）"),
    ("curl.exe -sf ", "Win10+ 自带真 curl（写 .exe 最稳）"),
    ("certutil -hashfile ", "算哈希：certutil -hashfile 文件 SHA256"),
    ("powershell -Command ", "逃生舱：复杂逻辑借 PowerShell"),
    // git
    ("git clone --depth 1 ", "浅克隆（只要最新一层）"),
    ("git pull --ff-only", "只快进合并（有分叉直接失败）"),
    ("git status -s", "短状态"),
    ("git log --oneline -5", "最近 5 条提交"),
    ("git tag -a ", "打标签：git tag -a v1.2.0 -m 说明"),
    // Maven
    ("mvn -B clean package -DskipTests", "构建：批处理模式 + 跳测试"),
    ("mvn -T 1C clean package -P ", "并行构建 + 指定 profile"),
    ("mvn -pl ", "只构建指定模块及其依赖（-pl 模块 -am）"),
    ("mvn help:evaluate -Dexpression=project.version -q -DforceStdout", "捕版本号（配 outputs）"),
    ("mvn dependency:tree -Dincludes=", "查依赖来源"),
    ("mvn clean deploy -DskipTests", "发私服"),
    // Gradle
    ("gradlew.bat clean bootJar --no-daemon", "Windows 本地构建（必须带 .bat）"),
    ("gradlew.bat build -x test", "跳过 test 构建"),
    ("gradlew.bat --refresh-dependencies build", "强刷依赖"),
    // 前端 Node
    ("corepack enable && corepack prepare pnpm@latest --activate", "按 packageManager 钉 pnpm 版本"),
    ("pnpm install --frozen-lockfile", "CI 标准装依赖（lock 不一致直接失败）"),
    ("pnpm run build", "前端构建"),
    ("npm ci", "干净安装：严格按 lock 装、绝不改 lockfile"),
    ("npm run build", "前端构建"),
    // kubectl（k8s-apply / 排障）
    ("kubectl apply -f ", "应用清单（-n 指命名空间）"),
    ("kubectl get pods -o wide", "列 Pod（-w 持续 watch）"),
    ("kubectl logs -f --tail=100 ", "跟 Pod 日志（多容器 -c）"),
    ("kubectl describe pod ", "排查拉不起 / 镜像失败 / 挂载错误"),
    ("kubectl rollout restart deployment/", "滚动重启（改了 ConfigMap 后）"),
    ("kubectl rollout status deployment/", "等滚动完成（--timeout=120s）"),
    ("kubectl rollout undo deployment/", "回滚上一版"),
    ("kubectl set image deployment/", "改镜像版本"),
    // Docker
    ("docker build -t 名:tag . ", "构建镜像（-f 指定 Dockerfile；版本号用 outputs 捕获）"),
    ("docker push ", "推镜像到 registry"),
    ("docker save -o ", "导出离线包（目标机 docker load -i）"),
    ("docker load -i ", "导入离线包"),
    ("docker logs -f --tail=100 ", "跟容器日志"),
    ("docker login ", "登录 registry（-u 用户，密码交互输入）"),
    // Java 运行时
    ("java -Xms512m -Xmx2g -jar ", "启动 jar（JVM 参数必须在 -jar 前面）"),
    ("java -Dfile.encoding=UTF-8 -jar ", "治日志乱码"),
    ("jps -l", "列 java 进程"),
    ("jstack ", "线程栈快照（卡死 / 死锁排查）"),
    ("jar tf ", "看 jar 内容不解包"),
    ("keytool -importcert -alias ca -keystore trust.jks ", "导证书（默认库口令 changeit）"),
    // 数据库客户端
    ("mysql -h ", "MySQL 客户端（-P 大写端口 -u 用户 -D 库 -e SQL）"),
    ("mysqldump --single-transaction ", "全量备份不锁表（空格后出参数提示）"),
    ("psql -h ", "PG 客户端（-p 小写端口 -U 用户 -d 库 -c SQL）"),
    ("pg_dump -Fc ", "pg 备份（自定义压缩格式；空格后出参数提示）"),
    ("pg_restore -h 主机 -U postgres -d 库名 ", "恢复 pg 备份（-Fc 格式；-j 4 并行）"),
    ("redis-cli -h ", "redis 客户端（ping 验通）"),
];

/// 命令模式补全：输入词非空时对骨架表首词做不区分大小写前缀匹配
fn command_suggestions_for_word(word: &str) -> Vec<egui::Suggestion> {
    if word.is_empty() {
        return vec![];
    }
    let wl = word.to_lowercase();
    CMD_SUGGESTIONS
        .iter()
        .filter(|(ins, _)| {
            ins.split_whitespace()
                .next()
                .is_some_and(|head| head.to_lowercase().starts_with(&wl))
        })
        .map(|(ins, desc)| egui::Suggestion::new((*ins).to_string()).description(*desc))
        .collect()
}

/// 参数级提示表：命令头 → 常用参数（上屏骨架, 说明）。命令骨架表只到
/// 「命令 + 常用组合」，逐参数（mysqldump 的 --single-transaction / --no-data…）
/// 在这张表里：行首词命中命令头后，参数位（词为空或匹配参数前缀）时弹出。
/// 提炼自 docs/命令速查.md 的参数说明段
const CMD_ARG_HINTS: &[(&str, &[(&str, &str)])] = &[
    (
        "mysqldump",
        &[
            ("-h 主机 -P 3306 -u root ", "主机 / 大写端口 / 用户（密码走 secret_env 注入 MYSQL_PWD）"),
            ("--single-transaction ", "InnoDB 一致性快照不锁表，生产必备"),
            ("--routines ", "含存储过程"),
            ("--triggers ", "含触发器"),
            ("--events ", "含事件"),
            ("--no-data ", "只导表结构"),
            ("--no-create-info ", "只导数据"),
            ("--where=\"created_at >= '…'\" ", "条件导出单表"),
            ("--databases db1 db2 ", "备多库且带 CREATE DATABASE"),
            ("--set-gtid-purged=OFF ", "GTID 库备份导到非 GTID 库报错时加"),
            ("--max-allowed-packet=512M ", "超大行报包超限时加"),
            ("--login-path=名 ", "mysql_config_editor 存过的混淆凭据路径"),
        ],
    ),
    (
        "mysql",
        &[
            ("-h 主机 -P 3306 -u root -D 库名 ", "主机 / 大写端口 / 用户 / 默认库"),
            ("-e \"SELECT COUNT(*) FROM t;\" ", "执行 SQL（-D 选库后不用写库前缀）"),
            ("-t ", "表格化输出"),
            ("库表 < backup/mysql.sql", "导入恢复（重定向进 mysql）"),
        ],
    ),
    (
        "psql",
        &[
            ("-h 主机 -p 5432 -U postgres -d 库名 ", "主机 / 小写端口 / 用户 / 库"),
            ("-c \"\\dt\" ", "执行 SQL / 元命令"),
        ],
    ),
    (
        "pg_dump",
        &[
            ("-h 主机 -p 5432 -U postgres ", "主机 / 小写端口 / 用户（密码走 PGPASSWORD）"),
            ("-Fc ", "自定义压缩格式（pg_restore 恢复）"),
            ("-f backup/pg.sql ", "输出到文件"),
            ("-s ", "只结构"),
            ("-a ", "只数据"),
            ("-t 表名 ", "只导指定表"),
            ("-n schema ", "按 schema 范围导出"),
        ],
    ),
    (
        "pg_restore",
        &[("-j 4 ", "并行恢复"), ("-d 库名 ", "目标库")],
    ),
    (
        "redis-cli",
        &[
            ("-h 主机 -p 6379 ", "主机 / 端口（密码建议 REDISCLI_AUTH 环境变量）"),
            ("ping", "PONG = 通"),
            ("--rdb backup/dump.rdb ", "远程拉 RDB 备份"),
            ("-n 1 ", "选 db"),
        ],
    ),
    (
        "curl",
        &[
            ("-s ", "静默"),
            ("-sf ", "静默 + 非 2xx 返回非零（健康检查关键）"),
            ("-X POST ", "指定方法"),
            ("-H \"Content-Type: application/json\" ", "请求头"),
            ("-d '{\"k\":\"v\"}' ", "请求体"),
            ("-o 文件 ", "输出写文件"),
            ("-m 10 ", "总超时秒"),
            ("--connect-timeout 3 ", "连接超时秒"),
            ("-k ", "自签证书跳过校验"),
        ],
    ),
    (
        "tar",
        &[
            ("-czf 包.tar.gz -C 目录 . ", "打包（-C 进目录，条目不带前缀）"),
            ("-xzf 包.tar.gz -C 目标目录 ", "解包（目录先 mkdir -p）"),
            ("-tzf 包.tar.gz ", "只看包里有什么，不解"),
        ],
    ),
    (
        "grep",
        &[
            ("-i ", "忽略大小写"),
            ("-n ", "显示行号"),
            ("-v ", "反选（剔除匹配行）"),
            ("-c ", "只输出计数"),
            ("-r ", "递归目录"),
            ("-E ", "扩展正则"),
            ("-A2 ", "命中行的后 2 行"),
            ("-B2 ", "命中行的前 2 行"),
            ("-C2 ", "命中行的上下 2 行"),
        ],
    ),
    (
        "find",
        &[
            ("-name \"*.log\" ", "按通配匹配"),
            ("-mtime +7 ", "修改时间 7 天前"),
            ("-size +100M ", "大于 100M"),
            ("-delete", "删除命中（先不加它干跑看列表）"),
            ("-exec 命令 {} \\;", "对命中逐个执行"),
        ],
    ),
    (
        "rsync",
        &[
            ("-avz ", "递归保权限 + 压缩（增量）"),
            ("-avn ", "干跑预演：先看会传什么"),
            ("--delete ", "镜像同步：目标多出的删掉（想清楚）"),
            ("--exclude ", "排除（如 --exclude node_modules）"),
        ],
    ),
    (
        "mvn",
        &[
            ("-B ", "批处理：关进度条彩色，日志干净（CI 标配）"),
            ("-q ", "只输出 ERROR（配 outputs 捕获）"),
            ("-T 1C ", "每核 1 线程并行构建"),
            ("-P prod ", "激活 profile"),
            ("-DskipTests ", "跳测试执行"),
            ("-Dmaven.test.skip=true ", "连编译都跳"),
            ("-U ", "强刷 SNAPSHOT 依赖"),
            ("-o ", "离线模式：只吃本地仓库"),
            ("-pl 模块 -am ", "只建指定模块及其上游依赖"),
            ("-s settings.xml ", "指定 settings.xml"),
            ("-Dmaven.repo.local=路径 ", "指定本地仓库"),
        ],
    ),
    (
        "kubectl",
        &[
            ("-n 命名空间 ", "指定 namespace"),
            ("-o wide ", "宽输出（IP / 节点）"),
            ("-f 清单.yaml ", "清单文件"),
            ("-w ", "持续 watch"),
            ("--tail=100 ", "日志最近 N 行"),
            ("--timeout=120s ", "超时（rollout status）"),
        ],
    ),
];

/// 当前输入行的首词（命令头）：参数级提示按它查 CMD_ARG_HINTS
fn line_head(before: &str) -> &str {
    before
        .rsplit('\n')
        .next()
        .unwrap_or("")
        .split_whitespace()
        .next()
        .unwrap_or("")
}

/// 参数级提示：词为空列出该命令全部常用参数；词非空按参数骨架前缀过滤
/// （还在打命令头本身时参数骨架不匹配 → 自然不弹）。
/// 已出现在当前命令行里的参数不再提示——Tab 上屏后弹层重开还列着刚补全的
/// 那条很别扭；按词元精确比对（-H 这类可重复参数想要第二个就手敲）
fn arg_hints_for_word(head: &str, word: &str, line: &str) -> Vec<egui::Suggestion> {
    let Some((_, args)) = CMD_ARG_HINTS.iter().find(|(h, _)| *h == head) else {
        return vec![];
    };
    let used: std::collections::HashSet<&str> = line.split_whitespace().collect();
    args.iter()
        .filter(|(ins, _)| {
            let fresh = ins
                .split_whitespace()
                .next()
                .is_none_or(|token| !used.contains(token));
            (word.is_empty() || ins.starts_with(word)) && fresh
        })
        .map(|(ins, desc)| egui::Suggestion::new((*ins).to_string()).description(*desc))
        .collect()
}

/// 双括号变量补全：word 是 `${{ ` 之后的路径片段（`${{` 后没打空格时 word 会
/// 整体带上 `${{` 前缀——剥掉再过滤，上屏文本把前缀补回去）。候选 = 变量组
/// 变量 / env（步骤产出 + 凭据注入）/ targets 字段 / now 系列 / run.id；
/// insert 自动闭合 ` }}`。
fn double_brace_suggestions(
    word: &str,
    vars: &[(String, String, String)],
    group_selected: bool,
    target_names: &[String],
    env_keys: &[String],
) -> Vec<egui::Suggestion> {
    let (path, prefixed) = match word.strip_prefix("${{") {
        Some(rest) => (rest, true),
        None => (word, false),
    };
    let wrap = |p: &str| {
        // 手工拼接：`${{`/`}}` 的 format! 转义太绕，push_str 直观且不会数错
        let mut s = String::new();
        if prefixed {
            s.push_str("${{ ");
        }
        s.push_str(p);
        s.push_str(" }}");
        s
    };
    let mut out = Vec::new();
    // variables.*：流程选了变量组 → 只列该组的键（desc 带现值便于选对，变量表
    // 面板本就明文展示）；未选组 → 列出全部组的键并在 desc 标注组名 + ⚠
    //（未选组运行时不替换——先在流程第 2 行选组，或给表达式加 `|| 默认值`）
    for (group, name, value) in vars {
        let p = format!("variables.{name}");
        if p.starts_with(path) {
            let shown = value.lines().next().unwrap_or("");
            let shown = if shown.chars().count() > 40 {
                let t: String = shown.chars().take(40).collect();
                format!("{t}…")
            } else {
                shown.to_string()
            };
            let desc = if group_selected {
                if shown.is_empty() {
                    "变量组变量".to_string()
                } else {
                    format!("变量组变量 = {shown}")
                }
            } else {
                format!("变量组 {group} = {shown}（⚠ 流程未选变量组，运行时不会替换）")
            };
            out.push(
                egui::Suggestion::new(wrap(&p))
                    .content(p)
                    .description(desc),
            );
        }
    }
    // env.*：步骤产出（outputs 捕获）+ 本步骤凭据注入（secret_env）的键
    //（运行期捕获/注入，编辑期无从取值）
    for key in env_keys {
        let p = format!("env.{key}");
        if p.starts_with(path) {
            out.push(
                egui::Suggestion::new(wrap(&p))
                    .content(p)
                    .description("运行期 env（步骤产出 / 凭据注入）"),
            );
        }
    }
    // targets.*：本项目目标的常用字段（host/port/user/work_root）
    for t in target_names {
        for (field, desc) in [
            ("host", "主机"),
            ("port", "端口"),
            ("user", "用户"),
            ("work_root", "工作根目录"),
        ] {
            let p = format!("targets.{t}.{field}");
            if p.starts_with(path) {
                out.push(
                    egui::Suggestion::new(wrap(&p))
                        .content(p)
                        .description(format!("目标 {t} · {desc}")),
                );
            }
        }
    }
    // 内置变量（引擎侧生成，一次运行同值）
    for (p, desc) in [
        ("now", "运行启动时刻（一次运行所有步骤同值）"),
        ("now.date", "运行启动日期 20261023"),
        ("now.time", "运行启动时刻 153045"),
        ("now.iso", "可读格式 2026-10-23 15:30:45"),
        ("run.id", "本次运行唯一标识（与控制台日志同名）"),
    ] {
        if p.starts_with(path) {
            out.push(
                egui::Suggestion::new(wrap(p))
                    .content(p)
                    .description(desc),
            );
        }
    }
    out
}

/// 单括号 `${VAR` 补全（兼容 shell 习惯）：word 含 `${` 前缀（无空格边界）。
/// 候选 = 步骤产出 / 凭据注入键 + 常用环境变量；insert 闭合 `}`。
fn single_brace_suggestions(word: &str, env_keys: &[String]) -> Vec<egui::Suggestion> {
    let Some(partial) = word.strip_prefix("${") else {
        return vec![];
    };
    let mut out = Vec::new();
    for key in env_keys {
        if key.starts_with(partial) {
            out.push(
                egui::Suggestion::new(format!("${{{key}}}"))
                    .description("步骤产出 / 凭据注入（outputs / secret_env）"),
            );
        }
    }
    const COMMON: &[(&str, &str)] = &[
        ("JAVA_HOME", "JDK 路径"),
        ("PATH", "可执行搜索路径"),
        ("MYSQL_PWD", "MySQL 密码（secret_env 注入）"),
        ("PGPASSWORD", "PG 密码（secret_env 注入）"),
        ("REDISCLI_AUTH", "Redis 密码（secret_env 注入）"),
        ("KUBECONFIG", "kubectl 配置（引擎注入）"),
    ];
    for (name, desc) in COMMON {
        if name.starts_with(partial) {
            out.push(
                egui::Suggestion::new(format!("${{{name}}}"))
                    .description(*desc),
            );
        }
    }
    out
}

/// 命令输入的建议生成（CompletionPopup 的 suggest 回调）：
/// - 命令模式：输入词对骨架表首词前缀匹配（不区分大小写）；行首词是已知命令
///   时再叠加参数级提示（空格后列常用参数，参数前缀可过滤）
/// - 变量模式（光标在未闭合 `${{ ` / `${` 内）：变量组变量 / env / targets /
///   内置变量；单括号补 `${NAME}`
fn command_suggestions(
    query: &egui::CompletionQuery<'_>,
    vars: &[(String, String, String)],
    group_selected: bool,
    target_names: &[String],
    env_keys: &[String],
) -> Vec<egui::Suggestion> {
    // 光标前文本判定模式（query.word_range 是字符序号区间）
    let before_end = char_idx_to_byte(query.text, query.word_range.end.0);
    let before = &query.text[..before_end];
    match unclosed_var(before) {
        Some(VarMode::Double) => double_brace_suggestions(
            query.word,
            vars,
            group_selected,
            target_names,
            env_keys,
        ),
        Some(VarMode::Single) => single_brace_suggestions(query.word, env_keys),
        None => {
            // 光标所在行：骨架表的「行首词」判定和参数提示的「已用参数」判定都在行内做
            let line = before.rsplit('\n').next().unwrap_or("");
            // 命令骨架表只在「行首词」时提示：行中输入词对骨架表的匹配会把
            // 「systemctl re」提示成整条骨架，接受后变成 systemctl systemctl restart…
            // （word 恰是行尾词，剥掉它剩下的就是词前文本，全空白 = 在行首）
            let line_prefix = line.strip_suffix(query.word).unwrap_or(line);
            let mut out = if line_prefix.trim().is_empty() {
                command_suggestions_for_word(query.word)
            } else {
                Vec::new()
            };
            out.extend(arg_hints_for_word(line_head(before), query.word, line));
            out
        }
    }
}

fn delete_current_job(main: &mut MainState) {
    let (Some(pi), Some(ji)) = (main.selected_project, main.selected_job) else {
        return;
    };
    let (path, name) = {
        let je = &main.projects[pi].jobs[ji];
        (je.path.clone(), je.job.name.clone())
    };
    if let Err(e) = std::fs::remove_file(&path) {
        main.status_msg = Some(format!("删除文件失败: {e}"));
        return;
    }
    main.projects[pi].project.jobs.retain(|j| j != &name);
    let entry = &main.projects[pi];
    if let Err(e) =
        crate::storage::files::save_yaml(&entry.dir.join("project.yaml"), &entry.project)
    {
        main.status_msg = Some(format!("project.yaml 同步失败: {e}"));
    }
    main.reload();
    main.status_msg = Some(format!("流程「{name}」已删除"));
}

/// 渲染单个步骤卡片，返回本帧用户对该步骤的操作请求。
/// 下拉分组展示「本项目 / 全局」，选中后以徽标标注来源、悬停显示解析到的实际地址。
/// running = 有作业在跑（此时单步运行按钮禁用）。
/// target_names = 本项目可用的目标名；job_target_display = 流程默认目标的展示文本。
/// state = 本步骤的运行状态（卡片外围光圈：运行蓝闪/等待黄闪/成功绿/失败红，见 ui::status）。
/// timeline = Some((耗时 ms, 相对最慢步骤占比, 重试次数)) —— GHA 式耗时时间线，Idle 作业为 None。
#[allow(clippy::too_many_arguments)]
fn edit_step(
    ui: &mut egui::Ui,
    job: &mut Job,
    si: usize,
    tool_menu: &ToolMenu,
    running: bool,
    target_names: &[String],
    var_names: &[(String, String, String)],
    group_selected: bool,
    job_target_display: &str,
    default_timeout: u64,
    state: StepVisualState,
    dark: bool,
    timeline: Option<(u64, f32, u32)>,
) -> StepAction {
    let mut action = StepAction::None;

    status::step_frame(ui, state, dark).show(ui, |ui| {
        // 运行中显式禁用整张卡片（#12 双保险：外层 ScrollArea 内容已 disable，
        // 这里再按卡片显式包一层，任何布局调整都不会意外放开输入）
        ui.add_enabled_ui(!running, |ui| {
        let steps_len = job.steps.len();
        // 命令补全数据源：其他步骤 outputs 声明的产出键（${{ env.X }} / ${VAR}）。
        // 必须在 step 可变借用 job.steps[si] 之前收集；本步骤凭据注入键在拿到
        // step 之后并入
        let mut env_keys: Vec<String> = job
            .steps
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != si)
            .flat_map(|(_, s)| s.outputs.keys().cloned())
            .collect();
        let step = &mut job.steps[si];
        env_keys.extend(step.secret_env.keys().cloned());
        env_keys.sort();
        env_keys.dedup();
        ui.horizontal(|ui| {
            ui.checkbox(&mut step.enabled, "");
            ui.label(format!("Step {}:", si + 1));

            let name = step.name.get_or_insert_default();
            ui.add(
                egui::TextEdit::singleline(name)
                    .desired_width(150.0)
                    .hint_text("步骤名"),
            );

            ui.separator();
            let old_type = step.step_type;
            let mut st_idx = step_type_index(&step.step_type);
            egui::ComboBox::from_id_salt(format!("st_{si}"))
                .selected_text(STEP_TYPES_SHORT[st_idx])
                .width(90.0)
                .show_ui(ui, |ui| {
                    for (i, label) in STEP_TYPES.iter().enumerate() {
                        ui.selectable_value(&mut st_idx, i, *label);
                    }
                });
            step.step_type = step_type_from_index(st_idx);
            // 类型切换后命令语义完全不同（exec 的 echo、upload 的映射互不通用），清空遗留内容
            if step.step_type != old_type {
                step.command.clear();
            }

            if ui.add_enabled(
                !running,
                egui::Button::new("▶ 从此").small(),
            )
            .on_hover_text("从该步骤开始执行到流程结束（之前的步骤跳过，其产出变量不可用）")
            .clicked()
            {
                action = StepAction::RunFrom;
            }
            if ui.add_enabled(
                !running,
                egui::Button::new("▶ 仅此").small(),
            )
            .on_hover_text("只执行这一步（调试用）")
            .clicked()
            {
                action = StepAction::RunOnly;
            }
            // 上/下移在首尾自动禁用：第一步不能再上、最后一步不能再下——
            // 禁用态悬停说明原因（比"点了没反应"直观）
            let can_up = si > 0;
            let up_hover = if can_up { "上移一位" } else { "已是第一步" };
            if ui
                .add_enabled(can_up, egui::Button::new("↑").small())
                .on_hover_text(up_hover)
                .on_disabled_hover_text(up_hover)
                .clicked()
            {
                action = StepAction::MoveUp;
            }
            let can_down = si + 1 < steps_len;
            let down_hover = if can_down { "下移一位" } else { "已是最后一步" };
            if ui
                .add_enabled(can_down, egui::Button::new("↓").small())
                .on_hover_text(down_hover)
                .on_disabled_hover_text(down_hover)
                .clicked()
            {
                action = StepAction::MoveDown;
            }
            if ui.small_button("🗑").clicked() {
                action = StepAction::Delete;
            }
        });

        // ── 执行参数行：工具（env 注入 + PATH 前置选版本）| 超时 ──
        // 从第一行拆出：第一行塞不下全部控件（自然宽 956px，见布局回归测试）
        ui.horizontal(|ui| {
            ui.label("工具:");
            let tool_resp = egui::ComboBox::from_id_salt(format!("tool_{si}"))
                .truncate()
                .selected_text(if step.tool.is_empty() {
                    "（无）".to_string()
                } else {
                    step.tool.clone()
                })
                .width(110.0)
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut step.tool, String::new(), "（无）");
                    if !tool_menu.project.is_empty() {
                        ui.separator();
                        ui.weak("— 本项目 —");
                        for name in &tool_menu.project {
                            ui.selectable_value(&mut step.tool, name.clone(), name);
                        }
                    }
                    if !tool_menu.global.is_empty() {
                        ui.separator();
                        ui.weak("— 全局 —");
                        for name in &tool_menu.global {
                            ui.selectable_value(&mut step.tool, name.clone(), name);
                        }
                    }
                });
            tool_resp.response.on_hover_text({
                // 来源徽标（〔全局〕/〔项目〕）与解析路径并入悬停：行内常驻徽标
                // 会把步骤行撑长，窄窗口下后续控件被挤出卡片显示不全（实测 bug）。
                // 下拉列表本身的「— 本项目 — / — 全局 —」分组已标来源。
                let mut hover = "选择工具的作用：\n\
                 1. 该工具的环境变量注入本步骤；\n\
                 2. 工具所在目录前置到 PATH——命令里写裸名（java / mvn / node…）时，\n\
                    用的就是这个工具的版本（多个 java 并存时靠它选版本）。\n\
                 命令本身不变；具体解析路径见「👁 预览」或 🎯 目标管理 → 本机工具链。\n\
                 作用边界：只影响发版机本机执行的步骤；SSH 远端步骤在远端 shell 执行，\n\
                 不受本机工具表影响。运行期引擎会对用到的工具自动自检（version_check）"
                    .to_string();
                if !step.tool.is_empty() {
                    match tool_menu.merged.get(&step.tool) {
                        Some(tool) => {
                            let from_project =
                                tool_menu.project.iter().any(|n| n == &step.tool);
                            hover.push_str(&format!(
                                "\n\n当前「{}」来源: {}{}\n解析到: {}",
                                step.tool,
                                if from_project { "项目级覆盖" } else { "全局" },
                                if from_project { "（同名覆盖全局）" } else { "" },
                                tool.command
                            ));
                        }
                        None => hover.push_str(
                            "\n\n⚠ 未映射：工具清单里没有这个名字（运行自检会失败）；\
                             可在 ⚙ 设置 → 🔧 工具管理 添加",
                        ),
                    }
                }
                hover
            });
            // 未映射保留行内红标：需要用户处理的异常，不能只藏在悬停里
            if !step.tool.is_empty() && !tool_menu.merged.contains_key(&step.tool) {
                ui.colored_label(Color32::from_rgb(220, 120, 90), "⚠未映射")
                    .on_hover_text("工具清单里没有这个名字（运行自检会失败）；可在 ⚙ 设置 → 🔧 工具管理 添加");
            }

            ui.separator();
            match &mut step.timeout {
                Some(v) => {
                    ui.label("超时:");
                    // 定宽：DragValue 缺省按内容自适应，输入位数越多框越宽，
                    // 会把右侧「从此 / 仅此 / ↑ / 🗑」推出卡片可视区
                    let t_resp = ui.add(
                        egui::DragValue::new(v)
                            .suffix("s")
                            .speed(1.0)
                            .clip_text(true)
                            .min_size(egui::vec2(64.0, 0.0))
                            // 无符号数拖到 0 以下会回绕成 u64::MAX（真实发生过：
                            // yaml 里写出 timeout: 18446744073709551615），
                            // 上限 30 天足够任何构建，0 = 不限时
                            .range(0..=2_592_000),
                    );
                    t_resp.on_hover_text(
                        "秒。填 0 = 本步不限时（跑多久都行，仍受流程级总超时约束）；\n\
                         🗑 删掉 = 用全局默认；\n\
                         mvn 首次拉依赖这类慢步骤建议 1800",
                    );
                    if *v == 0 {
                        ui.weak("（不限时）");
                    }
                    if ui.small_button("🗑").clicked() {
                        step.timeout = None;
                    }
                }
                None => {
                    // 真实生效值 = config.yaml default_timeout（此前写死 "300s"，
                    // 与全局默认 600s 不符，有误导）；全局默认 0 = 不限时
                    if default_timeout == 0 {
                        ui.weak("超时:不限时");
                    } else {
                        ui.weak(format!("超时:默认{default_timeout}s"));
                    }
                    if ui.small_button("⏱").clicked() {
                        step.timeout = Some(300);
                    }
                }
            }

            ui.separator();
            // 凭据注入入口与工具选择相邻（数据库类工具 mysqldump / pg_dump 需要密码）：
            // 仅是入口，编辑行在卡片底部、仅有条目时渲染——不放这里免得每张卡片都多一行
            if step.secret_env.is_empty()
                && ui
                    .small_button("＋凭据注入")
                    .on_hover_text(
                        "从 Windows Credential Manager 取密码注入本步骤环境变量（如 MYSQL_PWD / PGPASSWORD）。\n\
                         配合工具绑定使用：工具选 mysqldump / pg_dump，密码经凭据注入。\n\
                         密码不进命令行（进程列表暴露）、不落 YAML。\n\
                         先保存密码：remote-build set-password db/prod-mysql <密码>"
                    )
                    .clicked()
            {
                step.secret_env
                    .insert("MYSQL_PWD".to_string(), "db/mysql".to_string());
            }
        });

        ui.add_space(2.0);
        // upload / download / pack 都是「映射」语义（每行 `源 -> 目标`），统一用
        // 结构化映射编辑器，不再让手写箭头文本；exec / wait 仍是自由命令
        match step.step_type {
            StepType::Upload => edit_mappings(
                ui,
                step,
                "本地路径（相对项目目录）",
                "远端路径（/ 开头绝对；相对 = work_root）",
                "本地路径：相对项目目录或绝对路径；具体文件或目录（不支持通配）",
                "远端路径：/ 开头为绝对路径；相对 = 目标的 work_root",
            ),
            StepType::Download => edit_mappings(
                ui,
                step,
                "远端路径（相对 = work_root）",
                "本地路径（相对项目目录）",
                "远端路径：/ 开头为绝对路径；相对 = 目标的 work_root（方向与上传相反）",
                "本地路径：相对项目目录或绝对路径；父目录自动创建，下载后 sha256 校验",
            ),
            StepType::Pack => edit_pack_mapping(ui, step),
            _ => {
                // 命令补全（egui 内置 CompletionPopup）：输入词匹配命令骨架表；
                // 光标落在未闭合 `${{ ` / `${` 内切换变量模式（变量组变量 /
                // env 产出 / targets 字段 / 内置变量），↑↓ 选 / Tab·Enter 上屏 / Esc 关
                egui::CompletionPopup::new(ui.make_persistent_id(("cmdcomplete", si))).show(
                    ui,
                    egui::TextEdit::multiline(&mut step.command)
                        .desired_rows(2)
                        .desired_width(f32::INFINITY)
                        .font(egui::TextStyle::Monospace)
                        .hint_text(
                            "命令，支持 ${{ variables.X }} / ${{ env.X }} / ${VAR}；\
                             输入有补全：↑↓ 选 / Tab 上屏 / Esc 关",
                        ),
                    |query| {
                        command_suggestions(
                            query,
                            var_names,
                            group_selected,
                            target_names,
                            &env_keys,
                        )
                    },
                );
            }
        }

        // wait 步骤探测模式（Compose healthcheck 同款：命令 / tcp / http）
        if matches!(step.step_type, StepType::Wait) {
            ui.horizontal(|ui| {
                match &mut step.wait {
                    Some(p) => {
                        ui.label("探测:").on_hover_text(
                            "wait 步骤的探测方式：命令（退出码 0 = 就绪，空则用上方命令）、\
                             tcp（从发版机直连 host:port）、http（GET 状态码 2xx；仅 http://）",
                        );
                        let mut m_idx = wait_mode_index(p.mode);
                        egui::ComboBox::from_id_salt(format!("waitmode_{si}"))
                            .selected_text(WAIT_MODES[m_idx])
                            .width(70.0)
                            .show_ui(ui, |ui| {
                                for (i, label) in WAIT_MODES.iter().enumerate() {
                                    ui.selectable_value(&mut m_idx, i, *label);
                                }
                            });
                        p.mode = wait_mode_from_index(m_idx);
                        let hint = match p.mode {
                            crate::models::WaitMode::Command => "探测命令（空 = 用上方命令）",
                            crate::models::WaitMode::Tcp => "host:port，如 10.0.0.5:8080",
                            crate::models::WaitMode::Http => "http://host[:port]/path",
                        };
                        // 命令模式探测目标同样给补全（tcp/http 是 host:port / URL，不弹命令表）
                        if matches!(p.mode, crate::models::WaitMode::Command) {
                            egui::CompletionPopup::new(ui.make_persistent_id((
                                "waitcomplete",
                                si,
                            )))
                            .max_height(160.0)
                            .show(
                                ui,
                                egui::TextEdit::singleline(&mut p.target)
                                    .desired_width(220.0)
                                    .hint_text(hint),
                                |query| {
                                    command_suggestions(
                                        query,
                                        var_names,
                                        group_selected,
                                        target_names,
                                        &env_keys,
                                    )
                                },
                            );
                        } else {
                            ui.add(
                                egui::TextEdit::singleline(&mut p.target)
                                    .desired_width(220.0)
                                    .hint_text(hint),
                            );
                        }
                        ui.label("间隔");
                        ui.add(
                            egui::DragValue::new(&mut p.interval_seconds)
                                .suffix("s")
                                .clip_text(true)
                                .min_size(egui::vec2(56.0, 0.0))
                                .range(0..=120),
                        );
                        // 致命退出码（fire-and-poll 配方）：仅 command 模式有意义
                        if matches!(p.mode, crate::models::WaitMode::Command) {
                            let mut fatal = p.fail_exit_code.unwrap_or(0);
                            ui.label("失败码");
                            ui.add(
                                egui::DragValue::new(&mut fatal)
                                    .clip_text(true)
                                    .min_size(egui::vec2(48.0, 0.0))
                                    .range(0..=255),
                            )
                            .on_hover_text(
                                "探测命令返回该退出码 = 任务已死，立即失败停止轮询\n\
                                 （长任务配方：远端脚本 status=failed 时 exit 2，这里配 2）\n\
                                 0 = 不启用（非 0 一律视为还没就绪，继续等待）",
                            );
                            p.fail_exit_code = (fatal != 0).then_some(fatal);
                        }
                        if ui.small_button("🗑").clicked() {
                            step.wait = None;
                        }
                    }
                    None => {
                        ui.weak("探测:命令轮询");
                        if ui
                            .small_button("⏲ 模式")
                            .on_hover_text(
                                "切换探测方式：命令 / tcp 可连 / http 2xx\n\
                                 （部署后验证服务可达，http/tcp 从发版机直连）",
                            )
                            .clicked()
                        {
                            step.wait = Some(crate::models::WaitProbe {
                                mode: crate::models::WaitMode::Tcp,
                                target: String::new(),
                                interval_seconds: 0,
                                fail_exit_code: None,
                            });
                        }
                    }
                }
            });
        }

        // 耗时时间线（GHA 式）：耗时条（相对全流程最慢步骤）+ 重试次数标注
        if let Some((ms, frac, retries)) = timeline {
            ui.horizontal(|ui| {
                ui.monospace(format!("⏱ {}", human_duration(ms)));
                ui.add(
                    egui::ProgressBar::new(frac)
                        .desired_width(140.0)
                        .desired_height(8.0),
                );
                if retries > 0 {
                    ui.colored_label(
                        Color32::from_rgb(229, 192, 123),
                        format!("⟳ 重试 {retries} 次"),
                    );
                }
            });
        }

        // ── 步骤选项按主题分行（#11 布局回归）：单行塞全部选项在展开组合下
        // 横向溢出；按「执行去向 / 可靠性 / 变更与条件」分行，每行自然宽 ≤620px
        ui.horizontal(|ui| {
            // 步骤级目标（§3.3）：覆盖流程默认——混合流程的关键
            ui.label("目标:");
            let sel_text = match &step.target {
                Some(t) => t.clone(),
                None => format!("跟随流程（{job_target_display}）"),
            };
            egui::ComboBox::from_id_salt(format!("steptarget_{si}"))
                .truncate()
                .selected_text(sel_text)
                .width(150.0)
                .show_ui(ui, |ui| {
                    ui.selectable_value(
                        &mut step.target,
                        None,
                        format!("跟随流程（{job_target_display}）"),
                    );
                    for name in target_names {
                        ui.selectable_value(&mut step.target, Some(name.clone()), name);
                    }
                });

            ui.checkbox(&mut step.continue_on_error, "失败继续");
            // 重试配置（upload 未配置时引擎默认重试 1 次；显式配置覆盖之）
            match &mut step.retry {
                Some(r) => {
                    ui.label("重试:");
                    ui.add(
                        egui::DragValue::new(&mut r.max_attempts)
                            .suffix("次")
                            .clip_text(true)
                            .min_size(egui::vec2(52.0, 0.0))
                            .range(1..=10),
                    );
                    ui.label("间隔");
                    ui.add(
                        egui::DragValue::new(&mut r.backoff_seconds)
                            .suffix("s")
                            .clip_text(true)
                            .min_size(egui::vec2(56.0, 0.0))
                            .range(0..=300),
                    );
                    if ui.small_button("🗑").clicked() {
                        step.retry = None;
                    }
                }
                None => {
                    ui.weak("重试:默认");
                    if ui
                        .small_button("⟳")
                        .on_hover_text("显式配置重试次数与间隔（upload 不配置时默认重试 1 次）")
                        .clicked()
                    {
                        step.retry = Some(crate::models::RetryConfig {
                            max_attempts: 3,
                            backoff_seconds: 5,
                        });
                    }
                }
            }
        });
        ui.horizontal(|ui| {
            // creates 状态守卫（Ansible creates）：产物已存在 → 跳过本步。
            // 回答「creates 是干嘛的」：给本步声明一个"做完了长这样"的标记路径——
            // 重跑流程时它已存在 = 这步以前做过 → 秒过不重复（增量重跑的地基）
            match &mut step.creates {
                Some(c) => {
                    ui.label("creates:").on_hover_text(
                        "状态守卫（Ansible creates）：声明的产物路径已存在 → 整个步骤跳过。\n\
                         用途：重跑流程时「拉依赖 / 编译」这类已完成步骤秒过，不重复构建。\n\
                         路径：本地相对项目目录，支持 * 通配（如 target/*.jar）；\n\
                         SSH 目标为远端路径（相对 = work_root）",
                    );
                    // 独占一行：输入框吃掉剩余宽度（给 × 留位），长路径不憋在定宽里
                    let creates_w = (ui.available_width() - 40.0).max(120.0);
                    ui.add(
                        egui::TextEdit::singleline(c)
                            .desired_width(creates_w)
                            .hint_text("产物路径，已存在则跳过本步：node_modules / target/*.jar"),
                    );
                    if ui.small_button("🗑").clicked() {
                        step.creates = None;
                    }
                }
                None => {
                    if ui
                        .small_button("＋creates")
                        .on_hover_text(
                            "状态守卫：声明本步的产物路径，已存在则跳过（重跑秒过已完成步骤）。\n\
                             例：「拉依赖」creates node_modules、「编译」creates target/*.jar",
                        )
                        .clicked()
                    {
                        step.creates = Some(String::new());
                    }
                }
            }
        });
        ui.horizontal(|ui| {
            // notify（Ansible handlers）：本步骤实际产生变更时，通知对应 handler 收尾执行
            if step.notify.is_empty() {
                if ui
                    .small_button("＋notify")
                    .on_hover_text(
                        "变更才触发：本步骤真的执行/真的传了新内容时，流程成功收尾后执行\n\
                         build.yaml handlers 表里的同名步骤（如「重启服务」）。多个用逗号分隔",
                    )
                    .clicked()
                {
                    step.notify.push("重启服务".to_string());
                }
            } else {
                ui.label("notify:").on_hover_text(
                    "变更才触发：流程成功收尾后执行 handlers 表里的同名步骤；\n\
                     creates / when 跳过或上传内容未变都不算变更。多个用逗号分隔",
                );
                let mut buf = step.notify.join(",");
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut buf)
                        .desired_width(crate::ui::input::SHORT),
                );
                if resp.changed() {
                    step.notify = buf.split(',').map(|s| s.trim().to_string()).collect();
                }
                if resp.lost_focus() {
                    step.notify.retain(|s| !s.is_empty());
                }
                if ui.small_button("🗑").clicked() {
                    step.notify.clear();
                }
            }
            if let Some(cond) = &mut step.condition {
                ui.label("条件:").on_hover_text(
                    "when / condition：条件不满足则跳过本步。\n\
                     例：${{ variables.ENV }} == prod、${{ env.VERSION != '' }}",
                );
                // 吃掉行内剩余宽度（给 × 留位）：定宽 220 在窄窗口下会把 × 挤出卡片右缘
                let cond_w = (ui.available_width() - 40.0).max(80.0);
                ui.add(egui::TextEdit::singleline(cond).desired_width(cond_w));
                if ui.small_button("🗑").clicked() {
                    step.condition = None;
                }
            } else if ui
                .small_button("＋条件")
                .on_hover_text(
                    "条件跳过（YAML 里写 when: 或 condition: 等价）：\n\
                     例：${{ variables.ENV }} == prod（生产专属步骤与通用流程合到一个流程）",
                )
                .clicked()
            {
                step.condition = Some("${{ env.VERSION != '' }}".to_string());
            }
        });

        // 📁 步骤级工作目录：相对项目代码目录的子目录（多模块仓库常用——
        // backend / frontend 各自一条流水线）。可选行模式同「＋条件」：
        // 多数步骤用不到，不设时常驻噪音压到最低
        ui.horizontal(|ui| {
            match &mut step.working_dir {
                Some(wd) => {
                    ui.label("工作目录:").on_hover_text(
                        "本步骤命令的执行子目录（相对项目代码目录，如 backend、apps/web）。\n\
                         留空 = 流程默认（job.working_dir）或项目根；支持 ${{ … }} 占位符",
                    );
                    ui.add(
                        egui::TextEdit::singleline(wd)
                            .desired_width(220.0)
                            .hint_text("backend"),
                    );
                    if ui.small_button("🗑").clicked() {
                        step.working_dir = None;
                    }
                }
                None => {
                    if ui
                        .small_button("＋工作目录")
                        .on_hover_text(
                            "本步骤命令在项目代码目录的哪个子目录下执行；\n\
                             命令里的相对路径（如 target/*.jar 产物）都以此为基准",
                        )
                        .clicked()
                    {
                        step.working_dir = Some(String::new());
                    }
                }
            }
        });

        // 🔒 凭据注入编辑行（TODO「第三方工具管理 · 方案 B」）：环境变量名 → 凭据键。
        // 运行时从凭据管理器取密码注入子进程 env——密码不进命令行、不落 YAML。
        // 仅在已有条目时渲染（入口「＋凭据注入」在执行参数行，与工具选择相邻——
        // 独立成行是每张卡片的噪音，多数步骤用不到凭据）。
        // 编辑走「快照编辑」：HashMap 收集为排序 Vec → 编辑 → 写回（上传映射同款）
        if !step.secret_env.is_empty() {
            let mut rows: Vec<(String, String)> =
                step.secret_env.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
            rows.sort();
            let mut removed: Option<String> = None;
            for (env_name, cred_key) in &mut rows {
                ui.horizontal(|ui| {
                    ui.label("🔒凭据:").on_hover_text(
                        "运行时从 Windows Credential Manager 取密码注入子进程环境变量。\n\
                         左 = 环境变量名（如 MYSQL_PWD / PGPASSWORD），右 = 凭据键。\n\
                         保存密码：remote-build set-password <凭据键> <密码>"
                    );
                    ui.add(
                        egui::TextEdit::singleline(env_name)
                            .desired_width(110.0)
                            .hint_text("环境变量名"),
                    );
                    ui.label("←");
                    let key_w = (ui.available_width() - 40.0).max(120.0);
                    ui.add(
                        egui::TextEdit::singleline(cred_key)
                            .desired_width(key_w)
                            .hint_text("凭据键，如 db/prod-mysql"),
                    );
                    if ui.small_button("🗑").on_hover_text("删除此凭据注入").clicked() {
                        removed = Some(env_name.clone());
                    }
                });
            }
            ui.horizontal(|ui| {
                if ui.small_button("＋凭据注入").clicked() {
                    step.secret_env
                        .insert(format!("ENV_{}", step.secret_env.len() + 1), "db/mysql".to_string());
                }
            });
            if let Some(rm) = &removed {
                rows.retain(|(k, _)| k != rm);
            }
            // 编辑写回（键改名 = 重建映射；删除在写回前生效）
            step.secret_env = rows.into_iter().collect();
        }
        }); // add_enabled_ui（运行中禁用整张卡片）
    });

    action
}

// ---------------------------------------------------------------------------
// 映射的结构化编辑（upload / download 共用）：from/to 输入框 + 加/减行。
// 落盘仍是引擎 parse_upload_mappings 的行格式（每行 `<源> -> <目标>`，
// download 方向相反），旧 YAML / 手写配置零迁移。
// ---------------------------------------------------------------------------

/// upload/download 命令 → 编辑行。与引擎同一格式：按行 split 首个 "->" 并 trim；
/// 没有 "->" 的行放进 from 槽等待补全；空命令给一行空行起步。
fn upload_rows(command: &str) -> Vec<(String, String)> {
    let mut rows = Vec::new();
    for line in command.lines() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        match t.split_once("->") {
            Some((l, r)) => rows.push((l.trim().to_string(), r.trim().to_string())),
            None => rows.push((t.to_string(), String::new())),
        }
    }
    if rows.is_empty() {
        rows.push((String::new(), String::new()));
    }
    rows
}

/// 编辑行 → 命令。空行序列化为 "->" 占位：能原样解析回空行（否则
/// "＋ 添加映射" 的空行会在写回时被过滤掉、看起来没反应），引擎把空映射当空行跳过。
fn upload_command(rows: &[(String, String)]) -> String {
    rows.iter()
        .map(|(l, r)| format!("{} -> {}", l.trim(), r.trim()).trim().to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

/// 提示文案分离：输入框内只放短提示（长提示会被截断且没有地方看全），
/// 完整说明放悬停（on_hover_text）
fn edit_mappings(
    ui: &mut egui::Ui,
    step: &mut Step,
    from_hint: &str,
    to_hint: &str,
    from_hover: &str,
    to_hover: &str,
) {
    let mut rows = upload_rows(&step.command);

    let mut del: Option<usize> = None;
    for (ri, (l, r)) in rows.iter_mut().enumerate() {
        ui.horizontal(|ui| {
            if ui
                .small_button("🗑")
                .on_hover_text("删除此映射")
                .clicked()
            {
                del = Some(ri);
            }
            let from_w = ((ui.available_width() - 150.0) * 0.5).max(140.0);
            ui.add(
                egui::TextEdit::singleline(l)
                    .desired_width(from_w)
                    .hint_text(from_hint),
            )
            .on_hover_text(from_hover);
            ui.label("→");
            // to 框吃掉剩余全部宽度：行总宽与 exec 文本框一致，切换类型卡片不跳宽
            ui.add(
                egui::TextEdit::singleline(r)
                    .desired_width(ui.available_width())
                    .hint_text(to_hint),
            )
            .on_hover_text(to_hover);
        });
    }
    if let Some(ri) = del {
        rows.remove(ri);
    }
    if ui
        .small_button("＋ 添加映射")
        .on_hover_text("再添加一条 源 → 目标 映射（微服务多 jar 场景）")
        .clicked()
    {
        rows.push((String::new(), String::new()));
    }
    step.command = upload_command(&rows);
}

/// pack 步骤的结构化编辑：单条映射（源目录/通配 → 目标 .zip/.tar.gz）+ 排除模式。
/// 落盘仍是引擎 pack_step 的格式 `<源> -> <目标> [--exclude 模式]…`。
fn edit_pack_mapping(ui: &mut egui::Ui, step: &mut Step) {
    // 解析：首个 "->" 分出源/其余；其余再按 "--exclude" 切出目标与排除模式
    let (mut src, mut dst, mut excludes): (String, String, Vec<String>) =
        match step.command.split_once("->") {
            Some((l, rest)) => {
                let mut parts = rest.split("--exclude");
                let dst = parts.next().unwrap_or("").trim().to_string();
                let excludes = parts
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect();
                (l.trim().to_string(), dst, excludes)
            }
            None => (step.command.trim().to_string(), String::new(), Vec::new()),
        };

    ui.horizontal(|ui| {
        let from_w = ((ui.available_width() - 150.0) * 0.5).max(140.0);
        ui.add(
            egui::TextEdit::singleline(&mut src)
                .desired_width(from_w)
                .hint_text("源：目录或通配"),
        )
        .on_hover_text(
            "打包源：目录或通配（相对项目目录，绝对路径原样）。\n\
             源以 /. 结尾 = tar -C 语义：包内条目不带目录根",
        );
        ui.label("→");
        ui.add(
            egui::TextEdit::singleline(&mut dst)
                .desired_width(ui.available_width())
                .hint_text("目标：xxx.zip / xxx.tar.gz（扩展名决定格式）"),
        )
        .on_hover_text(
            "打包目标：.zip / .tar.gz / .tgz（扩展名决定格式；tar 保留可执行权限）。\n\
             相对项目目录，支持 ${{ now.date }} 等变量",
        );
    });
    ui.horizontal(|ui| {
        ui.label("排除:");
        let mut buf = excludes.join(" ");
        let resp = ui
            .add(
                egui::TextEdit::singleline(&mut buf)
                    .desired_width(ui.available_width())
                    .hint_text("--exclude 模式，空格分隔；字面量 = 目录分量，* ? 通配"),
            )
            .on_hover_text(
                "rsync --exclude 语义：node_modules 整棵剪枝；*.map 按文件名通配。\n\
                 例：node_modules *.map",
            );
        if resp.changed() {
            excludes = buf.split_whitespace().map(str::to_string).collect();
        }
    });
    let tail = excludes
        .iter()
        .map(|p| format!(" --exclude {p}"))
        .collect::<String>();
    // 全空时不写 "->" 占位：保持空命令（pack 空命令在引擎侧有明确报错文案）
    step.command = if src.trim().is_empty() && dst.trim().is_empty() && tail.is_empty() {
        String::new()
    } else {
        format!("{} -> {}{}", src.trim(), dst.trim(), tail).trim().to_string()
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── 布局稳定性回归（「每个选项都输入一下，布局不能变」的自动化版本）─────
    // 同一组开关下，把每个输入都换成病态值（超长文本 / 5 位数超时 / 最大枚举），
    // 卡片总宽必须与默认值一致：任何随输入变宽的控件（DragValue 自适应、
    // ComboBox 选中文本 Extend、heading 不截断……）都会破坏这个等式。
    // 新增步骤选项时：把它的病态值加进病态分支，测试不过 = 布局会漂移。

    fn card_width(job: &mut Job, si: usize) -> f32 {
        let tool_menu = ToolMenu {
            project: vec!["java".to_string()],
            global: Vec::new(),
            merged: std::collections::HashMap::new(),
        };
        let target_names = vec![
            "prod".to_string(),
            "很长的目标名称用来测试下拉框文本截断".to_string(),
        ];
        // 面板预算 = 最窄可用编辑区（1080p + 左树 220 时中央 ~840，留余量）。
        // 723.4 的实测宽度含 🗑 按钮（比 × 宽 ~4px，统一删除图标后持平）
        let panel_w = 724.0;
        let mut width = 0.0f32;
        let ctx = egui::Context::default();
        ctx.set_pixels_per_point(1.0);
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(2400.0, 4000.0),
            )),
            ..Default::default()
        };
        let mut out = ctx.run_ui(raw, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                ui.allocate_ui(egui::vec2(panel_w, 3000.0), |ui| {
                    let _ = edit_step(
                        ui,
                        job,
                        si,
                        &tool_menu,
                        false,
                        &target_names,
                        &[],
                        false,
                        "无",
                        600,
                        StepVisualState::Idle,
                        true,
                        None,
                    );
                    width = ui.min_rect().width();
                });
            });
        });
        // 测试上下文不做真实渲染，字型图集增量直接丢弃（egui 要求显式处理）
        out.textures_delta.clear();
        width
    }

    /// 所有可选开关都打开、值为默认长短的基准步骤
    fn optioned_job() -> Job {
        Job {
            steps: vec![Step {
                name: Some("步骤A".into()),
                tool: "java".into(),
                command: "echo hi".into(),
                step_type: StepType::Exec,
                enabled: true,
                timeout: Some(300),
                retry: Some(crate::models::RetryConfig {
                    max_attempts: 3,
                    backoff_seconds: 5,
                }),
                creates: Some("a.jar".into()),
                notify: vec!["通知".into()],
                condition: Some("${{ env.V != '' }}".into()),
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    #[test]
    fn step_card_layout_stable_under_input() {
        let mut def = optioned_job();
        let w_def = card_width(&mut def, 0);

        let mut patho = optioned_job();
        {
            let st = &mut patho.steps[0];
            st.name = Some("一个特别特别特别长的步骤名称用来验证布局稳定性".into());
            st.tool = "一个名字特别特别长的工具名称映射".into();
            st.timeout = Some(99999);
            st.retry = Some(crate::models::RetryConfig {
                max_attempts: 10,
                backoff_seconds: 600,
            });
            st.creates = Some("target/very/long/path/to/artifacts/*.jar".into());
            st.notify = vec!["一个很长的handler通知名字".into()];
            st.condition =
                Some("${{ variables.A_VERY_LONG_VARIABLE_NAME }} == 一个很长的比较值".into());
            st.target = Some("很长的目标名称用来测试下拉框文本截断".into());
        }
        let w_patho = card_width(&mut patho, 0);

        assert!(
            w_def <= 726.0,
            "默认布局已超出 724 面板（有行溢出，需收紧定宽或拆行）: {w_def}"
        );
        assert!(
            w_patho <= w_def + 2.0,
            "病态输入使卡片变宽（布局漂移）: {w_def} -> {w_patho}"
        );
    }

    #[test]
    fn wait_probe_row_layout_stable_under_input() {
        let mut def = optioned_job();
        def.steps[0].step_type = StepType::Wait;
        def.steps[0].wait = Some(crate::models::WaitProbe {
            mode: crate::models::WaitMode::Command,
            target: "1.2.3.4:8080".into(),
            interval_seconds: 5,
            fail_exit_code: Some(2),
        });
        let w_def = card_width(&mut def, 0);

        let mut patho = optioned_job();
        patho.steps[0].step_type = StepType::Wait;
        patho.steps[0].wait = Some(crate::models::WaitProbe {
            mode: crate::models::WaitMode::Command,
            target: "一个特别特别特别长的探测目标地址用来验证布局稳定性:8080".into(),
            interval_seconds: 120,
            fail_exit_code: Some(255),
        });
        patho.steps[0].target = Some("很长的目标名称用来测试下拉框文本截断".into());
        let w_patho = card_width(&mut patho, 0);

        assert!(w_def <= 726.0, "默认 wait 布局超出 724 面板: {w_def}");
        assert!(
            w_patho <= w_def + 2.0,
            "病态输入使 wait 行变宽: {w_def} -> {w_patho}"
        );
    }

    #[test]
    fn upload_mappings_roundtrip() {
        // 旧格式（含注释行/空行）→ 结构化行：空行吸收；注释行保留显示（引擎按注释忽略）
        let rows = upload_rows(
            "target/a.jar -> /opt/app/a.jar\n\n# 备注\ntarget/b.jar -> /opt/app/b.jar",
        );
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0], ("target/a.jar".into(), "/opt/app/a.jar".into()));
        assert_eq!(rows[1], ("# 备注".into(), "".into()));
        assert_eq!(rows[2], ("target/b.jar".into(), "/opt/app/b.jar".into()));

        // 序列化仍是引擎 parse_upload_mappings 的行格式；"# ... ->" 依旧以 # 开头 = 仍是注释
        let cmd = upload_command(&rows);
        assert_eq!(
            cmd,
            "target/a.jar -> /opt/app/a.jar\n# 备注 ->\ntarget/b.jar -> /opt/app/b.jar"
        );

        // 无 "->" 的残留行放进 from 槽等待补全，不丢内容
        let rows = upload_rows("只有一半的行");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, "只有一半的行");

        // 无 "->" 的残留行放进 from 槽等待补全，不丢内容
        let rows = upload_rows("只有一半的行");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, "只有一半的行");

        // 空行序列化为 "->" 占位（保证 "＋ 添加映射" 的空行能稳定往返），
        // 且解析回来还是空行；引擎 parse_upload_mappings 会把空映射当空行跳过
        let cmd = upload_command(&[("".into(), "".into()), ("a".into(), "".into())]);
        assert_eq!(cmd, "->\na ->");
        let back = upload_rows(&cmd);
        assert_eq!(back, vec![("".to_string(), "".to_string()), ("a".to_string(), String::new())]);
    }

    // ── 命令补全（CompletionPopup suggest 回调的纯逻辑）─────────────────────

    /// 手工构造 CompletionQuery：光标在文末、word = 文末最后一词
    /// （真实调用时由弹层按光标与空白边界切分，这里只喂纯逻辑）
    fn query_of<'a>(text: &'a str, word: &'a str) -> egui::CompletionQuery<'a> {
        let cursor = text.chars().count();
        let word_start = cursor - word.chars().count();
        egui::CompletionQuery {
            text,
            word,
            word_range: egui::text::CharIndex(word_start)..egui::text::CharIndex(cursor),
        }
    }

    #[test]
    fn unclosed_var_detection() {
        // 双括号未闭合 → 变量模式；闭合后回到命令模式
        assert_eq!(unclosed_var("cp -a ${{ var"), Some(VarMode::Double));
        assert_eq!(unclosed_var("tail -n 100 app.log"), None);
        assert_eq!(unclosed_var("${{ a }} tail"), None);
        // 双闭合后又开新单括号
        assert_eq!(unclosed_var("${{ a }} ${JAV"), Some(VarMode::Single));
        // 单括号未闭合 / 已闭合
        assert_eq!(unclosed_var("echo ${JAV"), Some(VarMode::Single));
        assert_eq!(unclosed_var("echo ${JAVA_HOME} tail"), None);
        // 无占位符 / 裸 `$` 不算
        assert_eq!(unclosed_var("echo $VAR"), None);
        assert_eq!(unclosed_var(""), None);
        // 中文等多字节字符混排：按字节 rfind 也能找对位置
        assert_eq!(unclosed_var("备份到 /opt/${{ 备"), Some(VarMode::Double));
    }

    #[test]
    fn char_idx_to_byte_multibyte() {
        // ASCII：字符序号 = 字节偏移
        assert_eq!(char_idx_to_byte("tail -1", 7), 7);
        // 中文每字符 3 字节：第 3 个字符（序号 3）起始字节 = 9
        assert_eq!(char_idx_to_byte("备份到x", 3), 9);
        // 序号 = 字符总数 → 文本末尾（光标在文末的常见情况）
        assert_eq!(char_idx_to_byte("备份到x", 4), 10);
        // 空文本 / 序号超界 → 文本末尾
        assert_eq!(char_idx_to_byte("", 0), 0);
        assert_eq!(char_idx_to_byte("ab", 99), 2);
    }

    #[test]
    fn command_mode_suggestions() {
        // 前缀匹配 + 不区分大小写
        let hits = command_suggestions_for_word("tail");
        assert!(hits.iter().any(|s| s.insert.starts_with("tail -n 100 ")));
        let hits = command_suggestions_for_word("TAIL");
        assert!(hits.iter().any(|s| s.insert.starts_with("tail -f ")));
        // 多候选：systemctl 列出 restart / status 等形态
        let hits = command_suggestions_for_word("systemctl");
        assert!(hits.len() >= 4);
        // 空词不弹（每打一个空格弹全表会很吵）；无命中不弹
        assert!(command_suggestions_for_word("").is_empty());
        assert!(command_suggestions_for_word("zzzz").is_empty());
    }

    #[test]
    fn double_brace_suggestions_filter_and_close() {
        // (组名, 键, 值)：流程选了变量组时只传该组的条目
        let vars = vec![("prod".to_string(), "REGISTRY".to_string(), "registry:5000".to_string())];
        let targets = vec!["prod".to_string()];
        let env_keys = vec!["VERSION".to_string()];

        // word = 路径片段 → 命中 variables.*，insert 自动闭合 ` }}`，desc 带现值
        let hits = double_brace_suggestions("var", &vars, true, &targets, &env_keys);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].insert, "variables.REGISTRY }}");

        // 命名空间前缀继续过滤；targets 字段命中 host
        let hits = double_brace_suggestions("variables.REG", &vars, true, &targets, &env_keys);
        assert_eq!(hits.len(), 1);
        let hits = double_brace_suggestions("targets.prod.h", &vars, true, &targets, &env_keys);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].insert, "targets.prod.host }}");

        // 空词（`${{ ` 刚打完）→ 全量：变量 + env + targets 字段 + 内置变量
        let hits = double_brace_suggestions("", &vars, true, &targets, &env_keys);
        assert!(hits.iter().any(|s| s.insert == "variables.REGISTRY }}"));
        assert!(hits.iter().any(|s| s.insert == "env.VERSION }}"));
        assert!(hits.iter().any(|s| s.insert == "now.date }}"));
        assert!(hits.iter().any(|s| s.insert == "run.id }}"));
        assert!(hits.iter().any(|s| s.insert == "targets.prod.host }}"));

        // `${{` 后没打空格：word 带前缀 → insert 把前缀补回去（替换范围含前缀）
        let hits = double_brace_suggestions("${{variables.R", &vars, true, &targets, &env_keys);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].insert, "${{ variables.REGISTRY }}");
    }

    #[test]
    fn double_brace_vars_without_group_selected() {
        // 流程未选变量组：show 层会传入全部组的键，仍然列出（insert 不变），
        // 由 desc 标注组名 + ⚠ 运行时不替换（desc 文本不好断言，这里锁行为）
        let vars = vec![("prod".to_string(), "REGISTRY".to_string(), String::new())];
        let hits = double_brace_suggestions("var", &vars, false, &[], &[]);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].insert, "variables.REGISTRY }}");
    }

    #[test]
    fn arg_hints_after_known_command() {
        // 空格后（word 空）→ 列出该命令全部常用参数（mysqldump 提示不全的补法）
        let hits = command_suggestions(&query_of("mysqldump ", ""), &[], false, &[], &[]);
        assert!(hits.iter().any(|s| s.insert == "--single-transaction "));
        assert!(hits.iter().any(|s| s.insert == "--no-data "));
        // 参数前缀过滤：--s 只剩 --single-transaction / --set-gtid-purged
        let hits = command_suggestions(&query_of("mysqldump --s", "--s"), &[], false, &[], &[]);
        assert!(!hits.is_empty());
        assert!(hits.iter().all(|s| s.insert.starts_with("--s")));
        // 行首词不是已知命令 → 不出参数提示（骨架表也不命中 → 无弹层）
        let hits = command_suggestions(&query_of("echo ", ""), &[], false, &[], &[]);
        assert!(hits.is_empty());
        // 还在打命令头本身 → 只出骨架表候选，不混参数
        let hits = command_suggestions(&query_of("mysq", "mysq"), &[], false, &[], &[]);
        assert!(!hits.is_empty());
        assert!(hits.iter().all(|s| s.insert.starts_with("my")));
        // 多行命令取当前行首词
        assert_eq!(line_head("tail -n 100\nmysqldump "), "mysqldump");
        assert_eq!(line_head("mysqldump -h host"), "mysqldump");
    }

    #[test]
    fn arg_hints_skip_already_used() {
        // Tab 上屏过的参数不再提示（用户反馈：已补全的还列在弹层里很别扭）
        let line = "mysqldump --single-transaction ";
        let hits = command_suggestions(&query_of(line, ""), &[], false, &[], &[]);
        assert!(hits.iter().all(|s| s.insert != "--single-transaction "));
        assert!(hits.iter().any(|s| s.insert == "--no-data "));
        // 参数都上屏完的场景：tar 只用了 -czf → 它被滤掉，未用过的 -xzf/-tzf
        // 仍会提示（规则 = 只滤已用；无法可靠判定「命令已写完」，不做猜测）
        let line = "tar -czf x.tar.gz -C dir . ";
        let hits = command_suggestions(&query_of(line, ""), &[], false, &[], &[]);
        assert!(hits.iter().all(|s| s.insert != "-czf 包.tar.gz -C 目录 . "));
        assert!(hits.iter().any(|s| s.insert.starts_with("-xzf ")));
    }

    #[test]
    fn head_suggestions_only_at_line_start() {
        // 行中输入词不再提示整条骨架：旧行为会把「systemctl re」提示成骨架，
        // 接受后拼成 systemctl systemctl restart；行中改由参数提示接管
        let hits = command_suggestions(&query_of("systemctl re", "re"), &[], false, &[], &[]);
        assert!(hits.is_empty());
        // 行首（含仅空白前缀）照常出骨架表
        let hits = command_suggestions(&query_of("  tail", "tail"), &[], false, &[], &[]);
        assert!(hits.iter().any(|s| s.insert.starts_with("tail -n 100 ")));
    }

    #[test]
    fn single_brace_suggestions_close() {
        let env_keys = vec!["VERSION".to_string()];
        // 步骤产出/凭据注入键 + 常用环境变量都命中，insert 闭合 `}`
        let hits = single_brace_suggestions("${", &env_keys);
        assert!(hits.iter().any(|s| s.insert == "${VERSION}"));
        assert!(hits.iter().any(|s| s.insert == "${JAVA_HOME}"));
        let hits = single_brace_suggestions("${JAV", &env_keys);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].insert, "${JAVA_HOME}");
    }

    #[test]
    fn suggest_dispatch_by_mode() {
        let vars = vec![("prod".to_string(), "REGISTRY".to_string(), String::new())];
        // 命令模式：word 走骨架表（"tail" 不是参数提示命令头，无参数混入）
        let hits = command_suggestions(&query_of("tail", "tail"), &vars, true, &[], &[]);
        assert!(!hits.is_empty());
        // 变量模式（双括号）：word 走变量表
        let hits = command_suggestions(&query_of("cp ${{ var", "var"), &vars, true, &[], &[]);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].insert, "variables.REGISTRY }}");
        // 变量模式（单括号）：word 含 `${` 前缀
        let hits = command_suggestions(&query_of("echo ${JAV", "${JAV"), &vars, true, &[], &[]);
        assert_eq!(hits[0].insert, "${JAVA_HOME}");
    }
}
