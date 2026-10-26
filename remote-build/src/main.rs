//! Remote Build System 入口（设计文档 §16 main.rs）。
//! Windows 桌面单进程：eframe + egui UI，tokio 后台执行引擎。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod cli;
mod config;
mod engine;
mod env_detect;
mod group;
mod k8s;
mod models;
mod pack;
mod runlock;
mod secrets;
mod storage;
mod transport;
mod ui;
mod variables;
mod version;

use std::sync::Arc;

fn main() -> eframe::Result<()> {
    #[cfg(windows)]
    enable_utf8_console();
    init_logger();

    // 无头子命令（脚本 / CI 使用），不带子命令则进入 GUI
    let cli_args: Vec<String> = std::env::args().skip(1).collect();
    if cli::dispatch(&cli_args).is_some() {
        return Ok(());
    }
    let runtime = Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("创建 tokio runtime 失败"),
    );

    // 锁定 DX12 渲染后端：RDP 远程桌面（间接显示适配器）+ Intel 驱动下，
    // wgpu 默认优先的 Vulkan 会延迟段错误（0.37 升级实测，DX12 原生/远程均稳定）
    let mut wgpu_setup = egui_wgpu::WgpuSetupCreateNew::without_display_handle();
    wgpu_setup.instance_descriptor.backends = egui_wgpu::wgpu::Backends::DX12;
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 800.0])
            .with_icon(app_icon()),
        wgpu_options: egui_wgpu::WgpuConfiguration {
            wgpu_setup: egui_wgpu::WgpuSetup::CreateNew(wgpu_setup),
            ..Default::default()
        },
        ..Default::default()
    };

    eframe::run_native(
        "Remote Build System",
        options,
        Box::new(move |cc| Ok(Box::new(app::RemoteBuildApp::new(cc, runtime.clone())))),
    )
}

/// 64×64 应用图标：蓝底圆角方块 + 白色字母 R，运行时生成，免二进制资源文件。
/// 窗口标题栏 / Windows 任务栏共用这一个图标。
fn app_icon() -> egui::IconData {
    const S: usize = 64;
    let mut rgba = vec![0u8; S * S * 4];

    // 5×7 点阵字母 R（1 = 填充），8 倍放大居中
    const GLYPH_R: [[u8; 5]; 7] = [
        [0, 1, 1, 1, 0],
        [1, 0, 0, 0, 1],
        [1, 0, 0, 0, 1],
        [1, 1, 1, 1, 0],
        [1, 0, 1, 0, 0],
        [1, 0, 0, 1, 0],
        [1, 0, 0, 0, 1],
    ];
    let cell = 8usize;
    let (w, h) = (5 * cell, 7 * cell);
    let (x0, y0) = ((S - w) / 2, (S - h) / 2);

    for y in 0..S {
        for x in 0..S {
            // 圆角方块的带符号距离（内部为负），留 1px 抗锯齿过渡
            let (px, py) = (
                x as f32 + 0.5 - S as f32 / 2.0,
                y as f32 + 0.5 - S as f32 / 2.0,
            );
            let (half, r) = (30.0f32, 12.0f32);
            let qx = px.abs() - (half - r);
            let qy = py.abs() - (half - r);
            let d = qx.max(0.0).hypot(qy.max(0.0)) + qx.max(qy).min(0.0) - r;
            let cover = (0.5 - d).clamp(0.0, 1.0);
            if cover <= 0.0 {
                continue;
            }
            // 顶部亮蓝 → 底部深蓝的纵向渐变
            let t = y as f32 / (S - 1) as f32;
            let lerp = |a: f32, b: f32| (a + (b - a) * t).round() as u8;
            let (mut cr, mut cg, mut cb) = (
                lerp(74.0, 29.0),
                lerp(144.0, 78.0),
                lerp(245.0, 216.0),
            );
            let in_r = (x0..x0 + w).contains(&x)
                && (y0..y0 + h).contains(&y)
                && GLYPH_R[(y - y0) / cell][(x - x0) / cell] == 1;
            if in_r {
                (cr, cg, cb) = (255, 255, 255);
            }
            let i = (y * S + x) * 4;
            rgba[i] = cr;
            rgba[i + 1] = cg;
            rgba[i + 2] = cb;
            rgba[i + 3] = (cover * 255.0) as u8;
        }
    }
    egui::IconData {
        width: S as u32,
        height: S as u32,
        rgba,
    }
}

/// CLI 子命令在 GBK 代码页的终端里输出 UTF-8 会乱码，切换为 UTF-8（仅当前进程）
#[cfg(windows)]
fn enable_utf8_console() {
    const CP_UTF8: u32 = 65_001;
    unsafe {
        winapi::um::wincon::SetConsoleOutputCP(CP_UTF8);
        winapi::um::wincon::SetConsoleCP(CP_UTF8);
    }
}

/// 轻量 stderr logger：egui-winit / arboard / wgpu 的错误默认无人接收，
/// 全部丢弃会让「复制坏了」这类问题无从排查（这里只落 Warn 及以上）。
struct StderrLogger;

static STDERR_LOGGER: StderrLogger = StderrLogger;

impl log::Log for StderrLogger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= log::Level::Warn
    }

    fn log(&self, record: &log::Record) {
        eprintln!("[{} {}] {}", record.level(), record.target(), record.args());
    }

    fn flush(&self) {}
}

fn init_logger() {
    let _ = log::set_logger(&STDERR_LOGGER);
    log::set_max_level(log::LevelFilter::Warn);
}
