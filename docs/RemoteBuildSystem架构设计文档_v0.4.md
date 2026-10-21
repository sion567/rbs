# Remote Build System — 架构设计文档 v0.4

| 字段 | 值 |
|------|-----|
| 版本 | v0.4 |
| 日期 | 2026-10-19 |
| 状态 | 设计中（P0 待实施） |
| 客户端 | Windows 桌面，egui + eframe |
| 服务端 | **无**（已移除） |
| 传输层 | PuTTY 套件（内置捆绑） |
| 配置管理 | YAML 文件 + Git 仓库 |
| 权限/审批 | **无**（研发自用，已移除） |

---

## 变更记录

| 版本 | 日期 | 关键变更 |
|------|------|---------|
| v0.1 | 2026-01-10 | 初始设计：核心模型、执行引擎、三栏 UI、SSH/SFTP |
| v0.2 | 2026-01-12 | +k8s 支持、+环境变量探测、+Project 工程化分组、+变量系统 |
| v0.3 | 2026-01-15 | **移除** Web UI / Tauri / 中心服务端 / 权限模型 / 审批流；PuTTY 套件改为内置捆绑分发；新增项目本地目录输入框；更新目录结构与打包策略 |
| v0.4 | 2026-10-19 | 评审修复 10 项：serde 序列化对齐（`EnvironmentVariables` flatten、对外枚举补 `rename_all`）、`build_ssh_args` 解构修复、远端 `cd` 路径引号转义、补 `ValidationIssue` 定义、`FailureAction` 去重、澄清 tools.yaml putty 条目与 `PuttyTransport` 关系、完整性校验覆盖全部捆绑文件、UI 树修正、补充 `cancel_rx` 分发说明 |

### v0.3 相较 v0.2 的实质性变化

| 砍掉的 | 影响 |
|--------|------|
| Web UI / Tauri / 前端工程 | 不做前后端分离，纯 egui 单进程 |
| 中心服务端（队列/共享历史/WebSocket） | 各人本地跑，历史落本地 SQLite |
| `users.yaml` / RBAC / approvers | 无用户概念 |
| `StepType::Approval` | StepType 剩 4 个 |
| 邮件/钉钉通知服务 | 仅保留 Windows 桌面弹窗（可选） |

**净效果**：P0 代码量从预估 ~8000 行降至 ~3000 行，单人或两人两周可出可运行版本。

### v0.4 相较 v0.3 的实质性变化（评审修复）

本版不含功能变更，全部为评审问题的修复与文档澄清。

**代码 / 序列化修复（较严重）**

| # | 问题 | 修复 | 位置 |
|---|------|------|------|
| 1 | `EnvironmentVariables` 期望 `vars:` 包装层，与 YAML 直接展开键值对的结构不匹配，反序列化会失败 | `vars` 字段加 `#[serde(flatten)]` | §3.1 |
| 2 | `build_ssh_args` 模式匹配只解构 `port/auth`，`user/host` 不在作用域却仍被引用（编译错误） | 完整解构 `{ host, port, user, auth }`；非 SSH 目标由 panic 改为返回错误 | §5.6 |
| 3 | 远端 `cd` 未处理路径中的空格，`cd D:\My Projects\app` 会拆词失败 | 路径用单引号包裹 + POSIX 单引号转义 | §5.6 |

**文档一致性修复（中等）**

| # | 问题 | 修复 | 位置 |
|---|------|------|------|
| 4 | 对外枚举（PascalCase）与 YAML 值（kebab-case）之间缺 serde rename 标注 | 全部补 `#[serde(rename_all = "kebab-case")]`，并在 §3 开头声明命名约定 | §3.2 / 3.3 / 3.4 / 3.5 / 3.6 / 4.2 |
| 5 | 代码使用 `ValidationIssue::Error/Warning` 但枚举未定义 | 补充定义 | §9.4 |
| 6 | `FailureAction` 在 §3.2 与 §13.2 重复定义 | §13.2 改为引用 §3.2 + 行为语义表 | §13.2 |
| 7 | tools.yaml 的 putty 条目（§3.4）与 `PuttyTransport` 自行探测（§5）脱节 | 明确：putty 条目由探测自动生成、仅供 UI 展示与版本核对；传输路径一律以 `detect_putty_tools()` 为准，不回读 tools.yaml | §3.4 / §5.3 |

**轻微修复**

| # | 问题 | 修复 | 位置 |
|---|------|------|------|
| 8 | §5.5 只校验 3 个核心文件，与 §5.4 的 6 文件捆绑清单不一致（puttygen.exe 丢失不会报错，密钥转换才会失败） | 校验覆盖全部 5 个 exe + VERSION 清单 | §5.5 |
| 9 | §9.1 项目树中 deploy 节点出现两次（笔误） | 修正为 deploy 下分 stag / prod 两个子节点 | §9.1 |
| 10 | §12.2 创建了 `cancel_rx` 但未展示如何分发到各步骤执行任务 | 补充 `cancel_tx.subscribe()` 每步骤一份 receiver 的说明 | §12.2 |

---

## 目录

1. [设计目标与非目标](#1-设计目标与非目标)
2. [总体架构](#2-总体架构)
3. [核心数据模型](#3-核心数据模型)
4. [执行引擎](#4-执行引擎)
5. [传输层与 PuTTY 捆绑分发](#5-传输层与-putty-捆绑分发)
6. [变量系统](#6-变量系统)
7. [首次启动与环境探测](#7-首次启动与环境探测)
8. [k8s 支持](#8-k8s-支持)
9. [UI 设计](#9-ui-设计)
10. [工程与协作方式](#10-工程与协作方式)
11. [数据存储](#11-数据存储)
12. [并发与取消模型](#12-并发与取消模型)
13. [错误处理与重试](#13-错误处理与重试)
14. [安全策略](#14-安全策略)
15. [依赖清单](#15-依赖清单)
16. [工程目录结构](#16-工程目录结构)
17. [打包与分发](#17-打包与分发)
18. [实施路线](#18-实施路线)
19. [待确认问题](#19-待确认问题)

---

## 1. 设计目标与非目标

### 1.1 目标

- 一个 Windows 桌面程序，覆盖"下载 → 编译 → 打包 → 上传 → 部署"整条链路
- 作业由**有序步骤链**组成，每步 = 选工具 + 写命令，不做插件化、不做 DSL
- 支持 Project → Job → Step 三级组织，可工程化分组
- 支持本地执行与远端执行（通过 PuTTY 套件）
- 支持多环境（staging / prod）通过变量切换
- 支持 k8s 部署（kubectl / helm 作为工具，Wait / K8sApply 步骤类型）
- 首次启动自动探测开发环境变量，开箱即用
- 配置 YAML 化，Git 管理，团队共享

### 1.2 非目标

- 不做 Web UI、不做前后端分离
- 不做中心服务端、不做任务队列
- 不做用户权限模型、不做审批流
- 不做制品仓库（Nexus/Artifactory 集成留 P3）
- 不做插件系统（WASM 插件留 P3）
- 不做跨平台客户端（P0 只做 Windows，Linux/macOS 留 P2）

---

## 2. 总体架构

```
┌─────────────────────────────────────────────────────────────┐
│                 eframe + egui (Windows 单进程)                │
│                                                             │
│  ┌──────────┬──────────────────────────────────────────────┐│
│  │ Project  │  Job Editor                                  ││
│  │ Tree     │  (步骤列表 + 工具选择 + 命令输入)              ││
│  │          │  + Tool Config（折叠）                        ││
│  │ payment  │  + Target Config（折叠）                      ││
│  │  ├build  │  + Variable Preview（折叠）                   ││
│  │  ├test   │                                              ││
│  │  ├deploy │  [▶ Run] [⏹ Stop] [💾 Save]                  ││
│  │          │                                              ││
│  │ frontend │  Step 1: [git] git clone ...           ✅    ││
│  │  ├build  │  Step 2: [mvn] mvn clean package ...  ✅    ││
│  │  └deploy │  Step 3: [kubectl] kubectl apply ...  ✅    ││
│  │          │  Step 4: [kubectl] wait rollout ...    ✅    ││
│  │ shared   │  [+ Add Step ▼]                              ││
│  │  └release│                                              ││
│  └──────────┴──────────────────────────────────────────────┘│
│  ┌──────────────────────────────────────────────────────────┐│
│  │ Console Output (实时流式)                      [📋 Copy]  ││
│  │ $ git clone https://xxx .                                ││
│  │ Cloning into '.'...                                      ││
│  │ $ mvn clean package -DskipTests                          ││
│  │ [INFO] BUILD SUCCESS                                     ││
│  │ $ kubectl apply -f k8s/                                  ││
│  │ deployment.apps/myapp configured                         ││
│  │ $ kubectl rollout status deployment/myapp                ││
│  │ waiting for rollout to finish...                          ││
│  │ deployment "myapp" successfully rolled out ✅              ││
│  └──────────────────────────────────────────────────────────┘│
└─────────────────────────────────────────────────────────────┘
         │
         ├── plink.exe ──────→ 远端 Linux 构建机（执行命令）
         ├── pscp.exe ───────→ 远端 Linux 构建机（传文件）
         ├── Local ──────────→ 本机执行 git / mvn / docker / kubectl
         └── kubectl ────────→ k8s cluster（通过本地 kubeconfig）
```

### 数据流

```
YAML 配置文件
    ↓ serde_yaml 反序列化
Project / Job / Step / Tool / Target
    ↓ UI 编辑
用户点 Run
    ↓
变量解析（${{ variables.xxx }} 替换）
    ↓
Engine::run_job (async task, tokio)
    ↓ 按 step_type 分发
├── exec     → LocalExecutor 或 PuttyTransport::exec
├── upload   → PuttyTransport::upload
├── wait     → 轮询执行 check command
└── k8s-apply → kubectl apply + watch 资源状态
    ↓
LogEvent 流 (mpsc)
    ↓
Console UI 实时渲染 + 日志文件落盘
    ↓
JobStatus → SQLite 记录（P1）
```

---

## 3. 核心数据模型

> **serde 命名约定（v0.4 起）**：struct 字段统一 snake_case（serde 默认，与 YAML 一致，无需标注）；
> 出现在 YAML 中的枚举值统一 kebab-case，必须显式标注 `#[serde(rename_all = "kebab-case")]`，
> 如 `StepType::K8sApply` ↔ `step_type: k8s-apply`、`FailureAction::Stop` ↔ `on_failure: stop`。
>
> **实施注记（serde_yaml 0.9）**：derive 枚举在 serde_yaml 0.9 下会输出 YAML tag（`step_type: !exec`），
> 与本文约定的纯字符串值（`step_type: exec`）不符。实现须为对外枚举手写
> Serialize/Deserialize（字符串形式），语义与 `rename_all = "kebab-case"` 等价；
> 含结构体变体的枚举（TargetType / AuthMethod）按"字符串（单元变体）/ 单键映射（结构体变体）"
> 双形态实现。已在 P0 代码 models.rs 落地并以回归测试锁定。

### 3.1 Project

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub name: String,
    pub description: Option<String>,
    /// 本地代码目录（UI 输入框绑定）
    pub local_path: String,
    /// 环境 → 变量表
    pub variables: HashMap<String, EnvironmentVariables>,
    /// 项目级工具覆盖：键相同整体覆盖全局 tools.yaml 条目（§6.5），
    /// 用于 pnpm/yarn 等工程内工具链的按项目固定
    pub tools: HashMap<String, Tool>,
    /// 关联的 job 文件名列表
    pub jobs: Vec<String>,
    /// 项目级目标定义（键 = 目标名，§3.5）：不同项目的 prod 可以是不同服务器；
    /// 与全局 targets.yaml 合并时项目同名覆盖全局
    pub targets: HashMap<String, Target>,
    /// 可选：加载额外 .env 文件
    pub env_file: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvironmentVariables {
    /// flatten：YAML 中直接展开键值对（没有 vars 包装层），
    /// 与 project.yaml 的 variables: staging: {REGISTRY: ...} 结构一一对应。
    /// 若不加 flatten，serde 会要求 YAML 写成 staging: { vars: {...} }，与示例不符。
    #[serde(flatten)]
    pub vars: HashMap<String, String>,
}
```

对应 YAML：

```yaml
# projects/payment-service/project.yaml
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
tools: {}          # 项目级工具覆盖，键相同整体覆盖 tools.yaml（见 §6.5）
jobs:
  - build
  - test
  - deploy-staging
  - deploy-prod
targets:
  local:      { target_type: local }
  prod:       # 项目自己的 prod：不同项目的 prod 可以是不同服务器（§3.5）
    target_type:
      ssh:
        host: 192.168.40.198
        port: 22
        user: deployer
        auth: password
    work_root: /opt/yudao/prod
env_file: null
```

### 3.2 Job

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FailureAction {
    /// 失败立即停止（默认，YAML: stop）
    Stop,
    /// 跳过当前步骤继续（YAML: continue）
    Continue,
    /// 重试整个 job（YAML: retry）
    Retry,
}
```

对应 YAML：

```yaml
# projects/payment-service/build.yaml
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
```

### 3.3 Step

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    /// step_type=k8s-apply 时有效
    pub k8s_watch: Option<Vec<K8sWatchTarget>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StepType {
    /// 执行命令，退出即走（YAML: exec）
    Exec,
    /// 轮询等条件满足（YAML: wait）
    Wait,
    /// 文件传输（pscp）（YAML: upload）
    Upload,
    /// 打包为 zip（本地操作，供后续 upload 单文件上传；YAML: pack，见 §5.8）
    Pack,
    /// kubectl apply + 自动 watch 资源状态（YAML: k8s-apply）
    K8sApply,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetryConfig {
    pub max_attempts: u32,
    pub backoff_seconds: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct K8sWatchTarget {
    pub kind: String,        // Deployment / StatefulSet / Service / Pod
    pub name: String,
    pub namespace: Option<String>,
    pub min_ready: Option<u32>,   // 最少 ready 副本数
}
```

### 3.4 Tool

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ToolType {
    /// 系统已装，PATH 里能找到（YAML: system）
    System,
    /// 需要下载解压（YAML: archive）
    Archive,
    /// 自定义路径（YAML: custom）
    Custom,
    /// 一段脚本当工具（YAML: script）
    Script,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolInstall {
    pub url: String,
    pub extract_to: String,
    pub checksum: Option<String>,
}
```

对应 YAML：

```yaml
# ~/.remote-build/tools.yaml
tools:
  java:
    name: java
    tool_type: system
    command: java
    install_path: null
    version: "17.0.2"
    version_check: "java -version"
    detected_from: JAVA_HOME
    install: null
    env:
      JAVA_HOME: "C:/Program Files/Java/jdk-17.0.2"

  maven:
    name: maven
    tool_type: system
    command: mvn
    version: "3.9.6"
    detected_from: MAVEN_HOME
    env: {}

  # putty 条目由首次启动探测自动生成（见 §5.3），仅供 UI 展示与版本核对；
  # PuttyTransport 实际使用的路径以 detect_putty_tools() 结果为准，不回读本条目
  putty:
    name: putty
    tool_type: custom
    command: C:\Users\dev\.remote-build\tools\putty\pscp.exe
    install_path: C:\Users\dev\.remote-build\tools\putty
    version: "0.79"
    detected_from: builtin
    env: {}

  kubectl:
    name: kubectl
    tool_type: system
    command: kubectl
    detected_from: PATH
    env: {}
```

### 3.5 Target

> **项目级目标（v0.4 实施调整）**：目标定义可直接写在 project.yaml 的 `targets:` 表
> （键 = 目标名），**与全局 targets.yaml 合并时项目同名覆盖全局**——
> 不同项目的 `prod` 各指各的服务器。SSH 密码按 `{项目}/{目标}` 为键存入
> Windows Credential Manager（全局目标为 `global/{目标}`），互不干扰。
> 🎯 目标管理窗口提供「全局 / 本项目」双作用域编辑与连接测试。

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Target {
    pub name: String,
    pub target_type: TargetType,
    /// 远端工作目录
    pub work_root: Option<String>,
    pub env: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TargetType {
    // YAML 外部标签形式：target_type: { ssh: { host: ..., port: ... } }
    // 单元变体序列化为字符串：target_type: local
    Ssh {
        host: String,
        port: u16,
        user: String,
        auth: AuthMethod,
    },
    Kubernetes {
        kubeconfig: String,
        context: Option<String>,
        namespace: Option<String>,
        registry: Option<String>,
    },
    /// 本机执行（YAML: local）
    Local,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AuthMethod {
    // YAML: auth: { key: { path: ... } }
    Key { path: String },
    Password { encrypted: String },
    Agent,           // pageant / ssh-agent（YAML: agent）
    None,            // YAML: none
}
```

对应 YAML：

```yaml
# ~/.remote-build/targets.yaml
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
    work_root: null
    env: {}

  local:
    name: local
    target_type: local
    work_root: null
    env: {}
```

### 3.6 全局配置

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PuttyConfig {
    /// true = 用内置版本
    pub use_builtin: bool,
    /// 自定义路径，use_builtin=false 时生效
    pub custom_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Theme {
    Light,
    Dark,
    System,
}
```

### 3.7 变量文件

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VariablesFile {
    pub environments: HashMap<String, HashMap<String, String>>,
    /// secrets 加密存储路径
    pub secrets: Option<String>,
}
```

---

## 4. 执行引擎

### 4.1 执行上下文

```rust
pub struct ExecutionContext {
    pub job: Job,
    pub project: Project,
    pub target: Option<Target>,
    pub resolved_tools: HashMap<String, Tool>,
    /// 已解析的变量（环境 + 步骤产出）
    pub variables: HashMap<String, String>,
    /// 本次运行的工作目录（解析自 project.local_path）
    pub workspace: PathBuf,
    pub run_id: String,
}
```

### 4.2 日志事件流

```rust
/// 内部日志事件（仅进程内 mpsc 传输，不序列化到 YAML，无 rename 约束）
#[derive(Debug, Clone)]
pub enum LogEvent {
    StepStart { step_index: usize, name: String },
    Stdout { step_index: usize, line: String },
    Stderr { step_index: usize, line: String },
    StepEnd { step_index: usize, exit_code: i32, duration_ms: u64 },
    JobEnd { run_id: String, status: JobStatus, total_duration_ms: u64 },
    Error { message: String },
}

/// 落 SQLite 时按 kebab-case 转字符串：running / success / failed / cancelled
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum JobStatus {
    Running,
    Success,
    Failed { step_index: usize, reason: String },
    Cancelled,
    Timeout,
}
```

### 4.3 引擎主循环

```
Job 启动
  ↓
解析变量（environment → 选 staging/prod → 替换 ${{ variables.xxx }}）
  ↓
创建 ExecutionContext（workspace = project.local_path 解析后）
  ↓
for step in steps:
  if !enabled → skip
  if condition → 求值，false → skip
  ↓
  按 step_type 分发:
  ├── exec      → LocalExecutor 或 RemoteTransport::exec
  ├── wait      → 轮询执行 check command 直到成功或超时
  ├── upload    → RemoteTransport::upload
  └── k8s-apply → kubectl apply + watch 资源
  ↓
  超时 → tokio::time::timeout 包裹
  重试 → retry 配置控制
  取消 → broadcast channel 信号
  ↓
  输出 → mpsc 流式推给 UI + 落日志文件
  ↓
on_failure 策略:
  stop    → 终止整个 Job
  continue → 跳过当前步骤
  retry   → 重试整个 Job
```

### 4.4 步骤结果

```rust
#[derive(Debug)]
enum StepResult {
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
```

### 4.5 工作目录解析

```rust
impl Job {
    /// 解析步骤的实际工作目录
    pub fn resolve_working_dir(&self, project: &Project) -> PathBuf {
        let base = PathBuf::from(&project.local_path);

        // job 级 working_dir
        let base = if let Some(ref wd) = self.working_dir {
            base.join(wd)
        } else {
            base
        };

        base
    }
}

impl Step {
    /// 在 job 工作目录基础上再叠加 step 级 working_dir
    pub fn resolve_working_dir(&self, job_wd: &Path) -> PathBuf {
        if let Some(ref wd) = self.working_dir {
            job_wd.join(wd)
        } else {
            job_wd.to_path_buf()
        }
    }
}
```

---

## 5. 传输层与 PuTTY 捆绑分发

### 5.1 传输抽象

```rust
/// 日志行流向
pub enum Stream { Stdout, Stderr }

/// 日志接收器：传输层把每行输出推给它，由实现方转发到 LogEvent 流（§4.2）
pub trait LogSink: Send {
    fn on_line(&mut self, step_index: usize, stream: Stream, line: String);
}

#[async_trait]
pub trait RemoteTransport: Send + Sync {
    async fn exec(
        &self,
        target: &Target,
        command: &str,
        working_dir: &str,
        env: &HashMap<String, String>,
        sink: &mut dyn LogSink,
    ) -> anyhow::Result<ExitStatus>;

    async fn upload(&self, target: &Target, local: &Path, remote: &str) -> anyhow::Result<()>;
    async fn download(&self, target: &Target, remote: &str, local: &Path) -> anyhow::Result<()>;
}

#[async_trait]
pub trait LocalExecutor: Send + Sync {
    async fn exec(
        &self,
        command: &str,
        working_dir: &Path,
        env: &HashMap<String, String>,
        sink: &mut dyn LogSink,
    ) -> anyhow::Result<ExitStatus>;
}
```

实现计划：

| 实现 | 状态 | 说明 |
|------|------|------|
| `PuttyTransport` | **P0 主传输** | 调 plink / pscp / psftp |
| `OpenSshTransport` | P1 可选 | 调系统 ssh / scp / sftp |
| `RusshTransport` | P2 预留 | 纯 Rust SSH，做高级后端 |

### 5.2 为什么 P0 选 PuTTY

| 维度 | russh (纯 Rust) | PuTTY 套件 |
|------|----------------|-------------|
| 可靠性 | 边缘 case 多（密钥格式、代理转发、跳板机） | 20 年老牌，啥环境都跑过 |
| 部署 | 编译进二进制 | 开发机基本都有，没装就用内置 |
| 跳板机 / ProxyJump | 要自己实现 | PuTTY 自带 proxy 配置 |
| 密钥 | 要处理 OpenSSH / PuTTY 两种格式 | Pageant 一把梭 |
| 维护成本 | 跟 russh bug | 零维护，调命令行就完事 |
| 进程开销 | 无 | 每次 ~200ms，构建任务无感知 |

### 5.3 PuTTY 查找优先级

```rust
pub fn detect_putty_tools() -> PuttyDetectionResult {
    // 优先级：用户自定义 → 内置捆绑 → 系统 PATH → 常见安装路径
    let sources = vec![
        ("custom",   check_custom_path()),
        ("builtin",  check_builtin()),
        ("PATH",     which("plink")),
        ("Program Files",       search_in("C:/Program Files/PuTTY", "plink.exe")),
        ("Program Files (x86)", search_in("C:/Program Files (x86)/PuTTY", "plink.exe")),
    ];

    for (source, path) in sources {
        if let Some(p) = path {
            return PuttyDetectionResult {
                found: true,
                source: source.to_string(),
                plink: p.join("plink.exe"),
                pscp:  p.join("pscp.exe"),
                psftp: p.join("psftp.exe"),
                pageant: p.join("pageant.exe").filter(|x| x.exists()),
            };
        }
    }
    // ... not found
}
```

> **与 tools.yaml 的关系（v0.4 澄清）**：`tools.yaml` 里的 `putty` 条目（§3.4）由本函数的探测结果**自动生成**，仅供 UI 展示与版本核对；`PuttyTransport` 实际使用的路径一律以本函数返回值为准，**不回读 tools.yaml**，避免两处配置漂移。

内置目录定位：

```rust
fn get_builtin_putty_dir() -> Option<PathBuf> {
    // 1. 开发模式：CARGO_MANIFEST_DIR/tools/putty/
    // 2. 安装后：exe 同目录/tools/putty/
    // 3. 兜底：~/.remote-build/tools/putty/
}
```

### 5.4 捆绑文件清单

```
tools/putty/
├── plink.exe       # 远端命令执行
├── pscp.exe        # 文件上传/下载
├── psftp.exe       # SFTP 传输
├── puttygen.exe    # 密钥转换（ppk ↔ pem）
├── pageant.exe     # SSH agent（可选启动）
└── VERSION         # 版本号 + sha256
```

### 5.5 完整性校验

```rust
pub fn verify_putty_integrity(putty_dir: &Path) -> Result<(), String> {
    // 与 §5.4 捆绑清单一一对应：5 个 exe + VERSION 清单，缺一不可。
    // puttygen.exe（密钥转换）、pageant.exe（agent 认证）虽不在传输主链路上，
    // 缺失也必须在启动校验期暴露，而不是等到运行期密钥转换/agent 认证时才失败。
    const REQUIRED_EXES: [&str; 5] = [
        "plink.exe",
        "pscp.exe",
        "psftp.exe",
        "puttygen.exe",
        "pageant.exe",
    ];
    for tool in REQUIRED_EXES {
        let p = putty_dir.join(tool);
        if !p.exists() {
            return Err(format!("{} missing", tool));
        }
        let meta = std::fs::metadata(&p).map_err(|e| e.to_string())?;
        if meta.len() < 100_000 {
            return Err(format!("{} appears corrupted (too small)", tool));
        }
    }
    // VERSION 清单记录各 exe 的 sha256；P0 校验存在性，P1 做逐文件哈希比对
    if !putty_dir.join("VERSION").exists() {
        return Err("VERSION manifest missing".to_string());
    }
    Ok(())
}
```

### 5.6 PuttyTransport 关键实现

```rust
pub struct PuttyTransport {
    pub plink: PathBuf,
    pub pscp: PathBuf,
    pub psftp: PathBuf,
}

#[async_trait]
impl RemoteTransport for PuttyTransport {
    async fn exec(
        &self,
        target: &Target,
        command: &str,
        working_dir: &str,
        env: &HashMap<String, String>,
        sink: &mut dyn LogSink,
    ) -> anyhow::Result<ExitStatus> {
        let (dest, args_prefix) = self.build_ssh_args(target)?;

        let full_command = if working_dir.is_empty() {
            command.to_string()
        } else {
            // 远端是 Linux shell：cd 路径用单引号包裹，避免路径含空格时被拆词；
            // 路径本身含单引号时按 POSIX 规则转义为 '\''
            format!("cd '{}' && {}", escape_posix_single_quote(working_dir), command)
        };

        let mut args = args_prefix;
        args.push(dest);
        args.push(full_command);

        let mut cmd = Command::new(&self.plink);
        cmd.args(&args);
        for (k, v) in env {
            cmd.env(k, v);
        }
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());

        let mut child = cmd.spawn()?;

        // stdout / stderr 双 task 并发读，通过 mpsc 转发给 sink
        // ...（流式读取代码，见 4.2 LogEvent 流）
        // ...（取消信号通过 child.kill()）

        let status = child.wait().await?;
        Ok(ExitStatus {
            code: status.code().unwrap_or(-1),
            success: status.success(),
        })
    }

    async fn upload(&self, target: &Target, local: &Path, remote: &str) -> anyhow::Result<()> {
        let (dest, mut args) = self.build_ssh_args(target)?;
        args.push("-r".to_string());  // 递归
        args.push("-p".to_string());  // 保持权限
        args.push(local.to_string_lossy().to_string());
        args.push(format!("{}:{}", dest, remote));

        let status = Command::new(&self.pscp).args(&args).status().await?;
        if !status.success() {
            anyhow::bail!("pscp upload failed with code {:?}", status.code());
        }
        Ok(())
    }

    async fn download(&self, target: &Target, remote: &str, local: &Path) -> anyhow::Result<()> {
        let (dest, mut args) = self.build_ssh_args(target)?;
        args.push(format!("{}:{}", dest, remote));
        args.push(local.to_string_lossy().to_string());

        let status = Command::new(&self.pscp).args(&args).status().await?;
        if !status.success() {
            anyhow::bail!("pscp download failed with code {:?}", status.code());
        }
        Ok(())
    }
}

impl PuttyTransport {
    /// 解构出 host / port / user / auth，拼 plink / pscp 公共参数。
    /// 注意四个字段必须完整解构——用 `..` 省略会导致 host/user 不在作用域（编译错误）。
    /// 非 SSH 目标返回错误，由上层按 StepResult::Failed 处理，不 panic。
    fn build_ssh_args(&self, target: &Target) -> anyhow::Result<(String, Vec<String>)> {
        let mut args = vec!["-batch".to_string()];

        match &target.target_type {
            TargetType::Ssh { host, port, user, auth } => {
                args.push("-P".to_string());
                args.push(port.to_string());

                if let AuthMethod::Key { path } = auth {
                    args.push("-i".to_string());
                    args.push(path.clone());
                }
                // Agent / None 不需要额外参数

                let dest = format!("{}@{}", user, host);
                Ok((dest, args))
            }
            _ => anyhow::bail!("PuttyTransport called with non-SSH target"),
        }
    }
}

/// POSIX 单引号转义：' → '\''
fn escape_posix_single_quote(s: &str) -> String {
    s.replace('\'', "'\\''")
}
```

### 5.7 安全红线

- **密码认证（v0.4 实施调整）**：允许密码——但密码必须存 **Windows Credential Manager**
  （keyring，按目标名），**绝不落 targets.yaml**；执行时经 plink `-pw` 传递，
  本机进程列表存在短暂暴露窗口。内网研发工具可接受；面向外网 / 高安全场景请改用
  key 或 Pageant（agent）。
- **禁止把密码写进任何配置文件或日志**（日志输出统一脱敏）。
- **host key（v0.4 实施调整）**：支持**指纹固定（TOFU）**——首次连接测试失败时输出会给出
  服务器指纹（SHA256:...），把它填入目标配置的「主机键指纹」（host_key）字段即完成信任，
  执行时经 plink `-hostkey` 校验（比注册表缓存更严格、可入库审计）；也可在命令行手动
  `echo y | plink -ssh user@host "exit"` 缓存一次。工具**不自动信任**任何主机键。

### 5.8 打包步骤（step_type: pack，v0.4 实施补充）

上传大量小文件前先打成一个 zip，pscp 单文件传输显著更快。

命令格式与 upload 一致（`源 -> 目标` 约定），路径相对 workspace 解析：

```
target/ -> dist/app.zip          # 目录：递归打包，条目带 target/ 根（同 zip -r 习惯）
target/*.jar -> dist/jars.zip    # 通配：仅匹配文件，条目为文件名
src/config.yml -> dist/cfg.zip   # 单文件
```

- 引擎内置 Rust zip 实现（deflate），**不依赖系统 zip / 7z**；
- pack 是纯本地操作，与 target 无关；典型链路：**pack → upload**（单文件上传）；
- 目标必须以 `.zip` 结尾，父目录自动创建；文件清单先收集后写包，目标在源目录内也不会自吞。

### 5.9 上传映射与发布（v0.4 实施澄清）

**upload 支持多行映射**（微服务多 jar 场景），每行一条 `本地 -> 远端`，
空行与 `#` 注释忽略；相对路径的远端部分自动拼在 target 的 `work_root` 之后：

```
target/user-service.jar  -> /opt/app/user-service.jar
target/order-service.jar -> /opt/app/order-service.jar
target/config/           -> /opt/app/config/
```

**推荐的发布形态**：发布逻辑放服务器上的 `deploy.sh`（随代码库版本化，可做
备份 / 停服 / 替换 / 起服 / 健康检查），流程里两步完成发布：

```
upload  deploy.sh        -> /opt/app/deploy.sh
exec     sh /opt/app/deploy.sh   # SSH target 的 exec 即 plink 远端执行
```

比在流程 YAML 里堆一串远程命令更好维护；带参数时用变量替换

### 5.10 上传可靠性（v0.4 实施补充）

网络（尤其 VPN 内网）是上传最大的不稳定源，保证策略按层次：

1. **自动重试**：upload 步骤未显式配置 retry 时默认重试 1 次（共 2 次尝试，退避 3s）；
   需要更多在步骤上配置 retry（次数 + 退避间隔，UI 可编辑）。
2. **上传后 sha256 校验**：单文件上传完成即比对本地与远端（plink 执行 `sha256sum`）哈希，
   不一致判失败 → 按重试配置自动重传。**"传对了"由校验保证，而不是靠感觉。**
   目录递归上传跳过校验（建议先 pack 成单文件）。
3. **大文件先 pack**：单文件传输比目录树稳定（见 §5.8）。
4. **超时按大小预估**：upload 步骤 timeout 默认 300s，大文件请显式调大。
5. **断点续传**：pscp 不支持；zip 化后单文件场景收益低。真有超大文件续传需求，
   P1 可用 psftp 的 reput 脚本模式实现。
6. **VPN / 内网**：先 `test-conn` 验证可达再跑流程（host_key 指纹固定不受影响）；
   频繁断线的环境考虑改为推中间制品库（Nexus / OSS）由服务器拉取（架构级，P2）。
7. **进度**：pscp 非 TTY 模式无进度输出；上传前后打印文件大小 +
   `[verify] sha256 一致 ✓` 让用户确认完成。真实进度条需额外连接轮询远端大小，P1 备选。
（如 `sh /opt/app/deploy.sh ${{ env.VERSION }}`）。

---

## 6. 变量系统

### 6.1 变量来源（优先级从高到低）

```
步骤级 env (Step.env)
  > 全局 env (Target.env / Tool.env)
  > 环境变量表 (project.yaml variables[env_name])
  > 系统环境变量 (std::env::var)
```

### 6.2 替换语法

> 📄 速查表：[变量速查.md](./变量速查.md)（四种形态 + 条件表达式 + 完整示例，GUI 里也有「？ 变量语法」窗口）

```
${{ variables.REGISTRY }}     → 从 environment 变量表替换
${{ env.VERSION }}            → 从步骤产出 / 全局 env 替换
${{ targets.prod.host }}      → 从 target 配置替换
${VAR_NAME}                  → 从系统环境变量替换（兼容 shell 习惯）
```

### 6.3 解析时机

**执行前一次性替换**，不是运行时动态读。原因：命令字符串要能预览、能日志、能审计。

```rust
fn resolve_variables(&self, input: &str, vars: &HashMap<String, String>) -> String {
    let mut result = input.to_string();
    // ${{ variables.xxx }}
    // ${{ env.xxx }}
    // ${{ targets.xxx.yyy }}
    // ${VAR} (系统环境变量兜底)
    result
}
```

### 6.4 步骤间变量传递

```rust
// StepResult::Success 携带 output_vars
// 引擎合并进 ctx.variables
for (k, v) in result.output_vars {
    ctx.variables.insert(k, v);
}
```

示例：步骤里 `echo "VERSION=1.0.0" >> $WORKSPACE/.env`，下一步读取。P0 先做文件约定，P1 做引擎自动捕获（正则匹配 `KEY=VALUE` 输出行）。

### 6.5 前端工程工具链与私服（v0.4 实施补充）

**机器级 vs 工程级工具的边界**：tools.yaml 只放机器级工具（mvn / node / git 这类，
探测自环境变量或全局安装）；pnpm / yarn / vite 这类**工程级工具链不注册全局**。

- **pnpm / yarn**：首选 corepack——`package.json` 写 `"packageManager": "pnpm@9.15.0"`
  钉版本，步骤命令直接写 `pnpm install`；个别项目要钉路径时用**项目级 tools 覆盖**
  （project.yaml 的 `tools:` 段，键相同整体覆盖全局条目，可带自己的 env）。
  **引擎自动 corepack（实施补充）**：检测到 `workspace/package.json` 含 `packageManager`
  字段且 corepack 可用时，本地步骤中以 pnpm / yarn 开头的命令**自动**经 corepack 执行——
  用户无需知道 corepack 存在，也不需要全局安装 pnpm；未钉版项目原样执行；
  用户已手写 corepack 前缀的命令不会二次包装。
- **工程内命令（vite / jest / tsc）**：引擎检测到 `<workspace>/node_modules/.bin`
  存在时自动把它**前置到步骤 PATH**，零配置可用；工程内版本优先于全局安装。
- **工具地址落地**：tools.yaml / 项目级 tools 的 `command` 若是完整路径，其所在目录
  自动前置到该步骤 PATH——步骤命令里的裸名（`mvn` / `node` / `git`）优先解析到
  tools.yaml 映射的那份，而不是系统 PATH 里的任意版本。
- **PATH 合并规则**：步骤 env 里的 PATH 不会被系统 PATH 顶掉——
  注入顺序 = 工具目录 → node_modules/.bin → 步骤/工具显式 PATH → 系统 PATH。

**私服地址（npm / pip / maven）**，按优先级：

1. 工程内配置文件：`.npmrc`（registry=）、`pip.conf`、maven `settings.xml`——
   命令在工程目录执行，天然生效，git 可管，**首选**；
2. 变量切换：`NPM_CONFIG_REGISTRY` / `PIP_INDEX_URL` 放进 project.yaml 的
   `variables.<env>`（staging / prod 各指一个私服），或步骤命令引用
   `${{ variables.NPM_REGISTRY }}`，环境一键切换；
3. 工具级 env：tools.yaml 或项目级 tools 的 `env` 段，如
   `python: { env: { PIP_INDEX_URL: https://nexus.internal/repository/pypi/simple } }`。

不建议把私服地址写死在命令参数里——审计和切换都差。
工具级 env（方案 3）对"python 这类既能当命令又需要私服参数"的工具最合适：
私服地址跟工具走，命令保持干净。

---

## 7. 首次启动与环境探测

### 7.1 流程

```
启动
  ↓
检查 ~/.remote-build/ 是否存在
  ├── 不存在 → 创建目录结构
  │            拷贝内置 tools/putty/ → ~/.remote-build/tools/putty/
  │            拷贝 config/*.template → ~/.remote-build/
  └── 存在 → 快速校验（跑一下 java -version 看还在不在）
  ↓
探测系统环境:
  ├── JAVA_HOME → java 路径 + 版本
  ├── MAVEN_HOME → mvn 路径 + 版本
  ├── GRADLE_HOME → gradle 路径 + 版本
  ├── NODE_HOME / NVM_HOME → node 路径
  ├── PATH → git, docker, kubectl, npm, python, python3
  └── PuTTY → 自定义 → 内置 → 系统 PATH → 常见路径（顺序同 §5.3，内置兜底一定有）
  ↓
展示探测结果页:
  ├── ✅ java  v17.0.2  (from JAVA_HOME)
  ├── ✅ mvn   v3.9.6   (from MAVEN_HOME)
  ├── ✅ git   v2.43.0  (from PATH)
  ├── ✅ node  v20.11.0 (from PATH)
  ├── ❌ gradle → [手动输入路径]
  └── ✅ putty v0.79    (builtin)
  ↓
确认 → 生成 tools.yaml / targets.yaml / config.yaml
  ↓
进入主界面
```

### 7.2 Windows 特殊处理

- `which` 不是内置命令，需手动扫 PATH 并拼接 `PATHEXT`（`.EXE;.BAT;.CMD;.PS1`）
- 多 JDK 共存：探测只拿 PATH 第一个，首次启动后允许用户选默认 JDK，或 job 级指定 `tool: java-17`
- NVM 装的 Node：`NODE_HOME` 未必设，`node` 在 PATH 里但指向 nvm shim，探测到 shim 路径即可
- 网络路径（Z:\tools\maven）：探测设超时，避免卡 UI

### 7.3 探测结果缓存

探测完存 `config.yaml`，下次启动只做快速校验（重跑 `java -version`），不在了才重新全量探测。

---

## 8. k8s 支持

### 8.1 核心结论

**k8s 不需要特殊对待。** 它只是步骤多一点、有些步骤要等、有些步骤依赖前面产出的变量。把 StepType 和 TargetType 做好，k8s 自然支持。

### 8.2 k8s 在系统里的映射

| k8s 操作 | 系统表达 |
|---------|---------|
| docker build / push | docker 步骤 |
| kubectl apply | kubectl 步骤（step_type: exec 或 k8s-apply） |
| helm upgrade | helm 步骤 |
| 等 rollout 完成 | step_type: wait |
| 多集群 | 不同 k8s target |
| 金丝雀 / 蓝绿 | 多个步骤 + wait |
| 验证 | curl / 跑测试脚本 |
| 回滚 | 加一个 job 专门跑 `kubectl rollout undo` |

### 8.3 K8sApply 步骤行为

```
1. 跑 kubectl apply -f <manifests>
2. 若配置了 k8s_watch，自动轮询每个声明的资源:
   kubectl get <kind> <name> -n <ns> -o jsonpath='{.status.readyReplicas}'
3. 每个资源都 Ready 了才算步骤成功
4. 超时或失败，自动 kubectl describe 拿事件输出到控制台
```

### 8.4 k8s target 的执行方式

走**本地 kubectl + kubeconfig**，不是 SSH 到某个机器再跑 kubectl。因为 kubectl 本身就是 C/S 架构，本地就能控制集群。执行时注入 `KUBECONFIG` 环境变量。

### 8.5 示例：完整 k8s 部署 job

```yaml
# projects/payment-service/deploy-prod.yaml
name: deploy-prod
project: payment-service
description: "生产环境 K8s 部署"
target: prod-k8s
environment: prod
timeout: 900
on_failure: stop
steps:
  - name: "docker build"
    tool: docker
    command: docker build -t ${{ variables.REGISTRY }}/myapp:${{ env.VERSION }} .
    step_type: exec

  - name: "docker push"
    tool: docker
    command: docker push ${{ variables.REGISTRY }}/myapp:${{ env.VERSION }}
    step_type: exec

  - name: "kubectl apply"
    tool: kubectl
    command: kubectl apply -f k8s/prod/ -n ${{ variables.NAMESPACE }}
    step_type: k8s-apply
    k8s_watch:
      - kind: Deployment
        name: myapp
        namespace: production
        min_ready: 2

  - name: "verify"
    tool: curl
    command: curl -f https://myapp.prod.internal/health
    step_type: exec
```

---

## 9. UI 设计

### 9.1 布局

```
┌──────────────────────────────────────────────────────────────────┐
│  工具栏: [+ 新建项目] [▶ Run] [⏹ Stop] [💾 Save] [⚙ 设置]       │
├───────────┬──────────────────────────────────────────────────────┤
│           │  📁 payment-service                    [⚙ 设置]     │
│ 📁 payment│  项目目录: [D:\workspace\payment-service] [📂]      │
│  ├ build  │                                                      │
│  ├ test   │  ── 作业列表 ──                                       │
│  ├ deploy │  ▶ build        mvn clean package                     │
│  │ ├ stag │  ▶ test         mvn test + sonar                      │
│  │ └ prod │  ▶ deploy-staging   build → push → kubectl apply      │
│           │  ▶ deploy-prod     build → push → kubectl apply      │
│           │                                                      │
│ 📁 frontend│  ── 步骤编辑器 ──                                     │
│  ├ build  │  Step 1: [maven ▼] [✅]                               │
│  └ deploy │  ┌──────────────────────────────────────────┐        │
│           │  │ mvn clean package -DskipTests            │        │
│ 📁 shared │  └──────────────────────────────────────────┘        │
│  └ release│  Step 2: [kubectl ▼] [✅]                            │
│           │  ┌──────────────────────────────────────────┐        │
│           │  │ kubectl apply -f k8s/prod/              │        │
│           │  └──────────────────────────────────────────┘        │
│           │  Step 3: [kubectl ▼] [✅]                            │
│           │  ┌──────────────────────────────────────────┐        │
│           │  │ kubectl rollout status deployment/myapp  │        │
│           │  │   --timeout=300s                         │        │
│           │  └──────────────────────────────────────────┘        │
│           │  [+ 添加步骤 ▼]                                       │
│           │                                                      │
│           │  ── 工具配置（折叠）──                                  │
│           │  maven: system | mvn | 3.9.6                          │
│           │  kubectl: system | kubectl | 1.29                     │
├───────────┴──────────────────────────────────────────────────────┤
│  控制台输出                                         [📋 复制]    │
│  $ mvn clean package -DskipTests                                 │
│  [INFO] Scanning for projects...                                  │
│  [INFO] Building myapp 1.0.0                                    │
│  [INFO] BUILD SUCCESS                                            │
│  $ kubectl apply -f k8s/prod/                                    │
│  deployment.apps/myapp configured                                │
│  $ kubectl rollout status deployment/myapp                       │
│  Waiting for rollout...                                           │
│  deployment "myapp" successfully rolled out ✅                    │
└──────────────────────────────────────────────────────────────────┘
```

### 9.2 项目设置弹窗

```
┌──────────────────────────────────────────────┐
│  项目设置: payment-service                    │
├──────────────────────────────────────────────┤
│  项目名称: [payment-service        ]          │
│  本地目录: [D:\workspace\payment-service] [📂]│
│  描述:     [支付核心服务...          ]          │
│                                              │
│  ── 环境 ──                                   │
│  staging:                                    │
│    REGISTRY  [registry-staging... ]          │
│    NAMESPACE [staging             ]          │
│  prod:                                       │
│    REGISTRY  [registry-prod...     ]          │
│    NAMESPACE [production          ]          │
│                                              │
│  ── 关联目标 ──                                │
│  ☑ build-server-01                           │
│  ☐ staging-k8s                               │
│  ☑ prod-k8s                                  │
│                                              │
│              [取消]  [保存]                    │
└──────────────────────────────────────────────┘
```

### 9.3 首次启动引导页

```
┌─────────────────────────────────────────────────────────────┐
│  工具栏: [+ 新建作业] [运行] [停止] [保存]                    │
├──────────┬──────────────────────────────────────────────────┤
│          │  作业: backend-deploy              [编辑配置]     │
│  backend │ ┌──────────────────────────────────────────────┐ │
│  frontend│ │ ① [git   ] git clone https://xxx .      ✅  │ │
│  deploy- │ ├──────────────────────────────────────────────┤ │
│  prod    │ │ ② [maven] mvn clean package -DskipTests ✅  │ │
│          │ ├──────────────────────────────────────────────┤ │
│          │ │ ③ [maven] mvn verify                     ❌  │ │
│          │ ├──────────────────────────────────────────────┤ │
│          │ │ ④ [putty] pscp target/*.jar user@host:/opt/✅│ │
│          │ ├──────────────────────────────────────────────┤ │
│          │ │ ⑤ [+ 添加步骤]                               │ │
│          │ └──────────────────────────────────────────────┘ │
│          │                                                  │
│          │  工具配置: [maven ▼] [npm ▼] [+ 添加工具]        │
│          │  ┌────────────────────────────────────────────┐ │
│          │  │ command: mvn                               │ │
│          │  │ type: system                               │ │
│          │  │ version_check: mvn -v                      │ │
│          │  └────────────────────────────────────────────┘ │
├──────────┴──────────────────────────────────────────────────┤
│  控制台输出                                    [清空] [导出] │
│  $ git clone https://xxx .                                  │
│  Cloning into '.'...                                        │
│  remote: Counting objects: 15420, done.                     │
│  $ mvn clean package -DskipTests                            │
│  [INFO] Scanning for projects...                            │
│  ...                                                        │
└──────────────────────────────────────────────────────────────┘
```

### 9.4 目录不存在时的提示

```
📁 payment-service ⚠ "目录不存在，点击设置"
```

加载项目时校验：

```rust
/// 项目校验问题（仅 UI 提示用，不序列化到 YAML）
#[derive(Debug, Clone)]
pub enum ValidationIssue {
    /// 阻断性问题：目录缺失、配置不完整，Run 前必须解决
    Error { field: String, message: String },
    /// 非阻断提醒：如目录存在但不是 git 仓库
    Warning { field: String, message: String },
}

pub fn validate_project(project: &Project) -> Vec<ValidationIssue> {
    let mut issues = Vec::new();
    let path = Path::new(&project.local_path);

    if !path.exists() {
        issues.push(ValidationIssue::Error {
            field: "local_path".to_string(),
            message: format!("目录不存在: {}，请检查路径或先 git clone", project.local_path),
        });
    } else if !path.join(".git").exists() {
        issues.push(ValidationIssue::Warning {
            field: "local_path".to_string(),
            message: "目录不是 git 仓库，部分步骤可能异常".to_string(),
        });
    }
    issues
}
```

---

## 10. 工程与协作方式

### 10.1 目录分离

```
~/.remote-build/                        ← 客户端根目录（配置仓库）
├── config.yaml                         # 全局设置
├── tools.yaml                          # 工具链（首次探测生成）
├── targets.yaml                        # SSH / k8s 目标
├── variables.yaml                      # 全局变量
├── projects/                           # 用户自己 git clone 配置仓库到这里
│   └── build-configs/                  #   git clone <repo> 到这
│       ├── payment-service/
│       │   ├── project.yaml            #   local_path 指向实际代码目录
│       │   ├── build.yaml
│       │   ├── test.yaml
│       │   └── deploy-prod.yaml
│       └── frontend-web/
│           ├── project.yaml
│           └── build.yaml
├── logs/                               # 运行日志
│   └── payment-service/
│       └── build-20240115-143022.log
└── data/                               # SQLite (P1)
    └── history.db

D:\workspace\payment-service\           ← project.yaml 里的 local_path
├── src/
├── pom.xml
└── .git/
```

**两层分离**：
- `~/.remote-build/projects/` → 构建配置（YAML，可 git 管理）
- `project.yaml` 里的 `local_path` → 实际代码目录（开发者自己的 workspace）

### 10.2 协作流程

```
开发者 A:
  git clone <config-repo> ~/.remote-build/projects/build-configs
  git pull（拿到别人加的作业）
  打开客户端 → 左边看到所有项目 → 选 job → 跑

开发者 B:
  加了新步骤 → 改了 deploy.yaml → git push
  群里说一声"deploy-prod 加了 k8s 健康检查"

开发者 C:
  git pull → 客户端自动加载新配置 → 直接能用
```

**不做自动 clone/pull**，理由：
- 开发者本来就要改配置、提 MR，自己 clone 才能 `git add/commit/push`
- 自动 pull 会覆盖本地未提交的修改，反而惹麻烦
- 少写一大堆 git 操作代码

客户端只做一件事：**知道配置目录在哪**（config.yaml 里配 `workspace_root`）。

### 10.3 本地执行 vs 远端执行的工作目录映射

待确认（见第 19 节问题 1）。

---

## 11. 数据存储

### 11.1 三层分离

| 数据 | 存储 | 说明 |
|------|------|------|
| 配置（人写的、版本控制的） | YAML 文件 | 可 git 跟踪、可 diff、可手动改 |
| 运行历史（机器生成的） | SQLite（P1） | 查询、趋势 |
| 日志原文（大文本、流式写） | 文件 | SQLite 只存路径 |

**配置本身永远不进 SQLite**——配置文件要能 git 管理、能 diff、能手动改、能复制给别人。

### 11.2 SQLite 表结构（P1）

```sql
runs (
    id INTEGER PRIMARY KEY,
    job_name TEXT NOT NULL,
    project_name TEXT NOT NULL,
    target TEXT,
    started_at DATETIME NOT NULL,
    finished_at DATETIME,
    status TEXT NOT NULL,         -- running / success / failed / cancelled
    exit_code INTEGER,
    trigger_type TEXT,            -- manual / api / scheduled
    config_snapshot TEXT NOT NULL  -- JSON，记录当时用的配置版本
)

step_runs (
    id INTEGER PRIMARY KEY,
    run_id INTEGER NOT NULL,
    step_index INTEGER NOT NULL,
    step_name TEXT,
    tool TEXT,
    command TEXT,
    started_at DATETIME,
    finished_at DATETIME,
    status TEXT,
    exit_code INTEGER,
    output_log_path TEXT,
    FOREIGN KEY (run_id) REFERENCES runs(id)
)
```

`config_snapshot` 是关键字段——三个月后排查问题能看到当时跑的是 JDK 17 还是 21。

### 11.3 实施节奏

| 阶段 | 存储 |
|------|------|
| P0 | YAML 文件 + 日志文件 |
| P1 | + SQLite 历史 |

---

## 12. 并发与取消模型

### 12.1 并发

- P0 单 job 串行执行步骤，UI 允许同时启动多个 job（上限 `max_concurrent_jobs`，默认 3）
- 每个 job 一个独立 tokio task
- P1 支持步骤级并行（标记 `parallel: true` 的步骤组内并发）

### 12.2 取消

```rust
// 每个 job 一个 broadcast sender；UI Stop 按钮 / job 超时向它发送取消信号
let (cancel_tx, _) = tokio::sync::broadcast::channel(4);

// 每个步骤执行任务启动前，各自 subscribe 一份独立的 cancel_rx。
// broadcast 的 Receiver 不可 clone 共享，必须每个任务单独 subscribe 一份，
// 这样广播消息才能同时送达所有在跑的步骤任务。
let cancel_rx = cancel_tx.subscribe();

// 步骤执行任务内部 select! 监听自己的 cancel_rx
tokio::select! {
    status = child.wait() => { /* 正常完成 */ }
    _ = cancel_rx.recv() => {
        let _ = child.kill().await;
        return StepResult::Cancelled;
    }
}

// 触发取消（UI Stop 按钮回调 / job 超时处理）：
// cancel_tx.send(()).ok();
// broadcast 语义：所有在跑的步骤任务同时收到，各自 kill 自己的子进程
```

### 12.3 超时

```rust
tokio::time::timeout(
    tokio::time::Duration::from_secs(step.timeout.unwrap_or(300)),
    self.execute_step(...)
).await
```

---

## 13. 错误处理与重试

### 13.1 重试策略

```rust
// 步骤级 retry 配置
retry:
  max_attempts: 3
  backoff_seconds: 5
```

### 13.2 失败策略

`FailureAction` 的定义见 §3.2（`#[serde(rename_all = "kebab-case")]`，YAML 值为 stop / continue / retry），此处不再重复贴代码。三种取值的执行语义：

| 值 | 行为 |
|----|------|
| stop | 失败立即终止整个 job（默认） |
| continue | 记录失败原因，跳过当前步骤，继续执行后续步骤 |
| retry | 按 §13.1 的 retry 配置重试整个 job（P1） |

### 13.3 条件执行

```rust
pub condition: Option<String>;  // 如 ${{ env.VERSION != '' }}
```

P0 只支持非空检查，P1 做完整表达式引擎。

---

## 14. 安全策略

| 项 | 措施 |
|----|------|
| 密码存储 | Windows Credential Manager（`keyring` crate），不落明文 YAML |
| 密码传输 | **禁止 `-pw` 明文传**，用 key 或 Pageant |
| 工具校验和 | `ToolInstall.checksum` 必填，下载后校验 |
| host key | 首次连接需用户确认，不自动信任 |
| 危险命令静态检查 | P1：CI 校验阶段拦截 `rm -rf /`、`chmod 777` 等 |
| secrets 文件 | P1：加密存储，运行时解密到内存 |

---

## 15. 依赖清单

```toml
[package]
name = "remote-build"
version = "0.1.0"
edition = "2021"

[package.metadata.bundle]
resources = ["tools/", "config/"]

[build-dependencies]
# 编译时校验 putty 文件存在

[dependencies]
# 渲染后端注记（v0.4 实施补充）：默认 glow 在 RDP/远程会话下只有 GDI OpenGL 1.1，
# 无法满足 GL2 要求；P0 实现采用 wgpu（DX12/Vulkan），本地与远程桌面均可用
eframe = { version = "0.27", default-features = false, features = [
    "accesskit", "default_fonts", "wgpu",
] }
egui = "0.27"
serde = { version = "1.0", features = ["derive"] }
serde_yaml = "0.9"
tokio = { version = "1", features = ["full"] }
async-trait = "0.1"
thiserror = "1.0"
anyhow = "1.0"
dirs = "5.0"
whoami = "1.4"
chrono = { version = "0.4", features = ["serde"] }
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
sha2 = "0.10"                    # 工具校验和
keyring = "2.0"                  # Windows Credential Manager
winapi = { version = "0.3", features = ["shellapi", "winuser"] }  # 快捷方式/通知
sqlx = { version = "0.7", features = ["sqlite", "runtime-tokio"], optional = true }  # P1 运行历史

[features]
default = []
sqlite = ["dep:sqlx"]
```

---

## 16. 工程目录结构

```
remote-build/
├── Cargo.toml
├── build.rs                         # 编译时校验 tools/putty/*.exe
├── src/
│   ├── main.rs                      # eframe 入口
│   ├── app.rs                       # egui App 主体 + 屏幕路由
│   ├── models.rs                    # 所有数据结构
│   ├── engine.rs                    # 执行引擎
│   ├── env_detect.rs                # 首次启动环境探测
│   ├── config.rs                    # YAML 读写 / 校验
│   ├── variables.rs                 # 变量解析器
│   ├── transport/
│   │   ├── mod.rs                   # RemoteTransport trait + LogSink
│   │   ├── putty.rs                 # PuTTY 套件调用（P0 主传输）
│   │   ├── openssh.rs               # 系统 OpenSSH 调用（P1 可选）
│   │   └── russh.rs                 # P2 预留（纯 Rust SSH）
│   ├── k8s.rs                       # kubectl 封装 / k8s_watch 逻辑
│   ├── ui/
│   │   ├── first_run.rs             # 首次启动引导页
│   │   ├── project_tree.rs          # 左侧项目/作业树
│   │   ├── job_editor.rs            # 中间步骤编辑器
│   │   ├── console.rs               # 底部控制台
│   │   └── settings.rs              # 工具/目标/环境变量配置页
│   └── storage/
│       ├── files.rs                 # YAML 文件 CRUD
│       └── sqlite.rs                # P1: 运行历史
├── tools/                           # 捆绑的工具
│   └── putty/
│       ├── plink.exe
│       ├── pscp.exe
│       ├── psftp.exe
│       ├── puttygen.exe
│       ├── pageant.exe
│       └── VERSION
├── config/                          # 默认配置模板（首次启动拷贝）
│   ├── tools.yaml.template
│   ├── targets.yaml.template
│   └── config.yaml.template
├── projects/                        # 示例项目
│   └── example/
│       ├── project.yaml
│       └── build.yaml
└── README.md
```

---

## 17. 打包与分发

### 17.1 策略

用 `cargo wix` 或 `cargo-dist` 生成 MSI / EXE 安装包。

### 17.2 安装后目录

```
C:\Program Files\RemoteBuild\
├── remote-build.exe
├── tools\
│   └── putty\
│       ├── plink.exe
│       ├── pscp.exe
│       ├── psftp.exe
│       ├── puttygen.exe
│       └── pageant.exe
├── config\
│   ├── tools.yaml.template
│   ├── targets.yaml.template
│   └── config.yaml.template
└── README.txt
```

### 17.3 首次启动初始化

```
1. 检测 ~/.remote-build/ 是否存在
2. 不存在 → 创建目录结构，从安装目录拷贝 tools/ 和 config/ 模板
3. 探测系统环境（JAVA_HOME 等）
4. 生成 tools.yaml（putty 路径指向安装目录的 tools/putty/）
5. 进入主界面
```

### 17.4 更新

- 覆盖安装 `remote-build.exe`
- `tools/putty/` 如果版本更新则覆盖
- `~/.remote-build/` 用户配置不动

### 17.5 用户可选自定义 PuTTY

```yaml
# config.yaml
putty:
  use_builtin: true
  custom_path: null          # 指定自定义路径，如 "C:/MyTools/PuTTY"
```

UI 设置页：

```
┌──────────────────────────────────────────────┐
│  PuTTY 配置                                  │
│  ○ 使用内置版本 (0.79)                       │
│  ● 使用自定义路径:                            │
│    [C:\Program Files\PuTTY        ] [📂]     │
│                                              │
│  状态: ✅ plink.exe found (v0.79)             │
│        ✅ pscp.exe found                      │
│        ✅ psftp.exe found                     │
└──────────────────────────────────────────────┘
```

---

## 18. 实施路线

| 阶段 | 内容 | 可交付 |
|------|------|--------|
| **P0** | 单项目、单作业、本地执行命令、控制台输出、首次启动探测、PuTTY 捆绑 | 可运行 MVP |
| **P0.5** | SSH 远端执行（plink）、SFTP 上传（pscp） | 能打远程包 |
| **P1** | 多项目分组、变量系统、YAML 仓库化、历史 SQLite、步骤级并行 | 团队共享 |
| **P1.5** | k8s 支持（K8sApply + watch）、条件执行、步骤产出变量捕获 | 完整 K8s 部署 |
| **P2** | 并行步骤、制品索引、通知（桌面弹窗）、OpenSshTransport | 锦上添花 |
| **P3** | RusshTransport（纯 Rust SSH）、WASM 插件系统、制品仓库集成 | 远期 |

**P0 目标**：两周内跑通一个 `echo` 步骤。**如果 P0 超过三周，说明抽象层做得太早了。**

---

## 19. 待确认问题

1. **本地执行 vs 远端执行的工作目录映射**
   当 target 是 SSH 远端时，`local_path` 是本地路径，但远端也有对应代码目录。两种模式：
   - **A）本地 build，产物 scp 上去**：`mvn package` 在本地跑，jar 打出来后 pscp 到远端
   - **B）远端 build**：pscp 把源码传上去（或远端 git clone），命令在远端跑
   
   现在是哪种场景多？还是两种都要支持（job 级别选）？

2. **变量替换语法**
   `${{ variables.registry.prod }}` 这个风格 OK？还是想要 `${REGISTRY}` 这种环境变量风格？

3. **步骤命令是单行还是多行？**
   输入框用单行 `TextEdit` 还是带语法高亮的 mini editor？k8s 场景基本都需要多行（helm 命令带一堆 `--set`）。

4. **russh 的定位**
   确认作为 P2 可选替换方案预留 `RemoteTransport` trait，P0 不引入 russh 依赖。是否同意？

5. **PuTTY 版本**
   内置哪个版本？最新稳定版（0.79 或更高）？是否需要支持 32 位系统？

6. **配置仓库路径**
   `workspace_root` 默认 `~/.remote-build/projects/`，还是让用户首次启动时选一个目录？

7. **日志保留**
   `log_retention_days` 默认多少天？7 天 / 30 天 / 不自动清理？
