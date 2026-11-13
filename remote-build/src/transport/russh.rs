//! russh 进程内 SSH 传输（v0.5 默认传输，设计文档 §5.2）。
//!
//! - 连接语义与 plink 现状一致：每步/每作业新建连接，**不做透明重连状态机**；
//!   断连直接失败，落引擎既有兜底（upload 默认重试 1 次 + 完整性校验 + on_failure: retry）
//! - keepalive 30s + 连接超时 10s（openssh.rs 硬化三件套语义移植）
//! - 主机键 TOFU：check_server_key 回调算 SHA256 指纹与目标 host_key 比对，
//!   未设置 = 拒连并给出指引（与 plink -hostkey 同一 UX，不自动信任）
//! - 认证：password（凭据走协议内存参数，不进命令行/日志）/ key（OpenSSH 格式）；
//!   agent 与 PPK 见 authenticate() 内的人话指引
//! - upload / download 走 SFTP：远端路径按字面处理（putty.rs 的
//!   「远端路径含空格/shell 特殊字符」告警在 russh 路径不适用）
//!
//! 取消 = drop 连接（进程内实现，无子进程树，win_job 守护不适用）。

use super::{Cancelled, ExitStatus, LogHandle, RemoteTransport, Stream};
use crate::models::{AuthMethod, Target, TargetType};
use crate::transport::putty::{
    escape_posix_single_quote, parse_server_info, parse_sha256sum_batch, ServerInfo, PROBE_CMD,
};
use anyhow::Context as _;
use async_trait::async_trait;
use russh::client::{AuthResult, Handle};
use russh::keys::PrivateKeyWithHashAlg;
use russh_sftp::client::SftpSession;
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::sync::broadcast;

/// 协议级 keepalive 间隔（§5.2）：治 VPN/NAT 掐空闲连接（plink 无 CLI 保活参数）
const KEEPALIVE_INTERVAL_SECS: u64 = 30;
/// keepalive 无响应次数上限：超限断连快速失败，不等步骤超时才发现链路已死
const KEEPALIVE_MAX: usize = 3;
/// 连接超时（TCP + 握手 + 认证）：openssh.rs 硬化件 ConnectTimeout 语义移植
const CONNECT_TIMEOUT_SECS: u64 = 10;
/// SFTP 传输块大小
const CHUNK_SIZE: usize = 64 * 1024;
/// 远端 sha256 批量探测超时（与 putty::remote_sha256_batch 对齐）
const SHA256_TIMEOUT_SECS: u64 = 30;

pub struct RusshTransport;

impl RusshTransport {
    pub fn new() -> Self {
        Self
    }
}

impl Default for RusshTransport {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// 连接（TOFU + 认证）
// ---------------------------------------------------------------------------

/// 主机键 TOFU（§5.2）：算 SHA256 指纹与目标 host_key 比对；未登记一律拒连。
/// 指纹经 `seen` 记录下来，拒连后由 connect_error 组装人话指引。
struct ClientHandler {
    /// 目标配置的期望指纹（规范化 SHA256: 形式；None = 未登记）
    expected: Option<String>,
    seen: Arc<Mutex<Option<String>>>,
}

impl russh::client::Handler for ClientHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &russh::keys::PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        let fp = server_public_key
            .public_key()
            .fingerprint(ssh_key::HashAlg::Sha256)
            .to_string();
        *self.seen.lock().unwrap_or_else(|e| e.into_inner()) = Some(fp.clone());
        Ok(self.expected.as_deref() == Some(fp.as_str()))
    }
}

struct Connection {
    handle: Handle<ClientHandler>,
}

/// 客户端连接配置（openssh.rs 硬化三件套语义的移植载体，单测锁死）：
/// keepalive 30s ≈ ServerAliveInterval、keepalive_max 3 次无响应断连、
/// 无 GC 式闲置断连（长任务静默期由 keepalive 维持活性）；
/// 连接超时 10s ≈ ConnectTimeout 由 connect_and_auth 的 timeout 包裹实现。
fn hardened_client_config() -> Arc<russh::client::Config> {
    Arc::new(russh::client::Config {
        keepalive_interval: Some(Duration::from_secs(KEEPALIVE_INTERVAL_SECS)),
        keepalive_max: KEEPALIVE_MAX,
        inactivity_timeout: None,
        ..Default::default()
    })
}

/// 每次操作新建连接（§5.2 连接语义）：TCP + 握手 + 认证 共享 CONNECT_TIMEOUT 上限。
/// 认证也在超时罩内——服务器接受 TCP 但认证往返挂死时不至于无限等
async fn connect_and_auth(
    target: &Target,
    transient_password: Option<&str>,
) -> anyhow::Result<Connection> {
    let TargetType::Ssh { host, port, user, auth, host_key } = &target.target_type else {
        anyhow::bail!("russh 传输仅支持 SSH 目标，收到目标「{}」", target.name);
    };
    let expected = normalize_expected_host_key(host_key.as_deref(), target)?;
    let seen = Arc::new(Mutex::new(None::<String>));
    let config = hardened_client_config();
    let handler = ClientHandler { expected: expected.clone(), seen: seen.clone() };

    let handle = tokio::time::timeout(
        Duration::from_secs(CONNECT_TIMEOUT_SECS),
        async {
            let mut h = russh::client::connect(config, (host.as_str(), *port), handler)
                .await
                .map_err(|e| connect_error(&seen, expected.as_deref(), e))?;
            authenticate(&mut h, target, user, auth, transient_password).await?;
            Ok::<_, anyhow::Error>(h)
        },
    )
    .await
    .map_err(|_| {
        anyhow::anyhow!(
            "连接/认证超时（{CONNECT_TIMEOUT_SECS}s）——请检查网络 / 防火墙 / 主机地址: {host}:{port}"
        )
    })??;
    Ok(Connection { handle })
}

/// 目标 host_key → 规范化 SHA256 指纹。russh 路径只认 SHA256 形式：
/// 手头只有 MD5 指纹时请改用连接测试输出中的 SHA256 值，或把该目标切回 putty 传输
fn normalize_expected_host_key(raw: Option<&str>, target: &Target) -> anyhow::Result<Option<String>> {
    let Some(raw) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    let norm = crate::transport::putty::normalize_host_key(raw).ok_or_else(|| {
        anyhow::anyhow!(
            "主机键指纹格式无效（当前值「{raw}」）：应为 SHA256: 开头的服务器指纹，\
             可从连接测试输出中选中复制"
        )
    })?;
    if !norm.starts_with("SHA256:") {
        anyhow::bail!(
            "russh 传输只支持 SHA256 主机键指纹（当前为 MD5 形式）：\
             请改用连接测试输出中的 SHA256:… 指纹，或把目标「{}」的传输切回 putty",
            target.name
        );
    }
    Ok(Some(norm))
}

/// 连接失败的错误组装：见到指纹但未匹配 = TOFU 拒连，给指纹指引；
/// 其余按原始错误透传
fn connect_error(
    seen: &Arc<Mutex<Option<String>>>,
    expected: Option<&str>,
    e: russh::Error,
) -> anyhow::Error {
    let seen_fp = seen.lock().unwrap_or_else(|e| e.into_inner()).clone();
    if let Some(fp) = &seen_fp {
        match expected {
            Some(exp) if fp != exp => {
                return anyhow::anyhow!(
                    "服务器指纹不匹配：配置为 {exp}，服务器实际为 {fp}。\n\
                     若服务器确实更换过（重装/换机），更新目标配置的「主机键指纹」后重试"
                );
            }
            None => {
                return anyhow::anyhow!(
                    "该主机尚未登记主机键指纹（TOFU，不自动信任）：\n\
                     把服务器指纹 {fp} 填入目标配置的「主机键指纹」字段后重试"
                );
            }
            _ => {}
        }
    }
    anyhow::anyhow!("连接失败: {e}")
}

async fn authenticate(
    handle: &mut Handle<ClientHandler>,
    target: &Target,
    user: &str,
    auth: &AuthMethod,
    transient_password: Option<&str>,
) -> anyhow::Result<()> {
    match auth {
        AuthMethod::Password { .. } => {
            // 密码走协议内存参数，不进命令行 / 日志（§5.7；对比 plink -pw 的进程列表暴露窗口）
            let pwd = super::resolve_target_password(target, transient_password)?;
            match handle.authenticate_password(user, pwd).await? {
                AuthResult::Success => Ok(()),
                other => anyhow::bail!(
                    "认证失败（密码）：用户 {user} 被服务器拒绝（{other:?}）——\
                     请检查用户名 / 密码，或在目标管理重新保存密码"
                ),
            }
        }
        AuthMethod::Key { path } => {
            if path.to_ascii_lowercase().ends_with(".ppk") {
                anyhow::bail!(
                    "russh 传输不支持 PuTTY PPK 私钥（{path}）：\
                     请用 PuTTY 自带的 puttygen 转换为 OpenSSH 格式 \
                     （puttygen \"{path}\" -O private-openssh -o \"{path}.pem\"）后改用转换产物，\
                     或把该目标的传输切回 putty"
                );
            }
            let key = russh::keys::load_secret_key(path, None)
                .map_err(|e| anyhow::anyhow!("加载私钥失败（{path}）: {e}"))?;
            // RSA 私钥按服务器支持的最高哈希签名（裸 ssh-rsa/SHA-1 已被现代服务端禁用）
            let hash = match handle.best_supported_rsa_hash().await? {
                Some(h) => h,
                None => None,
            };
            match handle
                .authenticate_publickey(user, PrivateKeyWithHashAlg::new(Arc::new(key), hash))
                .await?
            {
                AuthResult::Success => Ok(()),
                other => anyhow::bail!(
                    "认证失败（私钥）：用户 {user} 被服务器拒绝（{other:?}）——\
                     请确认公钥已加到远端 authorized_keys"
                ),
            }
        }
        AuthMethod::Agent => anyhow::bail!(
            "russh 传输暂不支持 agent（Pageant）认证：请改用 password / key 认证，\
             或把该目标的传输切回 putty"
        ),
        AuthMethod::None => anyhow::bail!(
            "目标「{}」未配置认证方式（auth: password / key / agent）",
            target.name
        ),
    }
}

// ---------------------------------------------------------------------------
// 远端命令执行
// ---------------------------------------------------------------------------

/// exec 输出按行切分缓冲：按**字节**累积，遇 \n 才整体解码。
/// SSH Data 块在任意字节边界切分——逐 chunk 解码时，跨块的多字节 UTF-8
/// （中文日志）解码失败会落进 OEM 代码页兜底变成乱码且无法恢复；
/// 0x0A 不会出现在 UTF-8 多字节序列内部，行边界解码无此问题
/// （与 local.rs 的 read_until(b'\n') 同语义）。
#[derive(Default)]
struct LineBuffer {
    pending: Vec<u8>,
}

impl LineBuffer {
    /// 喂入原始字节；完整行（含尾部 \n）解码后交给 sink（行尾 \r\n 已修剪）
    fn pump(&mut self, chunk: &[u8], mut sink: impl FnMut(String)) {
        self.pending.extend_from_slice(chunk);
        while let Some(pos) = self.pending.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.pending.drain(..=pos).collect();
            sink(decode_line(&line));
        }
    }

    /// 流结束后冲出未以 \n 结尾的残余行
    fn flush(&mut self, mut sink: impl FnMut(String)) {
        if !self.pending.is_empty() {
            let rest = std::mem::take(&mut self.pending);
            sink(decode_line(&rest));
        }
    }
}

fn decode_line(bytes: &[u8]) -> String {
    super::decode_console_bytes(bytes)
        .trim_end_matches(['\r', '\n'])
        .to_string()
}

/// 单连接单命令执行。log = Some 逐行转发输出（exec 步骤）；None 只收集 stdout
/// （sha256 批量探测等内部命令，输出不进控制台）。返回 (stdout 全文, 退出码)。
/// stdout 全文仅在 log = None 时收集——exec 步骤的输出捕获走 LogHandle
/// 的独立捕获缓冲，此前无条件整流缓冲一个大 String 纯属白占内存。
async fn run_remote_command(
    handle: &mut Handle<ClientHandler>,
    command: &str,
    log: Option<&LogHandle>,
    cancel: &mut broadcast::Receiver<()>,
) -> anyhow::Result<(String, i32)> {
    let mut channel = handle.channel_open_session().await?;
    channel.exec(true, command).await?;
    let mut stdout_lines = LineBuffer::default();
    let mut stderr_lines = LineBuffer::default();
    let mut all_stdout = String::new();
    let mut exit_code: Option<u32> = None;
    loop {
        let msg = tokio::select! {
            m = channel.wait() => m,
            // 取消 = 放弃连接：Handle drop 关闭传输，远端进程随会话终止
            _ = cancel.recv() => return Err(anyhow::Error::new(Cancelled)),
        };
        match msg {
            Some(russh::ChannelMsg::Data { ref data }) => {
                let collect = log.is_none();
                stdout_lines.pump(data.as_ref(), |line| {
                    if let Some(l) = log {
                        l.line(Stream::Stdout, &line);
                    }
                    if collect {
                        all_stdout.push_str(&line);
                        all_stdout.push('\n');
                    }
                });
            }
            Some(russh::ChannelMsg::ExtendedData { ref data, .. }) => {
                if let Some(l) = log {
                    stderr_lines.pump(data.as_ref(), |line| {
                        l.line(Stream::Stderr, &line);
                    });
                }
            }
            Some(russh::ChannelMsg::ExitStatus { exit_status }) => exit_code = Some(exit_status),
            Some(russh::ChannelMsg::Eof) => {}
            Some(russh::ChannelMsg::Close) | None => break,
            _ => {}
        }
    }
    let flush_stdout = |line: String| {
        if let Some(l) = log {
            l.line(Stream::Stdout, &line);
        }
        if log.is_none() {
            all_stdout.push_str(&line);
            all_stdout.push('\n');
        }
    };
    stdout_lines.flush(flush_stdout);
    stderr_lines.flush(|line| {
        if let Some(l) = log {
            l.line(Stream::Stderr, &line);
        }
    });
    Ok((all_stdout, exit_code.map(|c| c as i32).unwrap_or(-1)))
}

/// 内部命令用的取消信道：Sender 活在调用作用域内（否则 recv 立即 Closed 误触发取消）
fn internal_cancel() -> (broadcast::Sender<()>, broadcast::Receiver<()>) {
    broadcast::channel(1)
}

// ---------------------------------------------------------------------------
// SFTP（upload / download，§5.3 传输行为变化）
// ---------------------------------------------------------------------------

async fn sftp_session(handle: &mut Handle<ClientHandler>) -> anyhow::Result<SftpSession> {
    let channel = handle.channel_open_session().await?;
    // 先声明 sftp 子系统（服务端据此切换到 SFTP 协议处理），再交给 SftpSession
    channel
        .request_subsystem(true, "sftp")
        .await
        .context("请求 sftp 子系统失败")?;
    SftpSession::new(channel.into_stream())
        .await
        .context("SFTP 会话建立失败（服务端未启用 sftp 子系统？可在目标编辑器把传输切回 putty 走 pscp）")
}

/// 逐级创建远端目录；已存在的 create_dir 失败忽略，真正失败在后续 open/create 暴露
async fn sftp_mkdir_p(sftp: &SftpSession, path: &str) {
    let mut cur = String::new();
    for part in path.split('/') {
        if part.is_empty() {
            continue; // 绝对路径前导空段
        }
        if !cur.is_empty() {
            cur.push('/');
        }
        cur.push_str(part);
        let _ = sftp.create_dir(&cur).await;
    }
}

/// 远端路径的父目录（"a/b/c" → "a/b"；无父级返回 None）
fn parent_dir(path: &str) -> Option<String> {
    let path = path.trim_end_matches('/');
    match path.rfind('/') {
        Some(0) => Some("/".to_string()),
        Some(i) => Some(path[..i].to_string()),
        None => None,
    }
}

async fn upload_file(
    sftp: &SftpSession,
    local: &Path,
    remote: &str,
    cancel: &mut broadcast::Receiver<()>,
) -> anyhow::Result<()> {
    if let Some(parent) = parent_dir(remote) {
        sftp_mkdir_p(sftp, &parent).await;
    }
    let mut src = std::fs::File::open(local)
        .with_context(|| format!("读取本地文件失败: {}", local.display()))?;
    let mut dst = sftp
        .create(remote.to_string())
        .await
        .with_context(|| format!("远端打开写入失败: {remote}（路径无权限？）"))?;
    let mut buf = vec![0u8; CHUNK_SIZE];
    loop {
        // 本地磁盘读 64KB 同步执行（SSD 微秒级，不值得 spawn_blocking 拆碎片）
        let n = std::io::Read::read(&mut src, &mut buf)
            .with_context(|| format!("读取本地文件失败: {}", local.display()))?;
        if n == 0 {
            break;
        }
        tokio::select! {
            w = dst.write_all(&buf[..n]) => w?,
            _ = cancel.recv() => return Err(anyhow::Error::new(Cancelled)),
        }
    }
    dst.flush().await?;
    // shutdown = SFTP close：等服务端确认后返回（putty 路径由 pscp 保证）
    dst.shutdown().await?;
    Ok(())
}

/// 目录递归上传：远端路径即目标目录，内容逐级合并（§5.9 目录映射
/// `target/config/ -> /opt/app/config/` 语义；pscp -r 的嵌套行为不在此复刻，
/// 引擎对目录上传本就跳过校验并建议先 pack）。
/// async fn 递归必须装箱（E0733）：手动 Box::pin 返回。
/// 目录符号链接 / junction 不跟随（与 pack.rs 防环同款）：自引用源目录会把
/// 栈与内存打爆，且全程不触达文件导致取消信号无从观察；深度上限兜底。
fn upload_dir<'a>(
    sftp: &'a SftpSession,
    local: &'a Path,
    remote: String,
    cancel: &'a mut broadcast::Receiver<()>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send + 'a>> {
    upload_dir_inner(sftp, local, remote, cancel, 0)
}

const UPLOAD_DIR_MAX_DEPTH: usize = 64;

fn upload_dir_inner<'a>(
    sftp: &'a SftpSession,
    local: &'a Path,
    remote: String,
    cancel: &'a mut broadcast::Receiver<()>,
    depth: usize,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send + 'a>> {
    Box::pin(async move {
        if depth > UPLOAD_DIR_MAX_DEPTH {
            anyhow::bail!(
                "目录层级超过 {UPLOAD_DIR_MAX_DEPTH} 层，停止上传: {}（疑似符号链接环路）",
                local.display()
            );
        }
        sftp_mkdir_p(sftp, &remote).await;
        let entries = std::fs::read_dir(local)
            .with_context(|| format!("读取目录失败: {}", local.display()))?;
        for entry in entries {
            let entry = entry?;
            let child_local = entry.path();
            // file_type() 不跟随链接：目录链接（含 junction）跳过，防环
            let ft = entry.file_type().with_context(|| {
                format!("读取条目类型失败: {}", child_local.display())
            })?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let child_remote = format!("{}/{}", remote.trim_end_matches('/'), name);
            if ft.is_dir() {
                upload_dir_inner(sftp, &child_local, child_remote, cancel, depth + 1).await?;
            } else if ft.is_file() {
                upload_file(sftp, &child_local, &child_remote, cancel).await?;
            }
        }
        Ok(())
    })
}

#[async_trait]
impl RemoteTransport for RusshTransport {
    async fn exec(
        &self,
        target: &Target,
        command: &str,
        working_dir: &str,
        _env: &BTreeMap<String, String>,
        log: &LogHandle,
        cancel: &mut broadcast::Receiver<()>,
    ) -> anyhow::Result<ExitStatus> {
        let mut conn = connect_and_auth(target, None).await?;
        // cd 组合共用（空命令报错 / ~ 展开语义），见 mod.rs
        let full_command = super::compose_remote_command(working_dir, command)?;
        // env 与 putty 行为对齐：不影响远端 shell（远端变量请写在命令里）
        let (_, code) = run_remote_command(&mut conn.handle, &full_command, Some(log), cancel).await?;
        Ok(ExitStatus::from_code(code))
    }

    async fn upload(
        &self,
        target: &Target,
        local: &Path,
        remote: &str,
        _log: &LogHandle,
        cancel: &mut broadcast::Receiver<()>,
    ) -> anyhow::Result<()> {
        if !local.exists() {
            anyhow::bail!("上传源不存在: {}", local.display());
        }
        let mut conn = connect_and_auth(target, None).await?;
        let sftp = sftp_session(&mut conn.handle).await?;
        if local.is_dir() {
            upload_dir(&sftp, local, remote.to_string(), cancel).await
        } else {
            upload_file(&sftp, local, remote, cancel).await
        }
    }

    async fn download(
        &self,
        target: &Target,
        remote: &str,
        local: &Path,
        _log: &LogHandle,
        cancel: &mut broadcast::Receiver<()>,
    ) -> anyhow::Result<()> {
        let mut conn = connect_and_auth(target, None).await?;
        let sftp = sftp_session(&mut conn.handle).await?;
        // 与 putty 路径一致：单文件下载（pscp 同样不支持裸目录；目录先 pack）。
        // 先写 .tmp 再改名：中途失败/取消不留残缺文件在目标路径冒充完整产物
        let mut src = sftp
            .open(remote.to_string())
            .await
            .with_context(|| format!("远端打开失败: {remote}（文件不存在？目录请先 pack 成单文件）"))?;
        let tmp = local.with_extension("rbs-download.tmp");
        let write = async {
            let mut dst = std::fs::File::create(&tmp)
                .with_context(|| format!("创建本地文件失败: {}", tmp.display()))?;
            let mut buf = vec![0u8; CHUNK_SIZE];
            loop {
                let n = tokio::select! {
                    r = src.read(&mut buf) => r?,
                    _ = cancel.recv() => return Err(anyhow::Error::new(Cancelled)),
                };
                if n == 0 {
                    break;
                }
                // 本地写盘同步执行（同 upload 的取舍）
                std::io::Write::write_all(&mut dst, &buf[..n])
                    .with_context(|| format!("写入本地文件失败: {}", tmp.display()))?;
            }
            std::io::Write::flush(&mut dst)?;
            Ok(())
        };
        match write.await {
            Ok(()) => {}
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                return Err(e);
            }
        }
        std::fs::rename(&tmp, local).with_context(|| {
            format!(
                "下载完成但改名失败（{} → {}）",
                tmp.display(),
                local.display()
            )
        })?;
        Ok(())
    }

    async fn remote_sha256_batch(
        &self,
        target: &Target,
        remote_paths: &[String],
    ) -> anyhow::Result<HashMap<String, String>> {
        if remote_paths.is_empty() {
            return Ok(HashMap::new());
        }
        let mut conn = connect_and_auth(target, None).await?;
        let joined = remote_paths
            .iter()
            .map(|p| format!("'{}'", escape_posix_single_quote(p)))
            .collect::<Vec<_>>()
            .join(" ");
        let (tx, mut rx) = internal_cancel();
        let result = tokio::time::timeout(
            Duration::from_secs(SHA256_TIMEOUT_SECS),
            run_remote_command(&mut conn.handle, &format!("sha256sum {joined}"), None, &mut rx),
        )
        .await
        .map_err(|_| anyhow::anyhow!("远端 sha256sum 超时（{SHA256_TIMEOUT_SECS}s）"))??;
        drop(tx);
        let (stdout, code) = result;
        let map = parse_sha256sum_batch(&stdout);
        if map.is_empty() && code != 0 {
            anyhow::bail!("远端 sha256sum 失败（退出码 {code}，无可解析输出）");
        }
        Ok(map)
    }
}

// ---------------------------------------------------------------------------
// 连接测试（russh 版 probe，与 putty::probe_server 同 UX）
// ---------------------------------------------------------------------------

/// 连接测试 + 服务器体检（russh 版）：同 putty::probe_server 的采集与解析
/// （PROBE_CMD + parse_server_info），错误带 TOFU 指纹指引。命令 8s 超时。
pub async fn probe_server_russh(
    target: &Target,
    transient_password: Option<&str>,
) -> Result<(ServerInfo, u128), String> {
    let started = std::time::Instant::now();
    let run = async {
        let mut conn = connect_and_auth(target, transient_password).await.map_err(|e| e.to_string())?;
        let (tx, mut rx) = internal_cancel();
        let (out, _code) = run_remote_command(&mut conn.handle, PROBE_CMD, None, &mut rx)
            .await
            .map_err(|e| e.to_string())?;
        drop(tx);
        Ok(parse_server_info(&out))
    };
    match tokio::time::timeout(Duration::from_secs(8), run).await {
        Ok(Ok(info)) => Ok((info, started.elapsed().as_millis())),
        Ok(Err(e)) => Err(e),
        Err(_) => Err("连接超时（8s）——请检查网络 / 防火墙 / 主机地址".to_string()),
    }
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod e2e;

#[cfg(test)]
mod tests {
    use super::*;

    fn ssh_target(auth: AuthMethod) -> Target {
        Target {
            name: "t".into(),
            target_type: TargetType::Ssh {
                host: "192.168.1.100".into(),
                port: 22,
                user: "builder".into(),
                auth,
                host_key: None,
            },
            transport: Some(crate::models::TransportKind::Russh),
            work_root: None,
            env: BTreeMap::new(),
            cred_key: None,
        }
    }

    #[test]
    fn openssh_hardening_semantics_migrated() {
        // openssh.rs 删除后，其 build_ssh_args 三件套断言迁移到这里：
        // keepalive（ServerAliveInterval 语义）/ 断连上限 / 无闲置 GC。
        let cfg = hardened_client_config();
        assert_eq!(cfg.keepalive_interval, Some(Duration::from_secs(30)));
        assert_eq!(cfg.keepalive_max, 3, "3 次无响应即断连，不等步骤超时");
        assert_eq!(cfg.inactivity_timeout, None, "长任务静默期不因闲置被 GC 断连");
        // ConnectTimeout 语义：TCP+握手+认证 共享 10s 上限（connect_and_auth 包裹）
        assert_eq!(CONNECT_TIMEOUT_SECS, 10);
    }

    #[test]
    fn host_key_normalization_and_md5_rejection() {
        // SHA256 指纹规范化通过（复用 putty::normalize_host_key 的解析）
        let mut t = ssh_target(AuthMethod::Agent);
        if let TargetType::Ssh { host_key, .. } = &mut t.target_type {
            *host_key = Some(format!("  ssh-ed25519 255 {} \n", "SHA256:E4Wpmw1njCQ2d/UhkEpm9xMOTGDIZhgVLIsa0SnpGw4"));
        }
        let got = normalize_expected_host_key(
            if let TargetType::Ssh { host_key, .. } = &t.target_type { host_key.as_deref() } else { unreachable!() },
            &t,
        )
        .unwrap();
        assert_eq!(
            got.as_deref(),
            Some("SHA256:E4Wpmw1njCQ2d/UhkEpm9xMOTGDIZhgVLIsa0SnpGw4")
        );

        // MD5 指纹 → 明确报错（russh 只认 SHA256）
        let mut t2 = ssh_target(AuthMethod::Agent);
        if let TargetType::Ssh { host_key, .. } = &mut t2.target_type {
            *host_key = Some("aa:bb:cc:dd:ee:ff:00:11:22:33:44:55:66:77:88:99".to_string());
        }
        let err = normalize_expected_host_key(
            if let TargetType::Ssh { host_key, .. } = &t2.target_type { host_key.as_deref() } else { unreachable!() },
            &t2,
        )
        .unwrap_err();
        assert!(err.to_string().contains("SHA256"), "got: {err}");

        // 坏格式 → 与 plink 同样的人话报错
        let mut t3 = ssh_target(AuthMethod::Agent);
        if let TargetType::Ssh { host_key, .. } = &mut t3.target_type {
            *host_key = Some("SHA256:短了".to_string());
        }
        let err = normalize_expected_host_key(
            if let TargetType::Ssh { host_key, .. } = &t3.target_type { host_key.as_deref() } else { unreachable!() },
            &t3,
        )
        .unwrap_err();
        assert!(err.to_string().contains("主机键指纹格式无效"), "got: {err}");

        // None / 空串 = 未登记，不报错（TOFU 拒连发生在连接期）
        assert_eq!(normalize_expected_host_key(None, &t).unwrap(), None);
        assert_eq!(normalize_expected_host_key(Some("  "), &t).unwrap(), None);
    }

    #[test]
    fn connect_error_gives_tofu_guidance() {
        // 未登记指纹 + 见到服务器指纹 → 给「填入指纹」指引
        let seen = Arc::new(Mutex::new(Some("SHA256:abc".to_string())));
        let err = connect_error(&seen, None, russh::Error::UnknownKey);
        assert!(err.to_string().contains("把服务器指纹 SHA256:abc 填入"), "got: {err}");

        // 配置与实际不匹配 → 指纹不匹配提示
        let err = connect_error(&seen, Some("SHA256:xyz"), russh::Error::UnknownKey);
        assert!(err.to_string().contains("指纹不匹配"), "got: {err}");

        // 匹配但其他错误 → 透传（不会误报 TOFU）
        let err = connect_error(&seen, Some("SHA256:abc"), russh::Error::UnknownKey);
        assert!(!err.to_string().contains("指纹"), "got: {err}");

        // 没见到指纹（网络层失败）→ 透传
        let empty = Arc::new(Mutex::new(None::<String>));
        let err = connect_error(&empty, None, russh::Error::UnknownKey);
        assert!(err.to_string().contains("连接失败"), "got: {err}");
    }

    #[test]
    fn connect_rejects_non_ssh_target() {
        let t = Target { name: "l".into(), target_type: TargetType::Local, ..Default::default() };
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        // Connection 不必实现 Debug：match 取 Err，避免 unwrap_err 的 Debug 约束
        let err = match rt.block_on(connect_and_auth(&t, None)) {
            Err(e) => e,
            Ok(_) => panic!("非 SSH 目标应被拒绝"),
        };
        assert!(err.to_string().contains("仅支持 SSH"), "got: {err}");
    }

    #[test]
    fn parent_dir_extraction() {
        assert_eq!(parent_dir("a/b/c").as_deref(), Some("a/b"));
        assert_eq!(parent_dir("/opt/app/x.jar").as_deref(), Some("/opt/app"));
        assert_eq!(parent_dir("/root").as_deref(), Some("/"));
        assert_eq!(parent_dir("file.jar"), None);
        assert_eq!(parent_dir("/opt/app/").as_deref(), Some("/opt"));
    }

    #[test]
    fn line_buffer_pumps_complete_lines_only() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        let log = LogHandle::new(tx, 0);
        let mut lb = LineBuffer::default();
        // 一行中文 + \r\n：故意把多字节字符从中间切开喂入（SSH Data 块的真实
        // 边界行为）——按字节累积 + 行边界解码，不再落入 OEM 代码页乱码
        let text = "构建输出 hello\r\nwor";
        let bytes = text.as_bytes();
        let split = "构建输".len() - 1; // 切在「输」的 3 字节序列中间
        lb.pump(&bytes[..split], |line| log.line(Stream::Stdout, line));
        lb.pump(&bytes[split..], |line| log.line(Stream::Stdout, line));
        lb.pump(b"ld\n", |line| log.line(Stream::Stdout, line));
        lb.flush(|line| log.line(Stream::Stdout, line));
        drop(log);
        let mut lines = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            match ev {
                crate::models::LogEvent::Stdout { line, .. } => lines.push(line),
                _ => {}
            }
        }
        assert_eq!(lines, vec!["构建输出 hello", "world"]);
    }
}
