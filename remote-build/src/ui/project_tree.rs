//! 左侧项目/作业树（设计文档 §9.1）。
//!
//! 右键菜单：项目 = 改名 / 删除；流程 = 运行 / 改名 / 删除。
//! 删除走确认弹窗（避免误删），改名走输入弹窗；两者都带遮罩。

use crate::app::MainState;
use eframe::egui;
use egui::Color32;

/// 右键菜单待确认的删除目标（下标为 main.projects / jobs 的索引）
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TreeDeleteTarget {
    Project(usize),
    Job(usize, usize),
}

/// 右键菜单的改名目标
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TreeRenameTarget {
    Project(usize),
    Job(usize, usize),
}

pub fn show(main: &mut MainState, ui: &mut egui::Ui) {
    ui.horizontal(|ui| {
        ui.heading("项目");
        // 运行中禁用（统一"执行时所有输入禁用"；引擎正按路径读写工作区）
        if ui
            .add_enabled(
                !main.run_active(),
                egui::Button::new("＋ 新建项目").small(),
            )
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
                "未发现项目。\n点「＋ 新建项目」从 Git 克隆或选择本地目录；\n或在 {} 放置 <项目>/project.yaml 后点 ⟳",
                main.workspace_root.display()
            ),
        );
        return;
    }

    egui::ScrollArea::vertical().show(ui, |ui| {
        for pi in 0..main.projects.len() {
            // 预取需要的不可变数据并立刻结束借用：右键菜单闭包要 &mut main
            let (header, error_msgs) = {
                let entry = &main.projects[pi];
                let issues = crate::config::validate_project(&entry.project);
                let msgs: Vec<String> = issues
                    .iter()
                    .filter_map(|i| match i {
                        crate::models::ValidationIssue::Error { message, .. } => {
                            Some(message.clone())
                        }
                        _ => None,
                    })
                    .collect();
                (
                    format!("📁 {}{}", entry.project.name, if !msgs.is_empty() { " ⚠" } else { "" }),
                    msgs,
                )
            };
            let body = egui::CollapsingHeader::new(header)
                .default_open(true)
                .show(ui, |ui| {
                    for ji in 0..main.projects[pi].jobs.len() {
                        let (label, selected, dot_state) = {
                            let je = &main.projects[pi].jobs[ji];
                            (
                                je.job.name.clone(),
                                main.selected_project == Some(pi)
                                    && main.selected_job == Some(ji),
                                main.job_dot_state(&main.projects[pi].project.name, &je.job.name),
                            )
                        };
                        // 📄 强调"一个流程一个 yaml 文件"，与运行按钮的 ▶ 区分开；
                        // 运行中的作业在名字后面带状态圆点（蓝闪/黄闪/绿/红，见 ui::status）
                        let inner = ui.horizontal(|ui| {
                            let r = ui.selectable_label(selected, format!("📄 {label}"));
                            if let Some(st) = dot_state {
                                let dark =
                                    !matches!(main.global.theme, crate::models::Theme::Light);
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
                        resp.context_menu(|ui| {
                            if ui
                                .add_enabled(
                                    !main.run_active(),
                                    egui::Button::new("▶ 运行"),
                                )
                                .on_hover_text("运行此流程（运行中不可触发）")
                                .clicked()
                            {
                                main.selected_project = Some(pi);
                                main.selected_job = Some(ji);
                                main.start_run(0, None);
                                ui.close();
                            }
                            if ui
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
                });

            // 折叠头下方显示阻断性错误（§9.4 目录不存在提示）
            if body.body_response.is_some() || !error_msgs.is_empty() {
                for message in &error_msgs {
                    ui.colored_label(Color32::from_rgb(220, 120, 90), format!("⚠ {message}"));
                }
            }

            // 项目头右键菜单：改名 / 删除
            body.header_response.context_menu(|ui| {
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
    });

    confirm_delete_window(ui.ctx(), main);
    rename_window(ui.ctx(), main);
}

// ---------------------------------------------------------------------------
// 确认删除弹窗
// ---------------------------------------------------------------------------

fn confirm_delete_window(ctx: &egui::Context, main: &mut MainState) {
    let Some(target) = main.tree_delete else { return; };
    crate::ui::modal_backdrop(ctx, "mask_tree_delete");

    let mut open = true;
    egui::Window::new("⚠ 确认删除")
        .open(&mut open)
        .order(egui::Order::Tooltip)
        .collapsible(false)
        .resizable(false)
        .default_width(480.0)
        .show(ctx, |ui| {
            match target {
                TreeDeleteTarget::Project(pi) => {
                    let Some(entry) = main.projects.get(pi) else { return; };
                    ui.colored_label(
                        Color32::from_rgb(230, 120, 90),
                        format!("删除项目「{}」？", entry.project.name),
                    );
                    ui.weak(format!("将删除整个目录：{}", entry.dir.display()));
                    ui.weak(
                        "Git 模式克隆的项目代码也在其中；本地目录模式只删配置，源码目录不受影响。",
                    );
                }
                TreeDeleteTarget::Job(pi, ji) => {
                    let Some(je) = main.projects.get(pi).and_then(|p| p.jobs.get(ji)) else {
                        return;
                    };
                    ui.colored_label(
                        Color32::from_rgb(230, 120, 90),
                        format!("删除流程「{}」？", je.job.name),
                    );
                    ui.weak(format!("将删除文件：{}", je.path.display()));
                }
            }
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui.button("取消").clicked() {
                    main.tree_delete = None;
                }
                if ui
                    .add(
                        egui::Button::new("确认删除")
                            .fill(Color32::from_rgb(170, 60, 50)),
                    )
                    .clicked()
                {
                    match target {
                        TreeDeleteTarget::Project(pi) => delete_project(main, pi),
                        TreeDeleteTarget::Job(pi, ji) => delete_job(main, pi, ji),
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
    let Some(target) = main.tree_rename else { return; };
    crate::ui::modal_backdrop(ctx, "mask_tree_rename");

    let mut open = true;
    let mut do_rename = false;
    egui::Window::new("✏ 改名")
        .open(&mut open)
        .order(egui::Order::Tooltip)
        .collapsible(false)
        .resizable(false)
        .default_width(420.0)
        .show(ctx, |ui| {
            ui.label("新名称:");
            let resp = ui.add(
                egui::TextEdit::singleline(&mut main.tree_rename_buf)
                    .desired_width(320.0)
                    .lock_focus(true),
            );
            if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                do_rename = true;
            }
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui.button("取消").clicked() {
                    main.tree_rename = None;
                }
                if ui.button("确定").clicked() {
                    do_rename = true;
                }
            });
        });
    if do_rename {
        let buf = std::mem::take(&mut main.tree_rename_buf);
        match target {
            TreeRenameTarget::Project(pi) => rename_project(main, pi, &buf),
            TreeRenameTarget::Job(pi, ji) => rename_job(main, pi, ji, &buf),
        }
        main.tree_rename = None;
    }
    if !open {
        main.tree_rename = None;
    }
}

// ---------------------------------------------------------------------------
// 动作实现
// ---------------------------------------------------------------------------

/// 运行中禁止改动磁盘上的项目/流程文件（引擎正按路径读写）
fn busy_guard(main: &mut MainState) -> bool {
    if main.run_active() {
        main.status_msg = Some("⚠ 有作业运行中，先 ⏹ Stop 再执行此操作".to_string());
        true
    } else {
        false
    }
}

fn delete_project(main: &mut MainState, pi: usize) {
    if busy_guard(main) {
        return;
    }
    let Some((dir, name)) = main.projects.get(pi).map(|e| (e.dir.clone(), e.project.name.clone()))
    else {
        return;
    };
    match std::fs::remove_dir_all(&dir) {
        Ok(()) => {
            main.reload();
            main.status_msg = Some(format!("项目「{name}」已删除 🗑"));
        }
        Err(e) => main.status_msg = Some(format!("删除失败: {e}")),
    }
}

fn delete_job(main: &mut MainState, pi: usize, ji: usize) {
    if busy_guard(main) {
        return;
    }
    let Some((path, name)) =
        main.projects.get(pi).and_then(|p| p.jobs.get(ji)).map(|je| (je.path.clone(), je.job.name.clone()))
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
    let new_dir = main.workspace_root.join(&new_name);
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
    main.reload();
    main.select_project_by_name(&new_name);
    main.status_msg = Some(format!("项目已改名: {old_name} → {new_name} ✅"));
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
    let Some((path, mut job, project_name)) = main
        .projects
        .get(pi)
        .and_then(|p| p.jobs.get(ji).map(|je| (je.path.clone(), je.job.clone(), p.project.name.clone())))
    else {
        return;
    };
    let old_name = job.name.clone();
    if new_name == old_name {
        return;
    }
    job.name = new_name.clone();
    if let Err(e) = crate::storage::files::save_yaml(&path, &job) {
        main.status_msg = Some(format!("改名失败: {e}"));
        return;
    }
    // 同步 project.yaml 的 jobs 列表（与删除流程的同步行为一致）
    if let Some(entry) = main.projects.get_mut(pi) {
        if let Some(pos) = entry.project.jobs.iter().position(|j| j == &old_name) {
            entry.project.jobs[pos] = new_name.clone();
            let dir = entry.dir.clone();
            let project = entry.project.clone();
            if let Err(e) =
                crate::storage::files::save_yaml(&dir.join("project.yaml"), &project)
            {
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
