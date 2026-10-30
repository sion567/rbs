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

/// 「验证 version_check」按工具的真实执行语义解析：注入工具 env、command 为完整路径时
/// 把所在目录前置到 PATH（与引擎步骤注入一致，见 engine.rs 工具注入 + transport
/// 的 with_path_prepended / merged_env）。否则裸名命令解析到系统 PATH 上的同名程序，
/// 验出来的版本可能与工具实际指向不符（例：java-25 验出系统 PATH 的 jdk-17）。
pub fn run_version_check_for_tool(check: &str, tool: &crate::models::Tool) -> Option<String> {
    let mut parts = check.split_whitespace();
    let exe = parts.next()?;
    let args: Vec<&str> = parts.collect();

    let mut env = tool.env.clone();
    let path_like = tool.command.contains('\\') || tool.command.contains('/');
    if path_like {
        if let Some(parent) = std::path::Path::new(&tool.command).parent() {
            if parent.exists() {
                env = crate::transport::with_path_prepended(env, parent);
            }
        }
    }
    probe_with(exe, &args, &crate::transport::merged_env(&env))
}

/// 带超时地运行版本命令，取第一条非空输出行（java -version 输出在 stderr，一并考虑）
fn probe(exe: &str, args: &[&str]) -> Option<String> {
    probe_with(exe, args, &Default::default())
}

fn probe_with(exe: &str, args: &[&str], env: &std::collections::HashMap<String, String>) -> Option<String> {
    const TIMEOUT: Duration = Duration::from_secs(3);

    let mut cmd = build_probe_command(exe, args, env);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    cmd.envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
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
    // 跳过纯分隔线（gradle -v 首行是一串 "----"，拿它当版本结果没有信息量）：
    // 取第一条含字母/数字的非空行
    out.lines()
        .chain(err.lines())
        .map(str::trim)
        .find(|l| !l.is_empty() && l.chars().any(|c| c.is_alphanumeric()))
        .map(str::to_string)
}

// ---------------------------------------------------------------------------
// 版本命令解析（Windows 修复 mvn / gradle 一类 .cmd/.bat 工具的调用）
// ---------------------------------------------------------------------------

/// 工具 command 是否能解析到可执行文件（引擎自检的降级路径：未配 version_check 时用）。
/// 完整路径 = 文件存在；裸名 = 按 PATH 解析（Windows 依次找 .exe → .cmd / .bat）
pub(crate) fn probe_command_exists(command: &str) -> bool {
    let cmd = command.trim();
    if cmd.is_empty() {
        return false;
    }
    if cmd.contains('\\') || cmd.contains('/') {
        return std::path::Path::new(cmd).is_file();
    }
    resolve_probe_exe(cmd, &Default::default()).is_some()
}

/// 解析版本命令的真实落点：
/// - **批处理脚本**（.cmd/.bat，含 `mvn` / `gradle` 这种裸名——它们在 Windows 上
///   实际是 mvn.cmd / gradle.bat）：CreateProcess 的裸名解析只补 `.exe`，永远找不到
///   批处理；Rust 对 .bat/.cmd 的直接 spawn 也受限。解析到批处理统一经 `cmd /C` 执行。
/// - **裸名**：按 env 里的 PATH 依次搜 `.exe` → `.cmd` / `.bat`。必须显式搜索——
///   子进程 env 中前置的工具目录（with_path_prepended）对父进程的 exe 解析不可见，
///   不搜的话「工具绑定 java-25」实际验的还是系统 PATH 上的 jdk-17。
/// - 搜不到再原样 spawn 裸名，让系统报错（probe 返回 None → ❌）。
pub(crate) fn build_probe_command(
    exe: &str,
    args: &[&str],
    env: &std::collections::HashMap<String, String>,
) -> Command {
    let resolved = resolve_probe_exe(exe, env);
    let is_batch = |p: &str| {
        matches!(
            std::path::Path::new(p)
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e.to_ascii_lowercase())
                .as_deref(),
            Some("cmd" | "bat")
        )
    };
    let (program, via_cmd) = match resolved {
        Some(found) => {
            let batch = is_batch(&found);
            (found, batch)
        }
        None => (exe.to_string(), cfg!(windows) && is_batch(exe)),
    };
    let mut cmd = if via_cmd {
        let mut c = Command::new("cmd");
        // arg 让 Rust 自行给含空格的路径加引号：cmd /C 的双引号规则
        // （两个引号 + 中间含空白 + 是可执行名）会保留引号原样执行
        c.arg("/C").arg(&program);
        c
    } else {
        Command::new(&program)
    };
    cmd.args(args);
    cmd
}

/// 按 env PATH（缺失回退进程 PATH）解析裸名 / 校验路径，返回可执行文件落点。
/// 路径形态原样返回（存在性不在此校验，spawn 失败由 probe 统一返回 None）。
pub(crate) fn resolve_probe_exe(
    exe: &str,
    env: &std::collections::HashMap<String, String>,
) -> Option<String> {
    let looks_like_path = exe.contains('\\') || exe.contains('/');
    if looks_like_path {
        return Some(exe.to_string());
    }
    let path_var = env
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("PATH"))
        .map(|(_, v)| v.clone())
        .or_else(|| std::env::var("PATH").ok())?;
    let dirs: Vec<PathBuf> = std::env::split_paths(&path_var)
        .filter(|d| !d.as_os_str().is_empty())
        .collect();
    let find = |name: &str| {
        dirs.iter()
            .map(|d| d.join(name))
            .find(|p| p.is_file())
            .map(|p| p.to_string_lossy().to_string())
    };
    if cfg!(windows) {
        // Windows 只认 .exe / .cmd / .bat：不能回退到裸名匹配——maven 的 bin 目录里
        // 同时躺着无扩展名的 mvn（sh 脚本）与 mvn.cmd，裸名匹配会解析到 sh 脚本，
        // CreateProcess 启动不了它
        if let Some(hit) = find(&format!("{exe}.exe")) {
            return Some(hit);
        }
        for ext in ["cmd", "bat"] {
            if let Some(hit) = find(&format!("{exe}.{ext}")) {
                return Some(hit);
            }
        }
        None
    } else {
        // Unix：裸名即可执行
        find(exe)
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_command_resolves_and_wraps_batch_scripts() {
        // mvn / gradle 在 Windows 上是 mvn.cmd / gradle.bat 批处理：裸名 CreateProcess
        // 只补 .exe 找不到，必须解析到真实落点并经 cmd /C 执行（回归 #4）
        let tmp = std::env::temp_dir().join(format!("rbs_probe_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join("fakebatch.cmd"), "@echo off\r\necho hi\r\n").unwrap();
        std::fs::write(tmp.join("plain.exe"), b"MZ").unwrap(); // 只测解析，不真跑

        let mut env = std::collections::HashMap::new();
        env.insert("PATH".to_string(), tmp.to_string_lossy().to_string());

        if cfg!(windows) {
            // 裸名批处理 → 解析到 .cmd 且经 cmd /C
            let c = build_probe_command("fakebatch", &[], &env);
            assert_eq!(c.get_program(), "cmd");
            assert_eq!(
                c.get_args().nth(1).map(|s| s.to_string_lossy().to_string()),
                Some(tmp.join("fakebatch.cmd").to_string_lossy().to_string())
            );

            // 裸名 exe → 解析到完整路径直接 spawn
            let c = build_probe_command("plain", &["-v"], &env);
            assert!(
                c.get_program().to_string_lossy().ends_with("plain.exe"),
                "{:?}",
                c.get_program()
            );
            assert_eq!(c.get_args().len(), 1);

            // 完整路径批处理 → cmd /C
            let full = tmp.join("fakebatch.cmd").to_string_lossy().to_string();
            let c = build_probe_command(&full, &[], &env);
            assert_eq!(c.get_program(), "cmd");

            // 解析不到 → 原样保留（spawn 失败由 probe 返回 None → ❌）
            let c = build_probe_command("no_such_tool_xyz", &[], &env);
            assert_eq!(c.get_program(), "no_such_tool_xyz");
        } else {
            let c = build_probe_command("plain", &["-v"], &env);
            assert!(c.get_program().to_string_lossy().ends_with("plain"));
        }

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// 真机回归：本机装了 maven / gradle 时，「验证 version_check」必须能验出来
    /// （此前 Command::new("mvn") 裸名只补 .exe，永远找不到 mvn.cmd → 恒 ❌）
    #[test]
    fn version_check_works_for_batch_based_tools() {
        if !cfg!(windows) {
            return;
        }
        // mvn / gradle 不在本机 → 跳过（CI / 干净机器）
        if resolve_probe_exe("mvn", &Default::default()).is_none()
            && resolve_probe_exe("gradle", &Default::default()).is_none()
        {
            return;
        }
        if let Some(line) = run_version_check("mvn -v") {
            assert!(line.contains("Maven"), "mvn -v 应输出 Maven 版本: {line}");
        }
        if let Some(line) = run_version_check("gradle -v") {
            assert!(
                line.contains("Gradle"),
                "gradle -v 应输出 Gradle 版本: {line}"
            );
        }
    }
}
