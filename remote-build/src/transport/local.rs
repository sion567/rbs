//! 本机执行（设计文档 §2「Local → 本机执行 git / mvn / docker / kubectl」）。
//!
//! Windows 上经 `cmd /C` 执行自由文本命令；工作目录用 current_dir 传入，
//! 环境变量注入进程级（与远端 plink 行为对齐见 putty.rs 注释）。

use super::{ExitStatus, LogHandle, RemoteTransport, Stream};
use crate::models::Target;
use async_trait::async_trait;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tokio::io::AsyncBufReadExt;
use tokio::process::Command;
use tokio::sync::broadcast;

pub struct LocalTransport;

#[async_trait]
impl RemoteTransport for LocalTransport {
    async fn exec(
        &self,
        _target: &Target,
        command: &str,
        working_dir: &str,
        env: &HashMap<String, String>,
        log: &LogHandle,
        cancel: &mut broadcast::Receiver<()>,
    ) -> anyhow::Result<ExitStatus> {
        let mut cmd = Command::new("cmd");
        cmd.arg("/C").arg(command);
        if !working_dir.is_empty() {
            let dir = PathBuf::from(working_dir);
            if !dir.exists() {
                anyhow::bail!("工作目录不存在: {}", dir.display());
            }
            cmd.current_dir(dir);
        }
        cmd.envs(super::merged_env(env));
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
        // 不弹出额外控制台窗口（CREATE_NO_WINDOW）
        cmd.creation_flags(0x0800_0000);

        let mut child = cmd.spawn().map_err(|e| {
            anyhow::anyhow!("命令启动失败: {e}（command: {command}）")
        })?;
        pipe_child_output(&mut child, log);
        wait_with_cancel(&mut child, cancel).await
    }

    async fn upload(
        &self,
        _target: &Target,
        _local: &Path,
        _remote: &str,
        _log: &LogHandle,
        _cancel: &mut broadcast::Receiver<()>,
    ) -> anyhow::Result<()> {
        anyhow::bail!("local target 不支持 upload，请改用 SSH target（step_type: upload）")
    }

    async fn download(
        &self,
        _target: &Target,
        _remote: &str,
        _local: &Path,
        _log: &LogHandle,
        _cancel: &mut broadcast::Receiver<()>,
    ) -> anyhow::Result<()> {
        anyhow::bail!("local target 不支持 download，请改用 SSH target")
    }
}

/// 把子进程 stdout/stderr 双任务转发到 log（§5.6「双 task 并发读」）
pub(super) fn pipe_child_output(child: &mut tokio::process::Child, log: &LogHandle) {
    if let Some(stdout) = child.stdout.take() {
        let log = log.clone();
        tokio::spawn(async move {
            let mut reader = tokio::io::BufReader::new(stdout);
            let mut buf = Vec::new();
            loop {
                buf.clear();
                match reader.read_until(b'\n', &mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        let line = String::from_utf8_lossy(&buf);
                        log.line(Stream::Stdout, line.trim_end_matches(['\r', '\n']));
                    }
                }
            }
        });
    }
    if let Some(stderr) = child.stderr.take() {
        let log = log.clone();
        tokio::spawn(async move {
            let mut reader = tokio::io::BufReader::new(stderr);
            let mut buf = Vec::new();
            loop {
                buf.clear();
                match reader.read_until(b'\n', &mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        let line = String::from_utf8_lossy(&buf);
                        log.line(Stream::Stderr, line.trim_end_matches(['\r', '\n']));
                    }
                }
            }
        });
    }
}

/// 等待子进程退出；取消信号到达时 kill（§12.2 select! 模型）
pub(super) async fn wait_with_cancel(
    child: &mut tokio::process::Child,
    cancel: &mut broadcast::Receiver<()>,
) -> anyhow::Result<ExitStatus> {
    tokio::select! {
        status = child.wait() => {
            let status = status?;
            Ok(ExitStatus::from_code(status.code().unwrap_or(-1)))
        }
        _ = cancel.recv() => {
            let _ = child.kill().await;
            anyhow::bail!("__cancelled__")
        }
    }
}

/// 判断错误是否为取消（引擎据此返回 StepResult::Cancelled）
pub fn is_cancelled(err: &anyhow::Error) -> bool {
    err.to_string() == "__cancelled__"
}
