//! 版本要求匹配（mise 风格）。
//!
//! project.yaml 的 `tools_require` 声明工具版本要求，与 tools.yaml 探测到的版本对比：
//! 运行前偏差零步失败（`>=21`），干跑预览显示偏差——避免发版时才发现版本不对。
//!
//! 要求形态：
//! - `">=21"` / `">20"` / `"<=30"` / `"<99"`：按数字段逐段比较（缺段补 0）
//! - `"22"` / `"=22"`：前缀匹配（22 → 22.x；22.1 → 22.1.x；"22.1.3" → 精确到三位）
//!
//! 版本号解析容忍常见噪声：`v22.11.0`、`1.8.0_292`（取到第一个非数字/点为止）。

/// 从版本字符串提取数字段：第一个数字开始，取连续的数字与点
pub fn parse_version(s: &str) -> Vec<u64> {
    let Some(start) = s.find(|c: char| c.is_ascii_digit()) else {
        return Vec::new();
    };
    let rest = &s[start..];
    let end = rest
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(rest.len());
    rest[..end]
        .split('.')
        .filter(|p| !p.is_empty())
        .filter_map(|p| p.parse().ok())
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Op {
    Ge,
    Gt,
    Le,
    Lt,
    /// 前缀匹配（裸值 / "="）
    Prefix,
}

/// 要求是否满足；解析失败返回 Err（配置拼写问题要暴露，不静默放行）
pub fn version_matches(requirement: &str, detected: &str) -> Result<bool, String> {
    let req = requirement.trim();
    if req.is_empty() {
        return Err("版本要求为空".to_string());
    }
    let (op, ver_part) = if let Some(v) = req.strip_prefix(">=") {
        (Op::Ge, v)
    } else if let Some(v) = req.strip_prefix("<=") {
        (Op::Le, v)
    } else if let Some(v) = req.strip_prefix('>') {
        (Op::Gt, v)
    } else if let Some(v) = req.strip_prefix('<') {
        (Op::Lt, v)
    } else if let Some(v) = req.strip_prefix('=') {
        (Op::Prefix, v)
    } else {
        (Op::Prefix, req)
    };

    let req_ver = parse_version(ver_part);
    if req_ver.is_empty() {
        return Err(format!(
            "无法解析版本要求: {requirement}（示例: >=21 / 22 / 22.1.3）"
        ));
    }
    let det = parse_version(detected);
    if det.is_empty() {
        return Err(format!("无法从「{detected}」解析出版本号"));
    }

    match op {
        Op::Prefix => Ok(det.len() >= req_ver.len() && det[..req_ver.len()] == req_ver[..]),
        Op::Ge | Op::Gt | Op::Le | Op::Lt => {
            let n = req_ver.len().max(det.len());
            let mut r = req_ver.clone();
            let mut d = det.clone();
            r.resize(n, 0);
            d.resize(n, 0);
            Ok(match op {
                Op::Ge => d >= r,
                Op::Gt => d > r,
                Op::Le => d <= r,
                Op::Lt => d < r,
                Op::Prefix => unreachable!(),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_tolerates_common_noise() {
        assert_eq!(parse_version("21.0.3"), vec![21, 0, 3]);
        assert_eq!(parse_version("v22.11.0"), vec![22, 11, 0]);
        assert_eq!(parse_version("1.8.0_292"), vec![1, 8, 0]);
        assert_eq!(parse_version("openjdk 21.0.3"), vec![21, 0, 3]);
        assert_eq!(parse_version("17"), vec![17]);
        assert!(parse_version("unknown").is_empty());
    }

    #[test]
    fn comparison_operators() {
        assert!(version_matches(">=21", "21.0.3").unwrap());
        assert!(version_matches(">=21", "v22.11.0").unwrap());
        assert!(!version_matches(">=21", "17.0.9").unwrap());
        assert!(version_matches(">20", "21.0.3").unwrap());
        // 缺段补 0：>21 即 >21.0.0，所以 21.0.3 满足、21.0.0 不满足
        assert!(version_matches(">21", "21.0.3").unwrap());
        assert!(!version_matches(">21", "21.0.0").unwrap());
        assert!(!version_matches(">21", "17.0.9").unwrap());
        assert!(version_matches("<30", "21.0.3").unwrap());
        assert!(!version_matches("<21", "21.0.3").unwrap());
        assert!(version_matches("<=21.0.3", "21.0.3").unwrap());
    }

    #[test]
    fn prefix_semantics() {
        // 裸值 = 前缀匹配（mise 语义：22 → 22.x）
        assert!(version_matches("22", "22.11.0").unwrap());
        assert!(!version_matches("22", "21.0.3").unwrap());
        assert!(version_matches("22.11", "22.11.0").unwrap());
        assert!(!version_matches("22.11.2", "22.11.0").unwrap());
        assert!(version_matches("=17", "17.0.2").unwrap());
    }

    #[test]
    fn parse_failures_surface() {
        assert!(version_matches("abc", "1.0.0").is_err(), "要求拼写错误要暴露");
        assert!(version_matches(">=", "1.0.0").is_err());
        assert!(version_matches(">=21", "unknown").is_err(), "探测不到版本要暴露");
    }
}
