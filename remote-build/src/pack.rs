//! zip 打包（step_type: pack，设计文档 §5.8 v0.4 实施补充）。
//!
//! 上传大量小文件前先打成一个 zip，pscp 单文件传输显著更快。
//! 引擎内置 Rust 实现（deflate），不依赖系统 zip / 7z。
//!
//! 源支持三种形态：
//! - 目录：递归打包，条目带目录名根（如 `target/` → 条目 `target/classes/...`，同 zip -r 习惯）
//! - 单文件：条目为文件名
//! - 通配（如 `target/*.jar`）：仅匹配文件，条目为文件名

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// 打包入口：返回 (文件数, zip 字节数)。
/// 目标父目录自动创建；文件清单先收集完再写包，避免目标在源目录内时自吞。
pub fn pack(src: &Path, dst: &Path) -> Result<(usize, u64)> {
    let files = collect_files(src)?;
    if let Some(parent) = dst.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("创建目录失败: {}", parent.display()))?;
        }
    }

    let file = std::fs::File::create(dst)
        .with_context(|| format!("创建 zip 失败: {}", dst.display()))?;
    let mut zip = zip::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);

    for path in &files {
        let entry_name = entry_name_for(src, path)?;
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

/// 条目名：目录源 → 相对源父目录（带目录名根）；文件/通配 → 文件名。
/// zip 条目统一用 `/` 分隔。
fn entry_name_for(src: &Path, path: &Path) -> Result<String> {
    let rel: PathBuf = if src.is_dir() {
        let base = src.parent().unwrap_or(src);
        path.strip_prefix(base)?.to_path_buf()
    } else {
        path.file_name().map(PathBuf::from).unwrap_or_default()
    };
    Ok(rel.to_string_lossy().replace('\\', "/"))
}

/// 收集待打包文件：目录递归 → 全部文件；通配 → 匹配的文件
fn collect_files(src: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    if src.is_dir() {
        walk(src, &mut out);
        if out.is_empty() {
            anyhow::bail!("目录为空: {}", src.display());
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
        if entry.is_file() {
            out.push(entry);
        }
    }
    if out.is_empty() {
        anyhow::bail!("通配没有匹配到文件: {pattern}");
    }
    Ok(out)
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_dir() {
            walk(&p, out);
        } else {
            out.push(p);
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
        let (count, bytes) = pack(&ws.join("src"), &dst).unwrap();
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
        let (count, _) = pack(&ws.join("target/*.jar"), &dst).unwrap();
        assert_eq!(count, 2);
        let n = names(&dst);
        assert!(n.contains(&"x.jar".to_string()), "entries: {n:?}");
        assert!(n.contains(&"y.jar".to_string()), "entries: {n:?}");
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn single_file_and_empty_glob_errors() {
        let ws = make_ws("single");
        let (count, _) = pack(&ws.join("src/a.txt"), &ws.join("out/one.zip")).unwrap();
        assert_eq!(count, 1);
        assert!(pack(&ws.join("target/*.noext"), &ws.join("out/x.zip")).is_err());
        let _ = std::fs::remove_dir_all(&ws);
    }
}
