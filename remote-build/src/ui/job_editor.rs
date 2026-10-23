//! 中间步骤编辑器（设计文档 §9.1 Job Editor）。

use crate::app::MainState;
use crate::models::{FailureAction, Job, Step, StepType, Tool};
use eframe::egui;
use egui::Color32;
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

const STEP_TYPES: [&str; 5] = [
    "exec（执行命令）",
    "wait（轮询等待）",
    "upload（上传文件）",
    "pack（打包 zip）",
    "k8s-apply（kubectl 应用）",
];

fn step_type_label(st: &StepType) -> &'static str {
    STEP_TYPES[step_type_index(st)]
}

fn step_type_index(st: &StepType) -> usize {
    match st {
        StepType::Exec => 0,
        StepType::Wait => 1,
        StepType::Upload => 2,
        StepType::Pack => 3,
        StepType::K8sApply => 4,
    }
}

fn step_type_from_index(i: usize) -> StepType {
    match i {
        0 => StepType::Exec,
        1 => StepType::Wait,
        2 => StepType::Upload,
        3 => StepType::Pack,
        _ => StepType::K8sApply,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StepAction {
    None,
    Delete,
    MoveUp,
    /// 从该步骤执行到流程结束
    RunFrom,
    /// 只执行该步骤（调试）
    RunOnly,
}

pub fn show(main: &mut MainState, ui: &mut egui::Ui) {
    let Some(pi) = main.selected_project else {
        ui.vertical_centered(|ui| {
            ui.add_space(40.0);
            ui.label("← 从左侧选择一个作业开始");
        });
        return;
    };
    let Some(ji) = main.selected_job else {
        ui.vertical_centered(|ui| {
            ui.add_space(40.0);
            ui.label("← 从左侧选择一个作业开始");
        });
        return;
    };
    if main.projects.get(pi).is_none() || main.projects[pi].jobs.get(ji).is_none() {
        ui.vertical_centered(|ui| ui.label("作业不存在，请点 ⟳ 重新加载"));
        return;
    }

    // 校验提示（§9.4）——先取 owned，避免与下面的可变借用冲突
    let issues = crate::config::validate_project(&main.projects[pi].project);
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

    // 同一 ProjectEntry 的两个字段分别借用（字段级借用拆分）
    let entry = &mut main.projects[pi];
    let project = &mut entry.project;
    let Some(job_entry) = entry.jobs.get_mut(ji) else {
        return;
    };
    let job = &mut job_entry.job;

    // ── 第 1 行：流程名 + 项目目录（占满剩余宽度）──
    ui.horizontal(|ui| {
        ui.heading(format!("📄 {}", job.name));
        ui.separator();
        ui.label("项目目录:");
        let dir_width = (ui.available_width() - 90.0).max(280.0);
        ui.add(
            egui::TextEdit::singleline(&mut project.local_path)
                .desired_width(dir_width)
                .hint_text("D:\\workspace\\my-project"),
        );
        if ui.button("📂 浏览…").clicked() {
            if let Some(dir) = rfd::FileDialog::new()
                .set_title("选择项目本地目录")
                .pick_folder()
            {
                project.local_path = dir.to_string_lossy().to_string();
            }
        }
    });
    let env_names: Vec<String> = project.variables.keys().cloned().collect();
    // 目标下拉 = 本项目 project.yaml 的 targets 表（§3.5 目标随项目走）
    let mut target_names: Vec<String> = project.targets.keys().cloned().collect();
    target_names.sort();
    // 流程默认目标的展示文本（步骤「跟随流程」时显示它）
    let job_target_display = job.target.clone().unwrap_or_else(|| "无".to_string());
    // ── 第 2 行：变量组 / 目标 / 失败策略（悬停可见说明）──
    ui.horizontal(|ui| {
        ui.label("变量组:").on_hover_text(
            "决定 ${{ variables.X }} 从哪张变量表取值（project.yaml 的 variables.<组名>）。\n\
             例：变量组=prod 时 ${{ variables.DEPLOY_DIR }} 取 variables.prod.DEPLOY_DIR，\n\
             同一条流程换变量组即换一套参数（私服地址 / 部署目录等）。\n\
             注意：它不是 Spring profile 那样的运行环境；执行机器由「目标」决定。",
        );
        egui::ComboBox::from_id_source("env_combo")
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
        egui::ComboBox::from_id_source("target_combo")
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
        egui::ComboBox::from_id_source("on_failure_combo")
            .selected_text(on_failure_label)
            .width(110.0)
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut job.on_failure, FailureAction::Stop, "stop");
                ui.selectable_value(&mut job.on_failure, FailureAction::Continue, "continue");
                ui.selectable_value(&mut job.on_failure, FailureAction::Retry, "retry");
            });
    });

    // ── 第 3 行：流程管理按钮 ──
    let mut open_tool_overrides = false;
    let mut create_job_requested = false;
    let mut delete_job_confirmed = false;
    let mut open_var_help = false;
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        if ui.button("🔧 工具覆盖").clicked() {
            open_tool_overrides = true;
        }
        if ui.button("？ 变量语法").clicked() {
            open_var_help = true;
        }
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

    ui.add_space(8.0);
    ui.separator();
    ui.add_space(4.0);

    // ── 步骤列表 ─────────────────────────────────────────────────────
    let mut actions: Vec<StepAction> = Vec::new();
    egui::ScrollArea::vertical()
        .id_source("steps_scroll")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for si in 0..job.steps.len() {
                actions.push(edit_step(ui, job, si, &tool_menu, running, &target_names, job_target_display.as_str()));
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

    // 从后往前应用删除/上移，避免索引失效
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
    if open_var_help {
        main.show_var_help = true;
    }
    if create_job_requested {
        main.new_job.open = true;
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
    pub message: Option<(bool, String)>,
}

pub fn show_new_job_window(ctx: &egui::Context, main: &mut MainState) {
    let mut open = main.new_job.open;
    if open {
        crate::ui::modal_backdrop(ctx, "mask_new_job");
    }
    egui::Window::new("＋ 新建流程")
        .open(&mut open)
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
    crate::ui::raise_above_backdrop(ctx, "＋ 新建流程");
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
        main.new_job.message =
            Some((false, format!("文件已存在: {}", path.display())));
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
        on_failure: FailureAction::Stop,
        steps,
        working_dir: None,
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

/// 「？ 变量语法」帮助窗口（速查表，与 docs/变量速查.md 内容一致）
pub fn show_var_help_window(ctx: &egui::Context, main: &mut MainState) {
    let mut open = main.show_var_help;
    if open {
        crate::ui::modal_backdrop(ctx, "mask_var_help");
    }
    egui::Window::new("？ 变量语法速查")
        .open(&mut open)
        .default_width(720.0)
        .default_height(520.0)
        .show(ctx, |ui| {
            ui.weak("可用于步骤命令、步骤 env、工作目录、条件字段。执行前一次性替换；未解析的占位符原样保留。");
            ui.add_space(6.0);

            ui.strong("一、四种取值形态");
            egui::Grid::new("var_help_grid")
                .num_columns(3)
                .spacing([12.0, 4.0])
                .show(ui, |ui| {
                    ui.strong("写法");
                    ui.strong("来源");
                    ui.strong("示例");
                    ui.end_row();

                    let rows: Vec<(String, &str, &str)> = vec![
                        (
                            "${{ variables.X }}".into(),
                            "project.yaml 的 variables.<组名>（「变量组」下拉选中的表）",
                            "选 prod 组时 ${{ variables.DEPLOY_DIR }} → /opt/yudao/prod",
                        ),
                        (
                            "${{ env.X }}".into(),
                            "前序步骤产出（往 workspace/.env 写 KEY=VALUE，步骤成功后自动捕获）",
                            "docker build -t app:${{ env.VERSION }}",
                        ),
                        (
                            "${{ targets.名.字段 }}".into(),
                            "targets.yaml 目标字段（host/port/user/kubeconfig/context/namespace/registry/work_root）",
                            "${{ targets.prod.host }} → 192.168.40.198",
                        ),
                        (
                            "${VAR}".into(),
                            "步骤产出 env → 环境变量表 → 系统环境变量（依次查找）",
                            "${JAVA_HOME}",
                        ),
                    ];
                    for (syn, src, ex) in rows {
                        ui.monospace(syn);
                        ui.label(src);
                        ui.monospace(ex);
                        ui.end_row();
                    }
                });

            ui.add_space(8.0);
            ui.separator();
            ui.add_space(4.0);
            ui.strong("二、条件表达式（步骤的「＋条件」字段）");
            ui.monospace("${{ env.VERSION != '' }}      # 非空才执行本步；为空/未产出 → 跳过");
            ui.monospace("${{ variables.ENABLE == '' }}  # 反向判断");
            ui.weak("前缀支持 env. / variables. / targets.名.字段；P0 仅支持非空检查（!= '' / == ''）；解析失败按真值执行并警告。");

            ui.add_space(8.0);
            ui.separator();
            ui.add_space(4.0);
            ui.strong("三、env 注入优先级（后者被前者覆盖）");
            ui.monospace("步骤 env > 工具 env > target env > 系统环境变量");
            ui.weak("JAVA_HOME 这类注入只影响该步骤的子进程，不改系统设置；env.X 特指前序步骤产出，全局配置请走 variables.X。");

            ui.add_space(8.0);
            ui.weak("完整说明见 docs/变量速查.md（设计文档 §6）。");
        });
    crate::ui::raise_above_backdrop(ctx, "？ 变量语法速查");
    main.show_var_help = open;
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
fn edit_step(
    ui: &mut egui::Ui,
    job: &mut Job,
    si: usize,
    tool_menu: &ToolMenu,
    running: bool,
    target_names: &[String],
    job_target_display: &str,
) -> StepAction {
    let mut action = StepAction::None;

    ui.group(|ui| {
        let step = &mut job.steps[si];
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
            ui.label("工具:");
            egui::ComboBox::from_id_source(format!("tool_{si}"))
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
            // 来源徽标 + 悬停显示解析到的实际 command
            if !step.tool.is_empty() {
                match tool_menu.merged.get(&step.tool) {
                    Some(tool) => {
                        let from_project = tool_menu.project.iter().any(|n| n == &step.tool);
                        let (badge, color) = if from_project {
                            ("〔项目〕", Color32::from_rgb(230, 180, 80))
                        } else {
                            ("〔全局〕", Color32::GRAY)
                        };
                        ui.colored_label(color, badge).on_hover_text(format!(
                            "解析到: {}{}",
                            tool.command,
                            if from_project { "\n（项目级覆盖）" } else { "" }
                        ));
                    }
                    None => {
                        ui.colored_label(Color32::from_rgb(220, 120, 90), "（未映射）")
                            .on_hover_text("工具清单里没有这个名字；可在 ⚙ 设置或「🔧 工具覆盖」中添加");
                    }
                }
            }

            ui.separator();
            let old_type = step.step_type;
            let mut st_idx = step_type_index(&step.step_type);
            egui::ComboBox::from_id_source(format!("st_{si}"))
                .selected_text(step_type_label(&step.step_type))
                .width(150.0)
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

            ui.separator();
            match &mut step.timeout {
                Some(v) => {
                    ui.label("超时:");
                    ui.add(egui::DragValue::new(v).suffix("s").speed(1.0));
                    if ui.small_button("×").clicked() {
                        step.timeout = None;
                    }
                }
                None => {
                    ui.weak("超时:300s");
                    if ui.small_button("⏱").clicked() {
                        step.timeout = Some(300);
                    }
                }
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
            if ui.small_button("↑").clicked() && si > 0 {
                action = StepAction::MoveUp;
            }
            if ui.small_button("🗑").clicked() {
                action = StepAction::Delete;
            }
        });

        ui.add_space(2.0);
        if matches!(step.step_type, StepType::Upload) {
            edit_upload_mappings(ui, step);
        } else {
            ui.add(
                egui::TextEdit::multiline(&mut step.command)
                    .desired_rows(2)
                    .desired_width(f32::INFINITY)
                    .font(egui::TextStyle::Monospace)
                    .hint_text(match step.step_type {
                        StepType::Pack => "目录或通配 -> 目标.zip，如 target/ -> dist/app.zip",
                        _ => "命令，支持 ${{ variables.X }} / ${{ env.X }} / ${VAR}",
                    }),
            );
        }

        ui.horizontal(|ui| {
            // 步骤级目标（§3.3）：覆盖流程默认——混合流程的关键
            ui.label("目标:");
            let sel_text = match &step.target {
                Some(t) => t.clone(),
                None => format!("跟随流程（{job_target_display}）"),
            };
            egui::ComboBox::from_id_source(format!("steptarget_{si}"))
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
                            .clamp_range(1..=10),
                    );
                    ui.label("间隔");
                    ui.add(
                        egui::DragValue::new(&mut r.backoff_seconds)
                            .suffix("s")
                            .clamp_range(0..=300),
                    );
                    if ui.small_button("×").clicked() {
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
            if let Some(cond) = &mut step.condition {
                ui.label("条件:");
                ui.add(egui::TextEdit::singleline(cond).desired_width(220.0));
                if ui.small_button("×").clicked() {
                    step.condition = None;
                }
            } else if ui.small_button("＋条件").clicked() {
                step.condition = Some("${{ env.VERSION != '' }}".to_string());
            }
        });
    });

    action
}

// ---------------------------------------------------------------------------
// upload 映射的结构化编辑：from/to 输入框 + 加/减行。
// 落盘仍是引擎 parse_upload_mappings 的行格式（每行 `<本地> -> <远端>`），
// 旧 YAML / 手写配置零迁移。
// ---------------------------------------------------------------------------

/// upload 命令 → 编辑行。与引擎同一格式：按行 split 首个 "->" 并 trim；
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

/// 编辑行 → upload 命令。空行序列化为 "->" 占位：能原样解析回空行（否则
/// "＋ 添加映射" 的空行会在写回时被过滤掉、看起来没反应），引擎把空映射当空行跳过。
fn upload_command(rows: &[(String, String)]) -> String {
    rows.iter()
        .map(|(l, r)| format!("{} -> {}", l.trim(), r.trim()).trim().to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

fn edit_upload_mappings(ui: &mut egui::Ui, step: &mut Step) {
    let mut rows = upload_rows(&step.command);

    let mut del: Option<usize> = None;
    for (ri, (l, r)) in rows.iter_mut().enumerate() {
        ui.horizontal(|ui| {
            if ui
                .small_button("−")
                .on_hover_text("删除此映射")
                .clicked()
            {
                del = Some(ri);
            }
            let from_w = ((ui.available_width() - 150.0) * 0.5).max(140.0);
            ui.add(
                egui::TextEdit::singleline(l)
                    .desired_width(from_w)
                    .hint_text("本地路径：相对项目目录或绝对路径；具体文件或目录（不支持通配）"),
            );
            ui.label("→");
            // to 框吃掉剩余全部宽度：行总宽与 exec 文本框一致，切换类型卡片不跳宽
            ui.add(
                egui::TextEdit::singleline(r)
                    .desired_width(ui.available_width())
                    .hint_text("远端路径：/ 开头为绝对路径；相对 = 目标的 work_root"),
            );
        });
    }
    if let Some(ri) = del {
        rows.remove(ri);
    }
    if ui
        .small_button("＋ 添加映射")
        .on_hover_text("再添加一条 本地 → 远端 映射（微服务多 jar 场景）")
        .clicked()
    {
        rows.push((String::new(), String::new()));
    }
    step.command = upload_command(&rows);
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
