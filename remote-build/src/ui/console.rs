//! 底部控制台输出（设计文档 §9.1 Console Output，实时流式渲染）。

use crate::models::{JobStatus, LogEvent};
use eframe::egui;
use egui::Color32;

/// 日志行的语义角色：渲染时按当前主题映射为具体颜色（切换主题全量重新着色）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    StepStart,
    Stdout,
    Stderr,
    Ok,
    Fail,
    Error,
    Info,
    Cancelled,
}

/// 控制台配色：两套色板各自为深底/浅底调校——
/// 深底 = One Dark；浅底 ≈ VS Code Light+ 终端色。色相按语义分离，一眼可辨类型。
pub mod palette {
    use super::Role;
    use egui::Color32;

    pub fn color_for(role: Role, dark: bool) -> Color32 {
        use Role::*;
        match (role, dark) {
            // 步骤标题：青
            (StepStart, true) => Color32::from_rgb(86, 182, 194),
            (StepStart, false) => Color32::from_rgb(5, 152, 188),
            // 普通输出：柔灰 / 深灰
            (Stdout, true) => Color32::from_rgb(171, 178, 191),
            (Stdout, false) => Color32::from_rgb(59, 59, 59),
            // stderr：黄
            (Stderr, true) => Color32::from_rgb(229, 192, 123),
            (Stderr, false) => Color32::from_rgb(163, 134, 27),
            // 成功：绿
            (Ok, true) => Color32::from_rgb(152, 195, 121),
            (Ok, false) => Color32::from_rgb(13, 188, 121),
            // 失败：红
            (Fail, true) => Color32::from_rgb(224, 108, 117),
            (Fail, false) => Color32::from_rgb(205, 49, 49),
            // 引擎级错误：亮红 / 深红
            (Error, true) => Color32::from_rgb(241, 76, 76),
            (Error, false) => Color32::from_rgb(139, 26, 26),
            // 系统提示：蓝
            (Info, true) => Color32::from_rgb(97, 175, 239),
            (Info, false) => Color32::from_rgb(4, 81, 165),
            // 取消：中性灰黄 / 中灰
            (Cancelled, true) => Color32::from_rgb(200, 200, 150),
            (Cancelled, false) => Color32::from_rgb(119, 119, 119),
        }
    }
}

const MAX_LINES: usize = 5000;

#[derive(Debug, Clone)]
pub struct ConsoleLine {
    pub text: String,
    pub role: Role,
    /// 步骤头行（Role::StepStart）所属的步骤下标：只看错误模式下，
    /// 只显示失败步骤的骨架头（其余步骤头随整步隐藏）
    pub step: Option<usize>,
}

pub struct ConsoleState {
    pub lines: Vec<ConsoleLine>,
    /// 自动滚动到最新输出（勾选时新日志始终可见；关闭后可上翻检查历史，新行不会拉走视图）
    pub auto_scroll: bool,
    /// 只看错误（gh run view --log-failed 同款）：隐藏 stdout/提示/成功行，
    /// 只留失败步骤的骨架（Step 头 + stderr + ❌）与 ⚠ 错误、取消收尾。
    /// 全程无错误的运行在此模式下不显示任何行——本身就是"没有错误"的信号
    pub errors_only: bool,
    /// 折叠输出区：只留「控制台输出 / 自动滚动 / 只看错误」标题行，
    /// 编辑步骤时不再被控制台占掉半屏（再次点开展开）
    pub collapsed: bool,
    /// 退出码非 0 的步骤集合：只看错误模式下保留这些步骤的骨架头
    failed_steps: std::collections::HashSet<usize>,
    // ── 虚拟化渲染缓存 ──────────────────────────────────────────────
    /// 每行在 measured_width 下量得的高度（Fonts::layout 逐行排版太贵，
    /// 5000 行全量重排每帧会卡顿；缓存后每帧只排版可视区几十行）
    heights: Vec<f32>,
    measured_width: f32,
    /// 行集结构性变化（上限裁剪 / 清空）使 heights 索引失效 → 全量重测
    heights_dirty: bool,
}

impl Default for ConsoleState {
    fn default() -> Self {
        Self {
            lines: Vec::new(),
            auto_scroll: true,
            errors_only: false,
            collapsed: false,
            failed_steps: Default::default(),
            heights: Vec::new(),
            measured_width: 0.0,
            heights_dirty: true,
        }
    }
}

impl ConsoleState {
    pub fn push_event(&mut self, ev: &LogEvent) {
        match ev {
            LogEvent::StepStart { step_index, name, .. } => {
                let text = format!("── Step {} ── {name}", step_index + 1);
                self.lines
                    .push(ConsoleLine { text, role: Role::StepStart, step: Some(*step_index) });
                self.trim_to_limit();
            }
            LogEvent::Stdout { line, .. } => {
                self.push(line.clone(), Role::Stdout);
            }
            LogEvent::Stderr { line, .. } => {
                self.push(line.clone(), Role::Stderr);
            }
            LogEvent::StepEnd { step_index, exit_code, duration_ms } => {
                if *exit_code != 0 {
                    // 只看错误模式保留失败步骤的骨架头
                    self.failed_steps.insert(*step_index);
                }
                let (text, role) = if *exit_code == 0 {
                    (
                        format!("✅ Step {} 完成 ({duration_ms}ms)", step_index + 1),
                        Role::Ok,
                    )
                } else {
                    (
                        format!("❌ Step {} 失败，退出码 {exit_code} ({duration_ms}ms)", step_index + 1),
                        Role::Fail,
                    )
                };
                self.push(text, role);
            }
            // 退避等待期：告警行（Error）已上屏，恢复执行时还有 StepStart 行，这里不重复
            LogEvent::StepRetryWait { .. } => {}
            LogEvent::JobEnd { status, total_duration_ms, .. } => {
                let role = match status {
                    JobStatus::Success | JobStatus::SucceededWithFailures { .. } => Role::Ok,
                    JobStatus::Cancelled => Role::Cancelled,
                    JobStatus::Timeout => Role::Error,
                    _ => Role::Fail,
                };
                self.push(
                    format!("==== Job 结束: {} ({total_duration_ms}ms) ====", job_label(status)),
                    role,
                );
            }
            LogEvent::Error { message } => {
                self.push(format!("⚠ {message}"), Role::Error);
            }
            // 组运行边界（第五批）：单流程结束只发组事件，整个组结束才走 JobEnd 臂
            LogEvent::GroupJobBegin { job, index, total, .. } => {
                self.push(
                    format!("──── 组运行 {index}/{total} ── 启动流程 {job} ────"),
                    Role::Info,
                );
            }
            LogEvent::GroupJobEnd { job, status, duration_ms, .. } => {
                let role = if status.ok() {
                    Role::Ok
                } else if matches!(status, JobStatus::Cancelled) {
                    Role::Cancelled
                } else {
                    Role::Fail
                };
                self.push(
                    format!("──── 组流程 {job}: {}（{duration_ms}ms）────", job_label(status)),
                    role,
                );
            }
        }
    }

    fn push(&mut self, text: String, role: Role) {
        self.push_line_obj(ConsoleLine { text, role, step: None });
    }

    fn push_line_obj(&mut self, line: ConsoleLine) {
        self.lines.push(line);
        self.trim_to_limit();
    }

    fn trim_to_limit(&mut self) {
        if self.lines.len() > MAX_LINES {
            let drop = self.lines.len() - MAX_LINES;
            self.lines.drain(0..drop);
            // 前段裁剪让 heights 索引整体错位，标脏全量重测
            self.heights_dirty = true;
        }
    }

    /// 非引擎事件的普通提示行
    pub fn push_line(&mut self, text: impl Into<String>, role: Role) {
        self.push(text.into(), role);
    }

    pub fn clear(&mut self) {
        self.lines.clear();
        self.heights.clear();
        self.heights_dirty = true;
    }

    pub fn all_text(&self) -> String {
        self.lines.iter().map(|l| l.text.as_str()).collect::<Vec<_>>().join("\n")
    }
}

fn job_label(status: &JobStatus) -> String {
    job_label_public(status)
}

/// JobEnd 行/桌面通知共用的结论文案
pub fn job_label_public(status: &JobStatus) -> String {
    match status {
        JobStatus::Success => "成功 ✅".to_string(),
        JobStatus::SucceededWithFailures { failed_steps } => format!(
            "成功，但有 {} 步失败被跳过 ⚠（{}）",
            failed_steps.len(),
            failed_steps
                .iter()
                .map(|i| (i + 1).to_string())
                .collect::<Vec<_>>()
                .join(",")
        ),
        JobStatus::Failed { step_index, reason } => format!("失败（Step {}：{reason}）", step_index + 1),
        JobStatus::Cancelled => "已取消".to_string(),
        JobStatus::Timeout => "超时".to_string(),
        JobStatus::Running => "运行中".to_string(),
    }
}

/// 控制台主渲染。dark = 当前主题是否为暗色（由调用方按全局主题传入）
pub fn show(console: &mut ConsoleState, ui: &mut egui::Ui, running: bool, dark: bool) {
    // 控制台背景跟随主题，但两种主题下都保持"终端感"：
    // 深色主题深底、明亮主题浅底，日志配色各自调校
    let bg = if dark {
        Color32::from_rgb(27, 27, 27)
    } else {
        Color32::from_rgb(250, 250, 250)
    };
    // 折叠时把面板钉在标题行高度（resizable 关掉，min/max 同高 = 固定）；
    // 展开后恢复默认 280 可拖拽
    let mut panel = egui::Panel::bottom("console_panel")
        .frame(
            egui::Frame::default()
                .fill(bg)
                .inner_margin(egui::Margin::same(6)),
        )
        .resizable(!console.collapsed)
        .default_size(280.0);
    if console.collapsed {
        panel = panel.min_size(46.0).max_size(46.0);
    } else {
        panel = panel.min_size(120.0);
    }
    panel.show(ui, |ui| {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.strong("控制台输出");
            if ui
                .small_button(if console.collapsed { "⌄ 展开" } else { "⌃ 收起" })
                .on_hover_text(if console.collapsed {
                    "展开输出区域"
                } else {
                    "折叠输出区域，只留标题行（编辑步骤时不再被控制台占掉半屏）"
                })
                .clicked()
            {
                console.collapsed = !console.collapsed;
            }
            ui.checkbox(&mut console.auto_scroll, "自动滚动");
            if ui
                .checkbox(&mut console.errors_only, "只看错误")
                .on_hover_text(
                    "只留失败步骤的骨架（Step 头 + stderr + ❌）与 ⚠ 错误、取消收尾；\n\
                     成功步骤整体隐藏——全程无错误时此视图为空，即「没有错误」",
                )
                .changed()
            {
                console.auto_scroll = true;
            }
            if running {
                ui.add(egui::Spinner::new().size(14.0));
                ui.weak("运行中…");
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("清空").clicked() {
                    console.clear();
                }
                if ui.button("📋 复制").clicked() {
                    let text = console.all_text();
                    ui.ctx().copy_text(text);
                }
                if ui.button("💾 导出").clicked() {
                    // 导出为 .log 文件（§9.3 导出按钮；默认名带时间戳）
                    let name = format!(
                        "remote-build-{}.log",
                        chrono::Local::now().format("%Y%m%d-%H%M%S")
                    );
                    if let Some(path) = rfd::FileDialog::new()
                        .set_file_name(&name)
                        .set_title("导出控制台输出")
                        .save_file()
                    {
                        match std::fs::write(&path, console.all_text()) {
                            Ok(()) => console.push_line(
                                format!("已导出: {}", path.display()),
                                Role::Info,
                            ),
                            Err(e) => console.push_line(
                                format!("导出失败: {e}"),
                                Role::Error,
                            ),
                        }
                    }
                }
            });
        });
        // 折叠 = 隐藏输出内容区，只留上面那行标题/开关
        if console.collapsed {
            return;
        }
        ui.separator();

            // 虚拟化渲染：每行为一个可换行 galley，高度按宽度缓存；逐帧只排版/绘制
            // 可视区内的行（此前 5000 行 × 每帧全量 Label 布局是控制台卡顿的主因）。
            // 行本身无交互（悬停/选中），painter 直绘不损失任何行为。
            egui::ScrollArea::vertical()
                .id_salt("console_scroll")
                .auto_shrink([false, false])
                .stick_to_bottom(console.auto_scroll)
                .show_viewport(ui, |ui, viewport| {
                    const ROW_SPACING: f32 = 2.0;
                    let wrap_width = ui.max_rect().width().max(1.0);
                    let font_id = egui::TextStyle::Monospace.resolve(ui.style());

                    // 行高同步：宽度变化/裁剪/清空 → 全量重测；纯追加 → 只测新增尾部
                    if console.heights_dirty
                        || console.measured_width != wrap_width
                        || console.heights.len() > console.lines.len()
                    {
                        console.heights.clear();
                        console.measured_width = wrap_width;
                        console.heights_dirty = false;
                    }
                    if console.heights.len() < console.lines.len() {
                        let start = console.heights.len();
                        for line in &console.lines[start..] {
                            let h = ui.fonts_mut(|f| {
                                f.layout(
                                    line.text.clone(),
                                    font_id.clone(),
                                    Color32::PLACEHOLDER,
                                    wrap_width,
                                )
                                .size()
                                .y
                            });
                            console.heights.push(h);
                        }
                    }

                    // 只看错误的可见行索引（O(n) 过滤扫描，无控件开销）：
                    // 藏 stdout/提示/成功行与成功步骤的骨架头；
                    // 留失败步骤骨架（头 + stderr + ❌）、⚠ 错误、取消收尾
                    let visible_idx: Vec<usize> = if console.errors_only {
                        console
                            .lines
                            .iter()
                            .enumerate()
                            .filter(|(_, l)| match &l.role {
                                Role::Stdout | Role::Info | Role::Ok => false,
                                Role::StepStart => l
                                    .step
                                    .map(|i| console.failed_steps.contains(&i))
                                    .unwrap_or(true),
                                _ => true,
                            })
                            .map(|(i, _)| i)
                            .collect()
                    } else {
                        (0..console.lines.len()).collect()
                    };

                    let total_h: f32 = visible_idx.iter().map(|i| console.heights[*i]).sum::<f32>()
                        + ROW_SPACING * visible_idx.len().saturating_sub(1) as f32;
                    let origin = ui.cursor().min; // 内容左上角（此刻 cursor 未被推进）
                    ui.set_min_height(total_h);

                    let mut y = 0.0f32;
                    for &i in &visible_idx {
                        let h = console.heights[i];
                        // 与可视区相交才排版绘制；行序自上而下，越界即可停止
                        if y + h >= viewport.min.y && y <= viewport.max.y {
                            let line = &console.lines[i];
                            let color = palette::color_for(line.role, dark);
                            let galley = ui.fonts_mut(|f| {
                                f.layout(line.text.clone(), font_id.clone(), color, wrap_width)
                            });
                            ui.painter().galley(egui::pos2(origin.x, origin.y + y), galley, color);
                        }
                        y += h + ROW_SPACING;
                        if y > viewport.max.y {
                            break;
                        }
                    }
                });
        });
}
