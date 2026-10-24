//! 传输抽象（设计文档 §5.1）。
//!
//! P0 主传输 = PuttyTransport（§5.2）；本机执行走 LocalTransport；
//! OpenSshTransport（P1）/ RusshTransport（P2）为预留实现。

pub mod local;
pub mod openssh;
pub mod putty;
pub mod russh;
pub mod win_job;

use crate::models::{LogEvent, Target, TargetType};
use async_trait::async_trait;
use std::collections::HashMap;
use tokio::sync::{broadcast, mpsc};

/// 日志行流向（§5.1 v0.4 补充定义）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    Stdout,
    Stderr,
}

/// LogSink 的 mpsc 实现：把每行输出包装成 LogEvent 推给引擎/UI。
/// UI 侧在 RunHandle 里持有 receiver 逐帧消费。
#[derive(Clone)]
pub struct LogHandle {
    pub tx: mpsc::Sender<LogEvent>,
    pub step_index: usize,
}

impl LogHandle {
    pub fn new(tx: mpsc::Sender<LogEvent>, step_index: usize) -> Self {
        Self { tx, step_index }
    }

    pub fn line(&self, stream: Stream, text: impl Into<String>) {
        let ev = match stream {
            Stream::Stdout => LogEvent::Stdout {
                step_index: self.step_index,
                line: text.into(),
            },
            Stream::Stderr => LogEvent::Stderr {
                step_index: self.step_index,
                line: text.into(),
            },
        };
        // 通道满时丢弃最不关键的输出行，绝不阻塞执行
        let _ = self.tx.try_send(ev);
    }
}

/// 进程退出状态（§5.6）。与 std::process::ExitStatus 区分，统一跨本地/远端语义。
#[derive(Debug, Clone, Copy)]
pub struct ExitStatus {
    pub code: i32,
    pub success: bool,
}

impl ExitStatus {
    pub fn from_code(code: i32) -> Self {
        Self { code, success: code == 0 }
    }
}

#[async_trait]
pub trait RemoteTransport: Send + Sync {
    /// 执行一条命令；取消信号到达时 kill 子进程并返回 Cancelled 错误
    async fn exec(
        &self,
        target: &Target,
        command: &str,
        working_dir: &str,
        env: &HashMap<String, String>,
        log: &LogHandle,
        cancel: &mut broadcast::Receiver<()>,
    ) -> anyhow::Result<ExitStatus>;

    async fn upload(
        &self,
        target: &Target,
        local: &std::path::Path,
        remote: &str,
        log: &LogHandle,
        cancel: &mut broadcast::Receiver<()>,
    ) -> anyhow::Result<()>;

    /// P1 预留：step_type=download 落地前无调用方（PuttyTransport 已实现 pscp 下载）
    #[allow(dead_code)]
    async fn download(
        &self,
        target: &Target,
        remote: &str,
        local: &std::path::Path,
        log: &LogHandle,
        cancel: &mut broadcast::Receiver<()>,
    ) -> anyhow::Result<()>;
}

/// 按 target 类型构造传输实现（§5.1 实现计划表）
pub fn make_transport(target: Option<&Target>) -> anyhow::Result<std::sync::Arc<dyn RemoteTransport>> {
    match target.map(|t| &t.target_type) {
        Some(TargetType::Ssh { .. }) => {
            let detection = putty::detect_putty_tools();
            if !detection.found {
                anyhow::bail!(
                    "未找到 PuTTY（plink/pscp）。请安装 PuTTY、或在设置中指定路径、\
                     或把套件放入 {}",
                    putty::builtin_dir_hint()
                );
            }
            Ok(std::sync::Arc::new(putty::PuttyTransport::from_detection(&detection)))
        }
        _ => Ok(std::sync::Arc::new(local::LocalTransport)),
    }
}

/// 轮询间隔（step_type=wait）
pub const WAIT_POLL_SECONDS: u64 = 5;

/// 在 env 副本上把 dir 前置到 PATH（env 无 PATH 项时只插入 dir；
/// 进程原有 PATH 由 spawn 时的 merged_env 追加，避免重复）
pub fn with_path_prepended(
    mut env: HashMap<String, String>,
    dir: &std::path::Path,
) -> HashMap<String, String> {
    let key = env
        .keys()
        .find(|k| k.eq_ignore_ascii_case("PATH"))
        .cloned()
        .unwrap_or_else(|| "PATH".to_string());
    let mut paths = vec![dir.to_path_buf()];
    if let Some(current) = env.get(&key) {
        paths.extend(std::env::split_paths(current));
    }
    let joined = std::env::join_paths(&paths).unwrap_or_default();
    env.insert(key, joined.to_string_lossy().to_string());
    env
}

/// 注入子进程前合并 PATH：显式提供的 PATH 在前，进程原有 PATH 追加在后。
/// 否则步骤 env 里设置一次 PATH 就会把系统目录整个顶掉。
pub(crate) fn merged_env(env: &HashMap<String, String>) -> HashMap<String, String> {
    let mut out = env.clone();
    let Some((key, value)) = env
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("PATH"))
        .map(|(k, v)| (k.clone(), v.clone()))
    else {
        return out;
    };
    out.remove(&key);
    let mut paths = std::env::split_paths(&value).collect::<Vec<_>>();
    if let Ok(sys) = std::env::var("PATH") {
        paths.extend(std::env::split_paths(&sys));
    }
    let joined = std::env::join_paths(&paths).unwrap_or_default();
    out.insert(key, joined.to_string_lossy().to_string());
    out
}

/// 子进程输出按行解码：优先按 UTF-8（远端 Linux 的 plink 输出、node 系工具）；
/// 非 UTF-8 时按控制台 OEM 代码页兜底——中文 Windows 为 cp936/GBK，
/// cmd 内置命令（echo）与 JVM 系工具的管道输出都是这个编码。
pub(crate) fn decode_console_bytes(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(_) => decode_with_oem_codepage(bytes),
    }
}

#[cfg(windows)]
fn decode_with_oem_codepage(bytes: &[u8]) -> String {
    use winapi::um::stringapiset::MultiByteToWideChar;
    use winapi::um::winnls::GetOEMCP;

    if bytes.is_empty() {
        return String::new();
    }
    unsafe {
        // 每字节至多产生 2 个 UTF-16 单元（DBCS 情况），多留一倍
        let mut wide = vec![0u16; bytes.len() * 2];
        let n = MultiByteToWideChar(
            GetOEMCP(),
            0, // 不带 MB_ERR_INVALID_CHARS：宽松解码，坏字节不报错
            bytes.as_ptr() as *const i8, // winapi CHAR = i8
            bytes.len() as i32,
            wide.as_mut_ptr(),
            wide.len() as i32,
        );
        if n > 0 {
            wide.truncate(n as usize);
            String::from_utf16_lossy(&wide)
        } else {
            String::from_utf8_lossy(bytes).into_owned()
        }
    }
}

#[cfg(not(windows))]
fn decode_with_oem_codepage(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_helpers_prepend_and_merge() {
        let mut env = HashMap::new();
        env.insert("PATH".to_string(), "C:\\a".to_string());

        let env = with_path_prepended(env, std::path::Path::new("C:\\bin"));
        let paths: Vec<_> = std::env::split_paths(&env["PATH"]).collect();
        assert_eq!(paths[0], std::path::Path::new("C:\\bin"));
        assert_eq!(paths[1], std::path::Path::new("C:\\a"));

        let merged = merged_env(&env);
        let merged_paths: Vec<_> = std::env::split_paths(&merged["PATH"]).collect();
        assert_eq!(merged_paths[0], std::path::Path::new("C:\\bin"));
        assert_eq!(merged_paths[1], std::path::Path::new("C:\\a"));
        // 进程 PATH 追加在后面
        assert!(merged_paths.len() > 2);
    }

    #[test]
    fn console_decode_prefers_utf8() {
        // UTF-8 输出（远端 plink / node 系工具）必须走严格 UTF-8 路径
        assert_eq!(decode_console_bytes("中文 ok\r\n".as_bytes()), "中文 ok\r\n");
    }

    #[cfg(windows)]
    #[test]
    fn console_decode_falls_back_to_oem_codepage() {
        // GBK 编码的 "中文"（D6D0 CEC4）不是合法 UTF-8，应按 OEM 代码页还原；
        // 仅在中文 Windows（OEMCP=936）上断言结果，其他代码页下只要求不 panic
        let gbk = [0xD6u8, 0xD0, 0xCE, 0xC4];
        let cp = unsafe { winapi::um::winnls::GetOEMCP() };
        let decoded = decode_console_bytes(&gbk);
        if cp == 936 {
            assert_eq!(decoded, "中文");
        }
    }
}
