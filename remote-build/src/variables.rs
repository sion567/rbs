//! 变量解析器（设计文档 §6）。
//!
//! 替换语法与来源（优先级见 §6.1）：
//! - `${{ variables.X }}`  → project.variables[environment] 的环境变量表
//! - `${{ env.X }}`        → 步骤产出 / Target / Tool / Step 注入的全局 env
//! - `${{ targets.<名>.<字段> }}` → target 配置字段（host/port/user/...）
//! - `${VAR}`              → 合并变量表后兜底到系统环境变量（兼容 shell 习惯）
//!
//! 解析时机：执行前一次性替换（§6.3），命令字符串可预览、可日志、可审计。
//! 未解析到的占位符原样保留，便于用户发现拼写错误。

use crate::models::Target;
use std::collections::HashMap;

#[derive(Debug, Clone, Default)]
pub struct VarContext {
    /// project.variables[environment] — ${{ variables.X }}
    pub variables: HashMap<String, String>,
    /// 步骤产出 / 全局 env — ${{ env.X }}
    pub env: HashMap<String, String>,
    /// ${{ targets.<name>.<field> }}
    pub targets: HashMap<String, Target>,
}

/// 一次性替换输入串中的所有占位符
pub fn resolve(input: &str, ctx: &VarContext) -> String {
    let after_double = replace_double_brace(input, ctx);
    replace_single_brace(&after_double, ctx)
}

/// 替换 `${{ ... }}` 形式；未解析的占位符按原文保留（含内部空格）
fn replace_double_brace(input: &str, ctx: &VarContext) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;

    while let Some(start) = rest.find("${{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 3..];
        match after.find("}}") {
            Some(end) => {
                let expr = after[..end].trim();
                match lookup_double(expr, ctx) {
                    Some(v) => out.push_str(&v),
                    None => out.push_str(&rest[..start + 3 + end + 2]), // 原样保留 ${{ ... }}
                }
                rest = &after[end + 2..];
            }
            None => {
                // 没有闭合的 }}，原样保留剩余内容
                out.push_str("${{");
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

fn lookup_double(expr: &str, ctx: &VarContext) -> Option<String> {
    let parts: Vec<&str> = expr.split('.').map(str::trim).collect();
    match parts.as_slice() {
        ["variables", key] => ctx.variables.get(*key).cloned(),
        ["env", key] => ctx.env.get(*key).cloned(),
        ["targets", name, field] => lookup_target_field(name, field, ctx),
        _ => None,
    }
}

fn lookup_target_field(name: &str, field: &str, ctx: &VarContext) -> Option<String> {
    let t = ctx.targets.get(name)?;
    let v = match field {
        "host" => match &t.target_type {
            crate::models::TargetType::Ssh { host, .. } => host.clone(),
            _ => return None,
        },
        "port" => match &t.target_type {
            crate::models::TargetType::Ssh { port, .. } => port.to_string(),
            _ => return None,
        },
        "user" => match &t.target_type {
            crate::models::TargetType::Ssh { user, .. } => user.clone(),
            _ => return None,
        },
        "kubeconfig" => match &t.target_type {
            crate::models::TargetType::Kubernetes { kubeconfig, .. } => kubeconfig.clone(),
            _ => return None,
        },
        "context" => match &t.target_type {
            crate::models::TargetType::Kubernetes { context, .. } => context.clone()?,
            _ => return None,
        },
        "namespace" => match &t.target_type {
            crate::models::TargetType::Kubernetes { namespace, .. } => namespace.clone()?,
            _ => return None,
        },
        "registry" => match &t.target_type {
            crate::models::TargetType::Kubernetes { registry, .. } => registry.clone()?,
            _ => return None,
        },
        "work_root" => t.work_root.clone()?,
        "name" => t.name.clone(),
        _ => return None,
    };
    Some(v)
}

/// 替换 `${VAR}` 形式（跳过 `${{`，它已在上一轮处理）
fn replace_single_brace(input: &str, ctx: &VarContext) -> String {
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] == b'$' && i + 1 < bytes.len() && bytes[i + 1] == b'{' {
            if i + 2 < bytes.len() && bytes[i + 2] == b'{' {
                // "${{" 未被解析的残留，原样复制两个字符，避免死循环
                out.push_str("${");
                i += 2;
                continue;
            }
            if let Some(end_rel) = input[i + 2..].find('}') {
                let name = &input[i + 2..i + 2 + end_rel];
                if is_valid_var_name(name) {
                    out.push_str(&lookup_single(name, ctx));
                    i += 2 + end_rel + 1;
                    continue;
                }
            }
        }
        // 普通字符（按 UTF-8 边界推进）
        let ch_len = input[i..].chars().next().map(char::len_utf8).unwrap_or(1);
        out.push_str(&input[i..i + ch_len]);
        i += ch_len;
    }
    out
}

fn is_valid_var_name(s: &str) -> bool {
    !s.is_empty()
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn lookup_single(name: &str, ctx: &VarContext) -> String {
    if let Some(v) = ctx.env.get(name) {
        return v.clone();
    }
    if let Some(v) = ctx.variables.get(name) {
        return v.clone();
    }
    std::env::var(name).unwrap_or_else(|_| format!("${{{name}}}"))
}

/// P0 条件求值（§13.3）：只支持非空检查。
/// 支持形态（两侧允许带 ${{ }} 包装）：
///   env.X != ''   /   env.X == ''
///   variables.X != '' / variables.X == ''
/// 其他表达式 → None，由调用方决定策略（fail-open + 告警）。
pub fn eval_condition(condition: &str, ctx: &VarContext) -> Option<bool> {
    let inner = condition.trim();
    let inner = inner
        .strip_prefix("${{")
        .and_then(|s| s.strip_suffix("}}"))
        .map(str::trim)
        .unwrap_or(inner);

    let (lhs, op, rhs) = if let Some(pos) = inner.find("!=") {
        (&inner[..pos], "!=", &inner[pos + 2..])
    } else if let Some(pos) = inner.find("==") {
        (&inner[..pos], "==", &inner[pos + 2..])
    } else {
        return None;
    };

    let lhs = lhs.trim();
    let rhs = rhs.trim();
    let empty: &[&str] = &["''", "\"\""];
    if !empty.contains(&rhs) {
        return None;
    }

    let value = lookup_double(lhs, ctx);
    // 未解析（None / 占位符原样）视为空
    let is_empty = match value {
        None => true,
        Some(v) => v.is_empty() || v.starts_with("${{") || v.starts_with("${"),
    };

    Some(if op == "!=" { !is_empty } else { is_empty })
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{Target, TargetType};

    fn ctx() -> VarContext {
        let mut variables = HashMap::new();
        variables.insert("REGISTRY".to_string(), "registry-staging.internal:5000".to_string());
        variables.insert("NAMESPACE".to_string(), "staging".to_string());
        let mut env = HashMap::new();
        env.insert("VERSION".to_string(), "1.0.0".to_string());
        let mut targets = HashMap::new();
        targets.insert(
            "prod".to_string(),
            Target {
                name: "prod".to_string(),
                target_type: TargetType::Ssh {
                    host: "10.0.0.1".to_string(),
                    port: 2222,
                    user: "root".to_string(),
                    auth: crate::models::AuthMethod::None,
                    host_key: None,
                },
                work_root: Some("/opt/builds".to_string()),
                env: HashMap::new(),
                cred_key: None,
            },
        );
        VarContext { variables, env, targets }
    }

    #[test]
    fn resolves_double_brace() {
        let c = ctx();
        assert_eq!(
            resolve("docker build -t ${{ variables.REGISTRY }}/app:${{ env.VERSION }} .", &c),
            "docker build -t registry-staging.internal:5000/app:1.0.0 ."
        );
    }

    #[test]
    fn resolves_target_fields() {
        let c = ctx();
        assert_eq!(resolve("${{ targets.prod.host }}:${{ targets.prod.port }}", &c), "10.0.0.1:2222");
        assert_eq!(resolve("${{ targets.prod.work_root }}/out", &c), "/opt/builds/out");
    }

    #[test]
    fn unresolved_keeps_literal() {
        let c = ctx();
        assert_eq!(resolve("${{ variables.TYPO }}", &c), "${{ variables.TYPO }}");
        assert_eq!(resolve("${{ unknown.kind }}", &c), "${{ unknown.kind }}");
    }

    #[test]
    fn single_brace_falls_back_to_process_env() {
        let c = ctx();
        std::env::set_var("RBS_TEST_VAR_XYZ", "hello");
        assert_eq!(resolve("path=${RBS_TEST_VAR_XYZ}!", &c), "path=hello!");
        assert_eq!(resolve("missing=${RBS_NO_SUCH_VAR_QQ}", &c), "missing=${RBS_NO_SUCH_VAR_QQ}");
        // env 表优先于系统环境变量
        assert_eq!(resolve("v=${VERSION}", &c), "v=1.0.0");
        std::env::remove_var("RBS_TEST_VAR_XYZ");
    }

    #[test]
    fn unclosed_brace_is_safe() {
        let c = ctx();
        assert_eq!(resolve("abc ${{ variables.REGISTRY ", &c), "abc ${{ variables.REGISTRY ");
        assert_eq!(resolve("x=${UNCLOSED", &c), "x=${UNCLOSED");
    }

    #[test]
    fn condition_non_empty_check() {
        let c = ctx();
        assert_eq!(eval_condition("${{ env.VERSION != '' }}", &c), Some(true));
        assert_eq!(eval_condition("${{ env.MISSING != '' }}", &c), Some(false));
        assert_eq!(eval_condition("${{ env.MISSING == '' }}", &c), Some(true));
        assert_eq!(eval_condition("VERSION > 1", &c), None, "P0 只支持非空检查");
    }
}
