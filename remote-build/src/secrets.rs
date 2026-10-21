//! 敏感凭证存储（设计文档 §14：密码存 Windows Credential Manager，绝不落 YAML）。
//!
//! 目标主机的密码按「目标名」为键存取；targets.yaml 里只有认证方式，没有密码。

use anyhow::{Context, Result};

const SERVICE: &str = "remote-build";

pub fn get_password(target_name: &str) -> Option<String> {
    keyring::Entry::new(SERVICE, target_name)
        .ok()?
        .get_password()
        .ok()
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
