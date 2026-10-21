//! 左侧项目/作业树（设计文档 §9.1）。

use crate::app::MainState;
use eframe::egui;
use egui::Color32;

pub fn show(main: &mut MainState, ui: &mut egui::Ui) {
    ui.horizontal(|ui| {
        ui.heading("项目");
        if ui.small_button("＋ 新建项目").clicked() {
            main.new_project.open = true;
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
        for (pi, entry) in main.projects.iter().enumerate() {
            let issues = crate::config::validate_project(&entry.project);
            let has_error = issues
                .iter()
                .any(|i| matches!(i, crate::models::ValidationIssue::Error { .. }));

            let header = format!(
                "📁 {}{}",
                entry.project.name,
                if has_error { " ⚠" } else { "" }
            );
            let body = egui::CollapsingHeader::new(header)
                .default_open(true)
                .show(ui, |ui| {
                    for (ji, je) in entry.jobs.iter().enumerate() {
                        let selected = main.selected_project == Some(pi)
                            && main.selected_job == Some(ji);
                        let label = je.job.name.clone();
                        // 📄 强调"一个流程一个 yaml 文件"，与运行按钮的 ▶ 区分开
                        if ui
                            .selectable_label(selected, format!("📄 {label}"))
                            .clicked()
                        {
                            main.selected_project = Some(pi);
                            main.selected_job = Some(ji);
                            main.delete_job_armed = false; // 切换流程时解除删除确认
                        }
                    }
                });

            // 折叠头下方显示阻断性错误（§9.4 目录不存在提示）
            if body.body_response.is_some() || has_error {
                for issue in &issues {
                    if let crate::models::ValidationIssue::Error { message, .. } = issue {
                        ui.colored_label(Color32::from_rgb(220, 120, 90), format!("⚠ {message}"));
                    }
                }
            }
        }
    });
}
