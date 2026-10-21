# Remote Build System（P0）

Windows 桌面构建部署工具，覆盖"下载 → 编译 → 打包 → 上传 → 部署"链路。
架构设计见 `../docs/RemoteBuildSystem架构设计文档_v0.4.md`；变量语法速查见 `../docs/变量速查.md`（GUI 作业编辑器里也有「？ 变量语法」窗口）。

## 构建与运行

```powershell
cargo run          # 开发运行
cargo test         # 单元测试（models / variables / transport / engine）
cargo build --release
```

## 命令行（无头模式，脚本 / CI 可用）

```powershell
remote-build set-password <目标名> <密码>   # SSH 密码写入 Windows Credential Manager（不落盘）
remote-build test-conn <目标名>             # SSH 连接测试（5s 超时）
remote-build run <项目名> <流程名>          # 无头执行一条流程，实时输出，exit code 即结果
```

## 示例工程（~/.remote-build/projects/）

`yudao-cloud`（Maven 微服务，需 JDK25）/ `yudao-admin-uniapp`（pnpm，corepack 钉版本）/
`yudao-admin-vben`（turbo + pnpm）三个真实工程，各配 dev-build（本地）与
prod-deploy（编译 → pack 打 zip → 多行映射 upload 到 prod 目标 → 远端发布脚本）。
prod 目标：192.168.38.198（密码在凭据管理器，键名 prod）。
工程源码在 E:\workspace\rbs\project-samples\，配置与代码两层分离（§10.1）。

> 前置：把 PuTTY 套件 5 个 exe 放入 `tools/putty/`（见 `tools/putty/README.txt`）。
> 缺失时编译有告警，运行期回退到系统 PATH 中的 PuTTY。

## P0 已实现（对应文档 §18 路线）

- 数据模型与 YAML 配置（serde kebab-case / flatten，v0.4 评审修复全部落地）
- 首次启动环境探测（JAVA_HOME / MAVEN_HOME / GRADLE_HOME / NODE_HOME / PATH / PuTTY）
- 执行引擎：步骤链、exec / wait / **upload（多行映射，微服务多 jar）** / **pack（内置 zip 打包，§5.8）** /
  k8s-apply（P0 退化为本地 kubectl）、超时 / 重试 / 取消（broadcast + kill）、
  `workspace/.env` 变量传递约定
- 🎯 目标管理：SSH / Kubernetes / 本地目标增删改（targets.yaml）、SSH 连接测试；
  SSH 密码存 Windows Credential Manager（不落 YAML）；host key 指纹固定（TOFU，
  首次测试失败输出指纹 → 填入 host_key 字段即信任）；upload/exec 经 pscp/plink 使用
- 传输层：本地 `cmd /C` + PuTTY（plink / pscp，禁止 `-pw`，cd 路径单引号转义）
- egui UI：首启引导页、项目树、步骤编辑器、实时控制台、设置窗口
- 新建项目向导（＋ 新建项目）：Git 模式（地址/账号/密码/分支 → Clone 到
  `~/.remote-build/projects/<名称>`，进度实时显示、日志脱敏）与本地目录模式（浏览选择，
  配置生成在 projects/ 下）；创建后自动生成 project.yaml + 起始 build.yaml 并选中
- 工具管理（⚙ 设置窗口）：可视化增删改 tools.yaml（command 支持文件浏览），
  「验证」按钮后台跑 version_check；作业步骤的工具下拉与设置页共用同一份配置
- 项目目录输入框带「📂 浏览…」，可用系统文件夹选择器直接选取
- 变量系统：`${{ variables.X }}` / `${{ env.X }}` / `${{ targets.X.field }}` / `${VAR}`
- 工具链落地：tools.yaml / 项目级 tools（project.yaml `tools:` 段，键相同整体覆盖）的
  command 完整路径自动前置到步骤 PATH；`node_modules/.bin` 存在时自动前置（前端工程
  零配置）；私服地址建议走工程内配置文件或 `variables.<env>` 的
  `NPM_CONFIG_REGISTRY` / `PIP_INDEX_URL`（见设计文档 §6.5）

## P0 明确不做

- SQLite 历史（P1）、k8s_watch 轮询（P1.5）、OpenSshTransport（P1）、
  密码认证（禁止 `-pw`，用 key / Pageant）、job 级 on_failure=retry（P1）

## upload 步骤命令格式（P0 约定）

```
<本地路径或目录> -> <远端路径>
# 例：target/*.jar -> /opt/app/
```

## 已知限制

- YAML 变量值请写字符串（`PORT: "8080"` 而非 `PORT: 8080`），flatten 反序列化不收数字
- 远端工作目录取 `target.work_root`，本地/远端目录映射待确认（文档 §19 问题 1）
