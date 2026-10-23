//! UI 模块（设计文档 §9）。

pub mod console;
pub mod first_run;
pub mod job_editor;
pub mod new_project;
pub mod project_tree;
pub mod settings;
pub mod status;
pub mod targets;

use eframe::egui;

/// 弹窗遮罩：全屏半透明黑罩，打开弹窗时挡住下层 UI 的鼠标交互。
///
/// 遮罩画在 Order::Middle（与主界面同带、区域内置顶，点它也只会在这带内置顶）；
/// 配套的弹窗窗口用 `.order(egui::Order::Tooltip)` 罩在遮罩之上。
pub(crate) fn modal_backdrop(ctx: &egui::Context, id: &str) {
    egui::Area::new(egui::Id::unique(id))
        .order(egui::Order::Middle)
        .fixed_pos(egui::Pos2::ZERO)
        .interactable(true)
        .movable(false)
        .show(ctx, |ui| {
            // 0.37 移除了 Context::screen_rect，content_rect() 即视口内容矩形
            let screen = ui.input(|i| i.content_rect());
            // 必须分配一个全屏的可点击区域：Area 的可交互范围只由内容大小决定，
            // 纯 painter 画矩形不会扩大交互范围，也就挡不住下层点击
            ui.allocate_rect(screen, egui::Sense::click());
            ui.painter()
                .rect_filled(screen, 0.0, egui::Color32::from_black_alpha(110));
        });
}
