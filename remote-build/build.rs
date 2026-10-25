//! 编译时校验捆绑的 PuTTY 工具是否齐全（见设计文档 §16 / §5.4）。
//!
//! 套件随仓库提交在 tools/putty/（plink/pscp/psftp/puttygen/pageant.exe + VERSION）。
//! 本脚本只做存在性提醒：文件缺失不失败，运行期 detect_putty_tools()
//! 会回退到系统 PATH / 常见安装路径。

use std::path::Path;

fn main() {
    println!("cargo:rerun-if-changed=tools/putty");

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
