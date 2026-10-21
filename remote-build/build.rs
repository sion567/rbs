//! 编译时校验捆绑的 PuTTY 工具是否存在（见设计文档 §16 / §5.4）。
//!
//! 仓库不跟踪二进制文件：开发者需手动将 plink/pscp/psftp/puttygen/pageant.exe
//! 放入 tools/putty/（见 tools/putty/README.txt）。缺失只告警不失败，
//! 运行期 detect_putty_tools() 会回退到系统 PATH。

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
