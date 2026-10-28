//! 编译期资源嵌入（exe 图标）。
//!
//! PuTTY 套件存在性校验已随「内置捆绑目录」探测一并移除（2026-10-28）：
//! putty 传输现在只认 自定义目录 / 系统 PATH / Program Files，
//! 仓库 tools/putty/ 不再被构建与运行期读取。

use std::path::Path;

fn main() {
    println!("cargo:rerun-if-changed=assets/app.ico");
    println!("cargo:rerun-if-changed=assets/app_64.rgba");
    embed_icon();
}

/// 把 assets/app.ico 嵌进 exe 资源（资源管理器 / 任务栏 / 固定到任务栏的图标）。
/// 仅 Windows 生效；缺图标或本机没有 rc.exe（Windows SDK）时只告警不失败
#[cfg(windows)]
fn embed_icon() {
    if !Path::new("assets/app.ico").exists() {
        println!("cargo:warning=assets/app.ico 不存在，exe 使用默认图标");
        return;
    }
    let mut res = winresource::WindowsResource::new();
    res.set_icon("assets/app.ico");
    if let Err(e) = res.compile() {
        println!("cargo:warning=嵌入 exe 图标失败（不影响构建）: {e}");
    }
}

#[cfg(not(windows))]
fn embed_icon() {}
