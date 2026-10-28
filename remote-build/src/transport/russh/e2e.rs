//! russh 传输进程内 e2e（§5 验收清单）。
//!
//! 用 russh server + russh-sftp server 在 127.0.0.1 起真实 SSH 服务端，
//! SFTP 文件操作映射到临时目录（"假远端" = chroot 语义：远端绝对路径
//! `/opt/app/x.jar` → `<store>/opt/app/x.jar`），exec 命令按前缀识别给出
//! 仿真响应（sha256sum 真算、test -e 真判、PROBE_CMD 给样例输出）。
//!
//! 覆盖：TOFU（未登记拒连带指纹指引 / 指纹不匹配报错 / 登记后放行）、
//! 密码认证（含错误密码拒绝）、exec 输出与退出码、SFTP 上传 / 下载、
//! sha256 批量探测（缺失文件部分成功语义）。

use super::{connect_and_auth, probe_server_russh, run_remote_command, RusshTransport};
use crate::models::{AuthMethod, Target, TargetType, TransportKind};
use crate::transport::RemoteTransport;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;

/// 测试专用 ed25519 主机键（固定指纹，见 test 顶部断言；仅本测试文件使用）
const HOST_KEY_PEM: &str = "-----BEGIN OPENSSH PRIVATE KEY-----\n\
b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtzc2gtZW\n\
QyNTUxOQAAACD6fnXWD6p5/Wc/iZJyoUvqdQ96cq1NbF9i/c7tMTqv/QAAAJAZFK8yGRSv\n\
MgAAAAtzc2gtZWQyNTUxOQAAACD6fnXWD6p5/Wc/iZJyoUvqdQ96cq1NbF9i/c7tMTqv/Q\n\
AAAEAJzEaPQsmFb5rbf4U5+IaynTm4O1GefjuziKoxXRmTHfp+ddYPqnn9Zz+JknKhS+p1\n\
D3pyrU1sX2L9zu0xOq/9AAAADXJicy10ZXN0LWhvc3Q=\n\
-----END OPENSSH PRIVATE KEY-----";

const USERNAME: &str = "builder";
const PASSWORD: &str = "e2e-secret";

/// 凭据键带进程号：两个测试进程并发跑同一 e2e 时（如 IDE 与 CLI 同时 cargo test）
/// 互不覆盖——固定键会被对方结束时的 CredGuard 清理误删，导致本进程「密码凭空消失」
fn cred_key() -> String {
    format!("e2e-russh/prod-{}", std::process::id())
}

// ---------------------------------------------------------------------------
// 服务端：SSH 会话（认证 / exec / sftp 子系统）
// ---------------------------------------------------------------------------

struct ServerFactory {
    store: PathBuf,
}

impl russh::server::Server for ServerFactory {
    type Handler = SessHandler;

    fn new_client(&mut self, _peer: Option<std::net::SocketAddr>) -> Self::Handler {
        SessHandler {
            store: self.store.clone(),
            channels: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

struct SessHandler {
    store: PathBuf,
    channels: Arc<Mutex<HashMap<russh::ChannelId, russh::Channel<russh::server::Msg>>>>,
}

impl russh::server::Handler for SessHandler {
    type Error = russh::Error;

    async fn auth_password(
        &mut self,
        user: &str,
        password: &str,
    ) -> Result<russh::server::Auth, Self::Error> {
        if user == USERNAME && password == PASSWORD {
            Ok(russh::server::Auth::Accept)
        } else {
            Ok(russh::server::Auth::Reject {
                proceed_with_methods: None,
                partial_success: false,
            })
        }
    }

    /// key 认证：只认 HOST_KEY_PEM 的公钥（e2e 复用主机键当用户键，免 rand 生成）
    async fn auth_publickey(
        &mut self,
        user: &str,
        key: russh::server::VerifiedPublicKey<'_>,
    ) -> Result<russh::server::Auth, Self::Error> {
        let expected = russh::keys::PrivateKey::from_openssh(HOST_KEY_PEM)
            .expect("测试主机键解析")
            .public_key()
            .clone();
        // 只比 key_data：PublicKey 的派生 PartialEq 连 comment 一起比，
        // 而 PEM 注释（rbs-test-host）不随 SSH 线上协议传输
        if user == USERNAME && key.public_key.key_data() == expected.key_data() {
            Ok(russh::server::Auth::Accept)
        } else {
            Ok(russh::server::Auth::Reject {
                proceed_with_methods: None,
                partial_success: false,
            })
        }
    }

    async fn channel_open_session(
        &mut self,
        channel: russh::Channel<russh::server::Msg>,
        reply: russh::server::ChannelOpenHandle,
        _session: &mut russh::server::Session,
    ) -> Result<(), Self::Error> {
        self.channels.lock().unwrap().insert(channel.id(), channel);
        reply.accept().await;
        Ok(())
    }

    async fn exec_request(
        &mut self,
        channel: russh::ChannelId,
        data: &[u8],
        session: &mut russh::server::Session,
    ) -> Result<(), Self::Error> {
        let cmd = String::from_utf8_lossy(data).to_string();
        session.channel_success(channel)?;
        let (out, code) = exec_canned(&self.store, &cmd);
        if !out.is_empty() {
            session.data(channel, out.into_bytes())?;
        }
        let _ = session.eof(channel);
        session.exit_status_request(channel, code)?;
        session.close(channel)?;
        Ok(())
    }

    async fn subsystem_request(
        &mut self,
        channel: russh::ChannelId,
        name: &str,
        session: &mut russh::server::Session,
    ) -> Result<(), Self::Error> {
        if name != "sftp" {
            session.channel_failure(channel)?;
            return Ok(());
        }
        let ch = self.channels.lock().unwrap().remove(&channel);
        let Some(ch) = ch else {
            session.channel_failure(channel)?;
            return Ok(());
        };
        session.channel_success(channel)?;
        russh_sftp::server::run(ch.into_stream(), FsHandler::new(self.store.clone())).await;
        Ok(())
    }
}

/// exec 命令仿真：映射 chroot 后按前缀分发（sha256sum / test -e / 探测命令 / 其余回显）
fn exec_canned(store: &Path, raw_cmd: &str) -> (String, u32) {
    // `cd 'dir' && cmd` → 剥掉 cd 前缀（引擎 exec 带远端工作目录时的形态）
    let cmd = if raw_cmd.starts_with("cd '") {
        raw_cmd
            .split_once("' && ")
            .map(|(_, rest)| rest.to_string())
            .unwrap_or_else(|| raw_cmd.to_string())
    } else {
        raw_cmd.to_string()
    };

    if cmd.starts_with("sha256sum ") {
        // `sha256sum 'p1' 'p2' …`：真算哈希；缺失文件输出错误行 + 退出码 1
        //（sha256sum 部分成功语义，parse_sha256sum_batch 依赖此行为）
        let mut out = String::new();
        let mut missing = 0;
        for spec in cmd["sha256sum ".len()..].split('\'').filter(|s| !s.trim().is_empty()) {
            let full = resolve_remote(store, spec);
            match std::fs::File::open(&full) {
                Ok(f) => {
                    let h = crate::transport::putty::sha256_file(&full)
                        .unwrap_or_else(|_| sha256_of(f));
                    out.push_str(&format!("{h}  {spec}\n"));
                }
                Err(_) => {
                    missing += 1;
                    out.push_str(&format!("sha256sum: {spec}: No such file or directory\n"));
                }
            }
        }
        return (out, if missing > 0 { 1 } else { 0 });
    }
    if let Some(p) = cmd.strip_prefix("test -e '").and_then(|s| s.strip_suffix('\'')) {
        return (String::new(), u32::from(!resolve_remote(store, p).exists()));
    }
    if cmd.contains("__RBS_BEGIN__") {
        // probe 采集命令：给一组样例输出（parse_server_info 只认键值行）
        let out = "__RBS_BEGIN__\n\
                   os=Linux 6.1.0-e2e\n\
                   host=e2e-fake-host\n\
                   cores=4\n\
                   load=0.10 0.20 0.30 1/100 999\n\
                   uptimesec=3600.00 0.00\n\
                   meminfo=\n\
                   MemTotal:       4096000 kB\n\
                   MemAvailable:   3072000 kB\n\
                   disks=\n\
                   /dev/sda1 10240000 5120000 5120000 50% /\n\
                   __RBS_END__\n";
        return (out.to_string(), 0);
    }
    // 其余命令回显：exec e2e 断言用
    (format!("E2E:{cmd}\n"), 0)
}

fn sha256_of(mut f: std::fs::File) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    let _ = std::io::copy(&mut f, &mut hasher);
    format!("{:x}", hasher.finalize())
}

/// chroot 映射：远端绝对路径 → store 下相对路径（与 SFTP 服务端同规则）
fn resolve_remote(store: &Path, remote: &str) -> PathBuf {
    let rel = remote.trim_start_matches('/');
    let p = store.join(rel);
    #[cfg(windows)]
    {
        // Windows 下路径前导 / 已剥掉；盘符不存在问题（store 在 TEMP 下）
        let _ = &p;
    }
    p
}

// ---------------------------------------------------------------------------
// 服务端：SFTP 文件系统（chroot 到 store）
// ---------------------------------------------------------------------------

struct FsHandler {
    store: PathBuf,
    files: Mutex<HashMap<String, std::fs::File>>,
    next_handle: AtomicU32,
}

impl FsHandler {
    fn new(store: PathBuf) -> Self {
        Self { store, files: Mutex::new(HashMap::new()), next_handle: AtomicU32::new(1) }
    }
}

impl russh_sftp::server::Handler for FsHandler {
    type Error = russh_sftp::protocol::StatusCode;

    fn unimplemented(&self) -> Self::Error {
        russh_sftp::protocol::StatusCode::OpUnsupported
    }

    async fn open(
        &mut self,
        id: u32,
        filename: String,
        pflags: russh_sftp::protocol::OpenFlags,
        _attrs: russh_sftp::protocol::FileAttributes,
    ) -> Result<russh_sftp::protocol::Handle, Self::Error> {
        use russh_sftp::protocol::{Handle, OpenFlags, StatusCode};
        let full = resolve_remote(&self.store, &filename);
        let file = if pflags.contains(OpenFlags::WRITE) {
            if let Some(parent) = full.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(pflags.contains(OpenFlags::TRUNCATE))
                .open(&full)
        } else {
            std::fs::File::open(&full)
        }
        .map_err(|_| StatusCode::NoSuchFile)?;
        let h = self.next_handle.fetch_add(1, Ordering::Relaxed).to_string();
        self.files.lock().unwrap().insert(h.clone(), file);
        Ok(Handle { id, handle: h })
    }

    async fn read(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        len: u32,
    ) -> Result<russh_sftp::protocol::Data, Self::Error> {
        use russh_sftp::protocol::{Data, StatusCode};
        use std::io::{Read as _, Seek as _, SeekFrom};
        let mut map = self.files.lock().unwrap();
        let Some(f) = map.get_mut(&handle) else {
            return Err(StatusCode::NoSuchFile);
        };
        f.seek(SeekFrom::Start(offset)).map_err(|_| StatusCode::Failure)?;
        let mut buf = vec![0u8; len as usize];
        let n = f.read(&mut buf).map_err(|_| StatusCode::Failure)?;
        if n == 0 {
            return Err(StatusCode::Eof);
        }
        buf.truncate(n);
        Ok(Data { id, data: buf })
    }

    async fn write(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        data: Vec<u8>,
    ) -> Result<russh_sftp::protocol::Status, Self::Error> {
        use russh_sftp::protocol::{StatusCode, Status};
        use std::io::{Seek as _, SeekFrom, Write as _};
        let mut map = self.files.lock().unwrap();
        let Some(f) = map.get_mut(&handle) else {
            return Err(StatusCode::NoSuchFile);
        };
        f.seek(SeekFrom::Start(offset)).map_err(|_| StatusCode::Failure)?;
        f.write_all(&data).map_err(|_| StatusCode::Failure)?;
        Ok(Status {
            id,
            status_code: StatusCode::Ok,
            error_message: "Ok".to_string(),
            language_tag: "en-US".to_string(),
        })
    }

    async fn close(
        &mut self,
        id: u32,
        handle: String,
    ) -> Result<russh_sftp::protocol::Status, Self::Error> {
        use russh_sftp::protocol::{StatusCode, Status};
        self.files.lock().unwrap().remove(&handle);
        Ok(Status {
            id,
            status_code: StatusCode::Ok,
            error_message: "Ok".to_string(),
            language_tag: "en-US".to_string(),
        })
    }

    async fn mkdir(
        &mut self,
        id: u32,
        path: String,
        _attrs: russh_sftp::protocol::FileAttributes,
    ) -> Result<russh_sftp::protocol::Status, Self::Error> {
        use russh_sftp::protocol::{StatusCode, Status};
        // 逐级建目录语义（客户端 sftp_mkdir_p 逐级调用）：已存在不报错
        std::fs::create_dir_all(resolve_remote(&self.store, &path))
            .map_err(|_| StatusCode::Failure)?;
        Ok(Status {
            id,
            status_code: StatusCode::Ok,
            error_message: "Ok".to_string(),
            language_tag: "en-US".to_string(),
        })
    }

    async fn stat(
        &mut self,
        id: u32,
        path: String,
    ) -> Result<russh_sftp::protocol::Attrs, Self::Error> {
        use russh_sftp::protocol::{Attrs, FileAttributes, StatusCode};
        let full = resolve_remote(&self.store, &path);
        let meta = std::fs::metadata(&full).map_err(|_| StatusCode::NoSuchFile)?;
        Ok(Attrs {
            id,
            attrs: FileAttributes { size: Some(meta.len()), ..Default::default() },
        })
    }

    async fn lstat(
        &mut self,
        id: u32,
        path: String,
    ) -> Result<russh_sftp::protocol::Attrs, Self::Error> {
        self.stat(id, path).await
    }

    async fn realpath(
        &mut self,
        id: u32,
        path: String,
    ) -> Result<russh_sftp::protocol::Name, Self::Error> {
        use russh_sftp::protocol::{File, FileAttributes, Name};
        Ok(Name {
            id,
            files: vec![File::new(resolve_remote(&self.store, &path).to_string_lossy(), FileAttributes::dummy())],
        })
    }
}

// ---------------------------------------------------------------------------
// 测试脚手架
// ---------------------------------------------------------------------------

/// 测试结束自动清理凭据管理器条目（即使断言 panic 也执行）
struct CredGuard;
impl Drop for CredGuard {
    fn drop(&mut self) {
        let _ = crate::secrets::delete_password(&cred_key());
    }
}

/// 起服务端（随机端口），返回 (端口, chroot 目录)
fn spawn_server(store: PathBuf) -> (u16, PathBuf) {
    use russh::server::Server as _;
    let host_key = russh::keys::PrivateKey::from_openssh(HOST_KEY_PEM).expect("测试主机键解析");
    let config = Arc::new(russh::server::Config {
        auth_rejection_time: Duration::from_millis(50),
        auth_rejection_time_initial: Some(Duration::from_millis(0)),
        keys: vec![host_key],
        ..Default::default()
    });
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().unwrap().port();
    // from_std 要求 socket 已是非阻塞模式，否则注册进 tokio IO 驱动后会卡死 runtime
    listener.set_nonblocking(true).expect("nonblocking");
    let listener = tokio::net::TcpListener::from_std(listener).expect("listener");
    let mut factory = ServerFactory { store: store.clone() };
    tokio::spawn(async move {
        let _ = factory.run_on_socket(config, &listener).await;
    });
    (port, store)
}

fn target_for(port: u16, host_key: Option<String>, password: bool) -> Target {
    Target {
        name: "e2e".to_string(),
        target_type: TargetType::Ssh {
            host: "127.0.0.1".to_string(),
            port,
            user: USERNAME.to_string(),
            auth: if password {
                AuthMethod::Password { encrypted: String::new() }
            } else {
                AuthMethod::None
            },
            host_key,
        },
        transport: Some(TransportKind::Russh),
        work_root: None,
        env: HashMap::new(),
        cred_key: Some(cred_key()),
    }
}

fn server_fingerprint() -> String {
    russh::keys::PrivateKey::from_openssh(HOST_KEY_PEM)
        .unwrap()
        .public_key()
        .fingerprint(ssh_key::HashAlg::Sha256)
        .to_string()
}

/// 单个大 e2e（顺序步骤保证凭据清理与服务端状态确定）。
/// 注意：spawn_server 的 std listener 必须 set_nonblocking 后再 from_std——
/// 阻塞模式 socket 注册进 tokio IO 驱动（Windows IOCP）会让握手期整个 runtime 卡死。
#[tokio::test]
async fn russh_transport_end_to_end() {
    let store = std::env::temp_dir().join(format!("rbs_russh_e2e_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&store);
    std::fs::create_dir_all(&store).unwrap();
    let (port, _store) = spawn_server(store.clone());
    let fp = server_fingerprint();

    // -- 1. TOFU：未登记主机键 → 拒连，错误含服务器指纹与指引
    let t = target_for(port, None, false);
    let err = match connect_and_auth(&t, None).await {
        Err(e) => e.to_string(),
        Ok(_) => panic!("未登记主机键应拒连"),
    };
    assert!(err.contains("尚未登记主机键指纹"), "got: {err}");
    assert!(err.contains(&fp), "错误应含服务器指纹 {fp}: {err}");

    // -- 2. TOFU：指纹不匹配 → 明确报错（MITM 防御）
    let wrong = format!("SHA256:{}", "A".repeat(43));
    let t = target_for(port, Some(wrong), false);
    let err = match connect_and_auth(&t, None).await {
        Err(e) => e.to_string(),
        Ok(_) => panic!("指纹不匹配应拒连"),
    };
    assert!(err.contains("指纹不匹配"), "got: {err}");

    // -- 3. 错误密码 → 认证失败人话报错（指纹已登记）
    let _guard = CredGuard;
    crate::secrets::set_password(&cred_key(), "wrong-password").unwrap();
    let t = target_for(port, Some(fp.clone()), true);
    let err = match connect_and_auth(&t, None).await {
        Err(e) => e.to_string(),
        Ok(_) => panic!("错误密码应认证失败"),
    };
    assert!(err.contains("认证失败（密码）"), "got: {err}");

    // -- 4. 正确密码（凭据管理器路径）+ exec：输出 / 退出码 / stderr
    crate::secrets::set_password(&cred_key(), PASSWORD).unwrap();
    let t = target_for(port, Some(fp.clone()), true);
    let mut conn = connect_and_auth(&t, None).await.expect("登记指纹后应放行");
    let (tx, mut rx) = mpsc::channel(64);
    let log = crate::transport::LogHandle::new(tx, 0);
    let (_ctx, mut cancel) = tokio::sync::broadcast::channel(1);
    let (out, code) =
        run_remote_command(&mut conn.handle, "echo hello-russh", Some(&log), &mut cancel)
            .await
            .expect("exec 成功");
    assert_eq!(code, 0);
    assert_eq!(out, "E2E:echo hello-russh\n");
    drop(log);
    let mut stdout_lines = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        if let crate::models::LogEvent::Stdout { line, .. } = ev {
            stdout_lines.push(line);
        }
    }
    assert_eq!(stdout_lines, vec!["E2E:echo hello-russh"]);

    // 失败退出码透传（引擎据判定步骤失败）
    let (_o, code) = run_remote_command(&mut conn.handle, "test -e '/no/such/file'", None, &mut cancel)
        .await
        .expect("exec 成功");
    assert_eq!(code, 1);

    // -- 5. SFTP 上传（单文件）+ sha256 批量探测（缺失文件部分成功）
    let transport = RusshTransport::new();
    let local = store.join("local-src.txt");
    std::fs::write(&local, b"payload-russh-e2e").unwrap();
    transport
        .upload(&t, &local, "/opt/app/local-src.txt", &log_dummy(), &mut cancel)
        .await
        .expect("上传成功");
    let stored = store.join("opt/app/local-src.txt");
    assert_eq!(std::fs::read(&stored).unwrap(), b"payload-russh-e2e");

    let hashes = transport
        .remote_sha256_batch(
            &t,
            &["/opt/app/local-src.txt".to_string(), "/opt/app/missing.bin".to_string()],
        )
        .await
        .expect("sha256 部分成功");
    assert_eq!(hashes.len(), 1, "缺失文件不出现在结果: {hashes:?}");
    assert!(hashes.contains_key("/opt/app/local-src.txt"));

    // 上传到不存在的目录：自动建父目录（pscp 做不到的增强）
    transport
        .upload(&t, &local, "/opt/app/deep/nested/x.txt", &log_dummy(), &mut cancel)
        .await
        .expect("自动建父目录");
    assert!(store.join("opt/app/deep/nested/x.txt").exists());

    // -- 6. SFTP 下载 + 内容一致
    let local_dst = store.join("downloaded.txt");
    transport
        .download(&t, "/opt/app/local-src.txt", &local_dst, &log_dummy(), &mut cancel)
        .await
        .expect("下载成功");
    assert_eq!(std::fs::read(&local_dst).unwrap(), b"payload-russh-e2e");

    // -- 7. 连接测试（probe）：解析出样例体检信息
    let (info, _elapsed) =
        probe_server_russh(&t, None).await.expect("探测成功");
    assert_eq!(info.hostname, "e2e-fake-host");
    assert_eq!(info.os, "Linux 6.1.0-e2e");
    assert_eq!(info.cores, "4");
    assert_eq!(info.mem_total_mb, 4000);

    // -- 8. key 认证（OpenSSH 私钥文件）：服务端只认 HOST_KEY_PEM 的公钥，
    //      e2e 复用主机键当用户键（免 rand 依赖）；认证后 exec 可用
    let key_path = store.join("user-key.pem");
    std::fs::write(&key_path, HOST_KEY_PEM).unwrap();
    let t_key = {
        let mut tk = target_for(port, Some(fp.clone()), false);
        if let TargetType::Ssh { auth, .. } = &mut tk.target_type {
            *auth = AuthMethod::Key { path: key_path.to_string_lossy().into_owned() };
        }
        tk
    };
    let mut conn = connect_and_auth(&t_key, None).await.expect("key 认证应放行");
    let (_o, code) = run_remote_command(&mut conn.handle, "echo by-key", None, &mut cancel)
        .await
        .expect("key 认证后 exec 成功");
    assert_eq!(code, 0);

    // PPK 私钥 → 人话报错给 puttygen 转换指引（连接成功、认证前拦截）
    let mut t_ppk = target_for(port, Some(fp.clone()), false);
    if let TargetType::Ssh { auth, .. } = &mut t_ppk.target_type {
        *auth = AuthMethod::Key { path: "C:/keys/id.ppk".to_string() };
    }
    let err = match connect_and_auth(&t_ppk, None).await {
        Err(e) => e.to_string(),
        Ok(_) => panic!("PPK 私钥应被拒绝并给转换指引"),
    };
    assert!(err.contains("puttygen"), "got: {err}");

    let _ = std::fs::remove_dir_all(&store);
}

/// 空 LogHandle（内部命令探测不落日志；mpsc 接收端直接丢弃）
fn log_dummy() -> crate::transport::LogHandle {
    let (tx, _rx) = mpsc::channel(64);
    crate::transport::LogHandle::new(tx, 0)
}
