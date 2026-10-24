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

### 问题 3：超时形式粗糙（优化项，未做）

- deadline 在重试循环**外**计算（`engine.rs` `run_step_with_retry`），重试也吃同一个总时长：
  upload 步骤失败默认重试 1 次，等于每次尝试平均只有一半预算。
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

### 3. handlers + notify（变更才触发，未做；地基已就绪 ✅）

- 参照 Ansible「改了配置才 notify 重启服务」：play 结尾统一执行被通知到的 handler。
- 映射到本项目：**只有这次真的传了新 artifact，才执行远端重启脚本**——
  artifact 没变就跳过重启，避免每次跑流程都打断线上服务。
- 地基：upload 步骤已实现"本地/远端 sha256 比对，未变更跳过传输"
  （见「其他工具功能参考 · Task」✅ 条目），变更判定可直接复用。

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
- **healthcheck 三要素 `test / interval / retries`** + `depends_on: condition: service_healthy`：
  `wait` 步骤照此丰富为 tcp / http-200 / 命令三种探测模式（Ansible wait_for 同款），
  代替裸 sleep 轮询。
- 默认值插值一项已用 `||` 语法实现（见 Ansible 一节 ✅ 6），不再单列。

### rsync（upload/pack 步骤的参照物）

- **`--exclude` 排除模式**：pack/upload 大前端产物时排除 node_modules、.map 等，
  实现便宜（直接采用 rsync 的 glob 语义）。
- **`--dry-run` 输出形态**：列出"会传哪些、跳过哪些"——已实现的干跑预览（✅ 2）
  后续增强上传清单展示时可参照此形态。
- 已知差距：pscp 整文件复制，rsync 有增量传输；大文件高频场景在 P1 换 OpenSSH
  transport 后可评估 sftp / rsync 类方案。

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
