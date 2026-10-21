//! 系统 OpenSSH 传输（P1 可选，见设计文档 §5.1 实现计划表）。
//! P0 不启用；保留 trait 实现骨架以便后续替换。

use super::{ExitStatus, LogHandle, RemoteTransport};
use crate::models::Target;
use async_trait::async_trait;
use std::collections::HashMap;
use std::path::Path;
use tokio::sync::broadcast;

#[allow(dead_code)]
pub struct OpenSshTransport;

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
