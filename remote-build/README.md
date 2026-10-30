# Remote Build System

Windows 桌面构建部署工具，覆盖"下载 → 编译 → 打包 → 上传 → 部署"链路。
架构设计见 `../docs/RemoteBuildSystem架构设计文档_v0.4.md`；变量语法速查见 `../docs/变量速查.md`
（GUI 流程编辑器里也有「？ 变量语法」窗口）；功能路线与完成情况见 `../docs/TODO.md`。

## 构建与运行

```powershell
cargo run               # 开发运行（默认启用 sqlite 历史 feature）
cargo test              # 单元测试（models / variables / transport / engine / sqlite / runlock）
cargo build --release
cargo build --no-default-features   # 不带 SQLite 历史的精简构建
```

## 命令行（无头模式，脚本 / CI 可用）

```powershell
remote-build set-password <目标名> <密码>        # SSH 密码写入 Windows Credential Manager（不落盘）
remote-build test-conn <目标名>                  # SSH 连接测试 + 服务器体检（CPU/内存/磁盘）
remote-build run <项目名> <流程名> [--yes]       # 无头执行（--yes 跳过确认门；非交互终端必加）
remote-build run-group <项目名> <组名> [--yes] [--from <流程名>]
                                                # 组运行：组内流程串行·失败即停（--from 从指定流程续跑）
remote-build preview <项目名> <流程名>           # 干跑预览：解析后的命令 / 上传清单 / 将跳过的步骤
```

## 步骤类型（step_type）

| 类型 | 说明 |
|------|------|
| `exec` | 执行命令（本地 cmd / 远端经目标所选传输：russh 或 plink） |
| `wait` | 轮询等待就绪：命令 / tcp 可连 / http 2xx 三种探测模式；`fail_exit_code` 让"任务已死"立即失败（长任务配方） |
| `upload` | 上传（多行映射 `本地 -> 远端`；sha256 校验；内容未变自动跳过传输） |
| `download` | 下载（`远端 -> 本地`；sha256 校验；父目录自动创建） |
| `pack` | 本地 zip 打包（`目录 -> x.zip [--exclude node_modules --exclude *.map]`） |
| `k8s-apply` | kubectl 在发版本机执行（KUBECONFIG 自动注入），经 kubeconfig 连接远端集群；k8s_watch 为 P1.5 |

## 流程级能力（build.yaml）

- **条件与状态**：`when:` 条件（==/!= 比较）、`creates:` 产物守卫（重跑秒过已完成步骤）
- **运行时变量**：`outputs:` 从 stdout 正则捕获 → 后续步骤 `${{ env.键 }}` 引用
- **变更才触发**：`notify:` + `handlers:`（只有真的传了新 artifact 才执行重启等收尾步骤）
- **长任务（fire-and-poll）**：远端 `nohup` 后台跑 + 状态文件 + `wait` 轮询 +
  `fail_exit_code` 致命退出码——备份/迁移跑 2 小时不怕 SSH 断，任务死了秒失败
  不烧超时（配方见 `../docs/长任务配方.md`）
- **前置检查**：`preconditions:` 本地断言，失败零秒退出（人话报错）
- **确认门**：`confirm: prod` —— 运行前要求输入确认词，防手滑上生产
- **超时**：`timeout:`（流程级总超时）+ 步骤级 `timeout`（config.yaml `default_timeout` 兜底）
- **失败策略**：`on_failure: stop / continue / retry`（retry = 整个流程从头重跑 N 次）
- **工具自动自检**：运行启动时引擎对本次用到的工具（步骤 tool ∪ tools_require）逐个
  现跑 version_check（未配置的降级为检查 command 存在），失败零步失败 + 人话报错；
  tools_require 用自检现算的版本做门禁（tools.yaml 的 version 只是探测快照）
- **同项目运行互斥**：新运行自动取消旧运行（GUI / CLI 跨进程生效，GHA concurrency 语义）

## 组运行（金丝雀分批语义）

分组从「项目树展示单位」升级为**执行单位**：右键分组头「▶ 运行组」——组内流程按项目树
顺序**串行**执行，**任一失败即停**（后续流程不启动 = 金丝雀"任一批失败不碰剩余机器"）。

- **确认门统一收口**：组级预览（各流程干跑连展）→ 一次输入**全部**确认词
  （组内任一流程配了 `confirm:` 就要输入，连跑不降级防线）；`preconditions` 照常逐流程把关
- **续跑**：失败后右键分组「↪ 从失败处继续」（GUI 会话内记忆失败位置）；
  CLI 用 `--from <流程名>`
- **并行不做**：运行锁是项目级 cancel-in-progress（新运行接管 → 旧运行自杀），
  组内并行会互杀；且多机同时发 = 全量发布，与金丝雀目标相悖
- 历史：每个流程一条运行记录，run_id 同前缀（`{项目}-grp-时间戳-序号`）= 同一次组发布
- 典型结构：① 打包 → ② 金丝雀（上传 prod-01 → 重启 → wait 健康检查）→ ③ 铺开
  （prod-02、prod-03 同款三步，步骤级 `target` 覆盖）；job 边界 = 人工 / 审核间隔

## 环境与可观测

- **运行历史**：SQLite（`~/.remote-build/data/history.db`，config.yaml `save_history` 开关；
  设置页「🕘 运行历史」查看最近运行；保留期随 `log_retention_days` 清理）
- **👀 监听模式**：工作目录文件变化自动重跑当前流程（3s mtime 轮询，排除构建产物目录）
- **状态可视化**：步骤光圈（运行蓝/等待黄/成功绿/失败红）+ 耗时条 + 重试标注
- **桌面通知**：流程结束弹 Windows toast（`desktop_notify` 可关）
- **控制台**：实时流式输出、「只看错误」过滤器、可折叠（编辑步骤时收起）、复制 / 导出 .log
- **危险命令提醒**：`rm -rf /`、`chmod -R 777` 等模式预览 ⚠ + 执行前告警（不拦截）

## SSH 传输（v0.5 换防）

- **russh（默认）**：进程内 SSH/SFTP 实现，免装任何外部套件、单 exe 分发；协议级 keepalive
  30s（治 VPN/NAT 掐空闲连接）；SFTP 远端路径按字面处理（无 shell 解释问题）；
  上传自动创建远端父目录。
- **putty（兜底）**：目标配置 `transport: putty` 时走 plink/pscp——怪服务端互操作兜底。
  旧配置未写 transport 字段 = 兼容按 putty 执行（升级前行为不变），目标编辑器可切换。
- 主机键指纹（TOFU）两种传输同 UX：russh 只认 `SHA256:…` 形式（putty 兼容 MD5）。

## 认证与安全

- SSH 密码存 Windows Credential Manager（键 `{项目}/{目标}`），YAML 不落密码；russh 经协议
  内存参数传递（不进命令行），putty 经 plink `-pw`（本机进程列表短暂可见，高安全场景用 key）。
  目标编辑器支持用未保存密码直接「🔌 测试连接」。
- host key 指纹固定（TOFU）：首次连接测试输出指纹 → 填入「主机键指纹」字段即信任；
  未登记指纹 russh 直接拒连并给出指纹（不自动信任）。
- russh 私钥须为 OpenSSH 格式；PuTTY PPK 请先用 PuTTY 自带的 puttygen 转换
  （`puttygen key.ppk -O private-openssh -o key.pem`）。

## 示例工程（~/.remote-build/projects/）

目录布局（代码与配置两层分离，§10.1）：

```
~/.remote-build/
├── projects/<项目名>/        # 配置：project.yaml + jobs/<流程名>.yaml（新布局）
│   └── jobs/                 #   根目录的旧版 *.yaml 流程仍兼容发现
└── workspace/<项目名>/       # git 克隆的代码（默认目标；本地目录模式代码原位不动）
```

`yudao-cloud`（Maven 微服务，需 JDK25）/ `yudao-admin-uniapp`（pnpm，corepack 钉版本）/
`yudao-admin-vben`（turbo + pnpm）三个真实工程，各配 dev-build（本地）与
prod-deploy（编译 → pack 打 zip → 多行映射 upload 到 prod 目标 → 远端发布脚本）。
工程源码在 `E:\workspace\rbs\project-samples\`，配置与代码两层分离（§10.1）。

> 前置：russh 目标**零依赖**开箱即用；仅 `transport: putty` 的目标需要本机已安装 PuTTY
> （安装器默认进 Program Files 并加入 PATH；绿色版在 ⚙ 设置的「PuTTY 目录」指定位置，
> 需含 plink.exe / pscp.exe）。探测顺序：自定义目录 → 系统 PATH → Program Files。

## 已知限制

- YAML 变量值请写字符串（`PORT: "8080"` 而非 `PORT: 8080`），flatten 反序列化不收数字
- upload/download 为整文件复制（russh SFTP / pscp 均无增量、无断点续传）；大量小文件先 pack 成单 zip
- russh 传输暂不支持 agent（Pageant）认证——需要 agent 的目标请切回 putty 传输
- wait 的 http 探测仅支持 http://（https 请用命令模式 `curl -k`）
- SQLite 历史在编译期可用 `--no-default-features` 裁掉（UI 显示提示，不建库）
