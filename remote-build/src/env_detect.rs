//! 首次启动环境探测（设计文档 §7）。
//!
//! 探测 JAVA_HOME / MAVEN_HOME / GRADLE_HOME / NODE_HOME 与 PATH 中的
//! git / docker / kubectl 等，PuTTY 按 §5.3 优先级探测。
//! 探测在后台线程运行（含版本命令探测，可能各耗时数秒），结果经 std mpsc 回传。

use crate::transport::putty::{detect_putty_tools, PuttyDetection};
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct DetectedTool {
    pub name: String,
    pub version: Option<String>,
    /// JAVA_HOME / MAVEN_HOME / PATH / GRADLE_HOME / NODE_HOME
    pub source: String,
    /// 可执行文件路径（写入 tools.yaml 的 command）
    pub command: String,
    pub version_check: Option<String>,
    pub ok: bool,
}

#[derive(Debug, Clone, Default)]
pub struct DetectionReport {
    pub tools: Vec<DetectedTool>,
    pub putty: PuttyDetection,
}

/// 在后台线程执行全量探测
pub fn detect_async() -> std::sync::mpsc::Receiver<DetectionReport> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(detect_all());
    });
    rx
}

pub fn detect_all() -> DetectionReport {
    let tools = vec![
        detect_java(),
        detect_maven(),
        detect_gradle(),
        detect_node(),
        detect_path_tool("git", &["--version"]),
        detect_path_tool("docker", &["--version"]),
        detect_path_tool("kubectl", &["version", "--client"]),
        detect_path_tool("npm", &["-v"]),
    ];
    let putty = detect_putty_tools();
    DetectionReport { tools, putty }
}

// ---------------------------------------------------------------------------
// 各工具探测
// ---------------------------------------------------------------------------

fn detect_java() -> DetectedTool {
    let mut source = "PATH".to_string();
    let mut path = find_in_path("java.exe");
    if let Ok(home) = std::env::var("JAVA_HOME") {
        let candidate = PathBuf::from(&home).join("bin").join("java.exe");
        if candidate.exists() {
            path = Some(candidate);
            source = "JAVA_HOME".to_string();
        }
    }
    let command = path
        .as_ref()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|| "java".to_string());
    let version = if path.is_some() { probe(&command, &["-version"]) } else { None };
    DetectedTool {
        name: "java".to_string(),
        ok: path.is_some(),
        version,
        source,
        command,
        version_check: Some("java -version".to_string()),
    }
}

fn detect_maven() -> DetectedTool {
    let mut source = "PATH".to_string();
    let mut path = find_in_path("mvn.cmd").or_else(|| find_in_path("mvn.bat"));
    if let Ok(home) = std::env::var("MAVEN_HOME").or_else(|_| std::env::var("M2_HOME")) {
        for rel in ["bin/mvn.cmd", "bin/mvn.bat"] {
            let candidate = PathBuf::from(&home).join(rel);
            if candidate.exists() {
                path = Some(candidate);
                source = "MAVEN_HOME".to_string();
                break;
            }
        }
    }
    let command = path
        .as_ref()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|| "mvn".to_string());
    let version = if path.is_some() { probe(&command, &["-v"]) } else { None };
    DetectedTool {
        name: "mvn".to_string(),
        ok: path.is_some(),
        version,
        source,
        command,
        version_check: Some("mvn -v".to_string()),
    }
}

fn detect_gradle() -> DetectedTool {
    let mut source = "PATH".to_string();
    let mut path = find_in_path("gradle.bat");
    if let Ok(home) = std::env::var("GRADLE_HOME") {
        let candidate = PathBuf::from(&home).join("bin").join("gradle.bat");
        if candidate.exists() {
            path = Some(candidate);
            source = "GRADLE_HOME".to_string();
        }
    }
    let command = path
        .as_ref()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|| "gradle".to_string());
    let version = if path.is_some() { probe(&command, &["-v"]) } else { None };
    DetectedTool {
        name: "gradle".to_string(),
        ok: path.is_some(),
        version,
        source,
        command,
        version_check: Some("gradle -v".to_string()),
    }
}

fn detect_node() -> DetectedTool {
    let mut source = "PATH".to_string();
    let mut path = find_in_path("node.exe");
    for var in ["NODE_HOME", "NVM_HOME", "NVM_SYMLINK"] {
        if let Ok(home) = std::env::var(var) {
            let candidate = PathBuf::from(&home).join("node.exe");
            if candidate.exists() {
                path = Some(candidate);
                source = var.to_string();
                break;
            }
        }
    }
    let command = path
        .as_ref()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|| "node".to_string());
    let version = if path.is_some() { probe(&command, &["-v"]) } else { None };
    DetectedTool {
        name: "node".to_string(),
        ok: path.is_some(),
        version,
        source,
        command,
        version_check: Some("node -v".to_string()),
    }
}

fn detect_path_tool(name: &str, version_args: &[&str]) -> DetectedTool {
    let path = find_in_path(&format!("{name}.exe"));
    let command = path
        .as_ref()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|| name.to_string());
    let version = if path.is_some() { probe(&command, version_args) } else { None };
    DetectedTool {
        name: name.to_string(),
        ok: path.is_some(),
        version,
        source: "PATH".to_string(),
        command,
        version_check: Some(format!("{name} {}", version_args.join(" "))),
    }
}

// ---------------------------------------------------------------------------
// 基础设施
// ---------------------------------------------------------------------------

/// 扫 PATH 找可执行文件（§7.2：Windows 需手动扫 PATH）
fn find_in_path(exe: &str) -> Option<PathBuf> {
    let path = std::env::var("PATH").ok()?;
    for dir in path.split(';') {
        if dir.is_empty() {
            continue;
        }
        let p = PathBuf::from(dir).join(exe);
        if p.exists() {
            return Some(p);
        }
    }
    None
}

/// 执行 version_check 命令字符串（如 "java -version"），返回第一条非空输出行。
/// 用于设置页的「验证」按钮；超时 3s / 找不到命令 / 无输出 → None。
pub fn run_version_check(cmd: &str) -> Option<String> {
    let mut parts = cmd.split_whitespace();
    let exe = parts.next()?;
    let args: Vec<&str> = parts.collect();
    probe(exe, &args)
}

/// 带超时地运行版本命令，取第一条非空输出行（java -version 输出在 stderr，一并考虑）
fn probe(exe: &str, args: &[&str]) -> Option<String> {
    const TIMEOUT: Duration = Duration::from_secs(3);

    let mut cmd = Command::new(exe);
    cmd.args(args).stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let mut child = cmd.spawn().ok()?;
    let deadline = Instant::now() + TIMEOUT;

    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => return None,
        }
    };
    let _ = status;

    // read_to_string 遇到非 UTF-8 字节会整体失败（GBK 输出 → 空），
    // 因此先读原始字节再按控制台编码解码
    let mut out = Vec::new();
    let mut err = Vec::new();
    if let Some(mut o) = child.stdout.take() {
        let _ = o.read_to_end(&mut out);
    }
    if let Some(mut e) = child.stderr.take() {
        let _ = e.read_to_end(&mut err);
    }
    let out = crate::transport::decode_console_bytes(&out);
    let err = crate::transport::decode_console_bytes(&err);
    out.lines()
        .chain(err.lines())
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(str::to_string)
}

// ---------------------------------------------------------------------------
// 本机信息（local 目标展示用，与远端目标的 probe_server 体检对称）
// ---------------------------------------------------------------------------

/// 发版机本机概要：主机名 / 系统 / CPU / 内存。采集含子进程（ver），请在后台线程调用。
#[derive(Debug, Clone, Default)]
pub struct LocalSystemInfo {
    pub hostname: String,
    /// 如 "Microsoft Windows [Version 10.0.26300.1]" 的版本段
    pub os: String,
    pub arch: String,
    pub cpu_cores: u32,
    pub mem_total_mb: u64,
    pub mem_avail_mb: u64,
}

/// 采集本机信息（Windows：winapi + `cmd /C ver`；其他平台尽力而为）
pub fn local_system_info() -> LocalSystemInfo {
    let mut info = LocalSystemInfo {
        hostname: std::env::var("COMPUTERNAME").unwrap_or_default(),
        arch: std::env::consts::ARCH.to_string(),
        ..Default::default()
    };
    // OS 版本：GetVersionEx 无清单会谎报（固定 6.2），`ver` 命令最省事
    if let Some(line) = probe("cmd", &["/C", "ver"]) {
        // "Microsoft Windows [Version 10.0.26300.1]" → 取方括号段
        info.os = line
            .split(['[', ']'])
            .find(|s| s.contains("Version "))
            .map(|s| s.trim_start_matches("Version ").to_string())
            .unwrap_or(line);
    }
    #[cfg(windows)]
    unsafe {
        use std::mem;
        use winapi::um::sysinfoapi::{GlobalMemoryStatusEx, GetSystemInfo, MEMORYSTATUSEX, SYSTEM_INFO};
        let mut si: SYSTEM_INFO = mem::zeroed();
        GetSystemInfo(&mut si);
        info.cpu_cores = si.dwNumberOfProcessors;

        let mut ms: MEMORYSTATUSEX = mem::zeroed();
        ms.dwLength = mem::size_of::<MEMORYSTATUSEX>() as u32;
        if GlobalMemoryStatusEx(&mut ms) != 0 {
            info.mem_total_mb = ms.ullTotalPhys / (1024 * 1024);
            info.mem_avail_mb = ms.ullAvailPhys / (1024 * 1024);
        }
    }
    #[cfg(not(windows))]
    {
        info.cpu_cores = std::thread::available_parallelism()
            .map(|n| n.get() as u32)
            .unwrap_or(0);
    }
    info
}
