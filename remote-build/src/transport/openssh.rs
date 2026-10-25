//! 系统 OpenSSH 传输（P1 可选，见设计文档 §5.1 实现计划表）。
//! P0 不启用；保留 trait 实现骨架以便后续替换。
//!
//! `build_ssh_args` 是 P1 实现的起步件：硬化参数（超时 / 保活 / 禁交互）
//! 已经固化在这里，P0 先用单元测试锁住行为，届时接 exec/upload 时不至于裸奔。

use super::{ExitStatus, LogHandle, RemoteTransport};
use crate::models::{AuthMethod, Target, TargetType};
use async_trait::async_trait;
use std::collections::HashMap;
use std::path::Path;
use tokio::sync::broadcast;

#[allow(dead_code)]
pub struct OpenSshTransport;

/// OpenSSH 命令行公共参数（ssh / scp / sftp 通用）。
/// 三件硬参数（SSH 稳定性实践，plink 无 CLI 等价物）：
/// - `ConnectTimeout=10`：TCP/握手黑洞快速失败，不用等步骤超时才发现网不通
/// - `ServerAliveInterval=15`：闲置保活，防 NAT / 防火墙掐空闲连接（长任务轮询场景必需）
/// - `BatchMode=yes`：禁一切交互提示（等价 plink -batch）
const SSH_OPTIONS: [&str; 6] = [
    "-o", "ConnectTimeout=10",
    "-o", "ServerAliveInterval=15",
    "-o", "BatchMode=yes",
];

/// 为 ssh / scp / sftp 构建公共参数：硬化三件套 + 端口 + 私钥 + 目的地。
/// 返回 (user@host, args)；host key 固定（TOFU 指纹）P1 接线时经
/// per-target known_hosts（`-o UserKnownHostsFile=` + `StrictHostKeyChecking=yes`）实现，
/// 与 plink -hostkey 语义对齐。
#[allow(dead_code)]
pub(crate) fn build_ssh_args(target: &Target) -> anyhow::Result<(String, Vec<String>)> {
    let TargetType::Ssh { host, port, user, auth, .. } = &target.target_type else {
        anyhow::bail!("OpenSshTransport 仅支持 ssh 目标，收到 {:?}（请用 PuttyTransport 或 local）", target.name);
    };
    let mut args: Vec<String> = SSH_OPTIONS.iter().map(|s| s.to_string()).collect();
    args.push("-p".to_string());
    args.push(port.to_string());
    match auth {
        AuthMethod::Key { path } => {
            args.push("-i".to_string());
            args.push(path.clone());
        }
        // OpenSSH 没有 -pw 等价物：BatchMode 下密码认证必然交互失败。
        // P1 的取舍：密码目标继续走 plink，OpenSSH 只吃 key / agent。
        AuthMethod::Password { .. } => anyhow::bail!(
            "目标「{}」为密码认证：OpenSSH 传输不支持命令行传密码，\
             请改用私钥认证，或继续使用 PuTTY(plink) 传输",
            target.name
        ),
        AuthMethod::Agent | AuthMethod::None => {}
    }
    Ok((format!("{user}@{host}"), args))
}

#[async_trait]
impl RemoteTransport for OpenSshTransport {
    async fn exec(
        &self,
        _target: &Target,
        _command: &str,
        _working_dir: &str,
        _env: &HashMap<String, String>,
        _log: &LogHandle,
        _cancel: &mut broadcast::Receiver<()>,
    ) -> anyhow::Result<ExitStatus> {
        anyhow::bail!("OpenSshTransport 为 P1 预留实现，P0 请使用 PuttyTransport")
    }

    async fn upload(
        &self,
        _target: &Target,
        _local: &Path,
        _remote: &str,
        _log: &LogHandle,
        _cancel: &mut broadcast::Receiver<()>,
    ) -> anyhow::Result<()> {
        anyhow::bail!("OpenSshTransport 为 P1 预留实现")
    }

    async fn download(
        &self,
        _target: &Target,
        _remote: &str,
        _local: &Path,
        _log: &LogHandle,
        _cancel: &mut broadcast::Receiver<()>,
    ) -> anyhow::Result<()> {
        anyhow::bail!("OpenSshTransport 为 P1 预留实现")
    }
}

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
            work_root: None,
            env: HashMap::new(),
            cred_key: None,
        }
    }

    #[test]
    fn build_ssh_args_carries_hardening_options() {
        // 硬化三件套（ConnectTimeout / ServerAliveInterval / BatchMode）必须一直在场
        let target = ssh_target(AuthMethod::Key { path: r"C:\keys\id_ed25519".to_string() });
        let (dest, args) = build_ssh_args(&target).unwrap();
        assert_eq!(dest, "builder@192.168.1.100");
        for opt in ["ConnectTimeout=10", "ServerAliveInterval=15", "BatchMode=yes"] {
            assert!(args.contains(&opt.to_string()), "缺少 {opt}: {args:?}");
        }
        assert_eq!(args, vec![
            "-o", "ConnectTimeout=10",
            "-o", "ServerAliveInterval=15",
            "-o", "BatchMode=yes",
            "-p", "22",
            "-i", r"C:\keys\id_ed25519",
        ]);
    }

    #[test]
    fn build_ssh_args_rejects_non_ssh() {
        let target = Target { name: "l".into(), target_type: TargetType::Local, ..Default::default() };
        assert!(build_ssh_args(&target).is_err());
    }

    #[test]
    fn build_ssh_args_rejects_password_auth() {
        // BatchMode 下密码认证必然失败，构建期就报人话，不留到运行期挂死
        let target = ssh_target(AuthMethod::Password { encrypted: "x".to_string() });
        let err = build_ssh_args(&target).unwrap_err();
        assert!(err.to_string().contains("密码"), "got: {err}");
    }
}
