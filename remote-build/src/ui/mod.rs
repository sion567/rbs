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
/// 配套的弹窗窗口用 `.order(egui::Order::Foreground)` 罩在遮罩之上。
/// 注意不能用 Order::Tooltip：ComboBox 弹层固定在 Foreground 带，
/// Tooltip 带的窗口会把弹窗内所有下拉框的列表整个盖住。
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

/// 输入框标准宽度三档（短 / 中 / 长）：全应用统一，别逐处手写像素。
/// 短 = 端口/超时这类紧凑数值；中 = 账号 / 分支 / 项目名；
/// 长 = 路径 / URL / 失败说明这类长文本
pub mod input {
    pub const SHORT: f32 = 110.0;
    pub const MED: f32 = 260.0;
    pub const LONG: f32 = 440.0;
}

/// 弹窗首次出现时屏幕居中（之后仍可拖动）：pivot 让 default_pos 按窗口中心
/// 对齐屏幕中心，default_pos 只在首次出现生效
pub(crate) fn centered_window<'a>(
    win: egui::Window<'a>,
    ctx: &egui::Context,
) -> egui::Window<'a> {
    let center = ctx.input(|i| i.content_rect().center());
    win.pivot(egui::Align2::CENTER_CENTER)
        .default_pos(center)
}
