//! 异常诊断日志：文件日志 + panic 捕获 + UI 卡死看门狗。
//!
//! 三种异常场景各有一道防线，产物都落在 `~/.remote-build/logs/app-YYYY-MM-DD.log`
//! （按天分文件，保留 14 天；`logs/<项目名>/` 是按项目的任务日志，前缀 app- 不冲突）：
//!
//! 1. 平时运行：`log` 宏的输出双写 stderr 与日志文件。此前只有 stderr，而
//!    release 版 `windows_subsystem = "windows"` 下 stderr 是黑洞，出了问题毫无痕迹；
//! 2. panic：钩子把消息 / 位置 / 线程 / 回溯写进同一份日志。release 版回溯要有
//!    函数名与行号，依赖 Cargo.toml `[profile.release] debug = true` 生成的 .pdb；
//! 3. UI 卡死（Windows 标题栏标「未响应」）：看门狗线程监测 UI 心跳，静默超阈值
//!    记一条 ERROR（卡在哪个界面 / 进程时长 / 内存水位），恢复时补一条总时长。
//!
//! 比 panic 更底层的原生崩溃（SEH / 访问违例，如驱动层的段错误）Rust 钩子接不住。
//! 零代码补救：给 Windows 错误报告（WER）配本地转储，崩溃时生成 .dmp，用 WinDbg
//! 打开执行 `!analyze -v` 直接得到崩溃线程栈。管理员执行一次即可：
//!
//! ```text
//! reg add "HKLM\SOFTWARE\Microsoft\Windows\Windows Error Reporting\LocalDumps\remote-build.exe" /v DumpFolder /t REG_EXPAND_SZ /d "%LOCALAPPDATA%\CrashDumps" /f
//! reg add "HKLM\SOFTWARE\Microsoft\Windows\Windows Error Reporting\LocalDumps\remote-build.exe" /v DumpType /t REG_DWORD /d 2 /f
//! reg add "HKLM\SOFTWARE\Microsoft\Windows\Windows Error Reporting\LocalDumps\remote-build.exe" /v DumpCount /t REG_DWORD /d 10 /f
//! ```

use std::fs::File;
use std::io::BufWriter;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::Mutex;

/// UI 无帧超过这个秒数视为「未响应」（Windows 判定阈值约 5 秒，这里放宽避免慢帧误报；
/// app.rs 的 logic() 保证空闲期每 2 秒至少有一帧心跳，余量 7 倍以上）
const STALL_SECS: u64 = 15;

// ---------------------------------------------------------------------------
// 全局状态
// ---------------------------------------------------------------------------

/// 当前日志级别（数字与 log::Level 的判别值对齐：0=Off 1=Error 2=Warn 3=Info 4=Debug 5=Trace）
static MAX_LEVEL: AtomicU8 = AtomicU8::new(3); // 默认 Info：要看到启动 / 退出 / 卡顿记录

/// UI 心跳：最近一次帧调度时间（Unix 毫秒）。0 = 还没画过第一帧
static UI_HEARTBEAT_MS: AtomicU64 = AtomicU64::new(0);

/// 看门狗报告卡顿时展示的当前界面标签（app.rs 每帧更新）
static SCREEN: Mutex<&'static str> = Mutex::new("启动中");

struct LogState {
    /// 日志目录（init 后延迟填充 crate::config::logs_dir()，测试可指向临时目录）
    dir: PathBuf,
    /// 当前写入的日期（YYYY-MM-DD），变了就换文件
    day: String,
    writer: Option<BufWriter<File>>,
}

impl LogState {
    const fn new() -> Self {
        Self { dir: PathBuf::new(), day: String::new(), writer: None }
    }
}

static STATE: Mutex<LogState> = Mutex::new(LogState::new());

static FILE_LOGGER: FileLogger = FileLogger;

struct FileLogger;

impl log::Log for FileLogger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() as u8 <= MAX_LEVEL.load(Ordering::Relaxed)
    }

    fn log(&self, record: &log::Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let ts = chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f").to_string();
        let line = format_line(&ts, record.level(), record.target(), &record.args().to_string());
        // stderr 保留：CLI 子命令 / 开发调试时依然即时可见（GUI release 下无人接收，无害）
        eprintln!("{line}");
        let day = today();
        let mut state = STATE.lock().unwrap_or_else(|e| e.into_inner());
        if state.dir.as_os_str().is_empty() {
            state.dir = crate::config::logs_dir();
        }
        write_line(&mut state, &day, &line);
    }

    fn flush(&self) {}
}

// ---------------------------------------------------------------------------
// 初始化
// ---------------------------------------------------------------------------

/// main() 第一件事调用：装日志器 + panic 钩子 + 写启动记录 + 清理旧日志
pub fn init() {
    START_MS.store(now_ms(), Ordering::Relaxed);
    install_panic_hook();
    let filter = level_filter();
    MAX_LEVEL.store(filter as u8, Ordering::Relaxed);
    let _ = log::set_logger(&FILE_LOGGER);
    log::set_max_level(filter);
    cleanup_old_logs();
    let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default();
    log::info!(
        "==== 启动 remote-build v{}（pid {}，级别 {}）exe={} 数据目录={} ====",
        env!("CARGO_PKG_VERSION"),
        std::process::id(),
        filter,
        exe,
        crate::config::app_root().display()
    );
}

/// `RBS_LOG=off|error|warn|info|debug|trace` 覆盖默认级别（大小写不敏感）
fn level_filter() -> log::LevelFilter {
    let env = std::env::var("RBS_LOG").unwrap_or_default();
    match level_from_str(&env) {
        Some(n) => match n {
            0 => log::LevelFilter::Off,
            1 => log::LevelFilter::Error,
            2 => log::LevelFilter::Warn,
            4 => log::LevelFilter::Debug,
            5 => log::LevelFilter::Trace,
            _ => log::LevelFilter::Info,
        },
        None => log::LevelFilter::Info,
    }
}

/// 解析级别字符串为 log::Level 对应的判别值；无法识别返回 None（用默认值）
fn level_from_str(s: &str) -> Option<u8> {
    match s.trim().to_ascii_lowercase().as_str() {
        "off" => Some(0),
        "error" => Some(1),
        "warn" => Some(2),
        "info" => Some(3),
        "debug" => Some(4),
        "trace" => Some(5),
        _ => None,
    }
}

/// 启动时清理保留期之外的按天日志（app-*.log），解析不了的文件名不动
fn cleanup_old_logs() {
    let keep = 14;
    let cutoff = chrono::Local::now().date_naive() - chrono::Duration::days(keep);
    let Ok(entries) = std::fs::read_dir(crate::config::logs_dir()) else {
        return;
    };
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else { continue };
        let Some(day) = name.strip_prefix("app-").and_then(|s| s.strip_suffix(".log")) else {
            continue;
        };
        if let Ok(d) = chrono::NaiveDate::parse_from_str(day, "%Y-%m-%d") {
            if d < cutoff {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

// ---------------------------------------------------------------------------
// panic 钩子
// ---------------------------------------------------------------------------

/// 链式包装原钩子：panic 信息先落到日志文件，再交还原钩子保持默认行为。
/// 直写路径不阻塞拿锁——panic 可能恰好发生在持锁写日志的同一线程里
fn install_panic_hook() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        let name = thread.name().unwrap_or("<未命名线程>");
        let loc = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "<未知>".to_string());
        let bt = std::backtrace::Backtrace::force_capture();
        let text = format!(
            "======== panic ========\n线程[{name}] 位置[{loc}]\n{}\n回溯:\n{bt}\n",
            payload_str(info.payload())
        );
        eprint!("{text}");
        write_direct(&today(), &text);
        prev(info);
    }));
}

/// panic 载荷常见就 &str / String 两种，其余类型只有类型名可辨
fn payload_str(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "<非文本 panic 载荷>".to_string()
    }
}

/// panic 路径专用：锁被占用（可能就是当前线程持锁时 panic 的）就直接 append 文件
fn write_direct(day: &str, text: &str) {
    if let Ok(mut state) = STATE.try_lock() {
        if state.dir.as_os_str().is_empty() {
            state.dir = crate::config::logs_dir();
        }
        write_line(&mut state, day, text);
        return;
    }
    let dir = crate::config::logs_dir();
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(mut f) = File::options().create(true).append(true).open(dir.join(format!("app-{day}.log"))) {
        use std::io::Write;
        let _ = writeln!(f, "{text}");
    }
}

// ---------------------------------------------------------------------------
// 看门狗（UI「未响应」检测）
// ---------------------------------------------------------------------------

/// UI 帧调度入口调用（app.rs logic()）：刷新心跳时间戳
pub fn touch_ui() {
    UI_HEARTBEAT_MS.store(now_ms(), Ordering::Relaxed);
}

/// 更新看门狗日志里展示的当前界面标签（传字符串字面量）
pub fn set_screen(label: &'static str) {
    *SCREEN.lock().unwrap_or_else(|e| e.into_inner()) = label;
}

fn screen() -> &'static str {
    *SCREEN.lock().unwrap_or_else(|e| e.into_inner())
}

/// GUI 模式启动看门狗线程：每秒核对心跳，静默超阈值记 ERROR，恢复补 WARN。
/// 每次卡顿事件只记「开始 + 恢复」两条，不刷屏。进程被整体冻结时本线程也停——
/// 那种场景靠 WER 转储（见模块头注释）
pub fn spawn_watchdog() {
    let builder = std::thread::Builder::new().name("watchdog".into());
    let spawned = builder.spawn(|| {
        let mut stall_start: Option<u64> = None;
        loop {
            std::thread::sleep(std::time::Duration::from_secs(1));
            let last = UI_HEARTBEAT_MS.load(Ordering::Relaxed);
            if last == 0 {
                continue; // UI 还没画第一帧，不算卡
            }
            let silent = now_ms().saturating_sub(last) / 1000;
            if silent >= STALL_SECS {
                if stall_start.is_none() {
                    stall_start = Some(now_ms());
                    log::error!(
                        "UI 线程疑似卡死：{} 秒无帧。界面[{}] 进程已运行 {} 秒。{}",
                        silent,
                        screen(),
                        uptime_secs(),
                        memory_note()
                    );
                }
            } else if let Some(start) = stall_start.take() {
                log::warn!(
                    "UI 恢复响应：本次无帧共 {} 秒（界面[{}]）",
                    now_ms().saturating_sub(start) / 1000,
                    screen()
                );
            }
        }
    });
    if spawned.is_err() {
        log::warn!("看门狗线程启动失败（不影响功能，仅失去卡死检测）");
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn uptime_secs() -> u64 {
    now_ms().saturating_sub(START_MS.load(Ordering::Relaxed)) / 1000
}

/// 进程启动时刻（init() 记录，供看门狗日志计算运行时长）
static START_MS: AtomicU64 = AtomicU64::new(0);

#[cfg(windows)]
fn memory_note() -> String {
    unsafe {
        let mut ms = winapi::um::sysinfoapi::MEMORYSTATUSEX {
            dwLength: std::mem::size_of::<winapi::um::sysinfoapi::MEMORYSTATUSEX>() as u32,
            ..std::mem::zeroed()
        };
        if winapi::um::sysinfoapi::GlobalMemoryStatusEx(&mut ms) != 0 {
            let total_mb = ms.ullTotalPhys / 1024 / 1024;
            let avail_mb = ms.ullAvailPhys / 1024 / 1024;
            format!("内存占用 {}%（可用 {avail_mb}MB / {total_mb}MB）", ms.dwMemoryLoad)
        } else {
            "内存状态获取失败".to_string()
        }
    }
}

#[cfg(not(windows))]
fn memory_note() -> String {
    String::new()
}

// ---------------------------------------------------------------------------
// 落盘
// ---------------------------------------------------------------------------

fn today() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

/// 一行日志的统一格式：`[时间戳 级别 目标] 消息`
fn format_line(ts: &str, level: log::Level, target: &str, msg: &str) -> String {
    format!("[{ts} {} {target}] {msg}", level)
}

/// 按天写一行（带换行、即写即 flush——崩溃前最后一行必须已在盘上）；
/// 日期变化时换新文件。打开失败则丢弃（stderr 已有副本）
fn write_line(state: &mut LogState, day: &str, line: &str) {
    if state.day != day || state.writer.is_none() {
        let _ = std::fs::create_dir_all(&state.dir);
        state.writer = File::options()
            .create(true)
            .append(true)
            .open(state.dir.join(format!("app-{day}.log")))
            .ok()
            .map(BufWriter::new);
        state.day = day.to_string();
    }
    if let Some(w) = state.writer.as_mut() {
        use std::io::Write;
        let _ = writeln!(w, "{line}");
        let _ = w.flush();
    }
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_level_parsing() {
        assert_eq!(level_from_str("debug"), Some(4));
        assert_eq!(level_from_str(" WARN "), Some(2));
        assert_eq!(level_from_str("Off"), Some(0));
        assert_eq!(level_from_str("垃圾"), None);
        assert_eq!(level_from_str(""), None);
    }

    #[test]
    fn level_filter_defaults_and_overrides() {
        // 临时改环境变量做两次解析；env 改动是进程级的，测试串行性由 cargo 单线程保证
        std::env::set_var("RBS_LOG", "trace");
        assert!(matches!(level_filter(), log::LevelFilter::Trace));
        std::env::set_var("RBS_LOG", "不存在");
        assert!(matches!(level_filter(), log::LevelFilter::Info));
        std::env::remove_var("RBS_LOG");
        assert!(matches!(level_filter(), log::LevelFilter::Info));
    }

    #[test]
    fn line_format_has_ts_level_target_msg() {
        let line = format_line("2026-11-04 12:00:00.123", log::Level::Warn, "egui_winit", "复制坏了");
        assert!(line.starts_with("[2026-11-04 12:00:00.123 WARN egui_winit] "));
        assert!(line.ends_with("复制坏了"));
    }

    #[test]
    fn day_rollover_switches_file() {
        let dir = std::env::temp_dir().join(format!("rbs-logtest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut state = LogState { dir: dir.clone(), ..LogState::new() };
        write_line(&mut state, "2026-11-03", "第一天的日志");
        write_line(&mut state, "2026-11-04", "第二天的日志");
        write_line(&mut state, "2026-11-04", "第二天第二行");
        assert_eq!(
            std::fs::read_to_string(dir.join("app-2026-11-03.log")).unwrap(),
            "第一天的日志\n"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("app-2026-11-04.log")).unwrap(),
            "第二天的日志\n第二天第二行\n"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn panic_payload_text_extraction() {
        let s: &'static str = "&str 载荷";
        assert_eq!(payload_str(&s), "&str 载荷");
        assert_eq!(payload_str(&String::from("String 载荷")), "String 载荷");
        assert_eq!(payload_str(&42_u32), "<非文本 panic 载荷>");
    }
}
