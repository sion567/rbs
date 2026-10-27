//! 打包（step_type: pack，设计文档 §5.8 v0.4 实施补充）。
//!
//! 上传大量小文件前先打成一个压缩包，pscp 单文件传输显著更快。
//! 引擎内置 Rust 实现，不依赖系统 zip / tar。
//!
//! **格式由目标扩展名决定**：`.tar.gz` / `.tgz` → tar+gzip（Linux 惯用，
//! 保留可执行权限）；`.zip` → zip（deflate）。其他扩展名拒绝。
//!
//! **源以 `/.` 结尾 = `tar -C dir .` 语义**：打目录内容，条目不带目录名根
//! （`dist/. -> app.tar.gz` 解包 `-C /opt/app` 直接铺文件）；普通目录源
//! 带目录名根（`dist -> app.tar.gz` 条目为 `dist/...`）。zip / tar.gz 同一语义。
//!
//! 源支持三种形态：
//! - 目录：递归打包，条目带目录名根（如 `target/` → 条目 `target/classes/...`，
//!   同 `zip -r` / `tar -czf x.tgz target` 习惯）
//! - 单文件：条目为文件名
//! - 通配（如 `target/*.jar`）：仅匹配文件，条目为文件名

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// 打包格式（由目标扩展名判定）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveFormat {
    Zip,
    TarGz,
}

/// 按目标扩展名判定打包格式；不支持的扩展名返回 None（执行 / 预览据此报错）
pub fn dst_format(dst: &Path) -> Option<ArchiveFormat> {
    let lower = dst.to_string_lossy().to_ascii_lowercase();
    if lower.ends_with(".tar.gz") || lower.ends_with(".tgz") {
        Some(ArchiveFormat::TarGz)
    } else if lower.ends_with(".zip") {
        Some(ArchiveFormat::Zip)
    } else {
        None
    }
}

/// 打包入口：返回 (文件数, 压缩包字节数)。
/// 目标父目录自动创建；文件清单先收集完再写包，避免目标在源目录内时自吞。
/// `excludes`（rsync --exclude 语义）：字面量 = 路径任一目录/文件名分量匹配即排除
/// （如 `node_modules`）；含 `*` `?` = 对相对路径与文件名做通配匹配（如 `*.map`）。
pub fn pack(src: &Path, dst: &Path, excludes: &[String]) -> Result<(usize, u64)> {
    // 源以 `/.` 结尾 = tar -C dir . 语义：条目不带目录名根。
    // 必须按字符串剥尾点：Path::components 会把尾部 CurDir 归一化掉，
    // parent() 会错剥掉真实目录名（src/. 的 parent 是上级目录而非 src）
    let stripped = {
        let s = src.to_string_lossy();
        s.strip_suffix("/.")
            .or_else(|| s.strip_suffix("\\."))
            .map(str::to_string)
    };
    let no_root = stripped.is_some();
    let src = stripped.map(PathBuf::from).unwrap_or_else(|| src.to_path_buf());
    // 归一化 `.` 分量（workspace.join(".") 之类），避免条目出现 `dir/./file`
    let src = {
        let normalized: PathBuf = src
            .components()
            .filter(|c| !matches!(c, std::path::Component::CurDir))
            .collect();
        if normalized.as_os_str().is_empty() {
            src.to_path_buf()
        } else {
            normalized
        }
    };
    let files = collect_files(&src, excludes)?;
    if let Some(parent) = dst.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("创建目录失败: {}", parent.display()))?;
        }
    }

    match dst_format(dst) {
        Some(ArchiveFormat::Zip) => write_zip(dst, &src, &files, no_root),
        Some(ArchiveFormat::TarGz) => write_tar_gz(dst, &src, &files, no_root),
        None => anyhow::bail!("pack 目标必须是 .zip / .tar.gz / .tgz 文件: {}", dst.display()),
    }
}

fn write_zip(dst: &Path, src: &Path, files: &[PathBuf], no_root: bool) -> Result<(usize, u64)> {
    let file = std::fs::File::create(dst)
        .with_context(|| format!("创建 zip 失败: {}", dst.display()))?;
    let mut zip = zip::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);

    for path in files {
        let entry_name = entry_name_for(src, path, no_root)?;
        zip.start_file(entry_name.as_str(), options)
            .with_context(|| format!("写入条目失败: {entry_name}"))?;
        let mut f = std::fs::File::open(path)
            .with_context(|| format!("读取失败: {}", path.display()))?;
        std::io::copy(&mut f, &mut zip)
            .with_context(|| format!("压缩写入失败: {}", path.display()))?;
    }

    let handle = zip.finish().context("收尾 zip 失败")?;
    let bytes = handle
        .metadata()
        .map(|m| m.len())
        .unwrap_or_default();
    Ok((files.len(), bytes))
}

fn write_tar_gz(dst: &Path, src: &Path, files: &[PathBuf], no_root: bool) -> Result<(usize, u64)> {
    let file = std::fs::File::create(dst)
        .with_context(|| format!("创建 tar.gz 失败: {}", dst.display()))?;
    let enc = flate2::write::GzEncoder::new(file, flate2::Compression::default());
    let mut tar = tar::Builder::new(enc);

    for path in files {
        let entry_name = entry_name_for(src, path, no_root)?;
        let mut f = std::fs::File::open(path)
            .with_context(|| format!("读取失败: {}", path.display()))?;
        let meta = f.metadata().with_context(|| format!("读取元数据失败: {}", path.display()))?;
        let mut header = tar::Header::new_gnu();
        header.set_size(meta.len());
        header.set_mode(unix_mode(path));
        // mtime 取文件修改时间（秒级）；取不到就 0（tar 允许）
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        header.set_mtime(mtime);
        tar.append_data(&mut header, &entry_name, &mut f)
            .with_context(|| format!("写入条目失败: {entry_name}"))?;
    }

    let enc = tar.into_inner().context("收尾 tar 失败")?;
    enc.finish().context("收尾 gzip 失败")?;
    let bytes = dst.metadata().map(|m| m.len()).unwrap_or_default();
    Ok((files.len(), bytes))
}

/// Windows 文件系统没有 Unix 权限位，按扩展名约定映射，避免解包出来全是 0644：
/// 无扩展名（多为编译产物二进制）与脚本扩展名 → 0755，其余 → 0644
fn unix_mode(path: &Path) -> u32 {
    const EXEC_EXTS: &[&str] = &["sh", "bash", "run", "py", "pl", "rb"];
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase());
    match ext {
        None => 0o755,
        Some(e) if EXEC_EXTS.contains(&e.as_str()) => 0o755,
        Some(_) => 0o644,
    }
}

/// 条目名：目录源 → 相对源父目录（带目录名根；no_root = 相对源自身，即内容直接铺顶层）；
/// 文件/通配 → 文件名。条目统一用 `/` 分隔。
fn entry_name_for(src: &Path, path: &Path, no_root: bool) -> Result<String> {
    let rel: PathBuf = if src.is_dir() {
        let base = if no_root { src } else { src.parent().unwrap_or(src) };
        path.strip_prefix(base)?.to_path_buf()
    } else {
        path.file_name().map(PathBuf::from).unwrap_or_default()
    };
    Ok(rel.to_string_lossy().replace('\\', "/"))
}

/// 收集待打包文件：目录递归 → 全部文件；通配 → 匹配的文件；应用排除规则
fn collect_files(src: &Path, excludes: &[String]) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    if src.is_dir() {
        walk(src, src, excludes, &mut out, 0);
        if out.is_empty() {
            anyhow::bail!("目录为空: {}（或全部被 --exclude 排除）", src.display());
        }
        return Ok(out);
    }
    if src.is_file() {
        out.push(src.to_path_buf());
        return Ok(out);
    }
    // 既不是目录也不是文件 → 按通配模式匹配
    let pattern = src.to_string_lossy().to_string();
    let entries = glob::glob(&pattern)
        .with_context(|| format!("无效的通配模式: {pattern}"))?;
    for entry in entries.flatten() {
        if entry.is_file() && !is_excluded(&entry, src, excludes) {
            out.push(entry);
        }
    }
    if out.is_empty() {
        anyhow::bail!("通配没有匹配到文件: {pattern}（或全部被 --exclude 排除）");
    }
    Ok(out)
}

/// 排除判定：字面量 = 相对路径的任一分量或文件名相等（目录整棵排除）；
/// 含通配符 = glob 匹配相对路径（/ 分隔）或文件名
fn is_excluded(path: &Path, src_base: &Path, excludes: &[String]) -> bool {
    if excludes.is_empty() {
        return false;
    }
    // 相对源父目录的路径（与 entry_name_for 同基准），统一 / 分隔
    let base = if src_base.is_dir() {
        src_base.parent().unwrap_or(src_base)
    } else {
        src_base
    };
    let rel = path
        .strip_prefix(base)
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_default();
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    for ex in excludes {
        let ex = ex.trim();
        if ex.is_empty() {
            continue;
        }
        if ex.contains('*') || ex.contains('?') {
            if let Ok(p) = glob::Pattern::new(ex) {
                if p.matches(&rel) || p.matches(&name) {
                    return true;
                }
            }
        } else {
            // 字面量：匹配任一路径分量（node_modules 排除整个目录内容）
            if name == ex
                || path
                    .strip_prefix(base)
                    .unwrap_or(path)
                    .components()
                    .any(|c| c.as_os_str() == ex)
            {
                return true;
            }
        }
    }
    false
}

/// 递归深度上限：防 junction / 符号链接环把栈打爆（正常工程树远达不到此深度）
const MAX_WALK_DEPTH: usize = 64;

fn walk(dir: &Path, src_base: &Path, excludes: &[String], out: &mut Vec<PathBuf>, depth: usize) {
    if depth > MAX_WALK_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        // entry.file_type() 不跟随链接：真目录才下钻；指向目录的符号链接 / junction
        // 整棵跳过（防环，rsync 默认语义）；指向文件的链接按内容打包
        match entry.file_type() {
            Ok(ft) if ft.is_dir() => {
                // 目录整棵剪枝：字面量排除匹配目录名时不再下钻（node_modules 大目录性能）
                let dir_name = p
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                let pruned = excludes.iter().any(|ex| {
                    let ex = ex.trim();
                    !ex.is_empty() && !ex.contains('*') && !ex.contains('?') && ex == dir_name
                });
                if pruned {
                    continue;
                }
                walk(&p, src_base, excludes, out, depth + 1);
            }
            Ok(ft) if ft.is_symlink() => {
                if p.is_dir() {
                    continue; // 目录链接（junction 环风险），跳过
                }
                if !is_excluded(&p, src_base, excludes) {
                    out.push(p);
                }
            }
            Ok(_) => {
                if !is_excluded(&p, src_base, excludes) {
                    out.push(p);
                }
            }
            Err(_) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 注意：并行测试共享进程 id，必须用唯一 tag 避免互相 remove_dir_all
    fn make_ws(tag: &str) -> PathBuf {
        let ws = std::env::temp_dir().join(format!("rbs_pack_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&ws);
        std::fs::create_dir_all(ws.join("src/sub")).unwrap();
        std::fs::create_dir_all(ws.join("target")).unwrap();
        std::fs::write(ws.join("src/a.txt"), "A").unwrap();
        std::fs::write(ws.join("src/sub/b.txt"), "B").unwrap();
        std::fs::write(ws.join("target/x.jar"), "X").unwrap();
        std::fs::write(ws.join("target/y.jar"), "Y").unwrap();
        ws
    }

    fn names(zip_path: &Path) -> Vec<String> {
        let f = std::fs::File::open(zip_path).unwrap();
        let mut z = zip::ZipArchive::new(f).unwrap();
        (0..z.len())
            .map(|i| z.by_index(i).unwrap().name().to_string())
            .collect()
    }

    #[test]
    fn packs_directory_with_root_prefix() {
        let ws = make_ws("dir");
        let dst = ws.join("out/app.zip");
        let (count, bytes) = pack(&ws.join("src"), &dst, &[]).unwrap();
        assert_eq!(count, 2);
        assert!(bytes > 0);
        let n = names(&dst);
        assert!(n.contains(&"src/a.txt".to_string()), "entries: {n:?}");
        assert!(n.contains(&"src/sub/b.txt".to_string()), "entries: {n:?}");
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn packs_glob_matches_as_basenames() {
        let ws = make_ws("glob");
        let dst = ws.join("out/jars.zip");
        let (count, _) = pack(&ws.join("target/*.jar"), &dst, &[]).unwrap();
        assert_eq!(count, 2);
        let n = names(&dst);
        assert!(n.contains(&"x.jar".to_string()), "entries: {n:?}");
        assert!(n.contains(&"y.jar".to_string()), "entries: {n:?}");
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn single_file_and_empty_glob_errors() {
        let ws = make_ws("single");
        let (count, _) = pack(&ws.join("src/a.txt"), &ws.join("out/one.zip"), &[]).unwrap();
        assert_eq!(count, 1);
        assert!(pack(&ws.join("target/*.noext"), &ws.join("out/x.zip"), &[]).is_err());
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn excludes_literal_components_and_globs() {
        let ws = make_ws("excl");
        std::fs::create_dir_all(ws.join("src/node_modules/pkg")).unwrap();
        std::fs::write(ws.join("src/node_modules/pkg/index.js"), "N").unwrap();
        std::fs::write(ws.join("src/debug.map"), "M").unwrap();

        // 字面量排除整个 node_modules 目录；通配排除 *.map
        let dst = ws.join("out/excl.zip");
        let (count, _) = pack(
            &ws.join("src"),
            &dst,
            &["node_modules".to_string(), "*.map".to_string()],
        )
        .unwrap();
        assert_eq!(count, 2, "只剩 a.txt / sub/b.txt");
        let n = names(&dst);
        assert!(!n.iter().any(|e| e.contains("node_modules")), "{n:?}");
        assert!(!n.iter().any(|e| e.ends_with(".map")), "{n:?}");

        let _ = std::fs::remove_dir_all(&ws);
    }

    // ── tar.gz（扩展名决定格式）──

    #[test]
    fn dst_format_by_extension() {
        assert_eq!(dst_format(Path::new("a.tar.gz")), Some(ArchiveFormat::TarGz));
        assert_eq!(dst_format(Path::new("A.TGZ")), Some(ArchiveFormat::TarGz));
        assert_eq!(dst_format(Path::new("a.zip")), Some(ArchiveFormat::Zip));
        // 裸 .gz / 其他扩展名不支持（.gz 不是 tar.gz）
        assert_eq!(dst_format(Path::new("a.gz")), None);
        assert_eq!(dst_format(Path::new("a.rar")), None);
    }

    #[test]
    fn packs_tar_gz_with_root_prefix_and_modes() {
        let ws = make_ws("targz");
        std::fs::write(ws.join("src/run.sh"), "#!/bin/sh\necho hi").unwrap();

        let dst = ws.join("out/app.tar.gz");
        let (count, bytes) = pack(&ws.join("src"), &dst, &[]).unwrap();
        assert_eq!(count, 3);
        assert!(bytes > 0);

        // 解包核对：目录源带目录名根（dist -> dist.tar.gz 里面是 dist/...）
        let f = std::fs::File::open(&dst).unwrap();
        let gz = flate2::read::GzDecoder::new(f);
        let mut ar = tar::Archive::new(gz);
        let mut modes = std::collections::HashMap::new();
        for entry in ar.entries().unwrap() {
            let e = entry.unwrap();
            let name = e.path().unwrap().to_string_lossy().replace('\\', "/");
            modes.insert(name, e.header().mode().unwrap());
        }
        assert_eq!(modes.get("src/a.txt"), Some(&0o644), "{modes:?}");
        assert_eq!(modes.get("src/run.sh"), Some(&0o755), "{modes:?}");
        assert!(modes.contains_key("src/sub/b.txt"), "{modes:?}");

        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn packs_tar_gz_glob_as_basenames_and_tgz_alias() {
        let ws = make_ws("tgz");
        let dst = ws.join("out/jars.tgz"); // .tgz 别名同样走 tar.gz
        let (count, _) = pack(&ws.join("target/*.jar"), &dst, &[]).unwrap();
        assert_eq!(count, 2);
        let f = std::fs::File::open(&dst).unwrap();
        let mut ar = tar::Archive::new(flate2::read::GzDecoder::new(f));
        let names: Vec<String> = ar
            .entries()
            .unwrap()
            .map(|e| e.unwrap().path().unwrap().to_string_lossy().to_string())
            .collect();
        assert!(names.contains(&"x.jar".to_string()), "{names:?}");
        assert!(names.contains(&"y.jar".to_string()), "{names:?}");

        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn unsupported_extension_rejected() {
        let ws = make_ws("badext");
        assert!(pack(&ws.join("src"), &ws.join("out/x.rar"), &[]).is_err());
        assert!(pack(&ws.join("src"), &ws.join("out/x.gz"), &[]).is_err());
        let _ = std::fs::remove_dir_all(&ws);
    }

    /// 源以 `/.` 结尾 = tar -C dir . 语义：条目不带目录名根，zip / tar.gz 同一语义
    #[test]
    fn slash_dot_source_packs_contents_at_root() {
        let ws = make_ws("rootless");
        let root_entries = |dst: &Path| {
            if dst.extension().and_then(|e| e.to_str()) == Some("zip") {
                names(dst)
            } else {
                let f = std::fs::File::open(dst).unwrap();
                let mut ar = tar::Archive::new(flate2::read::GzDecoder::new(f));
                ar.entries()
                    .unwrap()
                    .map(|e| e.unwrap().path().unwrap().to_string_lossy().to_string())
                    .collect::<Vec<_>>()
            }
        };

        for (dst, tag) in [
            (ws.join("out/rl.zip"), "zip"),
            (ws.join("out/rl.tar.gz"), "targz"),
        ] {
            pack(&ws.join("src/."), &dst, &[]).unwrap();
            let n = root_entries(&dst);
            // 顶层即文件，无 src/ 根；子目录相对内容保留
            assert!(n.contains(&"a.txt".to_string()), "{tag}: {n:?}");
            assert!(n.contains(&"sub/b.txt".to_string()), "{tag}: {n:?}");
            assert!(!n.iter().any(|e| e.starts_with("src/")), "{tag}: {n:?}");
        }

        // 对照：普通目录源仍带目录名根（现有语义不变）
        let dst = ws.join("out/rooted.zip");
        pack(&ws.join("src"), &dst, &[]).unwrap();
        assert!(names(&dst).contains(&"src/a.txt".to_string()));

        let _ = std::fs::remove_dir_all(&ws);
    }
}
