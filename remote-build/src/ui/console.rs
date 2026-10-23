//! 底部控制台输出（设计文档 §9.1 Console Output，实时流式渲染）。

use crate::models::{JobStatus, LogEvent};
use eframe::egui;
use egui::{Color32, RichText};

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
}

pub struct ConsoleState {
    pub lines: Vec<ConsoleLine>,
    /// 自动滚动到最新输出（勾选时新日志始终可见；关闭后可上翻检查历史，新行不会拉走视图）
    pub auto_scroll: bool,
}

impl Default for ConsoleState {
    fn default() -> Self {
        Self { lines: Vec::new(), auto_scroll: true }
    }
}

impl ConsoleState {
    pub fn push_event(&mut self, ev: &LogEvent) {
        match ev {
            LogEvent::StepStart { step_index, name, .. } => {
                self.push(
                    format!("── Step {} ── {name}", step_index + 1),
                    Role::StepStart,
                );
            }
            LogEvent::Stdout { line, .. } => {
                self.push(line.clone(), Role::Stdout);
            }
            LogEvent::Stderr { line, .. } => {
                self.push(line.clone(), Role::Stderr);
            }
            LogEvent::StepEnd { step_index, exit_code, duration_ms } => {
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
                    JobStatus::Success => Role::Ok,
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
        }
    }

    fn push(&mut self, text: String, role: Role) {
        self.lines.push(ConsoleLine { text, role });
        if self.lines.len() > MAX_LINES {
            let drop = self.lines.len() - MAX_LINES;
            self.lines.drain(0..drop);
        }
    }

    /// 非引擎事件的普通提示行
    pub fn push_line(&mut self, text: impl Into<String>, role: Role) {
        self.push(text.into(), role);
    }

    pub fn all_text(&self) -> String {
        self.lines.iter().map(|l| l.text.as_str()).collect::<Vec<_>>().join("\n")
    }
}

fn job_label(status: &JobStatus) -> String {
    match status {
        JobStatus::Success => "成功 ✅".to_string(),
        JobStatus::Failed { step_index, reason } => format!("失败（Step {}：{reason}）", step_index + 1),
        JobStatus::Cancelled => "已取消".to_string(),
        JobStatus::Timeout => "超时".to_string(),
        JobStatus::Running => "运行中".to_string(),
    }
}

/// dark = 当前主题是否为暗色（由调用方按全局主题传入）
pub fn show(console: &mut ConsoleState, ui: &mut egui::Ui, running: bool, dark: bool) {
    // 控制台背景跟随主题，但两种主题下都保持"终端感"：
    // 深色主题深底、明亮主题浅底，日志配色各自调校
    let bg = if dark {
        Color32::from_rgb(27, 27, 27)
    } else {
        Color32::from_rgb(250, 250, 250)
    };
    egui::Panel::bottom("console_panel")
        .frame(
            egui::Frame::default()
                .fill(bg)
                .inner_margin(egui::Margin::same(6)),
        )
        .resizable(true)
        .default_size(280.0)
        .min_size(120.0)
        .show(ui, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.strong("控制台输出");
                ui.checkbox(&mut console.auto_scroll, "自动滚动");
                if running {
                    ui.add(egui::Spinner::new().size(14.0));
                    ui.weak("运行中…");
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("清空").clicked() {
                        console.lines.clear();
                    }
                    if ui.button("📋 复制").clicked() {
                        let text = console.all_text();
                        ui.ctx().copy_text(text);
                    }
                });
            });
            ui.separator();

            egui::ScrollArea::vertical()
                .id_salt("console_scroll")
                .auto_shrink([false, false])
                .stick_to_bottom(console.auto_scroll)
                .show(ui, |ui| {
                    // 紧凑行距；左对齐（不能用 add_sized——它会把文字在整行里居中）
                    ui.spacing_mut().item_spacing.y = 2.0;
                    for line in &console.lines {
                        let color = palette::color_for(line.role, dark);
                        ui.add(
                            egui::Label::new(
                                RichText::new(&line.text).monospace().color(color),
                            )
                            .wrap(),
                        );
                    }
                });
        });
}
