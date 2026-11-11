//! 左侧项目/作业树（设计文档 §9.1）。
//!
//! 右键菜单：项目 = 打开项目目录 / 添加分组 / 改名 / 删除；分组 = 添加任务 /
//! 删除（组成员一并删，有确认提示）；流程 = 运行 / 更新代码 / 移动到分组 /
//! 改名 / 删除。
//! 分组与流程支持拖拽排序（顺序写入 project.yaml 的 groups / jobs 列表）。
//! 删除走确认弹窗（避免误删），改名走输入弹窗；两者都带遮罩。

use crate::app::MainState;
use eframe::egui;
use egui::Color32;
#[cfg(windows)]
use std::os::windows::process::CommandExt;

/// 右键菜单待确认的删除目标（下标为 main.projects 的索引；分组用名字定位）
#[derive(Debug, Clone, PartialEq)]
pub enum TreeDeleteTarget {
    Project(usize),
    Job(usize, usize),
    Group(usize, String),
    /// 解散"项目"归类层（成员工程全部移出分类，工程本身不删）
    Category(String),
}

/// 归类操作目标：工程移入某"项目"分类 / 分类重命名（两者共用一个输入弹窗）
#[derive(Debug, Clone, PartialEq)]
pub enum TreeMoveTarget {
    /// 工程移入项目（下标为 main.projects 的索引）
    IntoProject(usize),
    /// "项目"分类重命名（成员批量改 category 并挪目录）
    RenameCategory(String),
}

/// 项目树拖拽排序载荷（分组名 / 流程名，项目内唯一）
#[derive(Clone)]
enum TreeDrag {
    Group(String),
    Job(String),
}

/// 右键菜单的改名目标（分组用名字定位：列表里的名字在 reload 后仍稳定）。
/// GroupNew = 添加分组流程（占位名已写入 groups 列表，弹窗给它起名）——
/// 与「改名」区分开：标题 / 按钮文案 / 成功提示都不同（此前添加分组弹的窗口
/// 标题就是「改名」，用户以为点错了菜单）
#[derive(Debug, Clone, PartialEq)]
pub enum TreeRenameTarget {
    Project(usize),
    Job(usize, usize),
    Group(usize, String),
    /// (项目下标, 占位组名)：占位组已在 project.yaml groups 列表里，弹窗确认后改成正式名
    GroupNew(usize, String),
}

pub fn show(main: &mut MainState, ui: &mut egui::Ui) {
    ui.horizontal(|ui| {
        ui.heading("项目");
        // 运行中禁用（统一"执行时所有输入禁用"；引擎正按路径读写工作区）
        if ui
            .add_enabled(!main.run_active(), egui::Button::new("＋ 新建工程").small())
            .clicked()
        {
            crate::ui::new_project::open_new(main);
        }
    });
    ui.add_space(4.0);

    if main.projects.is_empty() {
        ui.colored_label(
            Color32::GRAY,
            format!(
                "未发现工程。\n点「＋ 新建工程」从 Git 克隆或选择本地目录；\n或在 {} 放置 <工程>/project.yaml（可按 <项目>/<工程>/ 两级归类）后点 ⟳",
                main.projects_root.display()
            ),
        );
        return;
    }

    egui::ScrollArea::vertical().show(ui, |ui| {
        // 按归类链（"/" 路径段）构建嵌套树：目录多深树就嵌多深，不限层级。
        // 分类节点只是展示容器，工程仍是 main.projects 全局下标 pi，选中/运行不受影响
        let mut tree = CatTree::default();
        for (pi, entry) in main.projects.iter().enumerate() {
            let mut node = &mut tree;
            if let Some(cat) = entry.project.category.as_deref() {
                for seg in cat.split('/').map(str::trim).filter(|s| !s.is_empty()) {
                    node = node.child(seg);
                }
            }
            node.projects.push(pi);
        }
        for (seg, sub) in &tree.children {
            render_category_node(main, ui, seg, sub, "");
        }
        // 无分类工程平铺在分类之后
        for &pi in &tree.projects {
            render_project(main, ui, pi);
        }
    });

    confirm_delete_window(ui.ctx(), main);
    rename_window(ui.ctx(), main);
    category_window(ui.ctx(), main);
}

/// 归类嵌套树（渲染用）：children 保持插入序（= projects 目录序），
/// projects 存 main.projects 全局下标。树在渲染前一次性构建，不借 main
#[derive(Default)]
struct CatTree {
    children: Vec<(String, CatTree)>,
    /// 直接挂在本分类下的工程（main.projects 全局下标）
    projects: Vec<usize>,
}

impl CatTree {
    fn child(&mut self, name: &str) -> &mut CatTree {
        if let Some(i) = self.children.iter().position(|(n, _)| n == name) {
            &mut self.children[i].1
        } else {
            self.children.push((name.to_string(), CatTree::default()));
            let last = self.children.len() - 1;
            &mut self.children[last].1
        }
    }

    /// 子树工程总数（含本节点直接成员）——分类头右侧的数量
    fn total_projects(&self) -> usize {
        self.projects.len()
            + self
                .children
                .iter()
                .map(|(_, t)| t.total_projects())
                .sum::<usize>()
    }
}

/// 分类节点：可再嵌分类或挂工程。prefix = 本节点的完整父链（右键定位用）
fn render_category_node(
    main: &mut MainState,
    ui: &mut egui::Ui,
    seg: &str,
    node: &CatTree,
    prefix: &str,
) {
    let chain = if prefix.is_empty() {
        seg.to_string()
    } else {
        format!("{prefix}/{seg}")
    };
    let shown = crate::ui::truncate_label(seg, 14);
    // 项目级运行实时进度（▶ N/总数）：与分组头同款式，键 = 归类链
    let run_prog = main
        .category_run_progress
        .as_ref()
        .filter(|(c, _, _)| c == &chain)
        .map(|(_, i, t)| format!(" ▶ {i}/{t}"));
    let header_text = match &run_prog {
        Some(p) => format!("🗂 {}（{}）{p}", shown, node.total_projects()),
        None => format!("🗂 {}（{}）", shown, node.total_projects()),
    };
    let body = egui::CollapsingHeader::new(header_text)
        .icon(crate::ui::chevron_icon)
        // 不同层级可以有同名段（a/x 与 b/x），标签文本相同会撞 egui id，用完整链区分
        .id_salt(format!("cat-node:{chain}"))
        .default_open(true)
        .show(ui, |ui| {
            for (seg, sub) in &node.children {
                render_category_node(main, ui, seg, sub, &chain);
            }
            for &pi in &node.projects {
                render_project(main, ui, pi);
            }
        });
    if shown != seg {
        body.header_response
            .clone()
            .on_hover_text(format!("项目（归类）：{chain}"));
    }
    // 分类右键：项目级运行（跨工程串行流水线）/ 重命名 / 解散
    body.header_response.context_menu(|ui| {
        let can_run = !main.run_active() && node.total_projects() > 0;
        if ui
            .add_enabled(can_run, egui::Button::new("▶ 运行项目（全部流程）"))
            .on_hover_text(
                "把项目下所有工程的流程按树中顺序串成一条流水线：\n\
                 任一流程失败即停（金丝雀语义）；每个流程用自己工程的目标/变量/凭据",
            )
            .clicked()
        {
            main.start_category_run(&chain, None, None);
            ui.close();
        }
        // 同名分组：各工程里都有的分组名（如所有工程的「发布」）→ 只跑这条线
        let mut group_names: Vec<String> = {
            let pis = category_members(main, &chain);
            let mut v: Vec<String> = Vec::new();
            for pi in pis {
                if let Some(e) = main.projects.get(pi) {
                    for je in &e.jobs {
                        if let Some(g) = &je.job.group {
                            if !v.iter().any(|x| x == g) {
                                v.push(g.clone());
                            }
                        }
                    }
                }
            }
            v
        };
        group_names.sort();
        if !group_names.is_empty() {
            ui.menu_button("▶ 运行同名分组", |ui| {
                for g in &group_names {
                    if ui.button(g).clicked() {
                        main.start_category_run(&chain, Some(g), None);
                        ui.close();
                    }
                }
            });
        }
        // ↪ 从失败处继续：上次项目级运行在本项目未通过 → 从该流程继续
        let resume = main
            .last_category_failure
            .as_ref()
            .filter(|r| r.chain == chain)
            .cloned();
        let mut resume_btn = ui.add_enabled(resume.is_some(), egui::Button::new("↪ 从失败处继续"));
        if let Some(r) = &resume {
            resume_btn = resume_btn.on_hover_text(format!(
                "上次项目运行在「{}/{}」未通过，从它继续（跳过之前的流程）",
                r.project, r.job
            ));
        }
        if resume_btn.clicked() {
            if let Some(r) = &resume {
                let jobs = build_category_pipeline(&main.projects, &chain, None);
                if let Some(idx) = jobs
                    .iter()
                    .position(|gj| gj.project.name == r.project && gj.name == r.job)
                {
                    main.start_category_run(&chain, None, Some(idx));
                }
            }
            ui.close();
        }
        ui.separator();
        if ui.button("✏ 重命名项目").clicked() {
            main.tree_move_buf = seg.to_string();
            main.tree_move = Some(TreeMoveTarget::RenameCategory(chain.clone()));
            ui.close();
        }
        if ui.button("🗑 解散项目").clicked() {
            main.tree_delete = Some(TreeDeleteTarget::Category(chain.clone()));
            ui.close();
        }
    });
}

/// 单个工程节点（分类内 / 平铺共用）：折叠头 + 分组/流程 + 右键菜单。
/// pi 是 main.projects 的全局下标——分类只是渲染容器，不改任何索引。
fn render_project(main: &mut MainState, ui: &mut egui::Ui, pi: usize) {
    {
        // 预取需要的不可变数据并立刻结束借用：右键菜单闭包要 &mut main
        let (header, error_msgs, hover) = {
            let entry = &main.projects[pi];
            // 校验结果读缓存：每帧 stat 目录 + .git 在网络盘/死路径上会卡住整个 UI
            let issues = main
                .validation
                .get(&entry.project.name)
                .cloned()
                .unwrap_or_default();
            let msgs: Vec<String> = issues
                .iter()
                .filter_map(|i| match i {
                    crate::models::ValidationIssue::Error { message, .. } => Some(message.clone()),
                    _ => None,
                })
                .collect();
            // Git 仓库 / 本地目录模式的项目头标记（⎇ = git 仓库），
            // 悬停显示代码目录与配置目录——两套目录分离时一眼可辨
            let is_git = !entry.project.local_path.trim().is_empty()
                && std::path::Path::new(&entry.project.local_path)
                    .join(".git")
                    .exists();
            let name_shown = crate::ui::truncate_label(&entry.project.name, 14);
            let badge = if is_git { " ⎇" } else { "" };
            // 当前分支（reload 时缓存，git 项目才显示该行）
            let branch_line = main
                .git_branches
                .get(&entry.project.name)
                .and_then(|b| b.as_ref())
                .map(|b| format!("\n当前分支：{b}"))
                .unwrap_or_default();
            let hover = format!(
                "{}{}{branch_line}\n配置目录：{}\n代码目录：{}",
                if is_git {
                    "⎇ Git 仓库"
                } else {
                    "本地目录"
                },
                if entry.project.local_path.trim().is_empty() {
                    "（未配置代码目录）".to_string()
                } else {
                    String::new()
                },
                entry.dir.display(),
                entry.project.local_path.trim(),
            );
            (
                format!(
                    "📁 {name_shown}{badge}{}",
                    if !msgs.is_empty() { " ⚠" } else { "" }
                ),
                msgs,
                hover,
            )
        };
        let body = egui::CollapsingHeader::new(header)
            .icon(crate::ui::chevron_icon)
            .default_open(true)
            .show(ui, |ui| {
                // 分组清单 = project.groups 列表 ∪ 流程实际引用的组名
                //（手写 YAML 引用了列表外的组也能显示；列表保证空组持久存在）
                let group_names: Vec<String> = {
                    let entry = &main.projects[pi];
                    let mut v = entry.project.groups.clone();
                    for je in &entry.jobs {
                        if let Some(g) = &je.job.group {
                            if !v.iter().any(|x| x == g) {
                                v.push(g.clone());
                            }
                        }
                    }
                    v
                };

                for gname in &group_names {
                    let member_idx: Vec<usize> = main.projects[pi]
                        .jobs
                        .iter()
                        .enumerate()
                        .filter(|(_, je)| je.job.group.as_deref() == Some(gname.as_str()))
                        .map(|(i, _)| i)
                        .collect();
                    // 组运行实时进度（GroupJobBegin 推进 / JobEnd 清除）：
                    // 运行中的分组头显示 ▶ N/总数（1 起），一眼看到跑到第几个
                    let run_prog = main
                        .group_run_progress
                        .as_ref()
                        .filter(|(g, _, _)| g == gname)
                        .map(|(_, i, t)| format!(" ▶ {i}/{t}"));
                    // 长分组名截断显示（悬停看全名）：名字太长会把折叠头撑宽、
                    // 左栏随之变宽，右侧编辑器的可视宽度被挤坏；
                    // 运行中要给进度 ▶ N/t 腾位，截断预算收紧
                    let shown_gname =
                        crate::ui::truncate_label(gname, if run_prog.is_some() { 10 } else { 14 });
                    let group_body = egui::CollapsingHeader::new(format!(
                        "📂 {}{}{}",
                        shown_gname,
                        if member_idx.is_empty() {
                            String::new()
                        } else {
                            format!("（{}）", member_idx.len())
                        },
                        run_prog.unwrap_or_default(),
                    ))
                    .icon(crate::ui::chevron_icon)
                    .default_open(true)
                    .show(ui, |ui| {
                        for &ji in &member_idx {
                            job_row(main, ui, pi, ji, Some(gname));
                        }
                        if member_idx.is_empty() {
                            ui.weak("（空分组：右键 → ＋ 新建流程）");
                        }
                    });
                    if shown_gname != *gname {
                        group_body
                            .header_response
                            .clone()
                            .on_hover_text(format!("分组：{gname}"));
                    }
                    // 分组右键：组运行 / 新建流程 / 改名 / 删除
                    group_body.header_response.context_menu(|ui| {
                        // ▶ 运行组：组内流程按树中顺序串行，任一失败即停
                        if ui
                            .add_enabled(!member_idx.is_empty(), egui::Button::new("▶ 运行组"))
                            .on_hover_text(
                                "组内流程按树中顺序串行执行，任一失败即停（金丝雀分批语义）；\n\
                                     确认词取组内最严（一次输入全部确认词）",
                            )
                            .clicked()
                        {
                            main.selected_project = Some(pi);
                            main.start_group_run(pi, gname, None);
                            ui.close();
                        }
                        // ↪ 续跑：上次组运行在本组未通过 → 从该流程继续
                        let resume = main
                            .last_group_failure
                            .as_ref()
                            .filter(|r| {
                                r.project == main.projects[pi].project.name && r.group == *gname
                            })
                            .cloned();
                        let mut resume_btn =
                            ui.add_enabled(resume.is_some(), egui::Button::new("↪ 从失败处继续"));
                        if let Some(r) = &resume {
                            resume_btn = resume_btn.on_hover_text(format!(
                                "上次组运行在「{}」未通过，从它继续（跳过之前的流程）",
                                r.job
                            ));
                        } else {
                            resume_btn = resume_btn
                                .on_hover_text("上次组运行在本组全部通过（或没有失败记录）");
                        }
                        if resume_btn.clicked() {
                            if let Some(r) = &resume {
                                // 组内顺序下标：member_idx 里找该流程的位置
                                if let Some(member_pos) = member_idx
                                    .iter()
                                    .position(|&ji| main.projects[pi].jobs[ji].job.name == r.job)
                                {
                                    main.start_group_run(pi, gname, Some(member_pos));
                                }
                            }
                            ui.close();
                        }
                        ui.separator();
                        if ui
                            .add_enabled(!main.run_active(), egui::Button::new("＋ 新建流程"))
                            .on_hover_text("在本分组下新建流程（可复制当前流程步骤）")
                            .clicked()
                        {
                            main.selected_project = Some(pi);
                            main.new_job.open = true;
                            main.new_job.preset_group = Some(gname.clone());
                            ui.close();
                        }
                        if ui.button("✏ 改名").clicked() {
                            main.tree_rename_buf = gname.clone();
                            main.tree_rename = Some(TreeRenameTarget::Group(pi, gname.clone()));
                            ui.close();
                        }
                        if ui.button("🗑 删除分组").clicked() {
                            main.tree_delete = Some(TreeDeleteTarget::Group(pi, gname.clone()));
                            ui.close();
                        }
                    });
                    // 分组拖拽排序：头既是拖拽源也是放置目标
                    dnd_group_header(main, ui, pi, gname, &group_body.header_response);
                }

                // 未分组流程（group 为空）排在分组之后
                for ji in 0..main.projects[pi].jobs.len() {
                    let ungrouped = main.projects[pi].jobs[ji]
                        .job
                        .group
                        .as_deref()
                        .map(|g| !group_names.iter().any(|x| x == g))
                        .unwrap_or(true);
                    if ungrouped {
                        job_row(main, ui, pi, ji, None);
                    }
                }
                if main.projects[pi].jobs.is_empty() && group_names.is_empty() {
                    ui.weak("（无流程：右键工程 → 新建，或到分组里添加）");
                }
            });

        // 项目头悬停：git 仓库 / 本地目录 + 代码目录与配置目录（#23 提示）
        body.header_response.clone().on_hover_text(&hover);
        // 折叠头下方显示阻断性错误（§9.4 目录不存在提示）
        if body.body_response.is_some() || !error_msgs.is_empty() {
            for message in &error_msgs {
                ui.colored_label(Color32::from_rgb(220, 120, 90), format!("⚠ {message}"));
            }
        }

        // 项目头右键菜单：打开目录 / 添加分组 / 改名 / 删除
        body.header_response.context_menu(|ui| {
            let dir = main.projects[pi].dir.display().to_string();
            if ui.button("📂 打开项目目录").clicked() {
                if let Err(e) = open_in_explorer(std::path::Path::new(&dir)) {
                    main.status_msg = Some(e);
                }
                ui.close();
            }
            // 本地目录模式：源码在别处，给个直达入口（与项目目录相同时不重复显示）
            let local = main.projects[pi].project.local_path.trim().to_string();
            if !local.is_empty() && local != dir && ui.button("📂 打开源码目录").clicked() {
                if let Err(e) = open_in_explorer(std::path::Path::new(&local)) {
                    main.status_msg = Some(e);
                }
                ui.close();
            }
            // 归类层管理：移入"项目"分类（分类目录 + category 字段）/ 移出
            if ui.button("🗂 移入项目…").clicked() {
                main.tree_move_buf = main.projects[pi]
                    .project
                    .category
                    .clone()
                    .unwrap_or_default();
                main.tree_move = Some(TreeMoveTarget::IntoProject(pi));
                ui.close();
            }
            if main.projects[pi].project.category.is_some() && ui.button("🗂 移出项目").clicked()
            {
                move_project_category(main, pi, None);
                ui.close();
            }
            if ui.button("📂 添加分组").clicked() {
                add_group(main, pi);
                ui.close();
            }
            if ui.button("✏ 改名").clicked() {
                main.tree_rename_buf = main.projects[pi].project.name.clone();
                main.tree_rename = Some(TreeRenameTarget::Project(pi));
                ui.close();
            }
            if ui.button("🗑 删除").clicked() {
                main.tree_delete = Some(TreeDeleteTarget::Project(pi));
                ui.close();
            }
        });
    }
}

/// 流程行（分组内外共用）：选中/运行状态圆点 + 右键菜单
/// （运行 / 更新代码 / 移动到分组 / 改名 / 删除）+ 同层拖拽排序。
/// `layer` = 所在分组名（None = 未分组区），拖拽只做排序不跨组移动
/// （跨组用「移动到分组」菜单）。
fn job_row(main: &mut MainState, ui: &mut egui::Ui, pi: usize, ji: usize, layer: Option<&str>) {
    let (label, selected, dot_state) = {
        let je = &main.projects[pi].jobs[ji];
        (
            je.job.name.clone(),
            main.selected_project == Some(pi) && main.selected_job == Some(ji),
            main.job_dot_state(&main.projects[pi].project.name, &je.job.name),
        )
    };
    // 📄 强调"一个流程一个 yaml 文件"，与运行按钮的 ▶ 区分开；
    // 运行中的作业在名字后面带状态圆点（蓝闪/黄闪/绿/红，见 ui::status）。
    // 长流程名截断显示（悬停看全名），防止左栏被撑宽挤坏右侧布局
    let shown = crate::ui::truncate_label(&label, 16);
    let inner = ui.horizontal(|ui| {
        let mut r = ui.selectable_label(selected, format!("📄 {shown}"));
        if shown != label {
            r = r.on_hover_text(&label);
        }
        if let Some(st) = dot_state {
            let dark = !matches!(main.global.theme, crate::models::Theme::Light);
            ui.label(crate::ui::status::status_dot(ui, st, dark))
                .on_hover_text(st.label());
        }
        r
    });
    let resp = inner.inner;
    if resp.clicked() {
        main.selected_project = Some(pi);
        main.selected_job = Some(ji);
        main.delete_job_armed = false; // 切换流程时解除删除确认
    }
    // 拖拽排序：本行作为拖拽源 + 同层放置目标（蓝线指示插入位置）
    if resp.drag_started() {
        egui::DragAndDrop::set_payload(ui.ctx(), TreeDrag::Job(label.clone()));
    }
    dnd_job_row(main, ui, pi, &label, layer, &resp);
    resp.context_menu(|ui| {
        if ui
            .add_enabled(!main.run_active(), egui::Button::new("▶ 运行"))
            .on_hover_text("运行此流程（运行中不可触发）")
            .clicked()
        {
            main.selected_project = Some(pi);
            main.selected_job = Some(ji);
            main.start_run(0, None);
            ui.close();
        }
        // 本地目录不是 git 仓库（无 .git）= 单机项目，没有可更新的远端，不显示本项
        let is_git_repo = std::path::Path::new(&main.projects[pi].project.local_path)
            .join(".git")
            .exists();
        if is_git_repo
            && ui
                .add_enabled(
                    !main.run_active() && main.pull_done.is_none(),
                    egui::Button::new("⟳ 更新代码"),
                )
                .on_hover_text(
                    "对项目代码目录执行 git pull --ff-only\n\
                     （远端分叉 / 本地冲突会报错，不覆盖本地修改）",
                )
                .clicked()
        {
            main.pull_project_code(pi);
            ui.close();
        }
        // 移动到分组：列出各分组 + 不分组（有分组或本身已分组才有意义）
        let has_groups = !main.projects[pi].project.groups.is_empty()
            || main.projects[pi]
                .jobs
                .iter()
                .any(|je| je.job.group.is_some());
        if has_groups {
            ui.menu_button("📂 移动到分组", |ui| {
                let cur = main.projects[pi].jobs[ji].job.group.clone();
                if ui.selectable_label(cur.is_none(), "（不分组）").clicked() {
                    move_job_to_group(main, pi, ji, None);
                    ui.close();
                }
                for g in main.projects[pi].project.groups.clone() {
                    if ui
                        .selectable_label(cur.as_deref() == Some(g.as_str()), &g)
                        .clicked()
                    {
                        move_job_to_group(main, pi, ji, Some(g));
                        ui.close();
                    }
                }
            });
        }
        if ui.button("✏ 改名").clicked() {
            main.tree_rename_buf = label.clone();
            main.tree_rename = Some(TreeRenameTarget::Job(pi, ji));
            ui.close();
        }
        if ui.button("🗑 删除").clicked() {
            main.tree_delete = Some(TreeDeleteTarget::Job(pi, ji));
            ui.close();
        }
    });
}

// ---------------------------------------------------------------------------
// 确认删除弹窗
// ---------------------------------------------------------------------------

fn confirm_delete_window(ctx: &egui::Context, main: &mut MainState) {
    let Some(target) = main.tree_delete.clone() else {
        return;
    };
    crate::ui::modal_backdrop(ctx, "mask_tree_delete");

    let mut open = true;
    crate::ui::centered_window(egui::Window::new("⚠ 确认删除"), ctx)
        .open(&mut open)
        .order(egui::Order::Foreground)
        .collapsible(false)
        .resizable(false)
        .default_width(480.0)
        .show(ctx, |ui| {
            match &target {
                TreeDeleteTarget::Project(pi) => {
                    let Some(entry) = main.projects.get(*pi) else { return; };
                    ui.colored_label(
                        Color32::from_rgb(230, 120, 90),
                        format!("删除项目「{}」的配置？", entry.project.name),
                    );
                    ui.weak(format!("项目目录：{}", entry.dir.display()));
                    ui.weak(
                        "只删除其中的配置文件（project.yaml、各流程 yaml）；目录本身与其余文件（代码、构建产物）全部保留。",
                    );
                    let local = entry.project.local_path.trim();
                    if !local.is_empty() && local != entry.dir.display().to_string() {
                        ui.weak(format!("源码目录（不受影响）：{local}"));
                    }
                }
                TreeDeleteTarget::Job(pi, ji) => {
                    let Some(je) = main.projects.get(*pi).and_then(|p| p.jobs.get(*ji)) else {
                        return;
                    };
                    ui.colored_label(
                        Color32::from_rgb(230, 120, 90),
                        format!("删除流程「{}」？", je.job.name),
                    );
                    ui.weak(format!("将删除文件：{}", je.path.display()));
                }
                TreeDeleteTarget::Category(cat) => {
                    // 与解散逻辑同口径：本链 + 子级链（链/" 开头）的工程都算成员
                    let prefix = format!("{cat}/");
                    let members: Vec<&str> = main
                        .projects
                        .iter()
                        .filter(|e| {
                            e.project
                                .category
                                .as_deref()
                                .map(|c| c == cat.as_str() || c.starts_with(&prefix))
                                .unwrap_or(false)
                        })
                        .map(|e| e.project.name.as_str())
                        .collect();
                    ui.colored_label(
                        Color32::from_rgb(230, 120, 90),
                        format!("解散项目「{cat}」？"),
                    );
                    if members.is_empty() {
                        ui.weak("分类为空：仅删除分类文件夹。");
                    } else {
                        ui.weak(format!(
                            "{} 个成员工程将【升到上一级】（顶层分类则平铺到 projects 根；工程与流程全部保留）：{}",
                            members.len(),
                            members.join("、")
                        ));
                    }
                }
                TreeDeleteTarget::Group(pi, gname) => {
                    let Some(entry) = main.projects.get(*pi) else {
                        return;
                    };
                    let members: Vec<&str> = entry
                        .jobs
                        .iter()
                        .filter(|je| je.job.group.as_deref() == Some(gname.as_str()))
                        .map(|je| je.job.name.as_str())
                        .collect();
                    ui.colored_label(
                        Color32::from_rgb(230, 120, 90),
                        format!("删除分组「{gname}」？"),
                    );
                    if members.is_empty() {
                        ui.weak("分组为空：仅删除分组本身。");
                    } else {
                        ui.colored_label(
                            Color32::from_rgb(230, 180, 80),
                            format!(
                                "⚠ 组内 {} 个流程将【一并删除】：{}",
                                members.len(),
                                members.join("、")
                            ),
                        );
                        ui.weak(format!(
                            "目录 {} 下的流程 yaml 文件（project.yaml 保留，组与名单一并移除）",
                            entry.dir.display()
                        ));
                    }
                }
            }
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui.button("取消").clicked() {
                    main.tree_delete = None;
                }
                let confirm_label = match &target {
                    TreeDeleteTarget::Category(_) => "确认解散",
                    _ => "确认删除",
                };
                if ui
                    .add(
                        egui::Button::new(confirm_label)
                            .fill(Color32::from_rgb(170, 60, 50)),
                    )
                    .clicked()
                {
                    match target {
                        TreeDeleteTarget::Project(pi) => delete_project(main, pi),
                        TreeDeleteTarget::Job(pi, ji) => delete_job(main, pi, ji),
                        TreeDeleteTarget::Group(pi, gname) => delete_group(main, pi, &gname),
                        TreeDeleteTarget::Category(cat) => dissolve_category(main, &cat),
                    }
                    main.tree_delete = None;
                }
            });
        });
    if !open {
        main.tree_delete = None;
    }
}

// ---------------------------------------------------------------------------
// 改名弹窗
// ---------------------------------------------------------------------------

fn rename_window(ctx: &egui::Context, main: &mut MainState) {
    let Some(target) = main.tree_rename.clone() else {
        return;
    };
    crate::ui::modal_backdrop(ctx, "mask_tree_rename");

    // 添加分组与改名共用输入弹窗，但标题 / 按钮 / 提示按语义区分
    let is_add_group = matches!(target, TreeRenameTarget::GroupNew(..));
    let (title, label, confirm_label) = if is_add_group {
        ("✏ 添加分组", "分组名:", "添加")
    } else {
        ("✏ 改名", "新名称:", "确定")
    };

    let mut open = true;
    let mut do_rename = false;
    crate::ui::centered_window(egui::Window::new(title), ctx)
        .open(&mut open)
        .order(egui::Order::Foreground)
        .collapsible(false)
        .resizable(false)
        .default_width(420.0)
        .show(ctx, |ui| {
            ui.label(label);
            let resp = ui.add(
                egui::TextEdit::singleline(&mut main.tree_rename_buf)
                    .desired_width(320.0)
                    .lock_focus(true),
            );
            if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                do_rename = true;
            }
            if is_add_group {
                ui.weak(
                    "Enter 或「添加」确认；取消则保留默认名（空分组会持久存在，之后可右键改名）",
                );
            }
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui.button("取消").clicked() {
                    main.tree_rename = None;
                }
                if ui.button(confirm_label).clicked() {
                    do_rename = true;
                }
            });
        });
    if do_rename {
        let buf = std::mem::take(&mut main.tree_rename_buf);
        match target {
            TreeRenameTarget::Project(pi) => rename_project(main, pi, &buf),
            TreeRenameTarget::Job(pi, ji) => rename_job(main, pi, ji, &buf),
            TreeRenameTarget::Group(pi, old) => rename_group(main, pi, &old, &buf, false),
            TreeRenameTarget::GroupNew(pi, placeholder) => {
                rename_group(main, pi, &placeholder, &buf, true)
            }
        }
        main.tree_rename = None;
    }
    if !open {
        main.tree_rename = None;
    }
}

/// 归类操作弹窗：工程移入"项目" / 分类重命名（共用一个输入框，语义按目标区分）
fn category_window(ctx: &egui::Context, main: &mut MainState) {
    let Some(target) = main.tree_move.clone() else {
        return;
    };
    crate::ui::modal_backdrop(ctx, "mask_tree_move");

    let is_move_in = matches!(target, TreeMoveTarget::IntoProject(..));
    let (title, label, confirm_label) = if is_move_in {
        ("🗂 移入项目", "项目路径（归类层）:", "移入")
    } else {
        ("✏ 重命名项目", "新项目名:", "重命名")
    };
    // 已有分类提示（移入时引导复用同名，重命名时提醒撞名会合并）
    let existing: Vec<String> = {
        let mut v: Vec<String> = main
            .projects
            .iter()
            .filter_map(|e| e.project.category.clone())
            .collect();
        v.sort();
        v.dedup();
        v
    };

    let mut open = true;
    let mut do_move = false;
    crate::ui::centered_window(egui::Window::new(title), ctx)
        .open(&mut open)
        .order(egui::Order::Foreground)
        .collapsible(false)
        .resizable(false)
        .default_width(420.0)
        .show(ctx, |ui| {
            ui.label(label);
            let resp = ui.add(
                egui::TextEdit::singleline(&mut main.tree_move_buf)
                    .desired_width(320.0)
                    .lock_focus(true),
            );
            if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                do_move = true;
            }
            if is_move_in {
                ui.weak("可含 / 建多级（如 yudao/后台），层级不限；留空 = 移出项目");
            } else {
                ui.weak("只改本段名（子级分类与成员工程整体跟着挪，工程与流程不变）");
            }
            if !existing.is_empty() {
                ui.weak(format!("已有项目：{}", existing.join("、")));
            }
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui.button("取消").clicked() {
                    main.tree_move = None;
                }
                if ui.button(confirm_label).clicked() {
                    do_move = true;
                }
            });
        });
    if do_move {
        let buf = std::mem::take(&mut main.tree_move_buf);
        match target {
            TreeMoveTarget::IntoProject(pi) => {
                let cat = buf.trim().to_string();
                let cat = if cat.is_empty() {
                    None
                } else {
                    Some(cat.as_str())
                };
                move_project_category(main, pi, cat);
            }
            TreeMoveTarget::RenameCategory(old) => rename_category(main, &old, &buf),
        }
        main.tree_move = None;
    }
    if !open {
        main.tree_move = None;
    }
}

// ---------------------------------------------------------------------------
// 动作实现
// ---------------------------------------------------------------------------

/// 运行中禁止改动磁盘上的项目/流程文件（引擎正按路径读写）
fn busy_guard(main: &mut MainState) -> bool {
    if main.run_active() {
        main.status_msg = Some("⚠ 有流程运行中，先停止再执行此操作".to_string());
        true
    } else {
        false
    }
}

/// 拖拽插入指示线：在目标行/头的上缘或下缘画一条蓝线
fn paint_drop_marker(ui: &egui::Ui, rect: egui::Rect, before: bool) {
    let y = if before { rect.top() } else { rect.bottom() };
    ui.painter().hline(
        rect.x_range(),
        y,
        egui::Stroke::new(2.5, Color32::from_rgb(97, 175, 239)),
    );
}

/// 流程行作为放置目标：同层（同分组 / 都未分组）才显示指示线并接受释放
fn dnd_job_row(
    main: &mut MainState,
    ui: &egui::Ui,
    pi: usize,
    target: &str,
    layer: Option<&str>,
    resp: &egui::Response,
) {
    let ctx = ui.ctx();
    if !resp.contains_pointer() || !egui::DragAndDrop::has_payload_of_type::<TreeDrag>(ctx) {
        return;
    }
    let Some(payload) = egui::DragAndDrop::payload::<TreeDrag>(ctx) else {
        return;
    };
    let TreeDrag::Job(dragged) = payload.as_ref() else {
        return;
    };
    if dragged == target {
        return;
    }
    // 被拖流程必须与本行同层（拖拽只排序；跨组移动走「移动到分组」菜单）
    let dragged_layer = main
        .projects
        .get(pi)
        .and_then(|e| e.jobs.iter().find(|je| je.job.name == *dragged))
        .and_then(|je| je.job.group.clone());
    if dragged_layer.as_deref() != layer {
        return;
    }
    let before = ctx
        .pointer_hover_pos()
        .map(|p| p.y < resp.rect.center().y)
        .unwrap_or(true);
    paint_drop_marker(ui, resp.rect, before);
    if resp.dnd_release_payload::<TreeDrag>().is_some() {
        reorder_job_layer(main, pi, dragged, layer, target, before);
    }
}

/// 分组头：既是拖拽源也是放置目标（分组之间排序）
fn dnd_group_header(
    main: &mut MainState,
    ui: &egui::Ui,
    pi: usize,
    gname: &str,
    header: &egui::Response,
) {
    let ctx = ui.ctx();
    if header.drag_started() {
        egui::DragAndDrop::set_payload(ctx, TreeDrag::Group(gname.to_string()));
    }
    if !header.contains_pointer() || !egui::DragAndDrop::has_payload_of_type::<TreeDrag>(ctx) {
        return;
    }
    let Some(payload) = egui::DragAndDrop::payload::<TreeDrag>(ctx) else {
        return;
    };
    let TreeDrag::Group(dragged) = payload.as_ref() else {
        return;
    };
    if dragged == gname {
        return;
    }
    if main
        .projects
        .get(pi)
        .map(|e| group_order(e).iter().any(|x| x == dragged))
        .unwrap_or(false)
    {
        let before = ctx
            .pointer_hover_pos()
            .map(|p| p.y < header.rect.center().y)
            .unwrap_or(true);
        paint_drop_marker(ui, header.rect, before);
        if header.dnd_release_payload::<TreeDrag>().is_some() {
            reorder_groups(main, pi, dragged, gname, before);
        }
    }
}

/// 层内成员按新顺序回填全量槽位：全量列表中属于该层的位置依次填入 subset，
/// 其他层成员原位保留（reorder_job_layer 的纯逻辑部分）
fn refill_layer_order(full: Vec<String>, flags: Vec<bool>, subset: Vec<String>) -> Vec<String> {
    let mut it = subset.into_iter();
    full.iter()
        .zip(&flags)
        .map(|(n, f)| {
            if *f {
                it.next().unwrap_or_else(|| n.clone())
            } else {
                n.clone()
            }
        })
        .collect()
}

/// 当前分组的显示顺序：project.groups ∪ 流程引用的组名（追加在后）
fn group_order(entry: &crate::storage::files::ProjectEntry) -> Vec<String> {
    let mut v = entry.project.groups.clone();
    for je in &entry.jobs {
        if let Some(g) = &je.job.group {
            if !v.iter().any(|x| x == g) {
                v.push(g.clone());
            }
        }
    }
    v
}

/// 流程的当前显示顺序：project.jobs（过滤存在的）+ 未登记的按文件名序追加
fn job_order(entry: &crate::storage::files::ProjectEntry) -> Vec<String> {
    let mut v: Vec<String> = entry
        .project
        .jobs
        .iter()
        .filter(|n| entry.jobs.iter().any(|je| &je.job.name == *n))
        .cloned()
        .collect();
    for je in &entry.jobs {
        if !v.contains(&je.job.name) {
            v.push(je.job.name.clone());
        }
    }
    v
}

/// 同层流程重排：以 project.jobs 为全局顺序声明，层内成员按新顺序回填原槽位
fn reorder_job_layer(
    main: &mut MainState,
    pi: usize,
    dragged: &str,
    layer: Option<&str>,
    target: &str,
    before: bool,
) {
    if busy_guard(main) {
        return;
    }
    let Some(entry) = main.projects.get_mut(pi) else {
        return;
    };
    let full = job_order(entry);
    let in_layer = |n: &str| {
        entry
            .jobs
            .iter()
            .find(|je| je.job.name == n)
            .and_then(|je| je.job.group.as_deref())
            == layer
    };
    let flags: Vec<bool> = full.iter().map(|n| in_layer(n)).collect();
    let mut subset: Vec<String> = full
        .iter()
        .zip(&flags)
        .filter(|(_, f)| **f)
        .map(|(n, _)| n.clone())
        .collect();
    if !subset.iter().any(|n| n == dragged) || !subset.iter().any(|n| n == target) {
        return;
    }
    subset.retain(|n| n != dragged);
    let idx = subset
        .iter()
        .position(|n| n == target)
        .unwrap_or(subset.len());
    subset.insert(if before { idx } else { idx + 1 }, dragged.to_string());
    entry.project.jobs = refill_layer_order(full, flags, subset);
    let (dir, project, pname) = (
        entry.dir.clone(),
        entry.project.clone(),
        entry.project.name.clone(),
    );
    if let Err(e) = crate::storage::files::save_yaml(&dir.join("project.yaml"), &project) {
        main.status_msg = Some(format!("排序保存失败: {e}"));
        return;
    }
    main.reload();
    reselect_job(main, &pname, dragged);
    main.status_msg = Some("顺序已保存（project.yaml 的 jobs 列表）".to_string());
}

/// 分组重排：新顺序整体写回 groups 列表（动态组顺势收编进名单）
fn reorder_groups(main: &mut MainState, pi: usize, dragged: &str, target: &str, before: bool) {
    if busy_guard(main) {
        return;
    }
    let Some(entry) = main.projects.get_mut(pi) else {
        return;
    };
    let mut order = group_order(entry);
    if !order.iter().any(|g| g == dragged) || !order.iter().any(|g| g == target) {
        return;
    }
    order.retain(|g| g != dragged);
    let idx = order
        .iter()
        .position(|g| g == target)
        .unwrap_or(order.len());
    order.insert(if before { idx } else { idx + 1 }, dragged.to_string());
    entry.project.groups = order;
    let (dir, project) = (entry.dir.clone(), entry.project.clone());
    if let Err(e) = crate::storage::files::save_yaml(&dir.join("project.yaml"), &project) {
        main.status_msg = Some(format!("排序保存失败: {e}"));
        return;
    }
    main.reload();
    main.status_msg = Some("分组顺序已保存（project.yaml 的 groups 列表）".to_string());
}

/// 删除分组：组成员（流程 yaml 文件）一并删除；project.yaml 的组与名单同步移除
fn delete_group(main: &mut MainState, pi: usize, gname: &str) {
    if busy_guard(main) {
        return;
    }
    let Some(entry) = main.projects.get_mut(pi) else {
        return;
    };
    let members: Vec<(String, std::path::PathBuf)> = entry
        .jobs
        .iter()
        .filter(|je| je.job.group.as_deref() == Some(gname))
        .map(|je| (je.job.name.clone(), je.path.clone()))
        .collect();
    let names: Vec<String> = members.iter().map(|(n, _)| n.clone()).collect();
    entry.project.groups.retain(|g| g != gname);
    entry.project.jobs.retain(|n| !names.contains(n));
    let (dir, project) = (entry.dir.clone(), entry.project.clone());
    // 先删流程文件，再保存 project.yaml（名单已剔除）
    for (_, path) in &members {
        if let Err(e) = std::fs::remove_file(path) {
            main.status_msg = Some(format!("删除文件失败（{}）: {e}", path.display()));
            return;
        }
    }
    if let Err(e) = crate::storage::files::save_yaml(&dir.join("project.yaml"), &project) {
        main.status_msg = Some(format!("project.yaml 同步失败: {e}"));
        return;
    }
    main.reload();
    main.status_msg = Some(if names.is_empty() {
        format!("分组「{gname}」已删除 🗑（空分组）")
    } else {
        format!("分组「{gname}」与 {} 个流程已删除 🗑", names.len())
    });
}

/// 按项目名 + 流程名恢复选中（reload 后索引会变）
fn reselect_job(main: &mut MainState, project_name: &str, job_name: &str) {
    main.select_project_by_name(project_name);
    if let Some(p) = main.selected_project {
        if let Some(idx) = main
            .projects
            .get(p)
            .and_then(|e| e.jobs.iter().position(|j| j.job.name == job_name))
        {
            main.selected_job = Some(idx);
        }
    }
}

/// 添加分组：写入 project.yaml 的 groups 列表（占位名，空分组持久存在），
/// 随即弹出「添加分组」命名弹窗（与改名的窗口文案区分开）
fn add_group(main: &mut MainState, pi: usize) {
    if busy_guard(main) {
        return;
    }
    let Some(entry) = main.projects.get_mut(pi) else {
        return;
    };
    let mut name = "新建分组".to_string();
    let mut n = 1;
    while entry.project.groups.contains(&name)
        || entry
            .jobs
            .iter()
            .any(|je| je.job.group.as_deref() == Some(name.as_str()))
    {
        n += 1;
        name = format!("新建分组{n}");
    }
    entry.project.groups.push(name.clone());
    let (dir, project) = (entry.dir.clone(), entry.project.clone());
    if let Err(e) = crate::storage::files::save_yaml(&dir.join("project.yaml"), &project) {
        main.status_msg = Some(format!("保存 project.yaml 失败: {e}"));
        return;
    }
    main.reload();
    main.tree_rename_buf = name.clone();
    main.tree_rename = Some(TreeRenameTarget::GroupNew(pi, name));
    main.status_msg = Some("分组已创建，请输入名称后确认（Enter / 「添加」）".to_string());
}

/// 分组改名：改 groups 列表 + 同步引用该组的全部流程 yaml。
/// is_add = 添加分组的命名确认（成功文案按"添加"而非"改名"）
fn rename_group(main: &mut MainState, pi: usize, old: &str, new_name: &str, is_add: bool) {
    if busy_guard(main) {
        return;
    }
    let new_name = new_name.trim().to_string();
    if new_name.is_empty() || new_name == old {
        return;
    }
    let Some(entry) = main.projects.get_mut(pi) else {
        return;
    };
    if entry.project.groups.contains(&new_name)
        || entry
            .jobs
            .iter()
            .any(|je| je.job.group.as_deref() == Some(new_name.as_str()))
    {
        main.status_msg = Some(format!("⚠ 分组「{new_name}」已存在"));
        return;
    }
    if let Some(pos) = entry.project.groups.iter().position(|g| g == old) {
        entry.project.groups[pos] = new_name.clone();
    }
    // 引用该组的流程同步新组名（动态组——不在列表只有引用——会被顺手登记进列表）
    let mut job_updates = Vec::new();
    for je in &entry.jobs {
        if je.job.group.as_deref() == Some(old) {
            let mut job = je.job.clone();
            job.group = Some(new_name.clone());
            job_updates.push((je.path.clone(), job));
        }
    }
    let (dir, project) = (entry.dir.clone(), entry.project.clone());
    if let Err(e) = crate::storage::files::save_yaml(&dir.join("project.yaml"), &project) {
        main.status_msg = Some(if is_add {
            format!("添加分组失败（project.yaml）: {e}")
        } else {
            format!("改名失败（project.yaml）: {e}")
        });
        return;
    }
    for (path, job) in job_updates {
        if let Err(e) = crate::storage::files::save_yaml(&path, &job) {
            main.status_msg = Some(format!("改名失败（{}）: {e}", path.display()));
            return;
        }
    }
    main.reload();
    main.status_msg = Some(if is_add {
        format!("分组「{new_name}」已添加 ✅")
    } else {
        format!("分组已改名: {old} → {new_name} ✅")
    });
}

/// 流程移动分组：改 job.group 落盘；目标组不在 groups 列表则顺手登记（空组语义成立）
fn move_job_to_group(main: &mut MainState, pi: usize, ji: usize, group: Option<String>) {
    if busy_guard(main) {
        return;
    }
    let Some(entry) = main.projects.get_mut(pi) else {
        return;
    };
    let Some(je) = entry.jobs.get_mut(ji) else {
        return;
    };
    if je.job.group == group {
        return;
    }
    let (path, job, project_name) = (je.path.clone(), je.job.clone(), entry.project.name.clone());
    je.job.group = group.clone();
    if let Err(e) = crate::storage::files::save_yaml(&path, &je.job) {
        main.status_msg = Some(format!("移动失败: {e}"));
        return;
    }
    if let Some(g) = &group {
        let entry = &mut main.projects[pi];
        if !entry.project.groups.iter().any(|x| x == g) {
            entry.project.groups.push(g.clone());
            let (dir, project) = (entry.dir.clone(), entry.project.clone());
            if let Err(e) = crate::storage::files::save_yaml(&dir.join("project.yaml"), &project) {
                main.status_msg = Some(format!("project.yaml 同步失败: {e}"));
            }
        }
    }
    main.reload();
    main.select_project_by_name(&project_name);
    if let Some(p) = main.selected_project {
        if let Some(idx) = main
            .projects
            .get(p)
            .and_then(|e| e.jobs.iter().position(|j| j.job.name == job.name))
        {
            main.selected_job = Some(idx);
        }
    }
    main.status_msg = Some(format!(
        "「{}」已移至 {}",
        job.name,
        group.unwrap_or_else(|| "（不分组）".to_string())
    ));
}

/// 用系统文件管理器打开目录（Windows explorer / macOS open / Linux xdg-open）。
/// explorer 成功时退出码也非 0，只看能否启动、不校验退出码。
fn open_in_explorer(path: &std::path::Path) -> Result<(), String> {
    if !path.exists() {
        return Err(format!("⚠ 目录不存在: {}", path.display()));
    }
    #[cfg(windows)]
    {
        std::process::Command::new("explorer")
            .arg(path)
            .creation_flags(0x0800_0000) // 不闪控制台窗口
            .spawn()
            .map_err(|e| format!("⚠ 打开目录失败: {e}"))?;
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg(path)
            .spawn()
            .map_err(|e| format!("⚠ 打开目录失败: {e}"))?;
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        std::process::Command::new("xdg-open")
            .arg(path)
            .spawn()
            .map_err(|e| format!("⚠ 打开目录失败: {e}"))?;
    }
    Ok(())
}

/// 删除项目：只删配置（project.yaml + 各流程 yaml + 残留 .yaml.tmp），
/// 目录本身与其余文件（克隆的代码、源码、构建产物）一律保留；
/// 删完后目录若已为空（纯配置项目）则顺手移除空目录。
fn delete_project(main: &mut MainState, pi: usize) {
    if busy_guard(main) {
        return;
    }
    let Some(entry) = main.projects.get(pi) else {
        return;
    };
    let (dir, name) = (entry.dir.clone(), entry.project.name.clone());
    let mut files: Vec<std::path::PathBuf> = vec![dir.join("project.yaml")];
    files.extend(entry.jobs.iter().map(|je| je.path.clone()));
    // save_yaml 原子写中途崩溃会留下 *.yaml.tmp，一并清理
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for p in rd.flatten().map(|e| e.path()) {
            if p.file_name()
                .map(|n| n.to_string_lossy().ends_with(".yaml.tmp"))
                == Some(true)
            {
                files.push(p);
            }
        }
    }
    let mut failed = Vec::new();
    for f in &files {
        if f.exists() {
            if let Err(e) = std::fs::remove_file(f) {
                failed.push(format!("{}: {e}", f.display()));
            }
        }
    }
    if !failed.is_empty() {
        main.status_msg = Some(format!("删除失败: {}", failed.join("；")));
        return;
    }
    let empty = std::fs::read_dir(&dir)
        .map(|mut it| it.next().is_none())
        .unwrap_or(false);
    if empty {
        let _ = std::fs::remove_dir(&dir);
    }
    main.reload();
    main.status_msg = Some(format!("项目「{name}」配置已删除 🗑（目录及其余文件保留）"));
}

fn delete_job(main: &mut MainState, pi: usize, ji: usize) {
    if busy_guard(main) {
        return;
    }
    let Some((path, name)) = main
        .projects
        .get(pi)
        .and_then(|p| p.jobs.get(ji))
        .map(|je| (je.path.clone(), je.job.name.clone()))
    else {
        return;
    };
    if let Err(e) = std::fs::remove_file(&path) {
        main.status_msg = Some(format!("删除文件失败: {e}"));
        return;
    }
    // 同步 project.yaml 的 jobs 列表（与流程编辑器的删除行为一致）
    main.projects[pi].project.jobs.retain(|j| j != &name);
    let entry = &main.projects[pi];
    if let Err(e) =
        crate::storage::files::save_yaml(&entry.dir.join("project.yaml"), &entry.project)
    {
        main.status_msg = Some(format!("project.yaml 同步失败: {e}"));
    }
    main.reload();
    main.status_msg = Some(format!("流程「{name}」已删除 🗑"));
}

fn rename_project(main: &mut MainState, pi: usize, new_name: &str) {
    if busy_guard(main) {
        return;
    }
    let new_name = crate::config::sanitize_filename(new_name.trim());
    if new_name.is_empty() {
        main.status_msg = Some("⚠ 项目名称不能为空".to_string());
        return;
    }
    // 先取齐旧数据并结束借用，后续才能 &mut main
    let (dir, mut project, job_files) = {
        let entry = &main.projects[pi];
        (
            entry.dir.clone(),
            entry.project.clone(),
            entry
                .jobs
                .iter()
                .map(|je| (je.path.clone(), je.job.clone()))
                .collect::<Vec<_>>(),
        )
    };
    let old_name = project.name.clone();
    if new_name == old_name {
        return;
    }
    let new_dir = main.projects_root.join(&new_name);
    if new_dir.exists() {
        main.status_msg = Some(format!("⚠ 目标目录已存在: {}", new_dir.display()));
        return;
    }

    // 1. project.yaml 写入新名（仍在旧目录，改名目录前完成）
    project.name = new_name.clone();
    if let Err(e) = crate::storage::files::save_yaml(&dir.join("project.yaml"), &project) {
        main.status_msg = Some(format!("改名失败（project.yaml）: {e}"));
        return;
    }
    // 2. 各流程 yaml 的 project 字段同步新名
    for (path, mut job) in job_files {
        job.project = new_name.clone();
        if let Err(e) = crate::storage::files::save_yaml(&path, &job) {
            main.status_msg = Some(format!("改名失败（{}）: {e}", path.display()));
            return;
        }
    }
    // 3. 目录改名
    if let Err(e) = std::fs::rename(&dir, &new_dir) {
        main.status_msg = Some(format!("改名失败（目录重命名）: {e}"));
        return;
    }
    // 4. 凭据迁移：密码键 = {项目}/{目标}，项目改名后按新键取不到旧密码。
    //    逐目标搬移（best-effort：失败不阻断改名，大不了在目标管理里重新输入）
    for tname in project.targets.keys() {
        let old_key = format!("{old_name}/{tname}");
        let new_key = format!("{new_name}/{tname}");
        if let Ok(Some(pwd)) = crate::secrets::get_password(&old_key) {
            if crate::secrets::set_password(&new_key, &pwd).is_ok() {
                crate::secrets::delete_password(&old_key);
            }
        }
    }
    main.reload();
    main.select_project_by_name(&new_name);
    main.status_msg = Some(format!("项目已改名: {old_name} → {new_name} ✅"));
}

/// 工程移入/移出归类链的底层搬运：改 category 字段 + 挪工程目录。
/// 工程名不变——CLI 查找、运行历史、锁文件、凭据键全部按工程名工作，零影响。
/// category 是 "/" 分隔的层级路径；None = 平铺（目录挪回 projects 根）。
/// 字段写入净化后的链（与目录名逐段一致，reload 不串档）。
/// 返回 Err 时未做任何磁盘改动（先写 yaml 后挪目录；挪目录失败会回滚字段）。
fn relocate_project_dir(
    main: &mut MainState,
    pi: usize,
    category: Option<&str>,
) -> Result<(), String> {
    let Some(entry) = main.projects.get(pi) else {
        return Ok(());
    };
    let (dir, mut project) = (entry.dir.clone(), entry.project.clone());
    let name = project.name.clone();
    let chain = crate::config::sanitize_category_chain(category);
    let new_cat = (!chain.is_empty()).then(|| chain.join("/"));
    if project.category == new_cat {
        return Ok(()); // 已在目标归类（或都已移出）：无事可做
    }
    let mut new_dir = main.projects_root.clone();
    for seg in &chain {
        new_dir = new_dir.join(seg);
    }
    new_dir = new_dir.join(&name);
    if new_dir != dir && new_dir.exists() {
        return Err(format!("目标目录已存在: {}", new_dir.display()));
    }

    let old_category = project.category.clone();
    project.category = new_cat;
    let yaml = dir.join("project.yaml");
    if let Err(e) = crate::storage::files::save_yaml(&yaml, &project) {
        return Err(format!("写入 project.yaml 失败: {e}"));
    }
    if new_dir != dir {
        // 多级归类链的中间目录可能不存在（移入全新路径），rename 不会自建父目录
        if let Some(parent) = new_dir.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Err(e) = std::fs::rename(&dir, &new_dir) {
            // 回滚字段，保持 yaml 与目录位置一致（下次 reload 不串档）
            project.category = old_category;
            let _ = crate::storage::files::save_yaml(&yaml, &project);
            return Err(format!(
                "目录搬移失败（{} → {}）: {e}",
                dir.display(),
                new_dir.display()
            ));
        }
    }
    Ok(())
}

/// 右键「移入/移出项目」：单工程搬运 + reload 保选中
fn move_project_category(main: &mut MainState, pi: usize, category: Option<&str>) {
    if busy_guard(main) {
        return;
    }
    let name = main.projects.get(pi).map(|e| e.project.name.clone());
    if let Err(e) = relocate_project_dir(main, pi, category) {
        main.status_msg = Some(format!("⚠ {e}"));
        return;
    }
    main.reload();
    if let Some(name) = &name {
        main.select_project_by_name(name);
    }
    let chain = crate::config::sanitize_category_chain(category);
    let msg = if chain.is_empty() {
        format!("工程「{}」已移出项目 ✅", name.unwrap_or_default())
    } else {
        format!(
            "工程「{}」已移入项目「{}」✅",
            name.unwrap_or_default(),
            chain.join("/")
        )
    };
    main.status_msg = Some(msg);
}

/// 归类链的成员（category 等于链本身，或以 链/ 为前缀的子树成员）
pub(crate) fn category_members(main: &MainState, chain: &str) -> Vec<usize> {
    let prefix = format!("{chain}/");
    main.projects
        .iter()
        .enumerate()
        .filter(|(_, e)| {
            e.project
                .category
                .as_deref()
                .map(|c| c == chain || c.starts_with(&prefix))
                .unwrap_or(false)
        })
        .map(|(i, _)| i)
        .collect()
}

/// 跨工程项目运行的流水线展平：成员工程按树中顺序 × 各自 jobs 列表顺序，
/// group 有值时只收该分组的流程（同名分组跨工程；没有该分组的工程跳过）。
/// 每个 GroupJob 带所属工程——引擎按各自工程解析目标/变量/凭据/锁
pub(crate) fn build_category_pipeline(
    projects: &[crate::storage::files::ProjectEntry],
    chain: &str,
    group: Option<&str>,
) -> Vec<crate::group::GroupJob> {
    let mut out = Vec::new();
    for pi in category_members_of(projects, chain) {
        let entry = &projects[pi];
        for je in &entry.jobs {
            if let Some(g) = group {
                if je.job.group.as_deref() != Some(g) {
                    continue;
                }
            }
            out.push(crate::group::GroupJob {
                project: entry.project.clone(),
                name: je.job.name.clone(),
                job: je.job.clone(),
            });
        }
    }
    out
}

/// 纯数据版成员查找（不借 MainState，构建器与测试共用）
fn category_members_of(
    projects: &[crate::storage::files::ProjectEntry],
    chain: &str,
) -> Vec<usize> {
    let prefix = format!("{chain}/");
    projects
        .iter()
        .enumerate()
        .filter(|(_, e)| {
            e.project
                .category
                .as_deref()
                .map(|c| c == chain || c.starts_with(&prefix))
                .unwrap_or(false)
        })
        .map(|(i, _)| i)
        .collect()
}

/// 成员的链去掉 old 前缀后的剩余部分（"" = 正好在 old 上；"x/y" = 在子级里）
fn chain_rest<'a>(cat: &'a str, old: &str) -> &'a str {
    cat.strip_prefix(old)
        .map(|r| r.trim_start_matches('/'))
        .unwrap_or("")
}

/// 成员搬空后自底向上清掉归级链上变空的目录（remove_dir 只删空目录，
/// 非空自动失败——同级还有别的工程时不会误删）
fn prune_empty_chain(root: &std::path::Path, chain: &[String]) {
    let mut dir = root.to_path_buf();
    for seg in chain {
        dir = dir.join(seg);
    }
    // 从最深一段往上试删；删到非空（Err）即停
    loop {
        if std::fs::remove_dir(&dir).is_err() {
            break;
        }
        if !dir.pop() {
            break;
        }
        if dir == root {
            break;
        }
    }
}

/// 分类重命名（改链的最后一段，成员整子树跟着挪）；
/// 撞已有分类名 = 合并进该分类（目录名即分类名）
fn rename_category(main: &mut MainState, old: &str, new: &str) {
    if busy_guard(main) {
        return;
    }
    let new_seg = crate::config::sanitize_filename(new.trim());
    if new_seg.is_empty() {
        main.status_msg = Some("⚠ 项目名不能为空".to_string());
        return;
    }
    let old_chain: Vec<String> = old
        .split('/')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    if old_chain.is_empty() {
        return;
    }
    let parent = old_chain[..old_chain.len() - 1].join("/");
    let new_chain = if parent.is_empty() {
        new_seg.clone()
    } else {
        format!("{parent}/{new_seg}")
    };
    if new_chain == old {
        return;
    }
    let members = category_members(main, old);
    if members.is_empty() {
        return;
    }
    let mut failures = Vec::new();
    for &pi in &members {
        let rest = chain_rest(
            main.projects[pi].project.category.as_deref().unwrap_or(""),
            old,
        );
        let target = if rest.is_empty() {
            new_chain.clone()
        } else {
            format!("{new_chain}/{rest}")
        };
        if let Err(e) = relocate_project_dir(main, pi, Some(&target)) {
            failures.push(e);
        }
    }
    prune_empty_chain(&main.projects_root, &old_chain);
    main.reload();
    if failures.is_empty() {
        main.status_msg = Some(format!(
            "项目已改名: {old} → {new_chain}（{} 个工程）✅",
            members.len()
        ));
    } else {
        main.status_msg = Some(format!("部分工程搬移失败: {}", failures.join("；")));
    }
}

/// 解散分类：成员工程的链去掉本段（升到上一级；顶层则平铺），不删任何文件
fn dissolve_category(main: &mut MainState, cat: &str) {
    if busy_guard(main) {
        return;
    }
    let chain: Vec<String> = cat
        .split('/')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    if chain.is_empty() {
        return;
    }
    let parent = chain[..chain.len() - 1].join("/");
    let members = category_members(main, cat);
    let mut failures = Vec::new();
    for &pi in &members {
        let rest = chain_rest(
            main.projects[pi].project.category.as_deref().unwrap_or(""),
            cat,
        );
        let target = if rest.is_empty() {
            if parent.is_empty() {
                None
            } else {
                Some(parent.clone())
            }
        } else {
            Some(format!("{parent}/{rest}"))
        };
        if let Err(e) = relocate_project_dir(main, pi, target.as_deref()) {
            failures.push(format!("「{}」{e}", main.projects[pi].project.name));
        }
    }
    prune_empty_chain(&main.projects_root, &chain);
    main.reload();
    if failures.is_empty() {
        main.status_msg = Some(format!(
            "项目「{cat}」已解散（{} 个工程升到上一级，文件未删）✅",
            members.len()
        ));
    } else {
        main.status_msg = Some(format!("部分工程移出失败: {}", failures.join("；")));
    }
}

fn rename_job(main: &mut MainState, pi: usize, ji: usize, new_name: &str) {
    if busy_guard(main) {
        return;
    }
    let new_name = new_name.trim().to_string();
    if new_name.is_empty() {
        main.status_msg = Some("⚠ 流程名称不能为空".to_string());
        return;
    }
    let Some((path, mut job, project_name)) = main.projects.get(pi).and_then(|p| {
        p.jobs
            .get(ji)
            .map(|je| (je.path.clone(), je.job.clone(), p.project.name.clone()))
    }) else {
        return;
    };
    let old_name = job.name.clone();
    if new_name == old_name {
        return;
    }
    // 文件名跟随流程名同步改（此前只改 yaml 里的 name 字段，文件还叫旧名，
    // 之后新建同名流程会撞"文件已存在"）。目标文件已存在时拒绝改名，
    // 旧文件原样保留——不静默覆盖别人
    let dir = main.projects[pi].dir.clone();
    let new_path = crate::config::job_file_path(&dir, &new_name);
    if new_path != path && new_path.exists() {
        main.status_msg = Some(format!(
            "⚠ 改名失败：目标文件已存在（{}），换个名字或先处理它",
            new_path.display()
        ));
        return;
    }
    job.name = new_name.clone();
    if new_path != path {
        if let Err(e) = std::fs::rename(&path, &new_path) {
            main.status_msg = Some(format!("改名失败（重命名文件）: {e}"));
            return;
        }
    }
    if let Err(e) = crate::storage::files::save_yaml(&new_path, &job) {
        main.status_msg = Some(format!("改名失败: {e}"));
        return;
    }
    // 同步 project.yaml 的 jobs 列表（与删除流程的同步行为一致）
    if let Some(entry) = main.projects.get_mut(pi) {
        if let Some(pos) = entry.project.jobs.iter().position(|j| j == &old_name) {
            entry.project.jobs[pos] = new_name.clone();
            let dir = entry.dir.clone();
            let project = entry.project.clone();
            if let Err(e) = crate::storage::files::save_yaml(&dir.join("project.yaml"), &project) {
                main.status_msg = Some(format!("project.yaml 同步失败: {e}"));
            }
        }
    }
    main.reload();
    main.select_project_by_name(&project_name);
    if let Some(idx) = main
        .selected_project
        .and_then(|p| main.projects.get(p))
        .and_then(|p| p.jobs.iter().position(|j| j.job.name == new_name))
    {
        main.selected_job = Some(idx);
    }
    main.status_msg = Some(format!("流程已改名: {old_name} → {new_name} ✅"));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::Job;
    use crate::storage::files::{JobEntry, ProjectEntry};

    fn entry(groups: Vec<&str>, jobs: Vec<(&str, Option<&str>)>) -> ProjectEntry {
        ProjectEntry {
            dir: "x".into(),
            project: crate::models::Project {
                groups: groups.into_iter().map(String::from).collect(),
                jobs: jobs.iter().map(|(n, _)| n.to_string()).collect(),
                ..Default::default()
            },
            jobs: jobs
                .into_iter()
                .map(|(n, g)| JobEntry {
                    path: format!("x/{n}.yaml").into(),
                    job: Job {
                        name: n.into(),
                        group: g.map(String::from),
                        ..Default::default()
                    },
                })
                .collect(),
        }
    }

    #[test]
    fn order_helpers_list_and_union() {
        // 分组顺序 = 名单 ∪ 引用组（名单在前，引用的动态组追加）
        let e = entry(
            vec!["后端", "前端"],
            vec![("a", Some("后端")), ("b", Some("野组")), ("c", None)],
        );
        assert_eq!(group_order(&e), vec!["后端", "前端", "野组"]);
        // 流程顺序 = project.jobs 过滤存在的 + 未登记的按文件名序追加
        let e2 = entry(vec![], vec![("a", None), ("b", None)]);
        let mut e2 = e2;
        e2.project.jobs = vec!["b".into()]; // 名单只登记了 b（a 未登记 → 追加在后）
        assert_eq!(job_order(&e2), vec!["b".to_string(), "a".to_string()]);
    }

    #[test]
    fn refill_puts_subset_in_place() {
        // full=a b c d e，层内（flags=T）成员 a c d 重排为 d a c → 槽位回填
        let full = ["a", "b", "c", "d", "e"].map(String::from).to_vec();
        let flags = vec![true, false, true, true, false];
        let subset = ["d", "a", "c"].map(String::from).to_vec();
        assert_eq!(
            refill_layer_order(full, flags, subset),
            vec![
                "d".to_string(),
                "b".to_string(),
                "a".to_string(),
                "c".to_string(),
                "e".to_string()
            ]
        );
    }

    fn cat_entry(
        name: &str,
        category: Option<&str>,
        jobs: Vec<(&str, Option<&str>)>,
    ) -> ProjectEntry {
        let mut e = entry(vec![], jobs);
        e.project.name = name.to_string();
        e.project.category = category.map(String::from);
        e
    }

    #[test]
    fn category_pipeline_flattens_members_and_filters_group() {
        // 目录树：研发/api工程（发布: deploy；未分组 lint）+ 研发/web工程（发布: deploy）
        //        研发/子级/移动工程（发布: deploy）+ 平铺 工具箱
        let mut projects = vec![
            cat_entry(
                "api工程",
                Some("研发"),
                vec![("deploy", Some("发布")), ("lint", None)],
            ),
            cat_entry("web工程", Some("研发"), vec![("deploy", Some("发布"))]),
            cat_entry(
                "移动工程",
                Some("研发/子级"),
                vec![("deploy", Some("发布"))],
            ),
            cat_entry("工具箱", None, vec![("clean", None)]),
        ];

        // 全部流程：成员树中序 × 各自 jobs 序；每个 GroupJob 带所属工程名
        let all = build_category_pipeline(&projects, "研发", None);
        let got: Vec<String> = all
            .iter()
            .map(|gj| format!("{}/{}", gj.project.name, gj.name))
            .collect();
        assert_eq!(
            got,
            vec![
                "api工程/deploy",
                "api工程/lint",
                "web工程/deploy",
                "移动工程/deploy"
            ],
            "子链成员也要进流水线"
        );

        // 同名分组过滤：只跑各工程的「发布」，没有该分组的工程自然跳过
        let deploys = build_category_pipeline(&projects, "研发", Some("发布"));
        let got: Vec<String> = deploys
            .iter()
            .map(|gj| format!("{}/{}", gj.project.name, gj.name))
            .collect();
        assert_eq!(
            got,
            vec!["api工程/deploy", "web工程/deploy", "移动工程/deploy"]
        );

        // 深链只收子树：展平「研发/子级」只有移动工程
        let sub = build_category_pipeline(&projects, "研发/子级", None);
        assert_eq!(sub.len(), 1);
        assert_eq!(sub[0].project.name, "移动工程");

        // 平铺工程不在任何分类里
        assert!(build_category_pipeline(&projects, "工具箱", None).is_empty());
        // 不存在的链 → 空流水线
        assert!(build_category_pipeline(&projects, "不存在", None).is_empty());
        let _ = &mut projects;
    }
}
