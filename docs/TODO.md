# TODO

## 超时机制问题

> 现象：大项目编译 / 首次拉依赖容易触发单步超时（默认 300s），超时后 UI 显示作业已结束（timeout），
> 但 mvn / java / pnpm 等实际构建进程**没有被杀掉**，变成孤儿进程继续在后台跑，
> 可能占用 target/ 目录、文件锁，影响下一次运行。

### ✅ 问题 1：超时 / 取消都杀不掉进程树（已修复）

- 修复内容：
  1. `local.rs` / `putty.rs` spawn 时加 `cmd.kill_on_drop(true)` —— 超时丢弃 future 时杀掉直接子进程；
  2. 新增 `transport/win_job.rs`：Windows Job Object（`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`，winapi `jobapi2`），
     子进程挂进 job，超时（Drop 关句柄）/ 取消（`TerminateJobObject`）时整棵树终止；
  3. putty.rs 侧 plink / pscp 同样挂 job（叶子进程，主要覆盖超时丢弃路径）。
- 验证：`cancel_kills_running_step_quickly` / `job_level_timeout_stops_remaining` 单测 +
  CLI e2e（job.timeout=3s 跑 30s 的 ping，超时后 tasklist 确认 ping.exe 已终止）。
- 备注：job 挂载失败（嵌套 job 限制等）降级为直接 kill 兜底，不阻塞执行。

### ✅ 问题 2：死配置 —— job.timeout 与 config default_timeout 已接线

- `config.yaml default_timeout` → `RunInput.default_step_timeout`：步骤未显式配
  `step.timeout` 时的默认超时（模板 600s）。
- `job.timeout` → 作业级总超时：步与步之间检查（超时后剩余步骤不启动），
  运行中的步骤取 `min(步骤截止, 作业截止)` 提前掐断，返回 `timeout`。

### 问题 3：超时形式粗糙（部分优化，空闲超时未做）

- ✅ deadline 在重试循环外计算 → 已改为**每次尝试独立的 deadline**
  （`engine.rs` `run_step_with_retry`，作业级 deadline 更早则取更早者），
  重试不再均摊总预算，慢上传重试不会被误杀。
- 固定总时长分不清"慢但在动"和"卡死不动"：编译 / 拉依赖 legitimately 慢会被误杀；
  上传 / 下载更合理的是**空闲超时**（N 秒无任何输出 / 进度才判失败），而非总时长。
- 短期缓解（不改代码）：容易慢的步骤在 build.yaml 显式配大 `timeout:`（如 1800）；
  `wait` 步骤受同一总时长管，轮询探测别配太长的总时限。

## Ansible 功能参考

> 背景：项目形态与 Ansible 同构——build.yaml ≈ playbook、targets ≈ inventory、
> ${{ }} 变量 ≈ Jinja2、exec/upload/pack 步骤 ≈ 模块。以下按性价比排序，
> 只抄单人桌面场景用得上的，不抄集群编排（forks 并行、roles/collection 生态、期望状态模块体系）。

### ✅ 1. 条件跳过：`when` / `creates`（已实现）

- `when:`：`condition` 字段的别名（YAML 任写其一）。求值已从"仅非空检查"扩展为
  `==` / `!=` 值比较：右侧可为字面量（可带引号）或另一取值表达式，未定义变量按空串参与比较。
  例：`when: ${{ variables.ENV }} == prod`。
- `creates:`：步骤声明产物路径，已存在则跳过（重跑秒过已完成步骤）。
  本地相对项目目录，支持 `*` `?` 通配（glob crate）；SSH 步骤为远端路径
  （相对 = work_root / working_dir），远端 `test -e` 退出码判定，检查失败 fail-open 照常执行。
- 改动点：`variables.rs::eval_condition`、`engine.rs` 执行循环、`models.rs::Step`、
  job_editor 步骤卡片「＋条件 / ＋creates」。

### ✅ 2. check mode 干跑预览（已实现）

- 工具栏「👁 预览」按钮 + CLI `remote-build preview <项目名> <流程名>`：
  展示变量全量解析后的最终命令、上传清单（含本地源缺失 ⚠ / 远端路径拼接结果）、
  将跳过的步骤（未启用 / when / creates）与未解析变量 ⚠（顺带当变量拼错检查器用）。
- 实现为 `engine.rs::preview_job`：同步无副作用，不执行任何命令、不发任何网络请求
  （远端 creates 干跑不校验，仅标注）。

### ✅ 3. handlers + notify（变更才触发，已实现）

- 参照 Ansible「改了配置才 notify 重启服务」：play 结尾统一执行被通知到的 handler。
- 映射到本项目：**只有这次真的传了新 artifact，才执行远端重启脚本**——
  artifact 没变就跳过重启，避免每次跑流程都打断线上服务。
- 实现：`Step.notify: [名字]` + `Job.handlers`（同名步骤）。步骤实际产生变更
  （exec/wait/pack 真执行；upload 真传了新内容——hash 未变更跳过不算；creates/when/enabled
  跳过不算）→ 登记 handler，主流程成功收尾后按 handlers 定义顺序执行；handler 失败 = job 失败。
- 引擎重构：单步执行抽为 `run_single_step`（主流程与 handlers 共用），`StepResult::Success`
  新增 `changed` 标记。e2e 已验证「真执行触发 / creates 跳过不触发」。

### 4. block / rescue / always（try-catch-finally，未做；前置依赖「问题 1」已解决 ✅）

- 一组步骤声明"失败走 rescue 分支，无论成败 always 都执行"。
- 映射到本项目：上传前备份旧包、失败回滚、无论如何删远端临时目录等**清理步骤**。
- 与 P1 已排的 job 级 `on_failure` 是一家人，适合一起设计。
- 前置依赖：~~先解决上面"问题 1"孤儿进程问题~~（已完成，孤儿进程已根治）。

### 5. 步骤 tags 子集执行（未做）

- 参照 `--tags deploy` 只跑带标签的步骤；job 越长（build→pack→upload→远端发布），
  "只重跑后半段"越是高频需求。与 `creates` 粒度互补（tags 粗、按段；creates 细、按状态）。

### ✅ 6. 变量默认值语法（已实现）

- `${{ variables.X || 默认值 }}`：X 未定义 / 不可解析时取默认值（Jinja2 `| default()` 语义；
  已定义但为空串仍用空串；默认值可带引号）。所有 `${{ }}` 取值形态均支持（含 `run.id`、`now` 等）。
- 改动点：`variables.rs::lookup_expr`；UI 变量语法速查窗口与 docs/变量速查.md 已同步。

### 7. facts 缓存（优先级低，未做）

- 参照 gather_facts + fact cache：对 target 探测远端信息（java 版本、磁盘剩余），
  缓存后供变量与条件引用，如 `when: disk_free > 1G` 的部署前守卫。
- 本机首启环境探测已有，此项是其远端版。

## 其他工具功能参考（待规划）

> 第二批参照对象，按工具分组。不抄：Jenkins（服务器中枢模型）、Bazel（封闭单仓构建）、
> Packer/Dagger/Concourse（形态差太远）、Terraform 的 state/漂移检测（过度设计）。

### GitHub Actions（本项目 ${{ }} 语法的出处，最顺手的参照）

- **✅ concurrency 并发控制**（已实现）：同项目新运行自动取消旧运行。
  跨进程实现：每项目一把锁文件（`~/.remote-build/runs/<项目>.lock`），新运行**接管**锁，
  旧运行的 watcher（500ms 轮询）发现锁被夺走 → 向自己的取消信道发信号 → 引擎现有取消机制
  （win_job.rs 整树终止）自杀返回 Cancelled；引擎步骤间再查一次锁归属兜底。
  GUI「▶ 运行全部」运行中再次点击 = 同项目重跑；CLI 两个 run 进程同项目并发也会互相接管（e2e 已验证）。
- **environment + 人工审批**：production environment 可配"必须有人批准才继续"。
  映射：部署到 prod 目标的 job 执行前弹确认（甚至要求输入目标名才放行），防手滑上生产。

### ✅ 人工确认门（GHA environment protection，已实现）

- `job.confirm: prod`：运行前 GUI 弹窗要求输入「prod」才放行（确认词可自定义）；
  CLI 交互终端输入确认词，非交互终端必须显式 `--yes`。
- UI 步骤编辑器「＋确认门」按钮；干跑预览会展示确认门存在。

### Capistrano（原子发布 + 一键回滚）

- 目录约定：每次发布放 `releases/<时间戳>/`，`current` 软链接指向它，
  切链接 = 原子上线；指回上一时间戳 = 回滚。
- 现状"upload 覆盖 jar + 重启"是破坏式更新，传一半失败即废；改为上述结构后
  GUI 可提供差异化按钮：**回滚到上一版**。远端只需约定 releases/current 结构，发布脚本改动不大。
- 依赖：内容校验和变更判断已就绪（见下），hash 没变不重复发布的地基已有。

### Task / go-task（任务运行器）

- **`sources:` / `generates:` 内容校验和增量**：比 `creates` 更进一步，
  按内容（而非文件存在/时间戳）判断步骤要不要跑；"源码没变就跳过编译"的正确路线。
- **✅ `preconditions:` 前置断言**（已实现）：`job.preconditions`（check 本地命令 + message 人话说明），
  执行前发版机本地逐条运行，任一退出码非 0 → 整个 job 零秒失败并展示说明
  （`JobStatus::PRECONDITION_STEP`，short() 显示 `failed (precondition: …)`）。
  UI 步骤编辑器「＋前置检查」可增删；干跑预览列出检查项。
- **✅ 内容校验和判断"是否变更"**（已实现，handlers/notify 的地基）：upload 单文件上传前
  先比对本地与远端 sha256，一致 → 整个传输跳过（日志 `[upload] 内容未变更…跳过传输`），
  不一致 / 远端不可查才真正上传（上传后完整性校验保留）。handlers/notify（Ansible #3）
  后续可直接复用"远端 hash 是否变化"这个判定来决定跳过重启。

### Docker Compose

- **profiles 子集选择**：`--profile deploy` 只启带标签的服务——与 Ansible 一节
  第 5 条 tags 子集执行互为印证，选型时可对比两家的语法形态。
- **✅ healthcheck 三要素**（已实现）：`Step.wait: { mode: command|tcp|http, target, interval_seconds }`
  ——command 沿用旧行为（兼容旧 YAML），tcp 从发版机直连 host:port、http GET 状态码 2xx
  （仅 http://，https 用命令模式 curl）；轮询间隔可配（默认 5s），总时长仍受步骤/作业超时管。
  UI 步骤卡片「⏲ 模式」可切换；tcp 模式 e2e 已验证。
- 默认值插值一项已用 `||` 语法实现（见 Ansible 一节 ✅ 6），不再单列。

### rsync（upload/pack 步骤的参照物）

- **✅ `--exclude` 排除模式**（已实现，pack 步骤）：`target/ -> dist/app.zip --exclude node_modules --exclude *.map`
  ——字面量按路径分量整棵排除（递归剪枝，node_modules 大目录不白走）；`*` `?` 通配匹配
  相对路径与文件名。upload 的 exclude 仍受 pscp 限制（落到 pack 层做）。
- **`--dry-run` 输出形态**：列出"会传哪些、跳过哪些"——已实现的干跑预览（✅ 2）
  后续增强上传清单展示时可参照此形态。
- 已知差距：pscp 整文件复制，rsync 有增量传输；大文件高频场景待第六批 russh/sftp
  落地后再评估增量传输（SFTP 按偏移续传是第一候选）。

### kubectl（k8s-apply 步骤的两条现成模式）

- **`rollout status`**：apply 后盯 rollout 直到成功/超时——"执行后等收敛"应成为
  k8s-apply 内置行为，而非让用户自己接 wait 步骤。
- **`rollout undo`**：与 Capistrano 回滚同构，k8s 目标的一键回滚可直接包这条命令。

### ✅ 目标管理「测试连接」支持未保存密码（DBeaver 式，已实现）

- 目标编辑器本就支持用表单当前值测试（无需先保存）；本次补齐密码环节：
  表单里已输入、尚未点「保存密码」的密码直接参与本次测试（临时密码不落任何存储），
  首次配置即可验证连通性，避免发版时才报错。
- 实现：`putty.rs::build_ssh_args_with / probe_server_with` 支持临时密码覆盖
  Credential Manager；测试成功会在结果里注明密码尚未保存。

### 其余一句话

- **Make**：`creates`/增量的鼻祖（mtime 判断）；跨机器场景 mtime 不可靠（远端时钟），
  内容校验和才是正确路线（见 Task 条目）。
- **Terraform**：只抄 `plan` 的差异展示符号（`+` 新增 / `~` 修改 / `-` 删除），
  用于增强已有干跑预览界面的 diff 展示。
- **just**：recipe 参数（`just build release=prod`）——job 无参数的现状加参数后，
  一份 build.yaml 模板可多环境复用。

### 优先级（与现有 TODO 的咬合）

1. ✅ GHA concurrency「新跑取消旧跑」——已实现（锁文件接管 + watcher 自杀，跨进程 e2e 已验证）；
2. ✅ Task preconditions——已实现（job.preconditions 本地断言，失败零秒退出）；
3. ✅ 内容校验和判断"是否变更"——已实现（上传前远端 sha256 比对，未变更跳过传输）；
   handlers/notify（Ansible #3）本体在其上待做；
4. prod 人工确认门 ✅ 已实现（job.confirm 确认词）；Capistrano 回滚待做
   （内容校验和地基已就绪，需要先设计远端 releases/current 目录约定）。

## 更多工具功能参考（第三批，待规划）

> 前两批见「Ansible 功能参考」「其他工具功能参考」。本批按主题分组：
> 工具链管理 / 数据流 / 传输与开发回路 / 可观测性 / 配置组织 / 构建编排 / 部署可靠性。

### ✅ mise（原 rtx）—— 工具版本管理（对比校验已实现）

- **✅ 版本要求声明 + 偏差校验**（已实现）：project.yaml `tools_require: { java: ">=21" }`，
  运行前与 tools.yaml 探测的 version 对比（`version.rs`：>= > <= < 数字段比较缺段补 0；
  裸值前缀匹配 22 → 22.x），偏差零步失败并给人话说明；干跑预览逐条显示 ✓/⚠ 对比。
  e2e 已验证拦截 / 放行 / 未定义三条路径。
- 自动安装（winget/scoop 一键装）与 nix `tools.lock` 复刻（未做，后续增强）。

### ✅ GitHub Actions step outputs —— 运行时变量（已实现）

- 实现：`Step.outputs: { 键: 正则 }`，步骤成功后对捕获的 stdout 全文取首个匹配
  （有捕获组取组 1、无则整行匹配），存为运行时变量，后续步骤 `${{ env.键 }}` 引用——
  与 .env 约定同一命名空间，`||` 默认值、`when` 条件天然组合。
- 捕获管道：`LogHandle` 可选附带 stdout 缓冲（捕获只挂 stdout），`apply_step_outputs` 打日志
  `[outputs] 键 = 值`；未匹配留空并告警。Windows cmd 嵌套调用的引号尾巴做了 trim 防御
  （建议命令直接写 `echo VERSION=…`，引擎已包 `cmd /C`）。
- 语法示例：`outputs: { VER: 'VERSION=(\S+)' }`；docs/变量速查.md 已同步。

### WinSCP / watchexec —— 传输与开发回路（GUI 主场）

- **✅ watch 模式（粗档：文件变化自动重跑 job）**（已实现）：工具栏「👀 监听」开关——
  开启立即跑一次（确认门生效一次），之后 3 秒轮询工作目录 mtime，变化自动重跑
  （复用同项目重跑链路；排除 .git/node_modules/target/dist 等；目标流程被删自动停止；
  监听重跑绕过确认门——开启时已确认过）。mtime 轮询零新依赖（notify 事件监听留作增强）。
- **WinSCP "Keep remote directory up to date"（细档：只推文件）**（未做）：
  "改一行 → 自动构建 → dist 自动同步测试机"；pscp 不支持就自己组合
  （目录 watcher + 增量 upload 步骤）。
- **传输队列面板**（WinSCP/FileZilla 同款）（未做）：upload 已是多行映射，UI 却只有一个步骤状态；
  抄文件传输工具的队列视图——每文件一行、进度/速度/失败单独重试（需要引擎按文件粒度发事件）。
  rclone 的进度语义（速度/ETA/百分比）可作展示参照。

### gh CLI / Kamal audit —— 可观测性（接 P1 历史与状态可视化）

- **✅「只看错误」过滤器**（已实现）：控制台勾选后隐藏 stdout / 系统提示行，
  保留步骤头尾、stderr 与错误（自动滚动打开）。
- **`gh run watch` 的"跟随模式"**（未做）：跑到哪自动滚到哪——自动滚动已覆盖大半，可选增强。
- **步骤耗时时间线**（未做）：在「状态可视化」基础上加耗时条 + 重试标注。
- **步骤耗时时间线**：GHA/GitLab 的每步耗时条形时间线；在「状态可视化」基础上加
  耗时条 + 重试标注（哪次尝试、耗时多久），再往下可导出 trace 格式用 Perfetto 看。
- **`kamal audit`**：37signals Kamal（容器版 Capistrano）里小而好的命令——列出历史部署
  （谁/何时/什么版本）。P1 SQLite 历史照此最小形态起步：先有列表，再谈详情。

### Kustomize / Helm —— 环境分层的另一种解法

- 现状用 `variables.<env>` 做环境差异；overlay 路线是 base build.yaml + 环境覆盖层
  （只 patch 差异字段，后期覆盖前期）。两路线不冲突，overlay 的好处是
  GUI 能画出"这层改了什么"的差异视图。
- **helm diff 插件**最值得抄：升级前对比"远端现在跑的 vs 这次要部署的"——
  干跑预览再进一阶：不只展示"将做什么"，还展示"和现状比变了什么"。

### docker bake / Earthly / Airflow —— 构建编排（远期）

- **bake `matrix`**：一份定义展开 N 个变体（uniapp 三端构建正好是这个形状）；
  `inherits` 让 job 继承公共步骤基座；`--print` 又一个干跑印证。
- **Earthly 显式 artifact 流**：步骤声明"产出哪些文件、消费哪些文件"，引擎获得产物谱系——
  UI 可展示"这个 jar 是哪步产的"，中间产物可自动清理。
- **Airflow 教训**（远期，架构动作大）：线性步骤链 → DAG（独立的 pack 与 upload 并行）；
  固定间隔重试 → 指数退避。等线性链真的痛了再动。

### PM2 / Argo Rollouts —— 部署可靠性

- **PM2 reload vs restart**：restart 硬杀，reload 平滑逐个换新；远端重启步骤值得区分两档
  （至少文档提醒用户）。
- **Argo Rollouts 核心思想**：**部署后健康检查不过 → 自动回滚**。把待做的两块
  （wait 健康探测、Capistrano 回滚）串成闭环——人工回滚按钮是兜底，自动回滚是常态。
- 依赖：wait 三模式 ✅ 已就绪；Capistrano 回滚落地后此项即可做（需先出 releases/current 目录约定设计）。

### 其余一句话

- **direnv 信任模型**（部分 ✅）：运行启动时检测到项目目录已有 .env 会打提醒行
  （其变量将并入 `${{ env.X }}`，非预期请删除）；"信任此文件"式显式确认门仍未做。
- **✅ GHA `continue-on-error` 黄灯汇总**（已实现）：`continue_on_error` / `on_failure: continue`
  的失败步骤不再静默——job 返回 `SucceededWithFailures`（成功结论 + 短语
  `success（N 步失败被跳过: i,j）`，控制台 Job 结束行汇总标注）。

### 优先级

1. ✅ GHA step outputs 运行时变量——已实现（outputs 正则捕获 → ${{ env.X }}，e2e 已验证）；
2. ✅ mise 式工具版本声明 + 偏差显示——已实现（tools_require 校验 + 预览对比，e2e 已验证）；
3. 👀 watch 模式——粗档（变化自动重跑 job）已实现；细档 keep-up-to-date 与传输队列面板未做
   （队列面板需要引擎按文件粒度发事件）；
4. Argo 式自动回滚——wait 三模式 ✅ 已就绪，还差 Capistrano 回滚落地（需先出 releases/current
   目录约定设计，这是当前 4 号的主阻塞）。

## 第四批（便宜五件套 + SQLite 历史，✅ 已实现）

> 来源：v0.4 设计文档盘点出的未实现项 + 用户的 save_history 开关设想。

- **✅ job 级 `on_failure: retry`**（§13.2 P1）：失败后整个流程从头重跑（creates/when 守卫让重跑
  只补失败部分），`job.retry: { max_attempts, backoff_seconds }`（未配置默认 2 次 / 5s）；
  同一 run_id、同一把运行锁；重试退避期可取消。UI 在 on_failure 选 retry 时显示次数/间隔编辑。
- **✅ download 步骤类型**：`远端 -> 本地` 每行一条，本地相对 workspace、父目录自动创建，
  单文件下载后 sha256 校验（transport 层 pscp download 早已就绪，本轮接线）。
- **✅ 桌面通知**：JobEnd 弹 Windows toast（隐藏 PowerShell + WinRT Toast，零 Rust 依赖），
  `config.yaml desktop_notify` 可关；👀 监听模式自动重跑后尤其有用。
- **✅ 危险命令静态检查**（§14 P1）：`rm -rf /`、`chmod -R 777`、`mkfs` 等模式——
  预览 ⚠ + 执行前告警（不拦截，操作者自己判断）。
- **✅ 控制台导出**：💾 导出按钮，存 .log 文件（§9.3 的 [导出] 终于补上）。
- **✅ SQLite 运行历史**（§11.2 P1 + kamal audit 最小形态）：
  - 表：`runs`（run_id/项目/流程/目标/触发/状态/起止/耗时/日志路径/config_snapshot 快照）
    + `step_runs`（每步状态/退出码/耗时，UNIQUE(run,step) 重试覆盖）；
  - **`save_history` 开关**（config.yaml + 设置页勾选框，默认开；关掉完全不建库）；
    保留期复用 `log_retention_days`（启动清理 + 超 24h 仍 running 的残行标 cancelled）；
  - 入库点 = GUI / CLI 的事件转发任务（所有 LogEvent 必经之路）；触发来源 manual/watch/cli；
  - 设置页「🕘 运行历史」tab：最近 100 条列表（时间/项目/流程/状态图标/耗时，悬停看 run_id
    与日志路径）+ 清空按钮；sqlite 为默认 feature，`--no-default-features` 可裁掉。
  - e2e 已验证：CLI run 入库（failed + duration_ms 正确）；修复了 CLI 不发 JobEnd 导致
    行卡 running 的缺陷。
- **✅ README 重写**：修正过期描述（密码认证早已支持）、补齐 CLI preview/--yes 与全部新功能。

### 设计文档盘点出的剩余未做项（第五批候选）

- k8s_watch 轮询（§8.3，需要 k8s 环境验证）；步骤级并行（§12.1，单人桌面暂不痛）；
  ~~OpenSshTransport（P1）~~（v0.5 决策：不实现，被 russh 默认传输取代，见第六批）；
  全局 variables.yaml 兜底（§3.7）；watch 跟随模式增强、
  trace 导出；传输队列面板（需文件粒度事件）；Capistrano 回滚 + Argo 自动回滚（等目录约定设计）。

## 第五批（Spinnaker / Octopus Deploy 参照）

> 背景：参照讨论"Spinnaker / Octopus 是大厂平台，学思路不抄工具"。两家本体均否：
> Spinnaker 要专门养一套平台（政务内网没条件）；Octopus 是商业 Server 软件（内网装不了）。
> 按性价比只吸收了以下几条。

### ✅ 长任务 fire-and-poll（wait fail_exit_code，已实现）

- 痛点：1.5T 备份 / 迁移一跑两小时，`exec` 阻塞挂 plink 上必死——步骤超时杀 plink，
  远端进程收 SIGHUP 一起死；SSH 断一次任务也废。
- 实现：`WaitProbe.fail_exit_code`（command 模式专属）——探测命令返回约定退出码
  （如 2）= 任务已死，**立即失败停止轮询**；未配置保持旧行为（非 0 = 还没就绪继续等）。
  引擎 `engine.rs` wait Command 分支 + UI「失败码」DragValue（0 = 关）；
  单测 `wait_command_fail_exit_code_fails_fast` 覆盖 fail-fast 秒失败 / 未配置等到超时两条路径。
- 完整配方（远端 nohup + status 文件三态 + 轮询间隔 30~60s + grep 退出码陷阱提醒）：
  **docs/长任务配方.md**。

### ✅ OpenSSH 传输硬化参数（P1 起步件，已固化）

- `openssh.rs::build_ssh_args`：`ConnectTimeout=10`（连接黑洞快速失败）/
  `ServerAliveInterval=15`（闲置保活，防 NAT/防火墙掐空闲连接——长任务轮询场景必需）/
  `BatchMode=yes`（等价 plink -batch）；单测锁死三参数必须在场。
- 密码目标构建期即报人话错误（BatchMode 无法传密码）；P1 取舍：密码走 plink、key 走 OpenSSH。
- plink 没有 CLI 保活参数，现状由步骤超时兜底，不专门改。
- v0.5 后续：OpenSshTransport 不再实现，三参数语义移植进 RusshTransport（见第六批）。

### 备份校验三件套（并入 Capistrano 回滚线，未单独做）

- 存在 + 大小>0 + 魔数对（zip `PK` / gzip `1f 8b`），全过才写 status=success——
  样例在 docs/长任务配方.md §三；做 releases/current 回滚时把它定为"备份步骤"的验收约定：
  校验不过的备份等于没有备份，回滚的前提是备份真的可用。

### 多机分批金丝雀（远期记一笔，暂不做）

- Spinnaker 金丝雀的裸机版 = **机器分批**（先 1 台 → 健康检查 → 再铺其余），不需要 LB。
  但我们 Target 是单机模型、当前场景单机为主；真有多台 prod 时再设计目标组
  （hosts + canary + batch_size）+ 分批执行循环（任一批失败即停，不碰剩余机器）。
- 流量百分比金丝雀（split_clients 那套）没有负载均衡就是自欺，不做；
  审批流 / 指标自动判定 / 多环境权限体系同前批结论，不做。
- 单机的安全网 = 长任务配方（备份可校验）+ wait 健康检查 + 回滚（Capistrano 线）。

### ✅ 组运行（选组一次跑完：串行 + 失败即停 + 确认合并开始前，已实现）

- 形态：工具栏「▶ 运行组」（选中流程所属组）+ 项目树分组右键「▶ 运行组」/「↪ 从失败处继续」
  + CLI `run-group <项目名> <组名> [--yes] [--from <流程名>]`——组内 job 按拖拽顺序**串行**执行。
  启动前组级预览（各 job 干跑预览连展：最终命令 / 上传清单 / 将跳过的步骤）+
  一次确认门，**确认词取组内最严的**（全部确认词一次输入包含才放行，
  连跑不降级防线）；job 级 preconditions 照常各自把关（自动断言不打扰人）。
- 规则：任一 job 失败（或被取消）→ 后续不启动（金丝雀"任一批失败即停"）；
  续跑 = GUI 右键「↪ 从失败处继续」（会话内记忆 `last_group_failure`）/ CLI `--from <流程名>`；
  历史里每 job 一条记录、run_id 同前缀（`{项目}-grp-时间戳-序号`）= 同一次组发布（trigger=group）。
- **并行第一版不做**：运行锁是项目级 cancel-in-progress（新 run 接管 → 旧 run 自杀），
  组内并行会互杀，需先改锁模型（组持锁 + 子运行不抢）；且并行对金丝雀有害
  （= 全量发布，爆炸半径没缩小），仅"多个无关服务同发"才正当，等真有需求再设计。
- 实现要点：**引擎零改动**——锁由每次 run_job 获取/释放，串行天然安全（并行才互杀）；
  新增 `src/group.rs` 共享执行循环（GUI/CLI 共用：调度、失败即停、历史入库、日志落盘、
  取消传播），GUI = UiSink 转 UI 通道，CLI = CliSink 直接打印；
  新事件 `GroupJobBegin / GroupJobEnd`（组内单流程结束不发 JobEnd——UI 的 run.finished
  只在组级聚合 JobEnd 时置位，桌面通知显示组标签）；组运行中自动切换选中流程并复位步骤可视化。
- 打包 job → 发布 job 的衔接不发明新机制（outputs 不跨 run 共享）：产物带版本命名
  （`${{ now.date }}`）+ 发布 job preconditions 断言版本对，防拿旧包发。
- 背景讨论结论：分组 = 一次发布的工作集；审核场景（外部系统卡中间，job 必须分开）与
  金丝雀场景（目标不同的多个发布 job）统一为同一模型——job = 一口气能跑完的原子段，
  job 边界 = 人工 / 审核间隔；审核挪到组运行开始前（预览 + 确认）后，串行连跑自然成立。
- 单测：`group_runs_in_order_and_stops_on_failure`（失败即停 + 组边界事件）/
  `group_resume_skips_leading_jobs`（续跑跳过头部）/
  `group_continue_on_error_job_does_not_stop_group`（黄灯不停组）/ confirm_words_match。

### 与现有 TODO 的咬合

1. ✅ fail-fast 已落地（本批唯一引擎改动，长任务轮询的地基）；
2. ✅ 组运行（串行 + 失败即停 + 确认合并开始前）——已实现，金丝雀的第一个具体落点：
   组内目标不同的发布 job 串行走，配合单 job 内 step.target 序列即可裸机金丝雀；
3. 备份校验三件套 → 随 Capistrano 回滚线走（第三批 PM2/Argo 优先级 4 的配套）；
4. OpenSSH 硬化三件套 → 随 v0.5 传输换防语义移植进 russh 配置（OpenSshTransport 本体取消，见第六批）；
5. 目标组 batch_size 循环（引擎级分批）与并行组运行，等真实多机 / 无关服务批量需求再启动。

## 第六批（传输换防：russh 默认 + PuTTY 按目标兜底，✅ 已实现）

> 来源：设计文档 v0.5（2026-10-27）拍板 §19 待确认问题 4「russh 的定位」——结论与原提案相反：
> russh + russh-sftp 从 P3 远期预留升级为**默认传输**，PuTTY 套件转为**按目标可选的兜底**，
> OpenSshTransport（P1 占位）**移除不实现**。
> 动机：免安装单分发（不再要求 PuTTY 套件在位）、SFTP 替代 SCP（远端路径按字面，
> 消除 pscp 的 shell 解释告警）、协议级 keepalive（治 VPN/NAT 掐空闲连接）、
> 结构化回调替代 plink 文本解析；russh 互操作弱一档的风险由 putty 兜底对冲。
> PuTTY 捆绑分发保留（兜底开箱即用 + puttygen 密钥转换依赖它）。
> ⚠ 后续调整（2026-10-28）：**「内置捆绑目录」探测移除**——它解析到开发机仓库路径
> （tools/putty/），各机器行为不一致；putty 套件改只认 ⚙ 自定义目录 → 系统 PATH →
> Program Files，仓库里的捆绑 exe 已 git rm（PPK 转换提示改为「PuTTY 自带 puttygen」）。
> 配套修复：tools.yaml 的 merge_detection 只增改不删，捆绑时代写入的 putty 条目
> 会在目标面板永远刷出来——探测 not found 时现在会清理**自动探测**的 putty 条目
> （detected_from ∈ custom/builtin/PATH/Program Files；手动添加的保留）。
> 设计：docs/RemoteBuildSystem架构设计文档_v0.5.md §5。

### ✅ 1. 数据模型与传输选择器（已实现）

- `Target.transport: Option<TransportKind>`（YAML kebab-case：`russh` / `putty`）：
  - `None`（旧配置缺字段）= 兼容模式**按 putty 执行**（升级前行为不变），UI 提示可切换；
  - 新建目标默认写 `russh`。
- `make_transport` 按目标分支（`effective_transport` 解析缺省）：`russh` → RusshTransport；
  `putty` → `detect_putty_tools()`，找不到套件才报人话错误（含安装/路径指引 +
  「可切 russh 免装套件」提示）——**russh 目标不再因缺 PuTTY 而失败**。
- 目标编辑器加「传输」下拉（russh 默认 / putty）；「测试连接」与 CLI test-conn 走
  `probe_target` 按所选传输分发（russh = 临时 current_thread runtime 承载 probe_server_russh）。
  下拉按套件状态收敛：本机未检测到 PuTTY 且目标不走 putty → 收起下拉显示静态
  「russh（内置）」；目标正走 putty（含旧配置缺省）→ 保留下拉给切换入口，
  putty 选项标注「⚠ 未检测到套件」，旧配置缺省 + 无套件时提示语改为「运行会失败，建议切 russh」。

### ✅ 2. RusshTransport（transport/russh.rs，已实现）

- 依赖：`russh` 0.65 + `russh-sftp` 3.0（设计写 0.64 系，实现时取当期 0.65）；
  crypto 后端用 **ring**（默认的 aws-lc-rs 需要 cmake/nasm，发版机环境不可假设有）；
  rsa feature 保留兼容老服务端 RSA 主机键。
- 进程内实现——无外部进程、无进程树守卫，取消/超时 = drop 连接（远端进程随会话终止）。
- 连接语义与 plink 现状一致：**每步/每作业新建连接，不做透明重连状态机**；
  断连直接失败，落引擎既有兜底（upload 默认重试 1 次 + 完整性校验 + job 级 on_failure: retry）。
- keepalive 30s（无响应 3 次断连）+ 连接超时 10s（openssh.rs `build_ssh_args`
  ConnectTimeout / ServerAliveInterval 语义移植；openssh.rs 已删除，硬化单测迁移进 russh）。
- 主机键 TOFU：`check_server_key` 回调算 SHA256 指纹与目标 host_key 比对；未设置 = 拒连并把
  服务器指纹写进错误信息（填入即可信任）；不匹配 = 明确报「指纹不匹配」。
- **📌 一键登记指纹**（2026-10-28）：连接测试失败时从错误输出自动提取服务器指纹
  （`extract_fingerprint`，russh TOFU 中文文案与 plink 英文输出都覆盖），结果区给出
  「一键登记指纹」按钮——点一下写进「主机键指纹」字段（不用再拖选复制 43 位串）；
  切换目标 / 重新测试即作废旧指纹，防跨主机误回填。
- 认证：password（协议内存参数，不进命令行/日志）/ key（OpenSSH 格式，RSA 自动协商
  最高哈希签名）；**偏差**：agent（Pageant）认证暂不支持——人话指引改用 password/key
  或把该目标切回 putty；PPK 给出 puttygen 转换命令提示（PuTTY 安装自带）。
- 算法：russh 默认集（已拒 SHA-1 类），不额外收紧，`Preferred` 留口子。

### ✅ 3. 传输行为变化（已实现）

- upload / download 走 SFTP（先 `request_subsystem("sftp")` 再建会话）：远端路径按字面处理
  （putty.rs 的「远端路径含空格/shell 特殊字符」告警在 russh 路径不适用）；
  **上传前自动逐级创建远端父目录**（pscp 做不到的增强）；目录上传 = 内容递归合并到
  远端路径（§5.9 目录映射语义）；逐文件字节进度事件可发（传输队列面板的地基，UI 未做）。
- `remote_sha256_batch` 提升为 `RemoteTransport` trait 方法（两条传输各自实现：
  putty 一条 plink / russh 一条 exec 通道），上传预检/上传后校验/下载校验
  统一经 trait 分发——steps.rs 不再直接依赖 putty。
- 上传 zip 打包优化（§5.8）对 SFTP 同样适用，继续保留。

### ✅ 4. OpenSSH 封存（已完成）

- transport/openssh.rs 已删除（P1 占位、全 bail、不支持密码认证）；
  硬化语义（ConnectTimeout=10s / keepalive=30s+max3）移植进 russh Config，
  「三参数必须在场」式单测由 Config 构造 + TOFU/超时行为测试承接。

### ✅ 验收（进程内 SSH 服务端 e2e + 全量单测）

- **e2e 手段**：russh server + russh-sftp server 在 127.0.0.1 起真实 SSH 服务端
  （transport/russh/e2e.rs，SFTP 文件操作 chroot 到临时目录、exec 命令仿真：
  sha256sum 真算 / test -e 真判 / 探测命令给样例输出），固定 ed25519 主机键嵌入测试。
  ⚠ 坑：std listener 转 tokio 必须先 `set_nonblocking(true)` 再 `from_std`——
  阻塞模式 socket 注册进 IOCP 驱动会让握手期整个 runtime 卡死（连定时器都饿死），
  曾导致 e2e 无限挂起，已在 spawn_server 修复并注释锁定。
- ✅ TOFU：未登记拒连（错误含服务器指纹与填入指引）/ 指纹不匹配报错 / 登记后放行；
  ✅ 认证：错误密码拒绝（人话报错）+ 正确密码经 Credential Manager 放行；
  ✅ exec：stdout 逐行转发 + 退出码透传；✅ 上传（单文件 + 嵌套目录自动创建）；
  ✅ sha256 批量（缺失文件部分成功语义与 putty 一致）；✅ 下载内容一致；
  ✅ probe：PROBE_CMD 解析出体检信息（os/host/cores/mem）。
- ✅ putty 目标回归：transport 缺省 → putty 分支单测锁定（`effective_transport`），
  plink/pscp 路径零改动。
- ✅ 无 PuTTY 环境：make_transport 的 russh 分支不探测 PuTTY（单测
  `make_transport_russh_and_local_do_not_require_putty` 锁定）。
- ✅ key 认证 e2e（进程内服务端补齐，无需真实 Linux）：服务端 `auth_publickey`
  只认嵌入主机键的公钥，e2e 复用主机键当用户私钥（免 rand 依赖）；比较用
  `key_data()`——`PublicKey` 的派生 PartialEq 连 PEM comment 一起比，而 comment
  不随 SSH 线上协议传输。PPK 路径 e2e 验证给出 puttygen 转换指引。
- 硬化语义单测迁移落点：`russh::tests::openssh_hardening_semantics_migrated`
  （keepalive 30s / max 3 / 无闲置 GC / 连接超时 10s 四项断言）。
- 全量 `cargo test`：113 通过 0 失败（e2e 含 TOFU 三态 / 密码正反 / key / exec /
  SFTP 上传下载 / sha256 部分成功 / probe）。

## 2026-10-29 UI 批量反馈的设计核验与遗留项

### ✅ k8s 目标「本地 kubectl」设计核验（结论：设计成立，文案已改）

> 反馈：「目标类型是 k8s 的时候 kubectl 是本地的？啥意思？Windows 能装 kubectl 么？
> 装了后能连其他的 k8s 么？是不是设计就有问题？」

- **核验结论：设计没问题，是文案误导。** kubectl 是客户端工具（与 git / mvn 同类），
  「本地执行」指的是 kubectl 进程跑在发版机上，集群操作通过 kubeconfig 发到远端 API Server。
  Windows 完全可以装 kubectl（choco / scoop / 官网下载 exe），装好后只要 kubeconfig 指向
  哪个集群（公网 / 内网均可达即可），就能管理哪个集群——这是 k8s 官方推荐的使用方式。
- 引擎侧实现一致：`TargetType::Kubernetes` 只注入 `KUBECONFIG` 环境变量，
  k8s-apply 步骤经 LocalTransport 执行 kubectl 命令（`engine.rs` §8.4）。
- **已改**：类型下拉文案「Kubernetes（本地 kubectl）」→「Kubernetes（远端集群，经本机
  kubectl + kubeconfig）」，表单补了一段说明；见 `targets.rs`。
- 遗留建议（未做）：
  1. kubectl 未安装时，运行 k8s-apply 步骤目前报原始错误；可在 k8s 目标保存/测试时
     主动探测 kubectl 并给安装指引（对应「重新探测系统工具」已有 kubectl 探测）。
  2. `rollout status` / `rollout undo` 内置化（见上「kubectl」一节，P1.5 k8s_watch 一并做）。

### ✅ 步骤「工具」字段的设计核验（结论：有用但感知弱，已加强说明）

> 反馈：「构建合集第一步工具选 node 里面跑 java -version？第二步工具选无跑 mvn？
> 工具到底有没有用？」

- **工具字段的实际作用（引擎已实现，`engine.rs` §6.5）**：
  1. 该工具的 env 注入本步骤子进程；
  2. 工具 command 是完整路径时，其所在目录前置到本步骤 PATH——命令里的裸名
     （java / mvn / node…）就会解析到这个工具的版本（多版本 JDK 并存时选它）。
  命令本身不变，所以「看起来没作用」——它的作用在"用哪个版本"，不在"跑什么命令"。
- 示例工程核对：② 构建合集第一步是 `tool: java` + `java -version`（一致），与
  tools.yaml 里探测到的 java 对应；若你的 tools.yaml 没有 java 条目，步骤下拉会挂
  ⚠未映射 徽标。若本机示例是旧版本生成的，⚙ 设置 → 重新生成 example 可刷新。
- **建议（本轮未改代码，理由：改语义影响面大）**：保留工具字段作为"版本选择器"；
  后续可考虑在预览里把「本步将用到的工具目录 PATH 前置」打印得更醒目（已有
  `🔧 工具 java → 路径` 一行），以及在步骤卡片上直接显示解析后的真实路径。

### ✅ Stop 两段式重设计（2026-10-30，反馈 #13/#20 的兜底收口）

> 反馈：停止操作是不是还有问题？需不需要重新设计？或者有兜底？

- **链路审计结论**：取消信号（UI broadcast → 引擎 → 三种传输）本身是通的——
  local / putty 子进程挂 Windows Job Object，取消时整树终止；russh 的 exec / upload /
  download 全程 select 取消信号，连接有 10s 硬超时；组运行在流程间检查取消、
  引擎共享取消信道；运行锁 guard 在任务中止（Drop）时自动删锁文件，强制结束后
  新一轮运行可干净接管。
- **真实缺口与修复（两段式停止）**：
  1. 引擎卡在**不可取消的 spawn_blocking 段**（打大包 zip / 大文件 sha256）时，
     取消信号要等阻塞段做完才被消费——表现为「点了停止没反应」。此前 UI 无任何
     反馈，用户不知道引擎在收尾还是没收到。
     → 现在点击 ⏹ 后按钮进入「⏹ 停止中…」+ 转圈状态（引擎终止进程树并收尾）。
  2. 若超过 10 秒仍未收尾 → 按钮升级为「⛔ 强制结束」：abort 引擎任务 + 包装任务、
     复位界面、控制台留下明确记录。本机子进程随 Job Object 句柄关闭整树终止，
     SSH 连接随 future 丢弃断开；组运行未开始的流程不再启动。
     限制：已进入阻塞段的任务本身无法被杀（Rust spawn_blocking 语义），会在后台
     做完后被丢弃（pack / sha256 无子进程，无残留进程风险）。
  3. 「⟳ 更新代码」（git pull）此前**完全没有取消通道**，且 pull 期间工具栏
     无任何状态（容易误点运行/停止）。本轮先把状态亮出来（⏳ 更新代码中，禁用）；
     pull 的中途取消需要给 run_pull 接取消标志，列入待做。
- 停止语义不变：停止 = 终止当前步骤（整树）+ 组运行停掉未开始的后续流程；
  同项目重新运行（GHA concurrency）仍然自动取消旧运行。

### ⚠ 待复现：运行中步骤输入未被禁用 / Stop 按钮灰色（反馈 #12/#13）

- 代码层核对：步骤列表在运行中有两处禁用（`job_editor.rs` ScrollArea 内容
  `ui.disable()` + 本轮补的第二层卡片级 `add_enabled_ui`），egui 的 disabled 会向
  子控件传播；Stop 按钮与运行状态同源（`run_active()`），单实例下二者不可能矛盾
  （Stop 灰 + 步骤闪烁 = run_active() 返回 false 但事件还在上图，代码上没有这条路径）。
- 怀疑方向（需要复现信息）：① 当时的运行是 CLI / 另一个程序实例发起的（GUI 的
  `run_active()` 只看本进程）；② 当时跑的是旧构建（out/app.tar.gz 为 10-26 打包）。
- 下一步：若能复现，记录当时是否用 CLI 跑过、是否开了两个实例；长期可做
  「GUI 检测项目运行锁（~/.remote-build/runs/<项目>.lock）并在界面上显示外部运行中」。

### 其余本轮已修（对应反馈 #1-#23，除上面单列的）

- #1 浏览按钮统一从输入框当前路径出发（`ui::pick_folder`）。
- #2 确认门移到第三行，on_failure: retry 展开重试参数不再挤爆第二行。
- #3 「工具覆盖」按钮更名「🧰 工具管理」与设置页 tab 统一。
- #4 mvn/gradle 版本验证修复（.cmd/.bat 经 cmd /C、按 env PATH 解析真实落点，
  Windows 不再误匹配无扩展名的 sh 脚本；真机回归通过）。
- #5/#6 目标管理标题显示当前项目名；列表/表单 2:8 分栏。
- #7 添加目标不再提供「本地」类型（local 为固定目标）；类型切换不会再有"切出去回不来"；
  测试连接按钮仅 SSH 目标可用。
- #9 传输下拉框恒为下拉（套件缺失只禁用 putty 选项）；目标加载时若配置指向 putty
  而套件不在，自动改 russh 并显式提示（不再静默换实现）。
- #11 DragValue 全部 clip_text 定宽（输入不再把行撑长）；步骤选项拆两行；前置检查两行布局。
- #14 变量速查弹窗补全（时区说明 / targets 字段 / 关键行为 / 组合示例）。
- #15 组右键新增「改名」；「添加任务」统一为「新建流程」；界面文案 作业→流程 统一。
- #16 项目/分组/流程/目标名超长截断 + 悬停看全；左栏限宽 160~360。
- #17/#18 添加分组弹独立「添加分组」窗口（不再是「改名」窗口），提示文案对应调整。
- #19 控制台可折叠（只留标题行）。
- #20/#13 运行/停止合并为单按钮（空闲=▶运行，运行中=⏹停止）；工具栏「运行组」移除
  （组运行只在项目树分组右键触发）；「运行全部」更名为「运行」。
- #21 下载/打包步骤改用与上传一致的映射编辑器（pack 支持排除模式编辑）；
  上传 from 输入框提示缩短、完整说明放悬停。
- #22 步骤新增 ↓ 下移按钮。
- #23 项目头显示 ⎇（Git 仓库）标记，悬停显示代码目录 / 配置目录。

## 2026-10-30 UI 布局稳定性审计与步骤卡片重设计建议

### ✅ 布局稳定性：审计方法 + 已固化的回归测试

> 反馈：能不能把每个页面的每个选项、输入都操作一遍，凡导致布局变化的记录下来统一修改？

- **固化为准入测试**（`job_editor.rs` 布局回归测试，测试 UI 无头渲染真实卡片）：
  - `step_card_layout_stable_under_input` / `wait_probe_row_layout_stable_under_input`：
    同一组开关下，把每个输入换成病态值（超长文本 / 5 位数超时 / 最大枚举值 /
    全部可选项目展开），卡片总宽必须与默认值一致（±2px）。
    任何"输入越长控件越宽"的实现（DragValue 自适应、ComboBox 文本 Extend、
    heading 不截断……）都会让测试变红——**新增步骤选项时把它的病态值加进测试即可**。
  - 测试立刻抓到实锤：步骤卡片第一行自然宽 **956px**，1080p 全屏（左树 220px）
    中央区约 840px——默认值就横向溢出 ~100px，这正是反馈里"挤出去"的根源。
- **静态清点结论（全 UI）与修法约定**：
  | 控件形态 | 布局行为 | 约定 |
  |---|---|---|
  | DragValue | 缺省随内容/输入变宽 | 必须 `clip_text(true)` + `min_size` 盖住最大取值 |
  | ComboBox | 选中文本在水平布局默认 Extend，选中名一长按钮变宽 | 动态选中文本一律 `.truncate()` |
  | Heading/Label 动态文本 | 不截断，撑宽所在行/窗口 | 长名字 `truncate_label` + 悬停看全 |
  | TextEdit 定宽 | 输入超出即内部滚动，布局不变 | 路径/命令类优先定宽或填充 |
  | 窗口内容 | 单行超长文本/monospace 会把窗口横向撑到屏幕外 | 长命令区用双向滚动 |
  本轮按此修掉：动态 heading 2 处（流程名/项目名）、动态下拉 5 处、确认门预览行、
  wait 探测行 2 个 DragValue 补 min_size。
- **步骤卡片行结构（本轮落地）**：第一行 956px 拆成——
  标识行（✓ Step N 名称 类型 从此/仅此/↑/↓/🗑）→ 执行参数行（工具｜超时）→
  内容（命令/映射）→ wait 探测 → 耗时时间线 → 目标｜失败继续｜重试 →
  creates｜notify → 条件。全部行自然宽 ≤720px，布局测试锁死。

### 💡 步骤编辑器重设计建议（待拍板，未实施）

> 反馈：或者重新设计下它的 ui 会不会更好？

- **建议方案：master-detail（列表 + 详情），与 🎯 目标管理 / 🧰 工具管理 同一交互模型**
```
┌─ 步骤(3) ────────────┬─ Step 2 · Maven 构建 ────────────────────────┐
│ ✓ ① 查看版本   exec │  名称   [Maven 构建            ]              │
│ ✓ ② Maven构建  exec │  类型   [exec ▾]   工具 [java ▾]  超时 [300s] │
│ ▶ ③ 上传jar    uplo │  目标   [跟随流程 ▾]  失败继续 ☐  重试 默认    │
│   （拖拽排序/↑↓）    │  creates [target/*.jar]  notify [归档产物]    │
│ ＋ 添加步骤          │  ── 命令 ────────────────────────────────     │
│                      │  mvn -B clean package -DskipTests             │
│                      │  ▶从此  ▶仅此  🗑删除                          │
└──────────────────────┴───────────────────────────────────────────────┘
```
- **为什么更好**：横向空间问题从根上消失（左列表 ~260px，右详情独占全部宽度，
  每个选项独立成行）；与目标/工具管理一致，学习成本归零；步骤多时列表更紧凑
  （一行一步骤）；新增选项不再挤压别处。
- **代价**：失去"所有步骤命令平铺一览"的评审视角（👁 预览可补）；实现量约重写
  `edit_step` 的呈现层（引擎 / YAML 不动），风险中低。
- **不做的方案**：chip 徽标化（仍受 egui 无自动换行限制，治标）；横向滚动
  （可用性差）。
- 若认可，下一轮实施；现行为（分行卡片 + 布局回归测试）已可用且被测试锁住。

## 2026-10-30 性能调优（「小程序不该卡卡的」）

### 审计结论（每帧开销路径逐项核对）

| 开销点 | 结论 | 处理 |
|---|---|---|
| **每帧文件系统 stat** | `validate_project` 在项目树每项目/每帧 + 编辑器每帧各跑一次，每次 stat 本地目录 + `.git`。本地 SSD 上每帧 ~0.5ms（尚可）；**项目目录指向网络盘 / 已断开路径时每次 stat 可阻塞数秒——UI 每帧冻结，这是「卡卡的」的最典型根源** | ✅ 结果缓存进 `MainState.validation`，reload 时算一次，树/编辑器只读缓存 |
| **CJK 字形按需栅格化** | egui 首次显示某段中文时现场栅格化几百字（首开窗口 / 控制台首条新文案顿一下） | ✅ build.rs 从 UI/引擎源码收集 923 个汉字（含 CJK 标点/全角），首帧按 5 种字号屏幕外预热，之后全命中缓存 |
| **控制台渲染** | 已虚拟化（逐帧只排版可视区几十行，行高缓存） | 无需处理 |
| **空闲时 CPU** | 无常驻重绘（重绘只在 运行中50ms / 更新代码150ms / 监听500ms 时安排），无空闲忙轮询 | 无需处理 |
| **编译优化** | `[profile.dev.package."*"] opt-level=3`（依赖带优化，调试构建不发钝）+ `[profile.release] lto=thin, codegen-units=1` 均已配置 | 无需处理 |
| **渲染后端** | 已锁 DX12（RDP + Intel 下 Vulkan 会崩，注释有实测记录） | 无需处理 |
| watch 扫描 / putty 探测 / 本机信息 | 均在后台线程 + 进程内缓存 | 无需处理 |
| ToolMenu 每帧 clone tools 表 | ~10-30 条目，微秒级 | 暂不处理（收益太小） |

### 遗留可选项（未做，按需）

- 运行中重绘 50ms（20fps）：日志滚动流畅度与 GPU 占用折中，嫌闪可调 33ms。
- 首帧预热会把启动时间增加几十 ms（栅格化在后台线程，可感知度低）。
- 步骤极多（>50）时卡片列表滚动性能可再测；master-detail 重设计落地后天然缓解。

## 💡 工具系统重设计（2026-10-30 设计评审，待拍板）

> 背景：工具的概念用着迷茫——步骤选了工具看不出作用、想靠前置检查验证工具又觉得别扭。
> 本文盘点现状、诊断问题、给出「重来」方案。

### 现状盘点（代码位置已核对）

| 组件 | 现状 | 位置 |
|---|---|---|
| 全局工具表 tools.yaml | 首启探测生成（JAVA_HOME / MAVEN_HOME / GRADLE_HOME / NODE_HOME / PATH），机器级 | `env_detect.rs` `config.rs::merge_detection` |
| 项目级 tools（project.yaml） | 键相同**整体覆盖**全局条目 | `engine.rs::run_job_inner` merged_tools.extend |
| 步骤 tool 字段 | 只做两件事：① 该工具 env 注入本步骤子进程；② command 为完整路径时其所在目录前置到本步骤 PATH（命令里的裸名 java/mvn/node 解析到这个版本）。**命令文本本身不变** | `engine.rs::run_single_step` |
| tools_require | mise 风格版本要求，运行前与 **tools.yaml 里的 version 快照** 对比，偏差零步失败 | `engine.rs:214` |
| version_check | 每工具一条验证命令（如 `java -version`），**只有设置页「验证」按钮手动执行**，运行流程从不跑它 | `settings.rs:684` `env_detect.rs` |
| UI | 步骤工具下拉（本项目/全局分组 + 未映射红标）、设置工具管理（增删改/验证/预设）、目标窗口本机工具链只读 + 重新探测 | `job_editor.rs` `settings.rs` `targets.rs` |

### 设计问题诊断（为什么迷茫）

1. **作用不可见**：选工具不改命令，只改 env / PATH——「选了等于没选」的观感（#10 讨论）。
2. **版本是快照不是事实**：tools.yaml 的 version 是探测那一刻写的；之后升级 java、
   换 JAVA_HOME，它都还是旧值。tools_require 拿旧值当门禁——要么误拦要么漏放。
3. **验证能力存在但没接线**：version_check 只在设置页手动按。运行期不验证工具可用性，
   工具坏了要到具体步骤执行报错才发现，报错还是命令级的（难懂）。
4. **两个概念挤在一个字段**：机器上「装了什么」（能力登记）和「这一步用哪个版本」
   （选择）没有分层表达，全局/项目两级覆盖又加了一层。
5. **无契约感**：tool 字段可选且弱约束——不选也常能跑（PATH 恰好有），选错不报错。

### 对「步骤选工具 + 前置检查验证」想法的分析

方向对（工具应该在运行期被证明可用），但机制错位：

- 前置检查的本职是**业务断言**（产物在不在、网络通不通）；让它背工具验证的锅：
  - 每个用 java 的步骤都得手配一条 `java -version`——冗余、易漏、易过期；
  - 前置检查跑在所有步骤之前，工具却是步骤级的——粒度对不上；
  - 工具定义里**已经有 version_check**，再手配前置检查 = 同一信息写两遍。
- 正确机制是**引擎运行期自动自检**（见下），用户配置量为零。
- 保留的合理内核：验证结果要可见（控制台逐条打 ✓），这个自动自检天然满足。

### 重来方案（推荐）：工具 = 发版机能力登记 + 运行期自动自检

原则：步骤只声明「用哪个工具」（可选）；**验证是引擎的职责，不是用户的配置项**。

1. **运行期自动工具自检（核心改动）**
   - 每次运行启动时（前置检查之前），收集本次用到的工具集合：
     步骤 tool ∪ project.tools_require 的键，去重
   - 逐个跑 version_check（复用 `run_version_check_for_tool`，cmd/bat 已修复；
     每 run 每工具只跑一次；每个 ≤3s 超时）
   - 控制台逐条输出 `[自检] java 21.0.3 ✓（version_check）`——工具可用性成为
     运行记录的一部分，与前置检查同段落展示
   - 自检失败 = 零步失败 + 人话：「步骤 3 需要工具 java，验证命令 java -version
     无输出/无法执行：请检查安装，或在 ⚙ 设置 → 🧰 工具管理 重新验证」
   - 无 version_check 的工具退化为「command 解析是否存在」（用 env_detect 的
     解析器）：不存在才失败，存在只打一条 ✓（无版本）
   - **自检结果刷新内存里的 version，tools_require 门禁改用现算值**——快照过期问题消除
2. **前置检查回归本职**：只写业务断言；UI 提示与文档明确分工
   （工具可用性 = 自动自检；产物/网络断言 = 前置检查）
3. **文案统一**：tools.yaml = 「本机工具链（能力登记）」；步骤 tool = 「工具选择
   （版本选择器：env 注入 + PATH 前置，命令文本不变）」；tools_require = 「工具版本要求」
4. **写清作用边界**：工具只影响**发版机本机执行**的步骤；SSH 远端步骤在远端 shell
   执行，不受本机工具表影响（现状即如此，补进悬停与文档）
5. **工具管理页小增强**：列表加「最近验证」列（时间 + ✓/✗）；「验证」按钮与
   「重新探测」保持手动兜底
6. **不做**：步骤级版本钉死（单人场景 tools_require 足够）；archive 类型自动安装
   （本就未实现，归 P2 或砍掉）

### 迁移路径（✅ 2026-10-30 已实施）

1. ✅ 引擎自检阶段：`engine.rs` 运行启动时收集本次用到的工具（步骤 tool 含
   handlers ∪ tools_require 键），逐个现跑 version_check（复用设置页同一套
   cmd/bat 正确解析 + 3s 超时）；无 version_check 降级为 command 可解析检查
   （空 command 视为仅 env 注入跳过）；失败零步失败 + 人话；控制台 `[自检]` 行。
   e2e 4 例：坏 version_check 零步失败 / 现算版本喂给门禁（快照 1.0.0 +
   自检 9.9.9 满足 >=9，旧逻辑会误拦）/ 未定义工具 / 降级路径。
2. ✅ tools_require 改用自检现算的 version（内存刷新，快照仅作展示）。
3. ✅ UI：工具悬停补作用边界（只影响本机步骤）+ 自动自检说明；⚠未映射 文案
   对齐「🧰 工具管理」；工具管理列表加「最近验证」✓/✗（会话态，悬停看输出）；
   预览偏差行标注「运行时以自检现算为准」；本机工具链面板标注快照语义。
4. ✅ 文档同步：变量速查 tools_require 段重写为自动自检语义。

遗留（本轮不做）：version_check 并行执行（当前逐个 ≤3s，工具数少可接受）；
自检结果写回 tools.yaml 缓存。

## 2026-10-30 全面自查（文档 / 代码 / 性能）

### ✅ 发现并修复：settings.rs 全文双重编码损坏

- **现象**：clippy 报 12 个「不可见字符」错误 → 追查发现 `src/ui/settings.rs` 整个文件的
  中文被双重编码（mojibake 固化），设置窗口的中文在 UI 里会全部显示乱码（编译不报错，
  Rust 字符串只管字节——这类问题只能靠扫描/人工发现）。
- **根因**：一次用 python 脚本改 settings.rs 时读写编码不对称（默认 GBK 读 + UTF-8 写）。
  本环境下 `python - <<EOF` 的 heredoc 按 GBK 解码，含中文的批量脚本有编码风险——
  **此后代码修改一律走 Edit 工具或 ASCII-only 脚本**。
- **修复**：从 HEAD 恢复 settings.rs，用 Edit 工具重放两处改动（浏览按钮起点 +
  工具管理「最近验证」列）；全仓扫描（汉字区段 vs 高位字符计数）确认其余源码与
  docs 全部健康。

### ✅ 其余核对结论

- `version.rs` 对「自检现算版本串」的解析有测试覆盖（`openjdk 21.0.3` → 21.0.3），
  mvn/gradle 的首行噪声（分隔线 / "Apache Maven" 前缀）均已容忍——自检喂门禁正确。
- clippy 复扫：0 error；9 个轻微 lint（single-pattern match / large variant 等）
  全部位于历史代码，非本轮引入，暂不扰动。
- build.rs 加固：字形预热字符集写入失败时兜底写空文件（否则 include_str! 会让
  编译直接失败）；修掉一处多余引用 lint。
- README 同步 4 处过时内容：「作业编辑器→流程编辑器」「作业级→流程级」、
  工具栏「运行组」已移除（只剩分组右键）、工具自动自检与控制台折叠补录。

## 2026-10-30 第三轮全面自查（未深读过的核心文件）

### 新覆盖范围（前两轮只深读了近期改动路径，本轮补齐从未审过的核心模块）

- `variables.rs`（${{ }} 变量引擎）：替换 / 默认值 / 条件求值逻辑与测试完备，
  边界（未闭合括号、未知字段、未定义变量按空串）均有文档化与测试——无问题。
- `storage/files.rs`（配置读写）：save_yaml 临时文件 + rename 原子写 ✓；
  双布局发现（jobs/ 优先、根目录兼容、同名去重）有 e2e 测试——无问题。
- `runlock.rs`（跨进程运行锁）：接管语义、陈旧 PID 检测、Drop 归属校验、watcher
  自终止路径全部核对——无问题（force_finish 后 watcher 自行退出也已确认）。
- `cli.rs` / `engine/preview.rs` / `win_job.rs`：CLI 事件协议兼容自检新事件；
  Job Object 整树终止实现正确；预览保持零副作用——无问题。

### ✅ 本轮发现并修复

1. **自检的同步子进程等待跑在 async 上下文**（真性能缺陷）：
   `run_version_check_for_tool` 是同步等待（每工具 ≤3s），此前直接在引擎 future
   里调用——阻塞 tokio worker 线程，工具多时最长达十几秒。改为
   `spawn_blocking` 执行 + 每个工具之间检查取消信号（⏹ 在自检阶段不再失灵）。
2. **自检报错文案换行丢失**（上一轮 python 插入的遗留）：`\` 续行被吞成
   几十个空格的单行长句，控制台显示难看——已恢复为正常多行文案。
3. **CLI preview 过时措辞**：与 GUI 预览对齐（"运行时以自动自检现算值为准并拦截"）。

### 遗留（确认过、暂不动）

- `corepack_available()`（引擎内首次调用）同为同步探测（OnceLock 缓存后不再发生），
  单次 ≤3s 且有缓存，影响可忽略；
- `local_creates_exists` 的 glob 在超大目录下可能占用 worker 数百 ms（fail-open、
  低频路径），暂不处理；
- 其余与前两轮结论一致（clippy 历史轻微 lint、步骤编辑器 master-detail 待拍板等）。

## 2026-10-30 第四轮全面自查（剩余模块全覆盖 + 真机冒烟）

### 新覆盖范围（至此全部源码模块至少审读一遍）

- `pack.rs`：自吞防护（先收集清单再写包）、`/.` 无根语义、符号链接防环（目录链接
  跳过 + 深度上限）、目录剪枝（node_modules 整棵不下钻）、unix_mode 映射——无问题。
- `transport/russh.rs` 实现主体：exec（POSIX 单引号转义 + 远端 cd）、SFTP 上传
  （逐级 mkdir_p + 64KB 分块 + 取消感知 + shutdown 确认）、装箱递归目录上传——无问题。
- `group.rs` 剩余（plan_lines / format_event / confirm_words_match）、`storage/sqlite.rs`
  （WAL + busy_timeout + Arc<Mutex>）、`ui/first_run.rs`——无问题。

### ✅ 真机冒烟（release 二进制实跑，此前四轮均为静态审查）

- `remote-build --help` / `preview example "① Git 合集"` / `② 构建合集` 实跑通过：
  变量解析（${VAR} / ${{ now.iso }}）、版本门禁（java 17.0.6 满足 >=17）、
  when 跳过、outputs 捕获提示全部正常。
- **抓到 #10 反馈的实锤并修复**：用户磁盘上的 example 工程是旧版本代码生成的——
  `jobs/build.yaml` 第一步 `tool: node` 配 `java -version`（这正是当初
  「工具选 node 跑 java -version」困惑的来源，不是代码 bug）。已用官方
  `regen-example` 命令刷新示范工程，复跑 preview 确认显示
  `🔧 工具 java → D:\sdk\Java\jdk-17in\java.exe`。
- 结论：静态审查 + 真机冒烟组合后，已知问题清单清零（遗留项均为「评估过不動」的
  低价值项，见前三轮记录）。

## 2026-10-30 第五轮全面自查（构建组合 + 真实 CLI 端到端）

### 新覆盖维度

1. **`cargo check --no-default-features`**（README 承诺的精简构建，此前从未验证）：
   可编译，但有一批「仅 sqlite 历史入库使用」变量的未使用告警——已全部
   `#[cfg]`/`cfg_attr` 门控修干净，两种 feature 组合现在都是 0 警告。
   （`run_trigger_watch` 的复位特意保持无条件执行——状态复位不能跟 feature 走。）
2. **真实 CLI 端到端**（隔离临时 REMOTE_BUILD_HOME + release 二进制实跑）：
   - 未定义工具 → 自检零步失败 + 人话（exit 1）✓
   - 定义工具 + 故意写旧的快照版本（0.0.1）+ 要求 >=1 → 自检现跑 version_check
     得 1.2.3，门禁以现算值放行——**「快照过期」修复的端到端实锤** ✓
   - 步骤执行 → `E2E_RUN_OK` → `==== success ====` exit 0 ✓
   - SQLite history.db 落库 ✓（CLI 不落日志文件为已知现状，group.rs 有注明）

### 结论

五轮累计：全部源码模块至少审读一遍、两种 feature 组合 0 警告、静态审查 + 冒烟 +
真实端到端三重验证通过。代码库处于可提交状态；静态层面的边际收益已尽，
后续问题应靠真实使用反馈驱动。

## 2026-10-30 第六轮全面自查（最后几个未审段落）

### 新覆盖范围（至此 models serde / putty / russh / sqlite 实现细节全部审毕）

- `models.rs` 手写 serde（TargetType / AuthMethod 双形态：字符串或单键映射）：
  反序列化校验报错清晰（未知键 / 多键 / 类型错都有人话），roundtrip 测试覆盖
  project / step 新字段 / global config——无问题。
- `putty.rs` 传输实现：exec（POSIX cd 转义 + env 只进本地 plink）、pscp 上传
  （-r -p + risky-path 预警：空格 / shell 特殊字符上传前 stderr 提醒）、
  Job Object 挂载——无问题。
- `storage/sqlite.rs` 全文：WAL + busy_timeout + user_version 轻量迁移、
  参数化 SQL（无拼接注入面）、每方法独立容错（库满/锁失败不阻塞运行）、
  录制事件按 steps_len 过滤（handlers/preflight 超界不入库）——无问题。

### ✅ 本轮发现并修复

- **putty.rs 模块头自相矛盾**：头部安全红线写「禁止 -pw 明文传密码」，但实现
  （password 认证经 `plink -pw`）与 README 都是明确采用并注明暴露窗口取舍的。
  属旧版红线文档没跟上实现——已改写为与实现、README 一致的准确描述。

### 总结论

六轮自查全部源码模块审读完毕 + 三重动态验证（冒烟 / 端到端 / 双 feature 构建）。
静态审查可发现的问题已清零；此后的质量提升应来自真实使用反馈而非继续读代码。
