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

/// 超长名字按字符截断（中文按字符而非字节），超出部分以 … 结尾。
/// 树 / 列表用截断名防撑宽布局，调用方对被截断的名字补 on_hover_text 显示全名
pub(crate) fn truncate_label(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{cut}…")
}

/// Option<String> 字段的编辑行（有值显示输入框 + 🗑 清除；无值显示 — + ＋ 添加），
/// 返回是否发生了修改。调用方需处于 Grid 中（本函数负责 end_row）。
/// settings（工具表单）与 targets（目标表单）共用——此前两处各有一份逐字节相同的实现
pub(crate) fn option_text_row(
    ui: &mut egui::Ui,
    label: &str,
    value: &mut Option<String>,
    width: f32,
) -> bool {
    let mut changed = false;
    ui.label(label);
    match value {
        Some(v) => {
            let mut clear = false;
            ui.horizontal(|ui| {
                changed |= ui
                    .add(egui::TextEdit::singleline(v).desired_width(width))
                    .changed();
                if ui.small_button("🗑").clicked() {
                    clear = true;
                }
            });
            if clear {
                *value = None;
                changed = true;
            }
        }
        None => {
            ui.horizontal(|ui| {
                ui.weak("—");
                if ui.small_button("＋").clicked() {
                    *value = Some(String::new());
                    changed = true;
                }
            });
        }
    }
    ui.end_row();
    changed
}

/// 打开目录选择对话框，起点 = 输入框当前路径（约定：所有「浏览」按钮都从
/// 输入框已有的地址出发，而不是每次都弹到系统默认位置）。
/// 输入框是文件路径或不存在时，回退到其父目录；再不行才用系统默认。
pub(crate) fn pick_folder(starting: &str, title: &str) -> Option<std::path::PathBuf> {
    let mut dlg = rfd::FileDialog::new().set_title(title);
    let p = std::path::PathBuf::from(starting.trim());
    if p.is_dir() {
        dlg = dlg.set_directory(&p);
    } else if let Some(parent) = p.parent() {
        if parent.is_dir() {
            dlg = dlg.set_directory(parent);
        }
    }
    dlg.pick_folder()
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

/// 折叠头箭头：两条 2px 线段，收起 = ▸ 指右、展开 = ▾ 指下，随 openness 旋转。
///
/// 为什么是线段而不是文本字形（⌃/⌄ 或 ▸/▾）：
/// - 字形可用性随字体链变化——UI 快照 `emoji_glyph_gallery` 实测 ⌃(U+2303) 在
///   msyh 字体链下是豆腐块（控制台按钮曾因此只剩文字），▸/▾ 依赖 fallback；
/// - 线段是 Path 形状，与字体完全无关，且在折叠头 icon 闭包内实测可靠渲染。
///
/// 用法：`CollapsingHeader::new(..).icon(crate::ui::chevron_icon)`
pub(crate) fn chevron_icon(ui: &mut egui::Ui, openness: f32, response: &egui::Response) {
    let rect = response.rect;
    let c = rect.center();
    let h = rect.height() * 0.26;
    let openness = openness.clamp(0.0, 1.0);
    let lerp = |a: f32, b: f32| a + (b - a) * openness;
    // 收起态 ">"：尖端朝右、两臂在左；展开态 "v"：尖端朝下、两臂在上
    let tip = egui::pos2(c.x + lerp(h * 0.45, 0.0), c.y + lerp(0.0, h * 0.6));
    let arm1_end = egui::pos2(c.x + lerp(-h * 0.3, -h), c.y + lerp(-h, -h * 0.35));
    let arm2_end = egui::pos2(c.x + lerp(-h * 0.3, h), c.y + lerp(h, -h * 0.35));
    let stroke = egui::Stroke::new(2.0, ui.style().interact(response).fg_stroke.color);
    ui.painter().line_segment([arm1_end, tip], stroke);
    ui.painter().line_segment([tip, arm2_end], stroke);
}
