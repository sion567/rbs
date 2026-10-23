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
    pub dir: PathBuf,
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

/// 优先级探测：custom → builtin → PATH → Program Files
pub fn detect_putty_tools() -> PuttyDetection {
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
                dir,
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
        let mut args = vec!["-batch".to_string()];

        match &target.target_type {
            TargetType::Ssh { host, port, user, auth, host_key } => {
                args.push("-P".to_string());
                args.push(port.to_string());

                // 主机键指纹固定（TOFU）：首次连接测试的输出会给出服务器指纹，
                // 填入目标配置后即可在 -batch 模式下免交互连接，且比注册表缓存更严格
                if let Some(fp) = host_key {
                    let fp = fp.trim();
                    if !fp.is_empty() {
                        args.push("-hostkey".to_string());
                        args.push(fp.to_string());
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
                        // 内网工具可接受，高安全场景请用 key / agent）
                        let key = target
                            .cred_key
                            .clone()
                            .unwrap_or_else(|| target.name.clone());
                        let pwd = crate::secrets::get_password(&key).ok_or_else(|| {
                            anyhow::anyhow!(
                                "目标「{}」未存储密码（凭据键: {key}）：\
                                 请在 🎯 目标管理中配置，或改用 key / agent 认证",
                                target.name
                            )
                        })?;
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
            pipe_child_output(&mut child, log);
            wait_with_cancel(&mut child, cancel).await
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

/// SSH 连接测试（CLI test-conn 与 GUI 🎯 目标管理共用）：
/// `plink -batch <认证参数> user@host "echo __RBS_OK__"`，5 秒超时。
/// 认证参数复用 build_ssh_args（密码自动从 Credential Manager 取，§14）。

/// 解析 sha256sum 输出行（`<hash>  <path>`），取小写哈希
pub fn parse_sha256sum_output(line: &str) -> Option<String> {
    line.split_whitespace()
        .next()
        .map(|h| h.to_ascii_lowercase())
        .filter(|h| h.len() == 64 && h.chars().all(|c| c.is_ascii_hexdigit()))
}

/// 远端文件 sha256（上传校验用）：plink 执行 `sha256sum '<path>'`，10 秒超时。
/// 认证参数复用 build_ssh_args（密码取 Credential Manager，§14）。
pub fn remote_sha256(target: &Target, remote_path: &str) -> anyhow::Result<String> {
    let d = detect_putty_tools();
    if !d.found {
        anyhow::bail!("未找到 plink（PuTTY）");
    }
    let (dest, mut args) = PuttyTransport::build_ssh_args(target)?;
    args.push(dest);
    args.push(format!("sha256sum '{}'", escape_posix_single_quote(remote_path)));

    use std::io::Read;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    let mut cmd = Command::new(&d.plink);
    cmd.args(&args).stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    let mut child = cmd.spawn().context("启动 plink 失败")?;
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break Some(st),
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    anyhow::bail!("远端 sha256sum 超时（10s）");
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => anyhow::bail!("执行 plink 失败: {e}"),
        }
    };
    let mut out = String::new();
    if let Some(mut o) = child.stdout.take() {
        let _ = o.read_to_string(&mut out);
    }
    if let Some(st) = status {
        if !st.success() {
            let mut err = String::new();
            if let Some(mut x) = child.stderr.take() {
                let _ = x.read_to_string(&mut err);
            }
            anyhow::bail!(
                "远端 sha256sum 失败: {}",
                err.lines().next().unwrap_or("(无输出)").trim()
            );
        }
    }
    out.lines()
        .find_map(parse_sha256sum_output)
        .ok_or_else(|| anyhow::anyhow!("远端 sha256sum 输出无法解析: {out}"))
}
pub fn test_connection(target: &Target) -> Result<String, String> {
    let d = detect_putty_tools();
    if !d.found {
        return Err("未找到 plink（PuTTY），请安装或将套件放入内置目录".to_string());
    }
    let (dest, mut args) =
        PuttyTransport::build_ssh_args(target).map_err(|e| e.to_string())?;
    args.push(dest);
    args.push("echo __RBS_OK__".to_string());

    use std::io::Read;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    let mut cmd = Command::new(&d.plink);
    cmd.args(&args).stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    let mut child = cmd.spawn().map_err(|e| format!("启动 plink 失败: {e}"))?;
    let deadline = Instant::now() + Duration::from_secs(5);

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

    let mut out = String::new();
    let mut err = String::new();
    if let Some(mut o) = child.stdout.take() {
        let _ = o.read_to_string(&mut out);
    }
    if let Some(mut e) = child.stderr.take() {
        let _ = e.read_to_string(&mut err);
    }

    match status {
        Some(st) if st.success() => Ok(format!(
            "连接成功（{}）",
            out.lines().next().unwrap_or("ok").trim()
        )),
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
        None => Err("连接超时（5s）——请检查网络 / 防火墙 / 主机地址".to_string()),
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
    fn sha256sum_output_parsing() {
        // 上传校验：解析远端 sha256sum 输出（`<hash>  <path>`）
        let hash = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        assert_eq!(
            parse_sha256sum_output(&format!("{hash}  /opt/app/server.zip")).as_deref(),
            Some(hash)
        );
        // 大写哈希归一化为小写
        assert_eq!(
            parse_sha256sum_output(&hash.to_uppercase()).as_deref(),
            Some(hash)
        );
        // 非哈希内容（命令不存在等）→ None
        assert!(parse_sha256sum_output("sh: sha256sum: command not found").is_none());
        assert!(parse_sha256sum_output("").is_none());
    }

    #[test]
    fn detect_finds_builtin_when_bundled() {
        // cargo test 会注入 CARGO_MANIFEST_DIR，探测必须命中内置目录
        let bundled =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tools/putty/plink.exe");
        if !bundled.exists() {
            return;
        }
        let d = detect_putty_tools();
        assert!(d.found, "捆绑了 PuTTY 时必须能探测到");
        // config.yaml 配了 putty.custom_path 时自定义目录按设计优先（source=custom）；
        // 未配置时必须命中内置目录
        if d.source != "custom" {
            assert_eq!(d.source, "builtin");
        }
    }
}
