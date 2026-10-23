//! 核心数据模型（设计文档 §3 / §4.2 / §9.4）。
//!
//! serde 命名约定（v0.4 §3）：
//! - struct 字段统一 snake_case（serde 默认，与 YAML 一致）
//! - 出现在 YAML 中的枚举值统一 kebab-case，显式标注 `#[serde(rename_all = "kebab-case")]`
//! - 仅内部传输的结构（LogEvent / StepResult）不做序列化约束

use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::HashMap;

/// yaml_serde（serde_yaml 后继，YAML 官方组织维护）对 derive 枚举会输出 YAML tag（如 `!exec`），而文档约定 YAML 使用
/// 纯字符串值（`step_type: exec`）。此宏生成与 `#[serde(rename_all = "kebab-case")]`
/// 等价的手写实现：序列化为字符串、从字符串解析，字面量即 kebab-case 值。
macro_rules! kebab_string_enum {
    ($name:ident { $($variant:ident => $literal:literal),+ $(,)? }) => {
        impl $name {
            /// YAML 中的字符串形式
            pub fn as_str(&self) -> &'static str {
                match self { $($name::$variant => $literal),+ }
            }

            pub fn parse_opt(s: &str) -> Option<Self> {
                match s { $($literal => Some($name::$variant),)+ _ => None }
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl serde::Serialize for $name {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str(self.as_str())
            }
        }

        impl<'de> serde::Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let s = <String as serde::Deserialize>::deserialize(deserializer)?;
                Self::parse_opt(&s).ok_or_else(|| {
                    serde::de::Error::custom(format!(
                        "未知值: {s}（期望: {}）",
                        [$($literal),+].join(" / ")
                    ))
                })
            }
        }
    };
}

// ---------------------------------------------------------------------------
// Project（§3.1）
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Project {
    pub name: String,
    pub description: Option<String>,
    /// 本地代码目录（UI 输入框绑定）
    pub local_path: String,
    /// 环境 → 变量表
    pub variables: HashMap<String, EnvironmentVariables>,
    /// 项目级工具覆盖：键相同整体覆盖全局 tools.yaml 条目。
    /// 用于 pnpm/yarn 等工程内工具链的按项目固定（§6.5），如
    /// tools: { pnpm: { command: D:/tools/pnpm/pnpm.cmd, tool_type: custom } }
    #[serde(default)]
    pub tools: HashMap<String, Tool>,
    /// 关联的 job 文件名列表
    pub jobs: Vec<String>,
    /// 项目级目标定义（键 = 目标名）：不同项目的 prod 可以是不同服务器。
    /// 与全局 targets.yaml 合并时项目同名覆盖全局（§3.5）。
    #[serde(default)]
    pub targets: HashMap<String, Target>,
    /// 可选：加载额外 .env 文件
    pub env_file: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct EnvironmentVariables {
    /// v0.4 §3.1：flatten 使 YAML 中直接展开键值对（没有 vars 包装层），
    /// 与 project.yaml 的 variables: staging: {REGISTRY: ...} 结构一一对应。
    #[serde(flatten)]
    pub vars: HashMap<String, String>,
}

// ---------------------------------------------------------------------------
// Job（§3.2）
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Job {
    pub name: String,
    pub project: String,
    pub description: Option<String>,
    /// 默认执行目标（SSH host / k8s context 名）
    pub target: Option<String>,
    /// 默认环境（staging / prod），决定变量替换
    pub environment: Option<String>,
    /// 整个 job 超时（秒）
    pub timeout: Option<u64>,
    pub on_failure: FailureAction,
    pub steps: Vec<Step>,
    /// 项目级默认工作目录（相对 local_path）
    pub working_dir: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FailureAction {
    /// 失败立即停止（默认）
    #[default]
    Stop,
    /// 跳过当前步骤继续
    Continue,
    /// 重试整个 job（P1）
    Retry,
}
kebab_string_enum!(FailureAction {
    Stop => "stop",
    Continue => "continue",
    Retry => "retry",
});

// ---------------------------------------------------------------------------
// Step（§3.3）
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Step {
    pub name: Option<String>,
    /// 引用 tools.yaml 里的工具名
    pub tool: String,
    /// 自由输入，支持多行
    pub command: String,
    pub step_type: StepType,
    pub enabled: bool,
    pub continue_on_error: bool,
    /// 单步超时（秒），默认 300
    pub timeout: Option<u64>,
    pub retry: Option<RetryConfig>,
    /// 相对 project.local_path 的子目录
    pub working_dir: Option<String>,
    /// 步骤级额外环境变量
    pub env: HashMap<String, String>,
    /// 条件执行，如 ${{ env.VERSION != '' }}
    pub condition: Option<String>,
    /// step_type=k8s-apply 时有效（P1.5 生效）
    pub k8s_watch: Option<Vec<K8sWatchTarget>>,
    /// 步骤级目标（§3.3）：覆盖流程级默认目标——混合流程的关键。
    /// 例：下载/编译用 local，上传/发布用 prod。None = 跟随流程的 job.target。
    #[serde(default)]
    pub target: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StepType {
    /// 执行命令，退出即走
    #[default]
    Exec,
    /// 轮询等条件满足
    Wait,
    /// 文件传输（pscp）
    Upload,
    /// 打包为 zip（本地操作，供后续 upload 单文件上传，§5.8）
    Pack,
    /// kubectl apply + 自动 watch 资源状态（P1.5）
    K8sApply,
}
kebab_string_enum!(StepType {
    Exec => "exec",
    Wait => "wait",
    Upload => "upload",
    Pack => "pack",
    K8sApply => "k8s-apply",
});

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RetryConfig {
    pub max_attempts: u32,
    pub backoff_seconds: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct K8sWatchTarget {
    pub kind: String,
    pub name: String,
    pub namespace: Option<String>,
    pub min_ready: Option<u32>,
}

// ---------------------------------------------------------------------------
// Tool（§3.4）
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Tool {
    pub name: String,
    pub tool_type: ToolType,
    /// 可执行文件名或完整路径
    pub command: String,
    pub install_path: Option<String>,
    pub version: Option<String>,
    pub version_check: Option<String>,
    /// 首次启动时从哪个环境变量探测到
    pub detected_from: Option<String>,
    /// type=archive 时的下载安装配置
    pub install: Option<ToolInstall>,
    /// 工具级环境变量（如 JAVA_HOME）
    pub env: HashMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ToolType {
    /// 系统已装，PATH 里能找到
    #[default]
    System,
    /// 需要下载解压
    Archive,
    /// 自定义路径
    Custom,
    /// 一段脚本当工具
    Script,
}
kebab_string_enum!(ToolType {
    System => "system",
    Archive => "archive",
    Custom => "custom",
    Script => "script",
});

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolInstall {
    pub url: String,
    pub extract_to: String,
    pub checksum: Option<String>,
}

/// tools.yaml 的文件根结构
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ToolsFile {
    pub tools: HashMap<String, Tool>,
}

// ---------------------------------------------------------------------------
// Target（§3.5）
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Target {
    pub name: String,
    pub target_type: TargetType,
    /// 远端工作目录
    pub work_root: Option<String>,
    pub env: HashMap<String, String>,
    /// 凭据管理器的键（运行时按作用域回填：{项目}/{目标} 或 global/{目标}），不序列化
    #[serde(skip)]
    pub cred_key: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub enum TargetType {
    /// YAML 双形态：
    ///   target_type: { ssh: { host: ..., port: ..., user: ..., auth: ... } }
    ///   target_type: local
    /// yaml_serde 的 derive 枚举会输出 `!ssh` tag，与文档格式不符，故手写实现。
    Ssh {
        host: String,
        port: u16,
        user: String,
        auth: AuthMethod,
        /// 主机键指纹（SHA256:...），设置后经 plink -hostkey 固定，免去首次交互确认。
        /// 首次连接测试失败时输出会带上服务器指纹，复制填入即可（TOFU）。
        host_key: Option<String>,
    },
    Kubernetes {
        kubeconfig: String,
        context: Option<String>,
        namespace: Option<String>,
        registry: Option<String>,
    },
    /// 本机执行
    #[default]
    Local,
}

/// TargetType::Ssh 的字段载体（用于映射形式反/序列化）
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(default)]
struct SshTargetFields {
    host: String,
    port: u16,
    user: String,
    auth: AuthMethod,
    host_key: Option<String>,
}

impl Default for SshTargetFields {
    fn default() -> Self {
        Self {
            host: String::new(),
            port: 22,
            user: String::new(),
            auth: AuthMethod::default(),
            host_key: None,
        }
    }
}

/// TargetType::Kubernetes 的字段载体
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(default)]
struct KubernetesTargetFields {
    kubeconfig: String,
    context: Option<String>,
    namespace: Option<String>,
    registry: Option<String>,
}

impl Default for KubernetesTargetFields {
    fn default() -> Self {
        Self { kubeconfig: String::new(), context: None, namespace: None, registry: None }
    }
}

impl serde::Serialize for TargetType {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            TargetType::Local => serializer.serialize_str("local"),
            TargetType::Ssh { host, port, user, auth, host_key } => {
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry(
                    "ssh",
                    &SshTargetFields {
                        host: host.clone(),
                        port: *port,
                        user: user.clone(),
                        auth: auth.clone(),
                        host_key: host_key.clone(),
                    },
                )?;
                map.end()
            }
            TargetType::Kubernetes { kubeconfig, context, namespace, registry } => {
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry(
                    "kubernetes",
                    &KubernetesTargetFields {
                        kubeconfig: kubeconfig.clone(),
                        context: context.clone(),
                        namespace: namespace.clone(),
                        registry: registry.clone(),
                    },
                )?;
                map.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for TargetType {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = yaml_serde::Value::deserialize(deserializer)?;
        match value {
            yaml_serde::Value::String(s) => match s.as_str() {
                "local" => Ok(TargetType::Local),
                other => Err(serde::de::Error::custom(format!(
                    "未知 target_type: {other}（期望 ssh / kubernetes / local）"
                ))),
            },
            yaml_serde::Value::Mapping(m) => {
                if m.len() != 1 {
                    return Err(serde::de::Error::custom(
                        "target_type 映射只允许一个键（ssh / kubernetes）",
                    ));
                }
                let (k, v) = m.into_iter().next().expect("len checked");
                let key = k
                    .as_str()
                    .ok_or_else(|| serde::de::Error::custom("target_type 键必须是字符串"))?;
                match key {
                    "ssh" => {
                        let f: SshTargetFields =
                            yaml_serde::from_value(v).map_err(serde::de::Error::custom)?;
                        Ok(TargetType::Ssh {
                            host: f.host,
                            port: f.port,
                            user: f.user,
                            auth: f.auth,
                            host_key: f.host_key,
                        })
                    }
                    "kubernetes" => {
                        let f: KubernetesTargetFields =
                            yaml_serde::from_value(v).map_err(serde::de::Error::custom)?;
                        Ok(TargetType::Kubernetes {
                            kubeconfig: f.kubeconfig,
                            context: f.context,
                            namespace: f.namespace,
                            registry: f.registry,
                        })
                    }
                    other => Err(serde::de::Error::custom(format!(
                        "未知 target_type: {other}（期望 ssh / kubernetes / local）"
                    ))),
                }
            }
            _ => Err(serde::de::Error::custom(
                "target_type 必须是字符串（local）或单键映射（ssh / kubernetes）",
            )),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub enum AuthMethod {
    // YAML 双形态：auth: { key: { path: ... } } 或 auth: agent / none
    Key { path: String },
    Password { encrypted: String },
    /// pageant / ssh-agent
    Agent,
    #[default]
    None,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct KeyAuthFields {
    path: String,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct PasswordAuthFields {
    encrypted: String,
}

impl serde::Serialize for AuthMethod {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            AuthMethod::Agent => serializer.serialize_str("agent"),
            AuthMethod::None => serializer.serialize_str("none"),
            AuthMethod::Key { path } => {
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry("key", &KeyAuthFields { path: path.clone() })?;
                map.end()
            }
            AuthMethod::Password { encrypted } => {
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry("password", &PasswordAuthFields { encrypted: encrypted.clone() })?;
                map.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for AuthMethod {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = yaml_serde::Value::deserialize(deserializer)?;
        match value {
            yaml_serde::Value::Null => Ok(AuthMethod::None),
            yaml_serde::Value::String(s) => match s.as_str() {
                // password 简写合法：密码值存 Credential Manager（按目标名），YAML 里不带值
                "agent" => Ok(AuthMethod::Agent),
                "none" => Ok(AuthMethod::None),
                "password" => Ok(AuthMethod::Password { encrypted: String::new() }),
                other => Err(serde::de::Error::custom(format!(
                    "未知 auth: {other}（期望 key / password / agent / none）"
                ))),
            },
            yaml_serde::Value::Mapping(m) => {
                if m.len() != 1 {
                    return Err(serde::de::Error::custom("auth 映射只允许一个键（key / password）"));
                }
                let (k, v) = m.into_iter().next().expect("len checked");
                let key = k
                    .as_str()
                    .ok_or_else(|| serde::de::Error::custom("auth 键必须是字符串"))?;
                match key {
                    "key" => {
                        let f: KeyAuthFields = yaml_serde::from_value(v).map_err(serde::de::Error::custom)?;
                        Ok(AuthMethod::Key { path: f.path })
                    }
                    "password" => {
                        let f: PasswordAuthFields =
                            yaml_serde::from_value(v).map_err(serde::de::Error::custom)?;
                        Ok(AuthMethod::Password { encrypted: f.encrypted })
                    }
                    other => Err(serde::de::Error::custom(format!(
                        "未知 auth: {other}（期望 key / password / agent / none）"
                    ))),
                }
            }
            _ => Err(serde::de::Error::custom(
                "auth 必须是字符串（agent / none）或单键映射（key / password）",
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// 全局配置（§3.6）
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GlobalConfig {
    pub default_timeout: u64,
    pub max_concurrent_jobs: u32,
    pub log_retention_days: u32,
    pub workspace_root: String,
    pub auto_update_tools: bool,
    pub theme: Theme,
    /// PuTTY 配置
    pub putty: PuttyConfig,
}

impl Default for GlobalConfig {
    fn default() -> Self {
        Self {
            default_timeout: 600,
            max_concurrent_jobs: 3,
            log_retention_days: 30,
            workspace_root: String::new(), // 由 config::default_global() 填充实际路径
            auto_update_tools: false,
            theme: Theme::Dark,
            putty: PuttyConfig {
                use_builtin: true,
                custom_path: None,
            },
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PuttyConfig {
    /// true = 用内置版本
    pub use_builtin: bool,
    /// 自定义路径，use_builtin=false 时生效
    pub custom_path: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Theme {
    Light,
    #[default]
    Dark,
    System,
}
kebab_string_enum!(Theme {
    Light => "light",
    Dark => "dark",
    System => "system",
});

// ---------------------------------------------------------------------------
// 变量文件（§3.7，P1 使用）
// ---------------------------------------------------------------------------

/// P1 变量文件结构（当前仅全局 variables.yaml 兜底用）
#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct VariablesFile {
    pub environments: HashMap<String, HashMap<String, String>>,
    /// secrets 加密存储路径
    pub secrets: Option<String>,
}

// ---------------------------------------------------------------------------
// 日志事件与状态（§4.2 / §4.4）——仅进程内 mpsc 传输，不序列化到 YAML
// ---------------------------------------------------------------------------

/// §4.2 事件协议，字段按设计文档完整定义；P0 UI 未消费的 step_index / run_id
/// 供 P1 控制台按步骤过滤、历史检索使用
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub enum LogEvent {
    StepStart { step_index: usize, name: String },
    Stdout { step_index: usize, line: String },
    Stderr { step_index: usize, line: String },
    StepEnd { step_index: usize, exit_code: i32, duration_ms: u64 },
    JobEnd { run_id: String, status: JobStatus, total_duration_ms: u64 },
    Error { message: String },
}

/// 落 SQLite 时用 `.short()` 转字符串：running / success / failed / cancelled / timeout。
/// 仅内部传输，不做 serde（serde_yaml 会输出 tag 形式，与字符串语义不符）。
#[derive(Debug, Clone)]
pub enum JobStatus {
    /// 运行中：P0 用 RunHandle.finished == None 表达，落 SQLite（P1）时用本变体
    #[allow(dead_code)]
    Running,
    Success,
    Failed { step_index: usize, reason: String },
    Cancelled,
    Timeout,
}

impl JobStatus {
    pub fn short(&self) -> String {
        match self {
            JobStatus::Running => "running".into(),
            JobStatus::Success => "success".into(),
            JobStatus::Failed { step_index, reason } => {
                // 对外展示统一 1 起始（与 StepStart/StepEnd 事件一致）
                format!("failed (step {}: {})", step_index + 1, reason)
            }
            JobStatus::Cancelled => "cancelled".into(),
            JobStatus::Timeout => "timeout".into(),
        }
    }

    pub fn ok(&self) -> bool {
        matches!(self, JobStatus::Success)
    }
}

#[derive(Debug)]
pub enum StepResult {
    Success {
        exit_code: i32,
        duration_ms: u64,
        /// 步骤产出变量，合并进 ctx.variables
        output_vars: HashMap<String, String>,
    },
    Failed {
        exit_code: i32,
        duration_ms: u64,
        reason: String,
    },
    Cancelled,
    Timeout,
}

// ---------------------------------------------------------------------------
// 项目校验（§9.4）
// ---------------------------------------------------------------------------

/// 项目校验问题（仅 UI 提示用，不序列化到 YAML）
/// field 标识出问题的配置项；P0 规则只覆盖 local_path，扩展规则后 UI 按字段定位
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub enum ValidationIssue {
    /// 阻断性问题：目录缺失、配置不完整，Run 前必须解决
    Error { field: String, message: String },
    /// 非阻断提醒：如目录存在但不是 git 仓库
    Warning { field: String, message: String },
}

// ---------------------------------------------------------------------------
// 测试：YAML ↔ 结构体 序列化对齐（v0.4 评审修复的回归测试）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_yaml_direct_key_values() {
        // §3.1 示例：variables 下直接展开键值对，没有 vars 包装层
        let yaml = r#"
name: payment-service
description: "支付核心服务"
local_path: D:\workspace\payment-service
variables:
  staging:
    REGISTRY: registry-staging.internal:5000
    NAMESPACE: staging
  prod:
    REGISTRY: registry-prod.internal:5000
    NAMESPACE: production
jobs:
  - build
  - deploy-prod
targets:
  build-server-01:
    target_type:
      ssh:
        host: 10.0.0.5
        port: 22
        user: builder
        auth: password
env_file: null
"#;
        let p: Project = yaml_serde::from_str(yaml).unwrap();
        assert_eq!(p.name, "payment-service");
        assert_eq!(
            p.variables["staging"].vars["REGISTRY"],
            "registry-staging.internal:5000"
        );
        assert_eq!(p.variables["prod"].vars["NAMESPACE"], "production");

        // 反序列化后再序列化仍是无 vars 包装层的形态
        let out = yaml_serde::to_string(&p).unwrap();
        assert!(!out.contains("vars:"), "flatten 后不应出现 vars: 包装层");
    }

    #[test]
    fn step_type_kebab_case() {
        // K8sApply ↔ k8s-apply（v0.4 §3.3）
        let s: StepType = yaml_serde::from_str("k8s-apply").unwrap();
        assert!(matches!(s, StepType::K8sApply));
        assert_eq!(yaml_serde::to_string(&s).unwrap().trim(), "k8s-apply");

        for (raw, expected) in [
            ("exec", StepType::Exec),
            ("wait", StepType::Wait),
            ("upload", StepType::Upload),
        ] {
            let s: StepType = yaml_serde::from_str(raw).unwrap();
            assert!(std::mem::discriminant(&s) == std::mem::discriminant(&expected));
        }

        // 嵌在 struct 里序列化必须是纯字符串（serde_yaml derive 会变成 !exec tag）
        #[derive(serde::Serialize)]
        struct Wrap {
            step_type: StepType,
        }
        let yaml = yaml_serde::to_string(&Wrap { step_type: StepType::Exec }).unwrap();
        assert_eq!(yaml.trim(), "step_type: exec");
    }

    #[test]
    fn failure_action_kebab_case() {
        let f: FailureAction = yaml_serde::from_str("stop").unwrap();
        assert!(matches!(f, FailureAction::Stop));
        let f: FailureAction = yaml_serde::from_str("continue").unwrap();
        assert!(matches!(f, FailureAction::Continue));
        assert_eq!(yaml_serde::to_string(&FailureAction::Retry).unwrap().trim(), "retry");

        // 未知值给出可读错误
        let err = yaml_serde::from_str::<FailureAction>("halt").unwrap_err();
        assert!(err.to_string().contains("halt"));
    }

    #[test]
    fn job_yaml_parses() {
        // §3.2 示例
        let yaml = r#"
name: build
project: payment-service
description: "Maven 编译打包"
target: build-server-01
environment: staging
timeout: 600
on_failure: stop
working_dir: null
steps:
  - name: "mvn clean package"
    tool: maven
    command: mvn clean package -DskipTests
    step_type: exec
    enabled: true
"#;
        let j: Job = yaml_serde::from_str(yaml).unwrap();
        assert_eq!(j.steps.len(), 1);
        assert!(matches!(j.steps[0].step_type, StepType::Exec));
        assert!(j.steps[0].enabled);
        assert!(matches!(j.on_failure, FailureAction::Stop));
    }

    #[test]
    fn target_yaml_all_variants() {
        // §3.5 示例（v0.4 调整：目标定义内嵌 project.yaml，不再有全局 targets.yaml）：
        // project.yaml 的 targets 表 —— ssh / kubernetes / local 三种形态
        let yaml = r#"
name: demo
local_path: D:\ws\demo
targets:
  build-server-01:
    name: build-server-01
    target_type:
      ssh:
        host: 192.168.1.100
        port: 22
        user: builder
        auth:
          key:
            path: C:\Users\dev\.ssh\id_ed25519
    work_root: /opt/builds
    env: {}

  staging-k8s:
    name: staging-k8s
    target_type:
      kubernetes:
        kubeconfig: C:\Users\dev\.kube\config
        context: staging
        namespace: staging
        registry: registry-staging.internal:5000

  local:
    name: local
    target_type: local
"#;
        let p: Project = yaml_serde::from_str(yaml).unwrap();
        let targets = &p.targets;
        let ssh = &targets["build-server-01"];
        match &ssh.target_type {
            TargetType::Ssh { host, port, user, auth, .. } => {
                assert_eq!(host, "192.168.1.100");
                assert_eq!(*port, 22);
                assert_eq!(user, "builder");
                match auth {
                    AuthMethod::Key { path } => {
                        assert!(path.ends_with("id_ed25519"));
                    }
                    other => panic!("expect key auth, got {other:?}"),
                }
            }
            other => panic!("expect ssh, got {other:?}"),
        }
        assert!(matches!(
            targets["staging-k8s"].target_type,
            TargetType::Kubernetes { .. }
        ));
        assert!(matches!(targets["local"].target_type, TargetType::Local));

        // unit variant 序列化为字符串 local
        let s = yaml_serde::to_string(&TargetType::Local).unwrap();
        assert_eq!(s.trim(), "local");

        // ssh 变体序列化回映射形态（无 YAML tag）
        let yaml = yaml_serde::to_string(&ssh.target_type).unwrap();
        assert!(yaml.contains("ssh:"), "应序列化为 ssh 键映射，实际: {yaml}");
        assert!(!yaml.contains('!'), "不应出现 YAML tag，实际: {yaml}");
    }

    #[test]
    fn global_config_defaults_and_roundtrip() {
        let yaml = "theme: dark";
        let g: GlobalConfig = yaml_serde::from_str(yaml).unwrap();
        assert!(matches!(g.theme, Theme::Dark));
        assert!(g.putty.use_builtin, "默认用内置 putty");

        // 缺字段时用 Default
        let g: GlobalConfig = yaml_serde::from_str("{}").unwrap();
        assert_eq!(g.max_concurrent_jobs, 3);
    }

    #[test]
    fn status_kebab_strings_for_sqlite() {
        assert_eq!(JobStatus::Cancelled.short(), "cancelled");
        assert_eq!(JobStatus::Timeout.short(), "timeout");
        assert!(JobStatus::Success.short().starts_with("success"));
    }
}
