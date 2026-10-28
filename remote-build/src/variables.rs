//! 变量解析器（设计文档 §6）。
//!
//! 替换语法与来源（优先级见 §6.1）：
//! - `${{ variables.X }}`  → project.variables[environment] 的环境变量表
//! - `${{ variables.X || 默认值 }}` → 未定义时取默认值（Jinja2 default 语义）
//! - `${{ env.X }}`        → 步骤产出 / Target / Tool / Step 注入的全局 env
//! - `${{ targets.<名>.<字段> }}` → target 配置字段（host/port/user/...）
//! - `${{ now }}` 等内置变量 → 引擎侧生成的运行时刻（一次运行所有步骤同值）
//! - `${{ run.id }}`       → 本次运行唯一标识（与控制台日志文件同名）
//! - `${VAR}`              → 合并变量表后兜底到系统环境变量（兼容 shell 习惯）
//!
//! 解析时机：执行前一次性替换（§6.3），命令字符串可预览、可日志、可审计。
//! 未解析到的占位符原样保留，便于用户发现拼写错误。

use crate::models::Target;
use chrono::Local;
use std::collections::HashMap;

/// 内置时间变量：运行启动时生成一次，整个运行期间所有步骤同值
/// （备份后缀等场景要求同一次发版里各步骤引用一致）。
#[derive(Debug, Clone, Default)]
pub struct BuiltinNow {
    /// ${{ now }} → 20261023153045
    pub full: String,
    /// ${{ now.date }} → 20261023
    pub date: String,
    /// ${{ now.time }} → 153045
    pub time: String,
    /// ${{ now.iso }} → 2026-10-23 15:30:45
    pub iso: String,
}

impl BuiltinNow {
    /// 以当前本地时刻生成（时区 = 发版机；要以服务器时间为准请在远端命令里用 $(date …)）
    pub fn generate() -> Self {
        let n = Local::now();
        Self {
            full: n.format("%Y%m%d%H%M%S").to_string(),
            date: n.format("%Y%m%d").to_string(),
            time: n.format("%H%M%S").to_string(),
            iso: n.format("%Y-%m-%d %H:%M:%S").to_string(),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct VarContext {
    /// project.variables[environment] — ${{ variables.X }}
    pub variables: HashMap<String, String>,
    /// 步骤产出 / 全局 env — ${{ env.X }}
    pub env: HashMap<String, String>,
    /// ${{ targets.<name>.<field> }}
    pub targets: HashMap<String, Target>,
    /// 本次运行唯一标识（与日志文件同名）— ${{ run.id }}；None 时占位符原样保留
    pub run_id: Option<String>,
    /// 内置时间变量（引擎构建上下文时生成一次）
    pub now: BuiltinNow,
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
                match lookup_expr(expr, ctx) {
                    Some(v) => out.push_str(&v),
                    // 原样保留 ${{ ... }}（只推占位符段，前面的文本已在 out 里，勿重复）
                    None => out.push_str(&rest[start..start + 3 + end + 2]),
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
        // 内置变量：运行启动时刻（VarContext 构建时冻结，整个运行期间一致）
        ["now"] => Some(ctx.now.full.clone()),
        ["now", "date"] => Some(ctx.now.date.clone()),
        ["now", "time"] => Some(ctx.now.time.clone()),
        ["now", "iso"] => Some(ctx.now.iso.clone()),
        ["run", "id"] => ctx.run_id.clone(),
        _ => None,
    }
}

/// 取值表达式求值：在 `variables.X` 等形态之上支持 `expr || 默认值`
/// （Jinja2 `| default()` 语义）。左侧解析成功（含"定义了但为空串"）直接用；
/// 未定义 / 不可解析时取右侧默认值（可带引号，按原样文本注入）。
///
/// 已知局限（按需再扩展，勿过度设计）：按首个 `||` 切分，默认值本身含 `||`
/// 时会被再次切分；`||` 不区分"未定义"与"值为字面 `||` 场景"。
pub fn lookup_expr(expr: &str, ctx: &VarContext) -> Option<String> {
    if let Some((left, default)) = expr.split_once("||") {
        if let Some(v) = lookup_double(left.trim(), ctx) {
            return Some(v);
        }
        return Some(strip_quote_pairs(default.trim()).to_string());
    }
    lookup_double(expr, ctx)
}

/// 去掉成对的单/双引号（条件比较、默认值字面量用）
fn strip_quote_pairs(s: &str) -> &str {
    if s.len() >= 2
        && ((s.starts_with('\'') && s.ends_with('\'')) || (s.starts_with('"') && s.ends_with('"')))
    {
        &s[1..s.len() - 1]
    } else {
        s
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

/// 条件求值（§13.3）。支持形态（两侧允许带 ${{ }} 包装，Ansible when 风格）：
///   env.X != ''   /   env.X == ''          非空 / 空判断（未解析视为空）
///   ${{ variables.ENV }} == prod           与字面量比较（可带引号）
///   variables.A == variables.B             与另一取值表达式比较（支持 || 默认值）
/// 其他表达式 → None，由调用方决定策略（fail-open + 告警）。
///
/// 已知局限（低频、文档化即可）：
/// - 按首个 `!=` / `==` 切分，比较值本身含 `!=`（如 `== 'a!=b'`）会切错——
///   值比较仅面向简单的环境名 / 开关类字面量；
/// - 两侧都是**未定义**的取值表达式（`variables.A == variables.B`）时，右侧会
///   退化为字面量 "variables.B" 参与比较 → 结果 false，而非"两个空串相等"的 true。
pub fn eval_condition(condition: &str, ctx: &VarContext) -> Option<bool> {
    let trimmed = condition.trim();
    // 两种形态：
    // A. 整体一个包装、运算符在内：${{ env.VERSION != '' }}
    // B. 运算符在外、两侧各自包装：${{ variables.ENV }} == ${{ variables.TARGET }}
    //    （外层 strip 会把 B 形态剥坏，故剥完检查有没有残留包装再决定用哪个）
    let inner = match trimmed
        .strip_prefix("${{")
        .and_then(|s| s.strip_suffix("}}"))
        .map(str::trim)
    {
        Some(s) if !s.contains("${{") && !s.contains("}}") => s,
        _ => trimmed,
    };

    let (lhs, op, rhs) = if let Some(pos) = inner.find("!=") {
        (&inner[..pos], "!=", &inner[pos + 2..])
    } else {
        let pos = inner.find("==")?;
        (&inner[..pos], "==", &inner[pos + 2..])
    };

    let lhs = lhs.trim();
    let rhs = rhs.trim();

    fn unwrap_side(s: &str) -> &str {
        s.strip_prefix("${{")
            .and_then(|x| x.strip_suffix("}}"))
            .map(str::trim)
            .unwrap_or(s)
    }

    // 空串判断形态：== '' / != ''（未解析 / 占位符原样视为空）
    let empty: &[&str] = &["''", "\"\""];
    if empty.contains(&rhs) {
        let value = lookup_expr(unwrap_side(lhs), ctx);
        let is_empty = match value {
            None => true,
            Some(v) => v.is_empty() || v.starts_with("${{") || v.starts_with("${"),
        };
        return Some(if op == "!=" { !is_empty } else { is_empty });
    }

    // 值比较：lhs 未解析视为空串（与空判断同语义）；rhs 优先按取值表达式解析，
    // 解析不到再按字面量（去引号）
    let lhs_val = match lookup_expr(unwrap_side(lhs), ctx) {
        Some(v) if !v.starts_with("${{") && !v.starts_with("${") => v,
        _ => String::new(),
    };
    let rhs_unwrapped = unwrap_side(rhs);
    let rhs_val = match lookup_expr(rhs_unwrapped, ctx) {
        Some(v) if !v.starts_with("${{") && !v.starts_with("${") => v,
        _ => strip_quote_pairs(rhs_unwrapped).to_string(),
    };

    Some(if op == "==" { lhs_val == rhs_val } else { lhs_val != rhs_val })
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
            transport: None,
            work_root: Some("/opt/builds".to_string()),
            env: HashMap::new(),
            cred_key: None,
        },
        );
        VarContext { variables, env, targets, run_id: None, now: BuiltinNow::generate() }
    }

    #[test]
    fn builtin_now_formats() {
        let c = ctx();
        assert_eq!(resolve("${{ now }}", &c).len(), 14, "now 应为 yyyyMMddHHmmss");
        assert!(resolve("${{ now }}", &c).chars().all(|ch| ch.is_ascii_digit()));
        assert_eq!(resolve("${{ now.date }}", &c).len(), 8);
        assert_eq!(resolve("${{ now.time }}", &c).len(), 6);
        let iso = resolve("${{ now.iso }}", &c);
        assert_eq!(iso.len(), 19);
        assert!(chrono::NaiveDateTime::parse_from_str(&iso, "%Y-%m-%d %H:%M:%S").is_ok());
        // 同一上下文内多次取值一致（冻结语义）
        assert_eq!(resolve("${{ now }}", &c), resolve("${{ now }}", &c));
        // 未知子字段原样保留
        assert_eq!(resolve("${{ now.typo }}", &c), "${{ now.typo }}");
    }

    #[test]
    fn builtin_run_id() {
        let mut c = ctx();
        assert_eq!(resolve("${{ run.id }}", &c), "${{ run.id }}", "未设置时原样保留");
        c.run_id = Some("payment-service-20261023-153045".to_string());
        assert_eq!(resolve("echo ${{ run.id }}", &c), "echo payment-service-20261023-153045");
        // 与条件表达式兼容（非空检查）
        assert_eq!(eval_condition("${{ run.id != '' }}", &c), Some(true));
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
        // 回归：占位符前有文本时，未解析分支不得重复前缀
        assert_eq!(
            resolve("echo ${{ variables.TYPO }}!", &c),
            "echo ${{ variables.TYPO }}!"
        );
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
        assert_eq!(eval_condition("VERSION > 1", &c), None, "无 ==/!= 运算符不支持");
    }

    #[test]
    fn condition_value_comparison() {
        let c = ctx();
        // 与字面量比较（TODO「when」示例形态）
        assert_eq!(eval_condition("${{ variables.NAMESPACE }} == staging", &c), Some(true));
        assert_eq!(eval_condition("${{ variables.NAMESPACE }} == prod", &c), Some(false));
        assert_eq!(eval_condition("${{ variables.NAMESPACE }} != prod", &c), Some(true));
        // 引号字面量
        assert_eq!(eval_condition("${{ variables.NAMESPACE }} == 'staging'", &c), Some(true));
        // 未定义变量：视为空串参与比较
        assert_eq!(eval_condition("${{ variables.TYPO }} == prod", &c), Some(false));
        assert_eq!(eval_condition("${{ variables.TYPO }} != prod", &c), Some(true));
        // 两个取值表达式互比
        assert_eq!(eval_condition("variables.NAMESPACE == variables.NAMESPACE", &c), Some(true));
        // 两侧 ${{ }} 包装齐全的写法
        assert_eq!(
            eval_condition("${{ variables.NAMESPACE }} == ${{ variables.NAMESPACE }}", &c),
            Some(true)
        );
        // 数值字面量按字符串比较
        assert_eq!(eval_condition("${{ targets.prod.port }} == 2222", &c), Some(true));
    }

    #[test]
    fn default_value_syntax() {
        let c = ctx();
        // 已定义 → 用真值
        assert_eq!(
            resolve("${{ variables.REGISTRY || registry.local:5000 }}", &c),
            "registry-staging.internal:5000"
        );
        // 未定义 → 默认值
        assert_eq!(resolve("${{ variables.TYPO || fallback }}", &c), "fallback");
        // 默认值可带引号、含空格
        assert_eq!(resolve("${{ variables.TYPO || 'a b c' }}", &c), "a b c");
        assert_eq!(resolve("${{ variables.TYPO || \"x\" }}", &c), "x");
        // 未定义且无默认值 → 仍原样保留（便于发现拼写错误）
        assert_eq!(resolve("${{ variables.TYPO }}", &c), "${{ variables.TYPO }}");
        // 内置变量也可带默认值
        c_run_id_default();
    }

    fn c_run_id_default() {
        let c = ctx();
        assert_eq!(resolve("${{ run.id || manual-run }}", &c), "manual-run");
        assert_eq!(resolve("${{ run.id }}", &c), "${{ run.id }}", "无默认值仍原样保留");
    }
}
