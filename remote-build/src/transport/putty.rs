//! PuTTY 套件传输（设计文档 §5，P0 主传输）。
//!
//! - 查找优先级（§5.3）：自定义 → 内置捆绑 → 系统 PATH → 常见安装路径
//! - 与 tools.yaml 的关系（§3.4 v0.4 澄清）：tools.yaml 的 putty 条目由探测结果
//!   自动生成、仅供 UI 展示；本模块路径一律以 detect_putty_tools() 为准，不回读 tools.yaml
//! - 安全红线（§5.7）：禁止 -pw 明文传密码

use super::local::{pipe_child_output, wait_with_cancel};
use super::{ExitStatus, LogHandle, RemoteTransport};
use crate::models::{AuthMethod, Target, TargetType};
use anyhow::Context as _;
use async_trait::async_trait;
use sha2::Sha256;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tokio::process::Command;
use tokio::sync::broadcast;

// ---------------------------------------------------------------------------
// 探测（§5.3）
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct PuttyDetection {
    pub found: bool,
    /// custom / builtin / PATH / Program Files
    pub source: String,
    pub plink: PathBuf,
    pub pscp: PathBuf,
    pub psftp: PathBuf,
    /// P1 启动校验预留（探测仍填充；UI 探测展示已移除，暂无读取方）
    #[allow(dead_code)]
    pub puttygen: Option<PathBuf>,
    #[allow(dead_code)]
    pub pageant: Option<PathBuf>,
    pub version: Option<String>,
}

pub fn builtin_dir_hint() -> String {
    get_builtin_putty_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|| "<未知>".to_string())
}

/// 探测结果缓存：每步执行 / 每次哈希校验都会探测，逐次读盘解析 config.yaml
/// 再扫候选目录代价不小；结果进程内缓存，save_global（自定义路径可能变化）时失效。
static DETECTION_CACHE: std::sync::RwLock<Option<PuttyDetection>> = std::sync::RwLock::new(None);

pub fn invalidate_detection_cache() {
    *DETECTION_CACHE.write().unwrap_or_else(|e| e.into_inner()) = None;
}

/// 优先级探测：custom → builtin → PATH → Program Files（带进程内缓存）
pub fn detect_putty_tools() -> PuttyDetection {
    if let Some(hit) = DETECTION_CACHE.read().unwrap_or_else(|e| e.into_inner()).clone() {
        return hit;
    }
    let d = detect_putty_tools_uncached();
    *DETECTION_CACHE.write().unwrap_or_else(|e| e.into_inner()) = Some(d.clone());
    d
}

fn detect_putty_tools_uncached() -> PuttyDetection {
    let mut candidates: Vec<(String, PathBuf)> = Vec::new();

    // 1. 用户自定义路径（config.yaml putty.custom_path）
    if let Ok(cfg) = crate::config::load_global() {
        if let Some(custom) = cfg.putty.custom_path {
            candidates.push(("custom".to_string(), PathBuf::from(custom)));
        }
    }

    // 2. 内置捆绑（§5.3 内置目录定位）
    if let Some(dir) = get_builtin_putty_dir() {
        candidates.push(("builtin".to_string(), dir));
    }

    // 3. 系统 PATH
    if let Some(dir) = which_dir("plink.exe") {
        candidates.push(("PATH".to_string(), dir));
    }

    // 4. 常见安装路径
    for base in ["C:/Program Files/PuTTY", "C:/Program Files (x86)/PuTTY"] {
        candidates.push(("Program Files".to_string(), PathBuf::from(base)));
    }

    for (source, dir) in candidates {
        if dir.join("plink.exe").exists() && dir.join("pscp.exe").exists() {
            let read_version = || {
                std::fs::read_to_string(dir.join("VERSION"))
                    .ok()
                    .map(|s| s.lines().next().unwrap_or("").trim().to_string())
                    .filter(|s| !s.is_empty())
            };
            return PuttyDetection {
                found: true,
                source,
                plink: dir.join("plink.exe"),
                pscp: dir.join("pscp.exe"),
                psftp: dir.join("psftp.exe"),
                puttygen: Some(dir.join("puttygen.exe")).filter(|p| p.exists()),
                pageant: Some(dir.join("pageant.exe")).filter(|p| p.exists()),
                version: read_version(),
            };
        }
    }

    PuttyDetection::default()
}

/// 内置目录定位（§5.3）：
/// 1. cargo run / cargo test：运行时 CARGO_MANIFEST_DIR/tools/putty/
/// 2. 开发构建：编译期烙入 manifest 路径（直接双击 target/debug 的 exe 也能找到）
/// 3. 安装后：exe 同目录/tools/putty/
/// 4. 兜底：~/.remote-build/tools/putty/
fn get_builtin_putty_dir() -> Option<PathBuf> {
    if let Ok(manifest) = std::env::var("CARGO_MANIFEST_DIR") {
        let p = PathBuf::from(manifest).join("tools").join("putty");
        if p.join("plink.exe").exists() {
            return Some(p);
        }
    }
    #[cfg(debug_assertions)]
    {
        let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tools").join("putty");
        if p.join("plink.exe").exists() {
            return Some(p);
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            let p = parent.join("tools").join("putty");
            if p.join("plink.exe").exists() {
                return Some(p);
            }
        }
    }
    let p = crate::config::app_root().join("tools").join("putty");
    if p.join("plink.exe").exists() {
        return Some(p);
    }
    None
}

/// 在 PATH 中查找包含指定可执行文件的目录（Windows 需拼 PATHEXT）
fn which_dir(exe: &str) -> Option<PathBuf> {
    let path = std::env::var("PATH").unwrap_or_default();
    for dir in path.split(';') {
        if dir.is_empty() {
            continue;
        }
        let d = PathBuf::from(dir);
        if d.join(exe).exists() {
            return Some(d);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// 完整性校验（§5.5 v0.4：与 §5.4 捆绑清单一一对应）
// ---------------------------------------------------------------------------

/// 当前仅单元测试使用；启动期强制校验为 P1
#[cfg_attr(not(test), allow(dead_code))]
pub fn verify_putty_integrity(putty_dir: &Path) -> Result<(), String> {
    // 5 个 exe + VERSION 清单，缺一不可。
    // puttygen.exe（密钥转换）、pageant.exe（agent 认证）虽不在传输主链路上，
    // 缺失也必须在启动校验期暴露，而不是等到运行期才失败。
    const REQUIRED_EXES: [&str; 5] = [
        "plink.exe",
        "pscp.exe",
        "psftp.exe",
        "puttygen.exe",
        "pageant.exe",
    ];
    for tool in REQUIRED_EXES {
        let p = putty_dir.join(tool);
        if !p.exists() {
            return Err(format!("{} missing", tool));
        }
        let meta = std::fs::metadata(&p).map_err(|e| e.to_string())?;
        if meta.len() < 100_000 {
            return Err(format!("{} appears corrupted (too small)", tool));
        }
    }

    // VERSION 清单记录各 exe 的 sha256；存在即校验格式，逐文件哈希比对为 P1 加强项
    let version = putty_dir.join("VERSION");
    if !version.exists() {
        return Err("VERSION manifest missing".to_string());
    }
    Ok(())
}

pub fn sha256_file(path: &Path) -> anyhow::Result<String> {
    // 流式读取：大文件（几百 MB 的 jar/zip）不全量载入内存
    use sha2::Digest;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher)?;
    Ok(format!("{:x}", hasher.finalize()))
}

// ---------------------------------------------------------------------------
// 传输实现（§5.6）
// ---------------------------------------------------------------------------

pub struct PuttyTransport {
    pub plink: PathBuf,
    pub pscp: PathBuf,
    /// P1 SFTP 预留（当前 download 走 pscp）；按 §5.4 套件清单一并保留
    #[allow(dead_code)]
    pub psftp: PathBuf,
}

impl PuttyTransport {
    pub fn from_detection(d: &PuttyDetection) -> Self {
        Self { plink: d.plink.clone(), pscp: d.pscp.clone(), psftp: d.psftp.clone() }
    }

    /// 解构出 host / port / user / auth / host_key，拼 plink / pscp 公共参数。
    /// 非 SSH 目标返回错误，由上层按 StepResult::Failed 处理，不 panic。
    pub(crate) fn build_ssh_args(target: &Target) -> anyhow::Result<(String, Vec<String>)> {
        Self::build_ssh_args_with(target, None)
    }

    /// 同上，但 password 认证可用 `transient_password` 覆盖 Credential Manager——
    /// 目标管理「连接测试」允许用表单里已输入、尚未保存的密码先行验证（DBeaver 式）。
    pub(crate) fn build_ssh_args_with(
        target: &Target,
        transient_password: Option<&str>,
    ) -> anyhow::Result<(String, Vec<String>)> {
        let mut args = vec!["-batch".to_string()];

        match &target.target_type {
            TargetType::Ssh { host, port, user, auth, host_key } => {
                args.push("-P".to_string());
                args.push(port.to_string());

                // 主机键指纹固定（TOFU）：首次连接测试的输出会给出服务器指纹，
                // 填入目标配置后即可在 -batch 模式下免交互连接，且比注册表缓存更严格。
                // 先规范化：手抄/粘贴常带进杂质，坏格式 plink 只会报一句英文
                // "is not a valid format for a manual host key specification"
                if let Some(fp) = host_key {
                    if !fp.trim().is_empty() {
                        let norm = normalize_host_key(fp).ok_or_else(|| {
                            anyhow::anyhow!(
                                "主机键指纹格式无效（当前值「{}」）：应为 SHA256: 开头的服务器指纹，\
                                 可从连接测试输出中选中复制",
                                fp.trim()
                            )
                        })?;
                        args.push("-hostkey".to_string());
                        args.push(norm);
                    }
                }

                match auth {
                    AuthMethod::Key { path } => {
                        args.push("-i".to_string());
                        args.push(path.clone());
                    }
                    AuthMethod::Password { .. } => {
                        // §5.7 / §14：密码只存 Windows Credential Manager，
                        // 键 = {项目}/{目标}（项目级）或 global/{目标}（全局共享），
                        // 不落 YAML；执行时经 -pw 传递（本机进程列表存在短暂暴露窗口，
                        // 内网工具可接受，高安全场景请用 key / agent）。
                        // 连接测试场景允许用表单临时密码（不落任何存储）。
                        let pwd = match transient_password {
                            Some(p) => p.to_string(),
                            None => {
                                let key = target
                                    .cred_key
                                    .clone()
                                    .unwrap_or_else(|| target.name.clone());
                                match crate::secrets::get_password(&key) {
                                    Ok(Some(p)) => p,
                                    Ok(None) => {
                                        return Err(anyhow::anyhow!(
                                            "目标「{}」未存储密码（凭据键: {key}）：\
                                             请在 🎯 目标管理中配置，或改用 key / agent 认证",
                                            target.name
                                        ))
                                    }
                                    Err(e) => {
                                        // 凭据管理器故障与「没存过密码」分开报，避免误导排查方向
                                        return Err(e.context(format!(
                                            "读取目标「{}」的存储密码失败（凭据键: {key}）",
                                            target.name
                                        )));
                                    }
                                }
                            }
                        };
                        args.push("-pw".to_string());
                        args.push(pwd);
                    }
                    // Agent / None 不需要额外参数
                    _ => {}
                }

                let dest = format!("{}@{}", user, host);
                Ok((dest, args))
            }
            _ => anyhow::bail!("PuttyTransport called with non-SSH target"),
        }
    }

    fn new_remote_cmd(&self, exe: &Path, args: &[String]) -> Command {
        let mut cmd = Command::new(exe);
        cmd.args(args);
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
        cmd.creation_flags(0x0800_0000);
        // 超时丢弃 future 时杀掉 plink/pscp（叶子进程，无进程树）
        cmd.kill_on_drop(true);
        cmd
    }

    fn run_to_completion<'a>(
        &'a self,
        cmd: &'a mut Command,
        log: &'a LogHandle,
        cancel: &'a mut broadcast::Receiver<()>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<ExitStatus>> + Send + 'a>>
    {
        Box::pin(async move {
            let mut child = cmd
                .spawn()
                .with_context(|| format!("启动失败: {}", self.plink.display()))?;
            // 与 local.rs 同一套守卫：取消时整树终止；plink/pscp 是叶子进程，
            // 挂 job 主要为了超时丢弃 future 时句柄关闭能可靠杀掉它们
            let tree = super::win_job::ProcTreeGuard::attach(&child);
            pipe_child_output(&mut child, log);
            wait_with_cancel(&mut child, cancel, &tree).await
        })
    }
}

#[async_trait]
impl RemoteTransport for PuttyTransport {
    async fn exec(
        &self,
        target: &Target,
        command: &str,
        working_dir: &str,
        env: &HashMap<String, String>,
        log: &LogHandle,
        cancel: &mut broadcast::Receiver<()>,
    ) -> anyhow::Result<ExitStatus> {
        let (dest, args_prefix) = Self::build_ssh_args(target)?;

        let full_command = if working_dir.is_empty() {
            command.to_string()
        } else {
            // 远端是 Linux shell：cd 路径用单引号包裹，避免路径含空格时被拆词；
            // 路径本身含单引号时按 POSIX 规则转义为 '\''
            format!("cd '{}' && {}", escape_posix_single_quote(working_dir), command)
        };

        let mut args = args_prefix;
        args.push(dest);
        args.push(full_command);

        // 注意：env 只注入本地 plink 进程，不影响远端 shell（远端变量请写在命令里）
        let mut cmd = self.new_remote_cmd(&self.plink, &args);
        for (k, v) in super::merged_env(env) {
            cmd.env(k, v);
        }
        self.run_to_completion(&mut cmd, log, cancel).await
    }

    async fn upload(
        &self,
        target: &Target,
        local: &Path,
        remote: &str,
        log: &LogHandle,
        cancel: &mut broadcast::Receiver<()>,
    ) -> anyhow::Result<()> {
        if !local.exists() {
            anyhow::bail!("上传源不存在: {}", local.display());
        }
        warn_risky_remote_path(remote, log);
        let (dest, mut args) = Self::build_ssh_args(target)?;
        args.push("-r".to_string()); // 递归
        args.push("-p".to_string()); // 保持权限
        args.push(local.to_string_lossy().to_string());
        args.push(format!("{}:{}", dest, remote));

        let mut cmd = self.new_remote_cmd(&self.pscp, &args);
        let status = self.run_to_completion(&mut cmd, log, cancel).await?;
        if !status.success {
            anyhow::bail!("pscp upload failed with code {}", status.code);
        }
        Ok(())
    }

    async fn download(
        &self,
        target: &Target,
        remote: &str,
        local: &Path,
        log: &LogHandle,
        cancel: &mut broadcast::Receiver<()>,
    ) -> anyhow::Result<()> {
        warn_risky_remote_path(remote, log);
        let (dest, mut args) = Self::build_ssh_args(target)?;
        args.push(format!("{}:{}", dest, remote));
        args.push(local.to_string_lossy().to_string());

        let mut cmd = self.new_remote_cmd(&self.pscp, &args);
        let status = self.run_to_completion(&mut cmd, log, cancel).await?;
        if !status.success {
            anyhow::bail!("pscp download failed with code {}", status.code);
        }
        Ok(())
    }
}

/// POSIX 单引号转义：' → '\''
pub fn escape_posix_single_quote(s: &str) -> String {
    s.replace('\'', "'\\''")
}

/// pscp 远端路径告警：SCP 协议下远端路径会被远端 shell 解释（SFTP 则按字面处理），
/// 路径含空格 / shell 元字符时行为不可预期且难以排查。只提醒不拦截——
/// 加引号会破坏 SFTP 模式（引号会被当成路径字面量），提前提示用户避开最稳妥。
fn warn_risky_remote_path(remote: &str, log: &LogHandle) {
    let risky = remote.chars().any(|c| {
        c.is_whitespace()
            || matches!(
                c,
                ';' | '|' | '&' | '$' | '`' | '(' | ')' | '*' | '?' | '"' | '\''
            )
    });
    if risky {
        log.line(
            super::Stream::Stderr,
            format!(
                "⚠ 远端路径含空格或 shell 特殊字符，pscp 可能传输失败或被远端 shell 解释: {remote}"
            ),
        );
    }
}

/// 批量取远端文件 sha256：一条 plink 往返执行 `sha256sum p1 p2 …`（异步，30s 超时）。
/// 返回 远端路径 → 小写哈希；不存在的文件不出现在结果里（sha256sum 对缺失文件
/// 退出码非 0 但已找到的仍输出，按部分成功处理）。
/// 完全失败（连不上 / 命令不存在 / 输出无法解析）返回 Err，由调用方决定
/// fail-open（上传前预检）还是判步骤失败（上传后校验）。
///
/// 取代旧版逐文件同步实现：多 jar 上传场景从「每文件 2 次冷启动连接」
/// 降为「预检 + 校验各 1 次往返」，也不再阻塞 tokio worker 线程。
pub async fn remote_sha256_batch(
    target: &Target,
    remote_paths: &[String],
) -> anyhow::Result<HashMap<String, String>> {
    if remote_paths.is_empty() {
        return Ok(HashMap::new());
    }
    let d = detect_putty_tools();
    if !d.found {
        anyhow::bail!("未找到 plink（PuTTY）");
    }
    let (dest, mut args) = PuttyTransport::build_ssh_args(target)?;
    args.push(dest);
    let joined = remote_paths
        .iter()
        .map(|p| format!("'{}'", escape_posix_single_quote(p)))
        .collect::<Vec<_>>()
        .join(" ");
    args.push(format!("sha256sum {joined}"));

    let mut cmd = Command::new(&d.plink);
    cmd.args(&args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    let child = cmd.spawn().context("启动 plink 失败")?;
    let out = tokio::time::timeout(std::time::Duration::from_secs(30), child.wait_with_output())
        .await
        .map_err(|_| anyhow::anyhow!("远端 sha256sum 超时（30s）"))??;
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let map = parse_sha256sum_batch(&stdout);
    if map.is_empty() && !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!(
            "远端 sha256sum 失败: {}",
            stderr.lines().next().unwrap_or("(无输出)").trim()
        );
    }
    Ok(map)
}

/// 解析 `sha256sum` 多文件输出：每行 `<hash>  <path>`（GNU/busybox 均两个空格分隔，
/// 二进制模式为 ` *`），路径可含空格；非哈希行（错误提示等）忽略。
pub fn parse_sha256sum_batch(output: &str) -> HashMap<String, String> {
    output.lines().filter_map(parse_sha256sum_line).collect()
}

fn parse_sha256sum_line(line: &str) -> Option<(String, String)> {
    // 哈希固定 64 个十六进制字符且顶行首。len 检查之外还要确认字节 64 在 UTF-8
    // 字符边界上：plink 输出里混着服务器 MOTD 横幅，中文横幅一行 ≥23 字时
    // 字节 64 会落在多字节字符中间，直接 split_at(64) 会 panic（实测复现）
    if line.len() < 67 || !line.is_char_boundary(64) {
        return None;
    }
    let (hash, rest) = line.split_at(64);
    if !hash.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let rest = rest.strip_prefix(" *").or_else(|| rest.strip_prefix("  ")).unwrap_or(rest);
    let path = rest.trim();
    if path.is_empty() {
        return None;
    }
    // 返回 (路径, 哈希)：直接 collect 进 HashMap 时路径为键（调用方按路径查）
    Some((path.to_string(), hash.to_ascii_lowercase()))
}
// ---------------------------------------------------------------------------
// 连接测试 + 服务器体检
// ---------------------------------------------------------------------------

/// 服务器基础信息（probe_server 采集；字段缺失 = 远端命令未产出，展示时跳过）
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ServerInfo {
    pub hostname: String,
    pub os: String,
    pub cores: String,
    /// 1/5/15 分钟负载（/proc/loadavg 前三列）
    pub load: String,
    /// 已用 / 总量，MB（MemTotal - MemAvailable / MemTotal）
    pub mem_used_mb: u64,
    pub mem_total_mb: u64,
    pub uptime: String,
    pub disks: Vec<DiskMount>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct DiskMount {
    pub mount: String,
    pub total_gb: f64,
    pub used_gb: f64,
    /// 使用率文本（如 "71%"）
    pub percent: String,
}

/// 远端采集命令：分号串联的单行、无引号嵌套（busybox 亦兼容），逐项 echo key=…；
/// 单项命令失败不影响其余项。解析全部在本地（parse_server_info），远端保持最朴素。
pub const PROBE_CMD: &str = concat!(
    "echo __RBS_BEGIN__; ",
    "echo os=$(uname -sr); ",
    "echo host=$(hostname || uname -n); ",
    "echo cores=$(nproc || grep -c ^processor /proc/cpuinfo); ",
    "echo load=$(cat /proc/loadavg); ",
    "echo uptimesec=$(cat /proc/uptime); ",
    "echo meminfo=; ",
    "grep ^MemTotal: /proc/meminfo; ",
    "grep ^MemAvailable: /proc/meminfo; ",
    "echo disks=; ",
    "df -lP; ",
    "echo __RBS_END__",
);

/// 连接测试 + 服务器体检：一次 plink 往返同时验证 SSH 可达（网络/端口/认证）
/// 并采集 CPU / 内存 / 磁盘等基础信息。超时 8s（df 比单条 echo 慢）。
/// 成功返回 (ServerInfo, 耗时毫秒)；失败语义与原 test_connection 一致（含指纹引导）。
pub fn probe_server(target: &Target) -> Result<(ServerInfo, u128), String> {
    probe_server_with(target, None)
}

/// 同 probe_server，但 password 认证可用表单临时密码覆盖 Credential Manager
/// （目标管理「连接测试」支持验证已输入、尚未保存的密码）。
pub fn probe_server_with(
    target: &Target,
    transient_password: Option<&str>,
) -> Result<(ServerInfo, u128), String> {
    let d = detect_putty_tools();
    if !d.found {
        return Err("未找到 plink（PuTTY），请安装或将套件放入内置目录".to_string());
    }
    let (dest, mut args) = PuttyTransport::build_ssh_args_with(target, transient_password)
        .map_err(|e| e.to_string())?;
    args.push(dest);
    args.push(PROBE_CMD.to_string());

    use std::io::Read;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    let started = Instant::now();
    let mut cmd = Command::new(&d.plink);
    cmd.args(&args).stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    let mut child = cmd.spawn().map_err(|e| format!("启动 plink 失败: {e}"))?;
    let deadline = started + Duration::from_secs(8);

    // stdout/stderr 交给独立线程排空：若在 try_wait 轮询期间不读管道，
    // MOTD / 大输出超过管道缓冲（64KB）会让 plink 写阻塞，直到 8s 被误判为超时
    let stdout_drain = child.stdout.take().map(|mut s| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = s.read_to_end(&mut buf);
            buf
        })
    });
    let stderr_drain = child.stderr.take().map(|mut s| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = s.read_to_end(&mut buf);
            buf
        })
    });

    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break Some(st),
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    break None;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => return Err(format!("执行 plink 失败: {e}")),
        }
    };

    // kill 后管道关闭，排空线程随即结束；join 拿完整输出
    let out = stdout_drain
        .and_then(|h| h.join().ok())
        .map(|b| crate::transport::decode_console_bytes(&b))
        .unwrap_or_default();
    let err = stderr_drain
        .and_then(|h| h.join().ok())
        .map(|b| crate::transport::decode_console_bytes(&b))
        .unwrap_or_default();
    let elapsed_ms = started.elapsed().as_millis();

    match status {
        Some(st) if st.success() => {
            let info = parse_server_info(&out);
            Ok((info, elapsed_ms))
        }
        Some(st) => {
            let text = format!("{err}{out}");
            let tail: Vec<&str> = text.lines().rev().take(4).collect();
            let tail = tail.into_iter().rev().collect::<Vec<_>>().join(" | ");
            let mut msg = if tail.is_empty() {
                format!("连接失败（退出码 {}）", st.code().unwrap_or(-1))
            } else {
                format!("连接失败: {tail}")
            };
            // 首次连接：提取服务器指纹，提示填入 host_key（TOFU，不自动信任）
            if text.contains("Cannot confirm a host key") || text.contains("host key") {
                if let Some(fp) = extract_fingerprint(&text) {
                    msg.push_str(&format!(
                        "\n首次连接该主机：把指纹 {fp} 填入目标配置的「主机键指纹」字段后重试（TOFU）"
                    ));
                }
            }
            Err(msg)
        }
        None => Err("连接超时（8s）——请检查网络 / 防火墙 / 主机地址".to_string()),
    }
}

/// 解析 plink 输出：只认 __RBS_BEGIN__ / __RBS_END__ 之间带键前缀的行
/// （登录横幅 MOTD 会被自动忽略）。远端命令失败时对应键缺失，字段留空。
pub fn parse_server_info(output: &str) -> ServerInfo {
    let mut info = ServerInfo::default();
    let mut mem_total_kb: Option<u64> = None;
    let mut mem_avail_kb: Option<u64> = None;
    // in_disks = 正在读 df -lP 输出段
    let mut in_disks = false;

    for line in output.lines() {
        match line.trim() {
            "__RBS_BEGIN__" => continue,
            "__RBS_END__" => break,
            "meminfo=" => {
                in_disks = false;
                continue;
            }
            "disks=" => {
                in_disks = true;
                continue;
            }
            _ => {}
        }

        if in_disks {
            if let Some(d) = parse_df_row(line) {
                info.disks.push(d);
            }
            continue;
        }

        // meminfo 段：行形如 "MemTotal:  16384000 kB"（无 = 前缀，须在键值匹配前处理）
        if line.starts_with("MemTotal:") {
            mem_total_kb = parse_meminfo_kb(line);
            continue;
        }
        if line.starts_with("MemAvailable:") {
            mem_avail_kb = parse_meminfo_kb(line);
            continue;
        }

        if let Some((key, value)) = line.split_once('=') {
            match key.trim() {
                "os" => info.os = value.trim().to_string(),
                "host" => info.hostname = value.trim().to_string(),
                "cores" => info.cores = value.trim().to_string(),
                // /proc/loadavg: "0.52 0.58 0.59 1/850 12345" → 取前三列
                "load" => {
                    info.load = value.split_whitespace().take(3).collect::<Vec<_>>().join(" ");
                }
                "uptimesec" => {
                    if let Some(secs) = value.split_whitespace().next().and_then(|s| s.parse::<f64>().ok()) {
                        info.uptime = format_uptime(secs);
                    }
                }
                _ => {}
            }
        }
    }

    if let (Some(total), Some(avail)) = (mem_total_kb, mem_avail_kb) {
        info.mem_total_mb = total / 1024;
        info.mem_used_mb = total.saturating_sub(avail) / 1024;
    }
    info
}

/// /proc/meminfo 行："MemTotal:       16384000 kB" → 16384000
fn parse_meminfo_kb(line: &str) -> Option<u64> {
    let value = line.split_once(':')?.1.trim();
    value.split_whitespace().next()?.parse().ok()
}

/// df -lP 行："/dev/sda1 82045436 54893204 22969804 71% /" → DiskMount。
/// 过滤 tmpfs/devtmpfs/overlay 等伪文件系统与表头；列数不足或非数字的行忽略。
fn parse_df_row(line: &str) -> Option<DiskMount> {
    let cols: Vec<&str> = line.split_whitespace().collect();
    if cols.len() < 6 || cols[1].parse::<u64>().is_err() {
        return None;
    }
    let fs = cols[0];
    const PSEUDO: &[&str] = &["tmpfs", "devtmpfs", "udev", "overlay", "shm", "none", "cgroup", "squashfs"];
    if PSEUDO.contains(&fs) {
        return None;
    }
    // 表头（Filesystem ...）的容量列不是数字，已被上方 parse 拦截
    let total_kb: u64 = cols[1].parse().ok()?;
    let used_kb: u64 = cols[2].parse().ok()?;
    let kb_to_gb = |kb: u64| kb as f64 / (1024.0 * 1024.0);
    Some(DiskMount {
        mount: cols[5].to_string(),
        total_gb: kb_to_gb(total_kb),
        used_gb: kb_to_gb(used_kb),
        percent: cols[4].to_string(),
    })
}

/// 秒 → "3天 2小时 15分" / "2小时 15分" / "15分"
fn format_uptime(secs: f64) -> String {
    let mins = (secs / 60.0) as u64;
    let d = mins / 1440;
    let h = (mins % 1440) / 60;
    let m = mins % 60;
    if d > 0 {
        format!("{d}天 {h}小时 {m}分")
    } else if h > 0 {
        format!("{h}小时 {m}分")
    } else {
        format!("{m}分")
    }
}

/// KB → GB 文本：≥100 取整，否则 1 位小数
fn gb_text(gb: f64) -> String {
    if gb >= 100.0 {
        format!("{:.0}", gb)
    } else {
        format!("{gb:.1}")
    }
}

impl ServerInfo {
    /// 供 CLI 与 UI 共用的展示格式（多行；info.0 首行含连接耗时）
    pub fn display(&self, elapsed_ms: u128) -> String {
        let mut lines = Vec::new();
        let os = if self.os.is_empty() { "未知系统" } else { &self.os };
        lines.push(format!("连接成功（{:.1}s）· {os}", elapsed_ms as f64 / 1000.0));

        let mut row = Vec::new();
        if !self.hostname.is_empty() {
            row.push(format!("主机: {}", self.hostname));
        }
        if !self.cores.is_empty() {
            row.push(format!("CPU: {} 核", self.cores));
        }
        if !self.load.is_empty() {
            row.push(format!("负载: {}", self.load));
        }
        if !row.is_empty() {
            lines.push(row.join("   "));
        }

        if self.mem_total_mb > 0 {
            let pct = (self.mem_used_mb as f64 / self.mem_total_mb as f64 * 100.0).round() as u64;
            lines.push(format!(
                "内存: {:.1} / {:.1} GB（{pct}%）",
                self.mem_used_mb as f64 / 1024.0,
                self.mem_total_mb as f64 / 1024.0
            ));
        }
        if !self.uptime.is_empty() {
            lines.push(format!("运行: {}", self.uptime));
        }
        if !self.disks.is_empty() {
            lines.push("磁盘:".to_string());
            for d in &self.disks {
                lines.push(format!(
                    "  {:<14} {:>6}G  已用 {:>6}G  {}",
                    d.mount,
                    gb_text(d.total_gb),
                    gb_text(d.used_gb),
                    d.percent
                ));
            }
        }
        lines.join("\n")
    }
}

/// 从 plink 输出提取主机键指纹（SHA256:...）
fn extract_fingerprint(text: &str) -> Option<String> {
    let idx = text.find("SHA256:")?;
    let rest = &text[idx..];
    let end = rest
        .find(|c: char| c.is_whitespace())
        .unwrap_or(rest.len());
    Some(rest[..end].trim().to_string())
}

/// 规范化用户填写的「主机键指纹」为 plink `-hostkey` 可接受的规格：
/// 裸 `SHA256:base64`（PuTTY ≥0.75）或 MD5 十六进制冒号串；
/// 粘贴整行（如 `ssh-ed25519 255 SHA256:...`）时从中提取指纹。
/// 认不出（漏抄/多抄字符）返回 None，由调用方给出中文错误。
fn normalize_host_key(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    // SHA256 指纹 = 32 字节 base64 去掉尾部 = 后恰好 43 字符
    for (i, _) in raw.match_indices("SHA256:") {
        let token = raw[i..].split_whitespace().next().unwrap_or_default();
        let b64 = token["SHA256:".len()..].trim_end_matches('=');
        if b64.len() == 43
            && b64
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '/')
        {
            // 统一返回去 padding 的形式（PuTTY 显示规范）
            return Some(format!("SHA256:{b64}"));
        }
    }
    // MD5 指纹 = 16 组两位十六进制（可带 MD5: 前缀）
    raw.split_whitespace().find_map(|t| {
        let t = t.strip_prefix("MD5:").unwrap_or(t);
        (t.len() == 47
            && t.split(':').count() == 16
            && t.split(':').all(|g| {
                g.len() == 2 && g.chars().all(|c| c.is_ascii_hexdigit())
            }))
        .then(|| t.to_string())
    })
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::AuthMethod;

    fn ssh_target(auth: AuthMethod) -> Target {
        Target {
            name: "t".to_string(),
            target_type: TargetType::Ssh {
                host: "192.168.1.100".to_string(),
                port: 22,
                user: "builder".to_string(),
                auth,
                host_key: None,
            },
            work_root: None,
            env: HashMap::new(),
            cred_key: None,
        }
    }

    #[test]
    fn build_ssh_args_full_destructure() {
        // v0.4 评审 #2：必须完整解构 host/port/user/auth
        let target = ssh_target(AuthMethod::Key { path: r"C:\keys\id.ppk".to_string() });
        let (dest, args) = PuttyTransport::build_ssh_args(&target).unwrap();
        assert_eq!(dest, "builder@192.168.1.100");
        assert_eq!(args, vec!["-batch", "-P", "22", "-i", r"C:\keys\id.ppk"]);
    }

    #[test]
    fn password_auth_requires_stored_secret() {
        // §14：密码必须先存 Credential Manager（按目标名），否则明确报错。
        // 测试环境没有存储密码，因此应得到可读的错误而不是 -pw 空参数。
        let target = ssh_target(AuthMethod::Password { encrypted: "x".to_string() });
        let err = PuttyTransport::build_ssh_args(&target).unwrap_err();
        assert!(err.to_string().contains("未存储密码"), "got: {err}");
    }

    #[test]
    fn build_ssh_args_rejects_non_ssh() {
        let target = Target { name: "l".into(), target_type: TargetType::Local, ..Default::default() };
        assert!(PuttyTransport::build_ssh_args(&target).is_err());
    }

    #[test]
    fn normalize_host_key_accepts_valid_specs() {
        // 裸 SHA256 指纹（连接测试输出原样）
        let fp = "SHA256:E4Wpmw1njCQ2d/UhkEpm9xMOTGDIZhgVLIsa0SnpGw4";
        assert_eq!(normalize_host_key(fp).as_deref(), Some(fp));
        // 完整规格整行粘贴 → 提取裸指纹
        let full = format!("  ssh-ed25519 255 {fp} \n");
        assert_eq!(normalize_host_key(&full).as_deref(), Some(fp));
        // 带 padding 的 44 字符形式 → 去掉 = 统一为 43 字符
        assert_eq!(
            normalize_host_key("SHA256:abcdefghijkmnopqrstuvwxyzABCDEFGHJKLMNPQRST=")
                .as_deref(),
            Some("SHA256:abcdefghijkmnopqrstuvwxyzABCDEFGHJKLMNPQRST")
        );
        // MD5 指纹
        let md5 = "aa:bb:cc:dd:ee:ff:00:11:22:33:44:55:66:77:88:99";
        assert_eq!(normalize_host_key(md5).as_deref(), Some(md5));
        assert_eq!(
            normalize_host_key(&format!("MD5:{md5}")).as_deref(),
            Some(md5)
        );
    }

    #[test]
    fn normalize_host_key_rejects_garbled_input() {
        // 漏抄几位 → 报错而不是传给 plink 撞英文提示
        assert_eq!(normalize_host_key("SHA256:E4Wpmw1njCQ2d/UhkEpm9xMOTGDIZhgVLIsa0"), None);
        assert_eq!(normalize_host_key("aa:bb:cc"), None);
        assert_eq!(normalize_host_key("hello"), None);
        assert_eq!(normalize_host_key("   "), None);
        assert_eq!(normalize_host_key(""), None);
    }

    #[test]
    fn build_ssh_args_validates_host_key() {
        let mut target = ssh_target(AuthMethod::Agent);
        if let TargetType::Ssh { host_key, .. } = &mut target.target_type {
            *host_key = Some("SHA256:短了".to_string());
        }
        let err = PuttyTransport::build_ssh_args(&target).unwrap_err();
        assert!(err.to_string().contains("主机键指纹格式无效"), "got: {err}");
    }

    #[test]
    fn cd_quoting_handles_spaces_and_quotes() {
        // v0.4 评审 #3：路径空格必须加引号
        let quoted = escape_posix_single_quote("/opt/My Projects/app");
        assert_eq!(quoted, "/opt/My Projects/app");
        let cmd = format!("cd '{}' && ls -la", quoted);
        assert_eq!(cmd, "cd '/opt/My Projects/app' && ls -la");

        // 含单引号的路径按 POSIX 规则转义：' → '\''
        assert_eq!(escape_posix_single_quote("it's"), "it'\\''s");
    }

    #[test]
    fn integrity_requires_all_bundled_files() {
        // v0.4 评审 #8：5 个 exe + VERSION 全部校验
        let dir = std::env::temp_dir().join(format!("rbs_test_putty_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // 全空 → 报缺失
        let err = verify_putty_integrity(&dir).unwrap_err();
        assert!(err.contains("plink.exe missing"));

        // 只放 3 个核心文件（v0.3 的行为）→ 仍应报 puttygen 缺失
        for name in ["plink.exe", "pscp.exe", "psftp.exe"] {
            std::fs::write(dir.join(name), vec![0u8; 200_000]).unwrap();
        }
        let err = verify_putty_integrity(&dir).unwrap_err();
        assert!(err.contains("puttygen.exe missing"), "got: {err}");

        // 补齐全部文件 → 通过
        for name in ["puttygen.exe", "pageant.exe"] {
            std::fs::write(dir.join(name), vec![0u8; 200_000]).unwrap();
        }
        std::fs::write(dir.join("VERSION"), "0.85").unwrap();
        assert!(verify_putty_integrity(&dir).is_ok());

        // 文件过小 → 判定损坏
        std::fs::write(dir.join("plink.exe"), b"tiny").unwrap();
        let err = verify_putty_integrity(&dir).unwrap_err();
        assert!(err.contains("corrupted"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bundled_putty_passes_integrity_when_present() {
        // 开发机捆绑了真实 exe 时，内置目录必须通过完整性校验；
        // 未捆绑二进制的环境（如 fresh clone）跳过
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tools").join("putty");
        if !dir.join("plink.exe").exists() {
            return;
        }
        verify_putty_integrity(&dir).expect("捆绑的 PuTTY 应通过完整性校验");
    }

    #[test]
    fn sha256sum_batch_parsing() {
        let h1 = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        let h2 = "a".repeat(64);
        // 多文件输出：逐行解析，路径可含空格；错误行忽略
        let out = format!(
            "{h1}  /opt/app/user.jar\n\
             sh: /opt/missing.jar: No such file or directory\n\
             {h2}  /opt/My Projects/app.jar\n"
        );
        let map = parse_sha256sum_batch(&out);
        assert_eq!(map.len(), 2, "{map:?}");
        assert_eq!(map.get("/opt/app/user.jar").map(String::as_str), Some(h1));
        assert_eq!(map.get("/opt/My Projects/app.jar").map(String::as_str), Some(h2.as_str()));
        // 全错误输出 → 空 map（调用方按失败处理）
        assert!(parse_sha256sum_batch("sh: sha256sum: command not found").is_empty());
    }

    #[test]
    fn sha256sum_batch_survives_multibyte_motd() {
        // 回归：服务器 MOTD 含长中文行（≥23 个汉字时字节 64 落在多字节字符中间），
        // 旧实现 split_at(64) 直接 panic 崩掉整个运行任务；现在应忽略横幅行、正常解析
        let motd = "欢迎使用构建服务器，请勿随意登录本机器谢谢配合".repeat(2);
        assert!(motd.len() >= 67);
        let h1 = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        let out = format!("{motd}\n{h1}  /opt/app/app.jar\n");
        let map = parse_sha256sum_batch(&out);
        assert_eq!(map.len(), 1, "{map:?}");
        assert_eq!(map.get("/opt/app/app.jar").map(String::as_str), Some(h1));
    }

    #[test]
    fn detect_finds_builtin_when_bundled() {
        // cargo test 会注入 CARGO_MANIFEST_DIR，探测必须命中内置目录
        let bundled =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tools/putty/plink.exe");
        if !bundled.exists() {
            return;
        }
        invalidate_detection_cache(); // 排除其他测试留下的缓存
        let d = detect_putty_tools();
        assert!(d.found, "捆绑了 PuTTY 时必须能探测到");
        // config.yaml 配了 putty.custom_path 时自定义目录按设计优先（source=custom）；
        // 未配置时必须命中内置目录
        if d.source != "custom" {
            assert_eq!(d.source, "builtin");
        }
        // 缓存生效：第二次调用返回同一结果
        let d2 = detect_putty_tools();
        assert_eq!(d.source, d2.source);
        assert_eq!(d.plink, d2.plink);
    }

    // -- 服务器体检（probe_server 输出解析）---------------------------------

    #[test]
    fn parse_server_info_ignores_motd_and_parses_fields() {
        // MOTD 横幅在 __RBS_BEGIN__ 之前，必须被忽略；键行只认已知键
        let out = "\
Welcome to Ubuntu 22.04 LTS (GNU/Linux 5.15.0-91-generic x86_64)\n\
\n\
 * Documentation:  https://help.ubuntu.com\n\
__RBS_BEGIN__\n\
os=Linux 5.15.0-91-generic\n\
host=prod-web-01\n\
cores=8\n\
load=0.52 0.58 0.59 1/850 12345\n\
uptimesec=271935.28 1081423.06\n\
meminfo=\n\
MemTotal:       16384000 kB\n\
MemAvailable:   11468800 kB\n\
disks=\n\
Filesystem 1024-blocks Used Available Capacity Mounted on\n\
/dev/sda1 81779200 54525952 23085568 71% /\n\
tmpfs 8192000 0 8192000 0% /dev/shm\n\
/dev/sdb1 524288000 125829120 398458880 24% /data\n\
/dev/sda1 81779200 54525952 23085568 71% /boot\n\
__RBS_END__\n";
        let info = parse_server_info(out);
        assert_eq!(info.os, "Linux 5.15.0-91-generic");
        assert_eq!(info.hostname, "prod-web-01");
        assert_eq!(info.cores, "8");
        assert_eq!(info.load, "0.52 0.58 0.59", "loadavg 只取前三列");
        assert_eq!(info.uptime, "3天 3小时 32分");
        assert_eq!(info.mem_total_mb, 16000);
        assert_eq!(info.mem_used_mb, 4800);
        // tmpfs 过滤；/boot 与 / 同设备但挂载点不同，各自保留
        assert_eq!(info.disks.len(), 3);
        assert_eq!(info.disks[0].mount, "/");
        assert_eq!(info.disks[0].percent, "71%");
        assert!((info.disks[0].total_gb - 78.0).abs() < 0.1);
        assert_eq!(info.disks[1].mount, "/data");
        assert!((info.disks[1].used_gb - 120.0).abs() < 0.1);
    }

    #[test]
    fn parse_server_info_tolerates_missing_sections() {
        // 迷你系统（部分命令缺失）输出残缺 → 字段留空而非 panic
        let out = "__RBS_BEGIN__\nhost=barebox\ndisks=\n__RBS_END__";
        let info = parse_server_info(out);
        assert_eq!(info.hostname, "barebox");
        assert_eq!(info.os, "");
        assert!(info.disks.is_empty());
        assert_eq!(info.mem_total_mb, 0);
        // 完全没输出（非 Linux / 命令全失败）→ 全空结构
        assert_eq!(parse_server_info("garbage\nmore garbage"), ServerInfo::default());
    }

    #[test]
    fn server_info_display_readable() {
        let info = ServerInfo {
            hostname: "prod-web-01".into(),
            os: "Linux 5.15.0-91-generic".into(),
            cores: "8".into(),
            load: "0.52 0.58 0.59".into(),
            mem_used_mb: 5325,
            mem_total_mb: 16000,
            uptime: "3天 3小时 45分".into(),
            disks: vec![DiskMount {
                mount: "/".into(),
                total_gb: 78.0,
                used_gb: 52.0,
                percent: "71%".into(),
            }],
        };
        let text = info.display(2345);
        assert!(text.contains("连接成功（2.3s）"), "首行含耗时: {text}");
        assert!(text.contains("prod-web-01"));
        assert!(text.contains("内存: 5.2 / 15.6 GB（33%）"), "内存行: {text}");
        assert!(text.contains("71%"));
    }
}
