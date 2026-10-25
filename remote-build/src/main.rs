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
        viewport: egui::ViewportBuilder::default().with_inner_size([1280.0, 800.0]),
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
