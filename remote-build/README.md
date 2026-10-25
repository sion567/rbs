# Remote Build System

Windows 桌面构建部署工具，覆盖"下载 → 编译 → 打包 → 上传 → 部署"链路。
架构设计见 `../docs/RemoteBuildSystem架构设计文档_v0.4.md`；变量语法速查见 `../docs/变量速查.md`
（GUI 作业编辑器里也有「？ 变量语法」窗口）；功能路线与完成情况见 `../docs/TODO.md`。

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
| `exec` | 执行命令（本地 cmd / 远端 plink） |
| `wait` | 轮询等待就绪：命令 / tcp 可连 / http 2xx 三种探测模式；`fail_exit_code` 让"任务已死"立即失败（长任务配方） |
| `upload` | 上传（多行映射 `本地 -> 远端`；sha256 校验；内容未变自动跳过传输） |
| `download` | 下载（`远端 -> 本地`；sha256 校验；父目录自动创建） |
| `pack` | 本地 zip 打包（`目录 -> x.zip [--exclude node_modules --exclude *.map]`） |
| `k8s-apply` | 本地 kubectl（KUBECONFIG 自动注入；k8s_watch 为 P1.5） |

## 流程级能力（build.yaml）

- **条件与状态**：`when:` 条件（==/!= 比较）、`creates:` 产物守卫（重跑秒过已完成步骤）
- **运行时变量**：`outputs:` 从 stdout 正则捕获 → 后续步骤 `${{ env.键 }}` 引用
- **变更才触发**：`notify:` + `handlers:`（只有真的传了新 artifact 才执行重启等收尾步骤）
- **长任务（fire-and-poll）**：远端 `nohup` 后台跑 + 状态文件 + `wait` 轮询 +
  `fail_exit_code` 致命退出码——备份/迁移跑 2 小时不怕 SSH 断，任务死了秒失败
  不烧超时（配方见 `../docs/长任务配方.md`）
- **前置检查**：`preconditions:` 本地断言，失败零秒退出（人话报错）
- **确认门**：`confirm: prod` —— 运行前要求输入确认词，防手滑上生产
- **超时**：`timeout:`（作业级总超时）+ 步骤级 `timeout`（config.yaml `default_timeout` 兜底）
- **失败策略**：`on_failure: stop / continue / retry`（retry = 整个流程从头重跑 N 次）
- **工具版本要求**：project.yaml `tools_require: { java: ">=21" }`，偏差零步失败
- **同项目运行互斥**：新运行自动取消旧运行（GUI / CLI 跨进程生效，GHA concurrency 语义）

## 组运行（金丝雀分批语义）

分组从「项目树展示单位」升级为**执行单位**：工具栏「▶ 运行组」（选中流程所属组）或
右键分组头「▶ 运行组」——组内流程按项目树顺序**串行**执行，**任一失败即停**
（后续流程不启动 = 金丝雀"任一批失败不碰剩余机器"）。

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
- **桌面通知**：作业结束弹 Windows toast（`desktop_notify` 可关）
- **控制台**：实时流式输出、「只看错误」过滤器、复制 / 导出 .log
- **危险命令提醒**：`rm -rf /`、`chmod -R 777` 等模式预览 ⚠ + 执行前告警（不拦截）

## 认证与安全

- SSH 密码存 Windows Credential Manager（键 `{项目}/{目标}`），YAML 不落密码；执行经 plink `-pw`
  （内网可接受；高安全场景用 key / Pageant）。目标编辑器支持用未保存密码直接「🔌 测试连接」。
- host key 指纹固定（TOFU）：首次连接测试输出指纹 → 填入「主机键指纹」字段即信任。

## 示例工程（~/.remote-build/projects/）

`yudao-cloud`（Maven 微服务，需 JDK25）/ `yudao-admin-uniapp`（pnpm，corepack 钉版本）/
`yudao-admin-vben`（turbo + pnpm）三个真实工程，各配 dev-build（本地）与
prod-deploy（编译 → pack 打 zip → 多行映射 upload 到 prod 目标 → 远端发布脚本）。
工程源码在 `E:\workspace\rbs\project-samples\`，配置与代码两层分离（§10.1）。

> 前置：把 PuTTY 套件 5 个 exe 放入 `tools/putty/`（见 `tools/putty/README.txt`）。
> 缺失时编译有告警，运行期回退到系统 PATH 中的 PuTTY。

## 已知限制

- YAML 变量值请写字符串（`PORT: "8080"` 而非 `PORT: 8080`），flatten 反序列化不收数字
- upload/download 为 pscp 整文件复制（无增量/断点续传）；大量小文件先 pack 成单 zip
- wait 的 http 探测仅支持 http://（https 请用命令模式 `curl -k`）
- SQLite 历史在编译期可用 `--no-default-features` 裁掉（UI 显示提示，不建库）
