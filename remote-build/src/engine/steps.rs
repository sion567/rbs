//! 传输类步骤实现（upload / download / pack）与 creates 守卫检查。
//!
//! upload/download 的完整性校验走批量 sha256：一条往返校验多个文件（经传输的
//! remote_sha256_batch：putty 一条 plink / russh 一条 exec 通道），
//! 取代旧版逐文件冷启动连接（多 jar 上传的主要耗时）。本地哈希 / zip 压缩等
//! CPU 密集或大文件 IO 一律 `spawn_blocking`，不阻塞 async worker。

use crate::transport::{self, LogHandle, RemoteTransport};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::broadcast;

use super::{is_remote, target_dummy};

/// 人类可读的文件大小
pub(crate) fn human_size(bytes: u64) -> String {
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.2} GB", bytes as f64 / 1024.0 / 1024.0 / 1024.0)
    } else if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / 1024.0 / 1024.0)
    } else {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    }
}

/// creates 守卫的存在性检查（TODO「Ansible 参考 #1」）：
/// - 本地步骤：相对 workspace 解析，支持 `*` `?` 通配（任一匹配存在即算）
/// - SSH 步骤：远端 `test -e` 退出码判定（相对路径随 work_root / working_dir 展开）
///
/// 检查失败（如连不上远端）返回 Err，调用方 fail-open 照常执行步骤。
pub(crate) async fn check_creates_exists(
    pattern: &str,
    target: Option<&crate::models::Target>,
    transport: &Arc<dyn RemoteTransport>,
    remote_dir: &str,
    workspace: &Path,
    log: &LogHandle,
    cancel_rx: &mut broadcast::Receiver<()>,
) -> anyhow::Result<bool> {
    if is_remote(target) {
        let path = if pattern.starts_with('/') || remote_dir.is_empty() {
            pattern.to_string()
        } else {
            format!("{}/{}", remote_dir.trim_end_matches('/'), pattern)
        };
        log.line(
            transport::Stream::Stdout,
            format!("[creates] 检查远端路径: {path}"),
        );
        let cmd = format!(
            "test -e '{}'",
            crate::transport::putty::escape_posix_single_quote(&path)
        );
        let st = transport
            .exec(target_dummy(target), &cmd, "", &HashMap::new(), log, cancel_rx)
            .await?;
        Ok(st.success)
    } else {
        Ok(local_creates_exists(workspace, pattern))
    }
}

/// 本地 creates 检查：字面量路径直接 exists；含通配符的展开后任一匹配存在即算
pub(crate) fn local_creates_exists(workspace: &Path, pattern: &str) -> bool {
    let p = Path::new(pattern);
    let absolute = if p.is_absolute() { p.to_path_buf() } else { workspace.join(p) };
    if absolute.exists() {
        return true;
    }
    if pattern.contains('*') || pattern.contains('?') {
        // glob 模式统一用 / 分隔，避免 Windows 反斜杠被当转义符
        let norm = absolute.to_string_lossy().replace('\\', "/");
        if let Ok(paths) = glob::glob(&norm) {
            return paths.filter_map(Result::ok).any(|p| p.exists());
        }
    }
    false
}

/// pack 步骤：`<目录或通配> -> <目标.zip> [--exclude <glob>]…`，路径相对 workspace 解析。
/// zip 压缩（deflate）是 CPU 密集操作，进阻塞线程池执行。
pub(crate) async fn pack_step(
    command: &str,
    workspace: &Path,
    log: &LogHandle,
) -> anyhow::Result<transport::ExitStatus> {
    let (src_part, dst_part) = command.split_once("->").ok_or_else(|| {
        anyhow::anyhow!("pack 步骤命令格式应为: <目录或通配> -> <目标.zip>，实际: {command}")
    })?;
    // dst 段可带 --exclude <模式>…（rsync 语义）：字面量 = 目录/文件名分量，* ? = 通配
    let mut dst_tokens = dst_part.split("--exclude");
    let dst_str = dst_tokens.next().unwrap_or("").trim();
    let excludes: Vec<String> = dst_tokens
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    let resolve = |p: &str| {
        let path = Path::new(p);
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            workspace.join(path)
        }
    };
    let src = resolve(src_part.trim());
    let dst = resolve(dst_str);
    // 格式由扩展名决定（.zip / .tar.gz / .tgz），pack() 内部按同一判定分派写入器
    if crate::pack::dst_format(&dst).is_none() {
        anyhow::bail!("pack 目标必须是 .zip / .tar.gz / .tgz 文件: {}", dst.display());
    }

    log.line(
        transport::Stream::Stdout,
        format!("[pack] {} -> {}", src.display(), dst.display()),
    );
    if !excludes.is_empty() {
        log.line(
            transport::Stream::Stdout,
            format!("[pack] 排除: {}", excludes.join(", ")),
        );
    }
    let (count, bytes) = tokio::task::spawn_blocking({
        let src = src.clone();
        let dst = dst.clone();
        let excludes = excludes.clone();
        move || crate::pack::pack(&src, &dst, &excludes)
    })
    .await??;
    let size = human_size(bytes);
    log.line(
        transport::Stream::Stdout,
        format!("[pack] 完成：{count} 个文件，{size}"),
    );
    Ok(transport::ExitStatus::from_code(0))
}

/// 解析 upload 命令：每行一条映射 `<本地路径> -> <远端路径>`（空行与 # 注释忽略）。
/// 微服务多 jar 场景在一个步骤里写多条映射即可。
pub(crate) fn parse_upload_mappings(command: &str) -> anyhow::Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    for line in command
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
    {
        let Some((l, r)) = line.split_once("->") else {
            anyhow::bail!("upload 映射格式应为: <本地路径> -> <远端路径>，实际行: {line}");
        };
        let (l, r) = (l.trim(), r.trim());
        // 空映射行（UI 编辑器加行的 "-> " 占位）与空白行一样跳过
        if l.is_empty() && r.is_empty() {
            continue;
        }
        out.push((l.to_string(), r.to_string()));
    }
    if out.is_empty() {
        anyhow::bail!("upload 命令为空；每行一条映射：<本地路径> -> <远端路径>");
    }
    Ok(out)
}

/// upload 步骤：`<本地路径> -> <远端路径>` 每行一条映射。
/// 完整性策略（§5.9）：单文件先比本地/远端 sha256（一次批量往返），
/// 未变更整条跳过（notify 变更判断的地基）；传输后再批量校验一次。
/// 目录递归上传跳过校验（建议先用 pack 打成单文件）。
pub(crate) async fn upload_step(
    transport: &Arc<dyn RemoteTransport>,
    target: Option<&crate::models::Target>,
    command: &str,
    workspace: &Path,
    remote_dir: &str,
    log: &LogHandle,
    cancel_rx: &mut broadcast::Receiver<()>,
) -> anyhow::Result<(transport::ExitStatus, bool)> {
    // 返回值第二项 = 是否真的传了内容（变更判断；hash 未变更跳过的映射不计）
    if !is_remote(target) {
        anyhow::bail!("step_type: upload 需要 SSH target，当前 target 不支持上传");
    }
    let target = target.expect("checked above");
    let mappings = parse_upload_mappings(command)?;
    let total = mappings.len();

    // 先解析全部路径并预检源存在（原实现边传边检，出错时前面映射已传出，半途而废）
    struct Item {
        local: PathBuf,
        remote: String,
    }
    let mut items = Vec::with_capacity(total);
    for (local_part, remote_part) in &mappings {
        let local_path = {
            let p = Path::new(local_part);
            if p.is_absolute() {
                p.to_path_buf()
            } else {
                workspace.join(p)
            }
        };
        if !local_path.exists() {
            anyhow::bail!("上传源不存在: {}", local_path.display());
        }
        let remote_path = if remote_part.starts_with('/') || remote_dir.is_empty() {
            remote_part.clone()
        } else {
            format!("{}/{}", remote_dir.trim_end_matches('/'), remote_part)
        };
        items.push(Item { local: local_path, remote: remote_path });
    }

    // 本地哈希（单文件映射）：一次阻塞线程批量算完
    let file_hashes: HashMap<usize, String> = {
        let files: Vec<(usize, PathBuf)> = items
            .iter()
            .enumerate()
            .filter(|(_, it)| it.local.is_file())
            .map(|(i, it)| (i, it.local.clone()))
            .collect();
        tokio::task::spawn_blocking(move || {
            let mut out = HashMap::new();
            for (i, p) in files {
                out.insert(i, crate::transport::putty::sha256_file(&p)?);
            }
            anyhow::Ok(out)
        })
        .await??
    };

    // 远端现状批量探测（一次往返）。失败 fail-open：当作远端无信息，全部正常上传。
    let mut remote_hashes: HashMap<String, String> = HashMap::new();
    if !file_hashes.is_empty() {
        let paths: Vec<String> =
            file_hashes.keys().map(|i| items[*i].remote.clone()).collect();
        match transport.remote_sha256_batch(target, &paths).await {
            Ok(m) => remote_hashes = m,
            Err(e) => log.line(
                transport::Stream::Stderr,
                format!("[upload] 远端内容探测失败，跳过变更判断: {e}"),
            ),
        }
    }

    let mut uploaded_any = false;
    // (远端路径, 本地哈希)：传完的文件留给批量校验
    let mut uploaded: Vec<(String, String)> = Vec::new();
    for (i, it) in items.iter().enumerate() {
        log.line(
            transport::Stream::Stdout,
            format!("[upload {}/{}] {} -> {}", i + 1, total, it.local.display(), it.remote),
        );
        if let Some(local_hash) = file_hashes.get(&i) {
            let bytes = std::fs::metadata(&it.local).map(|m| m.len()).unwrap_or(0);
            log.line(
                transport::Stream::Stdout,
                format!("[upload {}/{}] 文件大小 {}", i + 1, total, human_size(bytes)),
            );
            if remote_hashes.get(&it.remote) == Some(local_hash) {
                log.line(
                    transport::Stream::Stdout,
                    format!(
                        "[upload {}/{}] 内容未变更（sha256 {}），跳过传输",
                        i + 1,
                        total,
                        &local_hash[..12]
                    ),
                );
                continue;
            }
            transport
                .upload(target, &it.local, &it.remote, log, cancel_rx)
                .await?;
            uploaded_any = true;
            uploaded.push((it.remote.clone(), local_hash.clone()));
        } else {
            transport
                .upload(target, &it.local, &it.remote, log, cancel_rx)
                .await?;
            uploaded_any = true;
            log.line(
                transport::Stream::Stderr,
                "[verify] 目录上传跳过校验（建议先用 pack 打成单文件再上传）",
            );
        }
    }

    // 传输后批量完整性校验（一次往返）；查询失败 = 步骤失败（按重试配置重传）
    if !uploaded.is_empty() {
        log.line(
            transport::Stream::Stdout,
            format!("[verify] 批量校验远端 sha256：{} 个文件", uploaded.len()),
        );
        let paths: Vec<String> = uploaded.iter().map(|(r, _)| r.clone()).collect();
        let remote = transport.remote_sha256_batch(target, &paths).await?;
        for (path, local_hash) in &uploaded {
            match remote.get(path) {
                Some(h) if h == local_hash => {}
                Some(h) => anyhow::bail!(
                    "上传校验失败：{path} sha256 不一致（本地 {} / 远端 {}）",
                    &local_hash[..12],
                    &h[..h.len().min(12)]
                ),
                None => anyhow::bail!("上传校验失败：远端文件不可读（无 hash）: {path}"),
            }
        }
        log.line(
            transport::Stream::Stdout,
            format!("[verify] {} 个文件 sha256 全部一致 ✓", uploaded.len()),
        );
    }
    Ok((transport::ExitStatus::from_code(0), uploaded_any))
}

/// download 步骤：`<远端路径> -> <本地路径>` 每行一条（复用 upload 的映射解析，
/// 方向相反）。本地路径相对 workspace 解析、父目录自动创建；单文件下载后
/// 批量 sha256 校验（远端一次往返 + 本地一次阻塞线程）。
pub(crate) async fn download_step(
    transport: &Arc<dyn RemoteTransport>,
    target: Option<&crate::models::Target>,
    command: &str,
    workspace: &Path,
    remote_dir: &str,
    log: &LogHandle,
    cancel_rx: &mut broadcast::Receiver<()>,
) -> anyhow::Result<transport::ExitStatus> {
    if !is_remote(target) {
        anyhow::bail!("step_type: download 需要 SSH target，当前 target 不支持下载");
    }
    let target = target.expect("checked above");
    let mappings = parse_upload_mappings(command)?;
    let total = mappings.len();
    let mut files: Vec<(String, PathBuf)> = Vec::new();
    for (i, (remote_part, local_part)) in mappings.iter().enumerate() {
        let remote_path = if remote_part.starts_with('/') || remote_dir.is_empty() {
            remote_part.clone()
        } else {
            format!("{}/{}", remote_dir.trim_end_matches('/'), remote_part)
        };
        let local_path = {
            let p = Path::new(local_part);
            if p.is_absolute() { p.to_path_buf() } else { workspace.join(p) }
        };
        if let Some(parent) = local_path.parent() {
            if !parent.exists() {
                std::fs::create_dir_all(parent)?;
            }
        }
        log.line(
            transport::Stream::Stdout,
            format!("[download {}/{}] {} -> {}", i + 1, total, remote_path, local_path.display()),
        );
        transport
            .download(target, &remote_path, &local_path, log, cancel_rx)
            .await?;
        if local_path.is_file() {
            files.push((remote_path, local_path));
        }
    }

    // 下载完整性校验：远端批量 + 本地批量，逐条比对
    if !files.is_empty() {
        let paths: Vec<String> = files.iter().map(|(r, _)| r.clone()).collect();
        let remote = transport.remote_sha256_batch(target, &paths).await?;
        let local_hashes: HashMap<String, String> =
            tokio::task::spawn_blocking(move || {
                let mut out = HashMap::new();
                for (r, p) in &files {
                    out.insert(r.clone(), crate::transport::putty::sha256_file(p)?);
                }
                anyhow::Ok(out)
            })
            .await??;
        for path in &paths {
            match (remote.get(path), local_hashes.get(path)) {
                (Some(rh), Some(lh)) if rh == lh => {}
                (Some(rh), Some(lh)) => anyhow::bail!(
                    "下载校验失败：{path} sha256 不一致（远端 {} / 本地 {}）",
                    &rh[..rh.len().min(12)],
                    &lh[..12]
                ),
                _ => anyhow::bail!("下载校验失败：无法取得 {path} 的哈希（远端不可读？）"),
            }
        }
        log.line(
            transport::Stream::Stdout,
            format!("[verify] {} 个文件 sha256 全部一致 ✓", paths.len()),
        );
    }
    Ok(transport::ExitStatus::from_code(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upload_mapping_parser_multi_line() {
        // 微服务多 jar：一个 upload 步骤多行映射；空行与 # 注释忽略
        let m = parse_upload_mappings(
            "target/user.jar -> /opt/app/user.jar\n\n# 注释行\ntarget/order.jar -> /opt/app/order.jar",
        )
        .unwrap();
        assert_eq!(m.len(), 2);
        assert_eq!(m[0], ("target/user.jar".to_string(), "/opt/app/user.jar".to_string()));
        assert_eq!(m[1], ("target/order.jar".to_string(), "/opt/app/order.jar".to_string()));

        // 缺箭头 → 明确报错
        assert!(parse_upload_mappings("target/a.jar").is_err());
        // 全空 → 明确报错
        assert!(parse_upload_mappings("  \n# only comment\n").is_err());
    }

    #[test]
    fn local_creates_glob() {
        // creates 通配：target/*.jar 任一匹配存在即算
        let dir = std::env::temp_dir().join(format!("rbs_creates_glob_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("target")).unwrap();
        assert!(!local_creates_exists(&dir, "target/*.jar"));
        std::fs::write(dir.join("target").join("a.jar"), "x").unwrap();
        assert!(local_creates_exists(&dir, "target/*.jar"));
        // 字面量路径
        assert!(local_creates_exists(&dir, "target"));
        assert!(!local_creates_exists(&dir, "no_such_thing"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
