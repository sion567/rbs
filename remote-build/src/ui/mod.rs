//! UI 模块（设计文档 §9）。

pub mod console;
pub mod first_run;
pub mod job_editor;
pub mod new_project;
pub mod project_tree;
pub mod settings;
pub mod targets;

use eframe::egui;

/// 弹窗遮罩：全屏半透明黑罩，打开弹窗时挡住下层 UI 的鼠标交互。
///
/// 遮罩与主界面同在 Order::Middle 带（区域内置顶，点它也只会在这带内置顶），
/// 配套弹窗必须在每帧渲染后调 [`raise_above_backdrop`] 把窗口层重新置顶，
/// 保证遮罩永远压不到弹窗本身。
pub(crate) fn modal_backdrop(ctx: &egui::Context, id: &str) {
    egui::Area::new(egui::Id::new(id))
        .order(egui::Order::Middle)
        .fixed_pos(egui::Pos2::ZERO)
        .interactable(true)
        .movable(false)
        .show(ctx, |ui| {
            let screen = ctx.screen_rect();
            // 必须分配一个全屏的可点击区域：Area 的可交互范围只由内容大小决定，
            // 纯 painter 画矩形不会扩大交互范围，也就挡不住下层点击
            ui.allocate_rect(screen, egui::Sense::click());
            ui.painter()
                .rect_filled(screen, 0.0, egui::Color32::from_black_alpha(110));
        });
}

/// 把弹窗窗口置顶到遮罩之上（在对应 `egui::Window::show` 之后调用）。
///
/// egui 0.27 的 Window 未暴露 order 设置，窗口层 id 就是 `Id::new(标题)`，
/// 这里直接操作区域顺序，把被遮罩（点击/首帧置顶）盖住的窗口抬回来。
pub(crate) fn raise_above_backdrop(ctx: &egui::Context, window_title: &str) {
    let layer = egui::LayerId::new(egui::Order::Middle, egui::Id::new(window_title));
    ctx.memory_mut(|mem| mem.areas_mut().move_to_top(layer));
}
