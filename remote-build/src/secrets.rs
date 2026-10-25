//! 敏感凭证存储（设计文档 §14：密码存 Windows Credential Manager，绝不落 YAML）。
//!
//! 目标主机的密码按「目标名」为键存取；targets.yaml 里只有认证方式，没有密码。

use anyhow::{Context, Result};

const SERVICE: &str = "remote-build";

/// 读取密码：Ok(None) = 未存储过；Err = 凭据管理器故障（服务异常等）。
/// 两者分开，调用方才不会把「系统问题」误报成「没配密码」。
pub fn get_password(target_name: &str) -> Result<Option<String>> {
    let entry = keyring::Entry::new(SERVICE, target_name)
        .with_context(|| format!("打开凭据失败: {target_name}"))?;
    match entry.get_password() {
        Ok(p) => Ok(Some(p)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(anyhow::anyhow!("读取凭据「{target_name}」失败: {e}")),
    }
}

pub fn set_password(target_name: &str, password: &str) -> Result<()> {
    let entry = keyring::Entry::new(SERVICE, target_name)
        .with_context(|| format!("打开凭据失败: {target_name}"))?;
    entry
        .set_password(password)
        .with_context(|| format!("保存密码失败: {target_name}"))
}

pub fn delete_password(target_name: &str) {
    if let Ok(entry) = keyring::Entry::new(SERVICE, target_name) {
        let _ = entry.delete_password();
    }
}
