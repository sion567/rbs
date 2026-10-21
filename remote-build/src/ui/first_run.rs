//! 首次启动引导页（设计文档 §7.1 探测结果页 / §9.3）。

use crate::env_detect::{self, DetectionReport};
use eframe::egui;
use egui::Color32;

pub struct FirstRunPage {
    report_rx: Option<std::sync::mpsc::Receiver<DetectionReport>>,
    pub report: Option<DetectionReport>,
    pub error: Option<String>,
    confirmed: bool,
}

impl FirstRunPage {
    pub fn new() -> Self {
        Self {
            report_rx: Some(env_detect::detect_async()),
            report: None,
            error: None,
            confirmed: false,
        }
    }

    pub fn confirmed(&self) -> bool {
        self.confirmed
    }

    /// 渲染引导页；用户点击「初始化并进入」后 confirmed() 变为 true
    pub fn show(&mut self, ctx: &egui::Context) {
        // 接收后台探测结果
        if let Some(rx) = &self.report_rx {
            match rx.try_recv() {
                Ok(report) => {
                    self.report = Some(report);
                    self.report_rx = None;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.report_rx = None;
                }
            }
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.add_space(24.0);
            ui.vertical_centered(|ui| {
                ui.heading("Remote Build System — 首次启动");
                ui.label("已扫描系统环境变量与 PATH，确认后生成配置文件到 ~/.remote-build/");
            });
            ui.add_space(16.0);

            match &self.report {
                None => {
                    ui.vertical_centered(|ui| {
                        ui.add(egui::Spinner::new().size(32.0));
                        ui.label("正在探测开发环境（java / mvn / gradle / node / git / docker / kubectl / PuTTY）…");
                    });
                    ctx.request_repaint_after(std::time::Duration::from_millis(120));
                    return;
                }
                Some(report) => {
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        egui::Grid::new("detected_tools")
                            .num_columns(4)
                            .spacing([12.0, 6.0])
                            .show(ui, |ui| {
                                ui.strong("状态");
                                ui.strong("工具");
                                ui.strong("版本");
                                ui.strong("来源");
                                ui.end_row();

                                for t in &report.tools {
                                    let (icon, color) = if t.ok {
                                        ("✅", Color32::from_rgb(120, 200, 120))
                                    } else {
                                        ("❌", Color32::from_rgb(200, 120, 120))
                                    };
                                    ui.colored_label(color, icon);
                                    ui.label(&t.name);
                                    ui.label(t.version.as_deref().unwrap_or("-"));
                                    ui.label(if t.ok { &t.source } else { "未找到" });
                                    ui.end_row();
                                }

                                let (icon, color, ver, src) = if report.putty.found {
                                    (
                                        "✅",
                                        Color32::from_rgb(120, 200, 120),
                                        report.putty.version.as_deref().unwrap_or("-"),
                                        report.putty.source.clone(),
                                    )
                                } else {
                                    ("❌", Color32::from_rgb(200, 120, 120), "-", "未找到".to_string())
                                };
                                ui.colored_label(color, icon);
                                ui.label("putty");
                                ui.label(ver);
                                ui.label(src);
                                ui.end_row();
                            });

                        if !report.putty.found {
                            ui.add_space(8.0);
                            ui.colored_label(
                                Color32::from_rgb(230, 180, 80),
                                "⚠ 未找到 PuTTY：SSH 执行/上传不可用。可安装 PuTTY、设置自定义路径，\
                                 或将 plink/pscp/psftp/puttygen/pageant.exe 放入 \
                                 ~/.remote-build/tools/putty/ 后重启。",
                            );
                        }

                        ui.add_space(20.0);
                        ui.vertical_centered(|ui| {
                            if ui
                                .add(egui::Button::new("初始化并进入").min_size(egui::vec2(160.0, 32.0)))
                                .clicked()
                            {
                                self.confirmed = true;
                            }
                        });
                    });
                }
            }

            if let Some(err) = &self.error {
                ui.add_space(8.0);
                ui.colored_label(Color32::from_rgb(220, 90, 90), format!("初始化失败: {err}"));
            }
        });
    }
}
