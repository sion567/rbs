//! 运行状态可视化：步骤卡片光圈 + 树上作业状态圆点。
//!
//! 状态由 app.rs 的 drain_events 按 LogEvent 驱动，start_run 时全部复位为 Idle：
//! 持续性状态（运行中蓝 / 等待黄）带约 1Hz 呼吸闪烁，
//! 终态（成功绿 / 失败红 / 跳过灰 / 取消灰）静态，直到下一次运行。
//! 色相与控制台 Role 色板一致，深浅主题各调一版保证两种主题下都醒目。

use eframe::egui;
use egui::{Color32, Stroke};

/// 单个步骤（或整个作业，树上圆点）在最近一次运行中的可视化状态
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StepVisualState {
    /// 未参与本次运行，无光圈
    #[default]
    Idle,
    /// 执行中：蓝色光圈，闪烁
    Running,
    /// 等待（wait 类型轮询 / 失败重试退避）：黄色光圈，闪烁
    Waiting,
    /// 成功：绿色光圈
    Success,
    /// 失败：红色光圈
    Failed,
    /// 跳过（未启用 / condition 不满足）：灰色光圈
    Skipped,
    /// 取消 / 超时中断：中性灰光圈
    Cancelled,
}

impl StepVisualState {
    /// 返回 (颜色, 是否闪烁)
    pub fn color(self, dark: bool) -> (Color32, bool) {
        use StepVisualState::*;
        match (self, dark) {
            (Running, true) => (Color32::from_rgb(97, 175, 239), true),
            (Running, false) => (Color32::from_rgb(4, 81, 165), true),
            (Waiting, true) => (Color32::from_rgb(229, 192, 123), true),
            (Waiting, false) => (Color32::from_rgb(163, 134, 27), true),
            (Success, true) => (Color32::from_rgb(152, 195, 121), false),
            (Success, false) => (Color32::from_rgb(13, 188, 121), false),
            (Failed, true) => (Color32::from_rgb(224, 108, 117), false),
            (Failed, false) => (Color32::from_rgb(205, 49, 49), false),
            (Skipped, true) => (Color32::from_rgb(115, 115, 122), false),
            (Skipped, false) => (Color32::from_rgb(170, 170, 170), false),
            (Cancelled, true) => (Color32::from_rgb(185, 185, 165), false),
            (Cancelled, false) => (Color32::from_rgb(160, 160, 160), false),
            (Idle, _) => (Color32::TRANSPARENT, false),
        }
    }

    /// 是否是"还在路上"的持续状态（闪烁 + 需要周期重绘）
    pub fn in_progress(self) -> bool {
        matches!(self, StepVisualState::Running | StepVisualState::Waiting)
    }

    /// 悬停提示文本
    pub fn label(self) -> &'static str {
        match self {
            StepVisualState::Idle => "",
            StepVisualState::Running => "执行中",
            StepVisualState::Waiting => "等待中（轮询 / 重试退避）",
            StepVisualState::Success => "成功",
            StepVisualState::Failed => "失败",
            StepVisualState::Skipped => "已跳过（未启用 / 条件不满足）",
            StepVisualState::Cancelled => "已取消 / 超时",
        }
    }
}

/// 闪烁相位 0..1（约 1Hz 正弦呼吸）。依赖调用方在运行中安排周期性重绘（app.rs 50ms）。
fn pulse(ui: &egui::Ui) -> f32 {
    let t = ui.input(|i| i.time);
    (0.5 + 0.5 * (t * 2.0 * std::f64::consts::PI).sin()) as f32
}

/// 状态着色：闪烁状态透明度随呼吸相位在 [lo, hi] 脉动，终态恒为全亮
fn animated(color: Color32, blink: bool, p: f32, lo: f32, hi: f32) -> Color32 {
    if blink {
        color.gamma_multiply(lo + (hi - lo) * p)
    } else {
        color
    }
}

/// 步骤卡片 Frame。
/// 描边宽度所有状态恒定（egui 把 stroke 宽度计入布局，宽度随状态变化会让卡片跳动）；
/// Idle 与 egui::Frame::group 外观一致（仅描边略宽）；运行/等待叠加彩色光晕呼吸。
pub fn step_frame(ui: &egui::Ui, state: StepVisualState, dark: bool) -> egui::Frame {
    let frame = egui::Frame::group(ui.style()).stroke(Stroke::new(
        2.0,
        ui.style().visuals.widgets.noninteractive.bg_stroke.color,
    ));
    if state == StepVisualState::Idle {
        return frame;
    }
    let (color, blink) = state.color(dark);
    let p = pulse(ui);
    frame
        .stroke(Stroke::new(2.0, animated(color, blink, p, 0.45, 1.0)))
        .shadow(if state.in_progress() {
            egui::epaint::Shadow {
                offset: [0, 0],
                blur: 8,
                spread: 2,
                color: animated(color, blink, p, 0.12, 0.7),
            }
        } else {
            egui::epaint::Shadow::NONE
        })
}

/// 树上作业行的小圆点文本（●），颜色随状态、持续状态闪烁
pub fn status_dot(ui: &egui::Ui, state: StepVisualState, dark: bool) -> egui::RichText {
    let (color, blink) = state.color(dark);
    let p = pulse(ui);
    egui::RichText::new("●").color(animated(color, blink, p, 0.35, 1.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persistent_states_blink_and_terminal_are_static() {
        assert!(StepVisualState::Running.in_progress());
        assert!(StepVisualState::Waiting.in_progress());
        for s in [
            StepVisualState::Idle,
            StepVisualState::Success,
            StepVisualState::Failed,
            StepVisualState::Skipped,
            StepVisualState::Cancelled,
        ] {
            assert!(!s.in_progress(), "{s:?} 不应闪烁");
        }
    }

    #[test]
    fn both_themes_have_visible_colors() {
        // 两套主题都必须给出非透明颜色（明亮主题曾因颜色过浅看不清的教训）
        for s in [
            StepVisualState::Running,
            StepVisualState::Waiting,
            StepVisualState::Success,
            StepVisualState::Failed,
        ] {
            for dark in [true, false] {
                let (c, _) = s.color(dark);
                assert_ne!(c, Color32::TRANSPARENT);
            }
        }
        // 明暗两版颜色必须有区分度（同一状态两套色值不同）
        let (cd, _) = StepVisualState::Running.color(true);
        let (cl, _) = StepVisualState::Running.color(false);
        assert_ne!(cd, cl);
    }
}
