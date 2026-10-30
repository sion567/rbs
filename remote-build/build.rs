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
    collect_ui_chars();
}

/// 收集 UI / 引擎源码里出现过的全部 CJK 字符写进 OUT_DIR，供启动期字形预热。
///
/// egui 按需栅格化字形：首次显示某段中文（打开一个新窗口 / 控制台出现一条新的
/// 错误文案）时才现场栅格化几百个字 → 可感知的顿挫。预热后全部命中缓存。
/// 只收 CJK 区段：ASCII 字形小、栅格化便宜，无需预热。
fn collect_ui_chars() {
    println!("cargo:rerun-if-changed=src/ui");
    println!("cargo:rerun-if-changed=src/app.rs");
    println!("cargo:rerun-if-changed=src/engine.rs");
    println!("cargo:rerun-if-changed=src/group.rs");
    println!("cargo:rerun-if-changed=src/models.rs");

    let is_cjk = |c: char| {
        ('\u{2E80}'..='\u{9FFF}').contains(&c)      // CJK 部首扩展 ~ 汉字
            || ('\u{3000}'..='\u{303F}').contains(&c) // CJK 标点（「」·……）
            || ('\u{FF00}'..='\u{FFEF}').contains(&c) // 全角符号（？！：）
    };
    let mut chars: std::collections::BTreeSet<char> = Default::default();
    let mut scan = |path: &Path| {
        if let Ok(text) = std::fs::read_to_string(path) {
            chars.extend(text.chars().filter(|c| is_cjk(*c)));
        }
    };
    scan(Path::new("src/app.rs"));
    for f in ["engine.rs", "group.rs", "models.rs"] {
        scan(&Path::new("src").join(f));
    }
    if let Ok(rd) = std::fs::read_dir("src/ui") {
        for e in rd.flatten() {
            scan(&e.path());
        }
    }
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap_or_default())
        .join("ui_prewarm_chars.txt");
    let text: String = chars.into_iter().collect();
    // 写失败时兜底写空文件：app.rs 的 include_str! 硬依赖此文件存在，
    // 缺了会直接编译失败（空文件 = 预热跳过，功能无损）
    if std::fs::write(&out, &text).is_err() {
        let _ = std::fs::write(&out, "");
        println!("cargo:warning=字形预热字符集写入失败（已降级为空预热）");
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
