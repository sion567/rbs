//! 编译时校验捆绑的 PuTTY 工具是否齐全（见设计文档 §16 / §5.4）。
//!
//! 套件随仓库提交在 tools/putty/（plink/pscp/psftp/puttygen/pageant.exe + VERSION）。
//! 本脚本只做存在性提醒：文件缺失不失败，运行期 detect_putty_tools()
//! 会回退到系统 PATH / 常见安装路径。

use std::path::Path;

fn main() {
    println!("cargo:rerun-if-changed=tools/putty");
    println!("cargo:rerun-if-changed=assets/app.ico");
    println!("cargo:rerun-if-changed=assets/app_64.rgba");
    embed_icon();

    let dir = Path::new("tools/putty");
    if !dir.exists() {
        println!("cargo:warning=tools/putty/ 不存在，内置 PuTTY 将不可用（回退系统 PATH）");
        return;
    }

    for exe in [
        "plink.exe",
        "pscp.exe",
        "psftp.exe",
        "puttygen.exe",
        "pageant.exe",
    ] {
        if !dir.join(exe).exists() {
            println!("cargo:warning=tools/putty/{} 缺失，内置 PuTTY 不完整", exe);
        }
    }
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
