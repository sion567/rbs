//! 底部控制台输出（设计文档 §9.1 Console Output，实时流式渲染）。

use crate::models::{JobStatus, LogEvent};
use eframe::egui;
use egui::{Color32, RichText};

#[derive(Debug, Clone)]
pub struct ConsoleLine {
    pub text: String,
    pub color: Color32,
}

/// 控制台行数上限，防止长任务撑爆内存
const MAX_LINES: usize = 5000;

/// 控制台配色（One Dark 色板，为深色背景调校；色相按语义分离，一眼可辨类型）
pub mod palette {
    use egui::Color32;

    /// 步骤标题（── Step N ──）：青
    pub const STEP_START: Color32 = Color32::from_rgb(86, 182, 194);
    /// 普通输出：柔灰（大批量内容不刺眼，与背景对比约 7.5:1）
    pub const STDOUT: Color32 = Color32::from_rgb(171, 178, 191);
    /// stderr 输出：黄
    pub const STDERR: Color32 = Color32::from_rgb(229, 192, 123);
    /// 成功：绿
    pub const OK: Color32 = Color32::from_rgb(152, 195, 121);
    /// 失败：红
    pub const FAIL: Color32 = Color32::from_rgb(224, 108, 117);
    /// 引擎级错误：亮红（比步骤失败更醒目）
    pub const ERROR: Color32 = Color32::from_rgb(241, 76, 76);
    /// 系统提示（启动作业等）：蓝
    pub const INFO: Color32 = Color32::from_rgb(97, 175, 239);
    /// 取消：中性灰黄
    pub const CANCELLED: Color32 = Color32::from_rgb(200, 200, 150);
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
            LogEvent::StepStart { step_index, name } => {
                self.push(
                    format!("── Step {} ── {name}", step_index + 1),
                    palette::STEP_START,
                );
            }
            LogEvent::Stdout { line, .. } => {
                self.push(line.clone(), palette::STDOUT);
            }
            LogEvent::Stderr { line, .. } => {
                self.push(line.clone(), palette::STDERR);
            }
            LogEvent::StepEnd { step_index, exit_code, duration_ms } => {
                let (text, color) = if *exit_code == 0 {
                    (
                        format!("✅ Step {} 完成 ({duration_ms}ms)", step_index + 1),
                        palette::OK,
                    )
                } else {
                    (
                        format!("❌ Step {} 失败，退出码 {exit_code} ({duration_ms}ms)", step_index + 1),
                        palette::FAIL,
                    )
                };
                self.push(text, color);
            }
            LogEvent::JobEnd { status, total_duration_ms, .. } => {
                let color = match status {
                    JobStatus::Success => palette::OK,
                    JobStatus::Cancelled => palette::CANCELLED,
                    JobStatus::Timeout => palette::ERROR,
                    _ => palette::FAIL,
                };
                self.push(
                    format!("==== Job 结束: {} ({total_duration_ms}ms) ====", job_label(status)),
                    color,
                );
            }
            LogEvent::Error { message } => {
                self.push(format!("⚠ {message}"), palette::ERROR);
            }
        }
    }

    fn push(&mut self, text: String, color: Color32) {
        self.lines.push(ConsoleLine { text, color });
        if self.lines.len() > MAX_LINES {
            let drop = self.lines.len() - MAX_LINES;
            self.lines.drain(0..drop);
        }
    }

    /// 非引擎事件的普通提示行
    pub fn push_line(&mut self, text: impl Into<String>, color: Color32) {
        self.push(text.into(), color);
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

pub fn show(console: &mut ConsoleState, ctx: &egui::Context, running: bool) {
    // 控制台固定深色底：日志配色按深底调校（One Dark），明亮主题下也保持可读
    egui::TopBottomPanel::bottom("console_panel")
        .frame(
            egui::Frame::default()
                .fill(Color32::from_rgb(27, 27, 27))
                .inner_margin(egui::Margin::same(6.0)),
        )
        .resizable(true)
        .default_height(280.0)
        .min_height(120.0)
        .show(ctx, |ui| {
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
                        ui.output_mut(|o| o.copied_text = text);
                    }
                });
            });
            ui.separator();

            egui::ScrollArea::vertical()
                .id_source("console_scroll")
                .auto_shrink([false, false])
                .stick_to_bottom(console.auto_scroll)
                .show(ui, |ui| {
                    // 紧凑行距；左对齐（不能用 add_sized——它会把文字在整行里居中）
                    ui.spacing_mut().item_spacing.y = 2.0;
                    for line in &console.lines {
                        ui.add(
                            egui::Label::new(
                                RichText::new(&line.text).monospace().color(line.color),
                            )
                            .wrap(true),
                        );
                    }
                });
        });
}
