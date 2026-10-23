# TODO

## 超时机制问题（待处理）

> 现象：大项目编译 / 首次拉依赖容易触发单步超时（默认 300s），超时后 UI 显示作业已结束（timeout），
> 但 mvn / java / pnpm 等实际构建进程**没有被杀掉**，变成孤儿进程继续在后台跑，
> 可能占用 target/ 目录、文件锁，影响下一次运行。

### 问题 1：超时 / 取消都杀不掉进程树（bug，优先）

- 原因 A（超时路径）：`engine.rs` `run_step_with_retry` 里超时靠 `tokio::time::timeout(remaining, fut)` 实现，
  超时后直接 drop future 返回 `Timeout`；而 `transport/local.rs` spawn 子进程时没设 `kill_on_drop(true)`，
  tokio 的 `Child` drop 默认不杀进程。
- 原因 B（取消路径同样中招）：`local.rs` `wait_with_cancel` 的 `child.kill()` 只杀最外层 `cmd.exe` 一层，
  Windows 上 kill 不级联杀进程树，`cmd /C mvn package` 起的 `java.exe` / `node.exe` 会存活。
- 修复方向：
  1. spawn 时加 `cmd.kill_on_drop(true)` —— 至少保证 drop 时杀掉直接子进程（改动最小）；
  2. 彻底方案：Windows Job Object（`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`，可用 `win32job` crate），
     把子进程挂到 job 对象，超时 / 取消时整棵树终止；或 kill 时改用 `taskkill /PID <pid> /T /F`。
  3. 远端 plink / pscp 步骤如有同样问题，需在 putty.rs 侧一并确认。

### 问题 2：死配置 —— job.timeout 与 config default_timeout 没接线

- `Job.timeout`（"整个 job 超时"）和 `config/config.yaml.template` 的 `default_timeout: 600`
  目前引擎里都没有使用；实际生效的只有步骤级 `step.timeout`（默认 `DEFAULT_STEP_TIMEOUT = 300`，
  见 `engine.rs`）。
- 处理：要么接线实现 job 级总超时，要么从 model / 模板里删掉，避免"配了不生效"的误导。

### 问题 3：超时形式粗糙（优化项）

- deadline 在重试循环**外**计算（`engine.rs` `run_step_with_retry`），重试也吃同一个总时长：
  upload 步骤失败默认重试 1 次，等于每次尝试平均只有一半预算。
- 固定总时长分不清"慢但在动"和"卡死不动"：编译 / 拉依赖 legitimately 慢会被误杀；
  上传 / 下载更合理的是**空闲超时**（N 秒无任何输出 / 进度才判失败），而非总时长。
- 短期缓解（不改代码）：容易慢的步骤在 build.yaml 显式配大 `timeout:`（如 1800）；
  `wait` 步骤受同一总时长管，轮询探测别配太长的总时限。

## Ansible 功能参考（待规划）

> 背景：项目形态与 Ansible 同构——build.yaml ≈ playbook、targets ≈ inventory、
> ${{ }} 变量 ≈ Jinja2、exec/upload/pack 步骤 ≈ 模块。以下按性价比排序，
> 只抄单人桌面场景用得上的，不抄集群编排（forks 并行、roles/collection 生态、期望状态模块体系）。

### 1. 条件跳过：`when` / `creates`（优先，成本低，收益最大）

- 现状：步骤纯命令式，一条流程重跑就是全部重来。
- `creates:`：步骤声明一个本地/远端路径，路径已存在则跳过。
  例：「拉依赖」creates `node_modules`、「编译」creates `target/*.jar`，重跑时秒过。
- `when:`：条件表达式为假则跳过。例：`when: ${{ variables.ENV }} == prod`，
  生产专属步骤与通用流程合到一个 job，不必拆两份 build.yaml。
- 改动点集中在 `engine.rs` 执行循环（步骤执行前判断）。

### 2. check mode 干跑预览

- 参照 `ansible-playbook --check --diff`：只展示将要做什么，不真执行。
- GUI 加「预览」按钮：展示变量全量解析后的最终命令、待上传文件列表、将跳过的步骤。
- 变量系统已全量解析，实现只是"把执行换成展示"；顺带当变量拼错检查器用。

### 3. handlers + notify（变更才触发）

- 参照 Ansible「改了配置才 notify 重启服务」：play 结尾统一执行被通知到的 handler。
- 映射到本项目：**只有这次真的传了新 artifact，才执行远端重启脚本**——
  artifact 没变就跳过重启，避免每次跑流程都打断线上服务。

### 4. block / rescue / always（try-catch-finally）

- 一组步骤声明"失败走 rescue 分支，无论成败 always 都执行"。
- 映射到本项目：上传前备份旧包、失败回滚、无论如何删远端临时目录等**清理步骤**。
- 与 P1 已排的 job 级 `on_failure` 是一家人，适合一起设计；
  前置依赖：先解决上面"问题 1"孤儿进程问题。

### 5. 步骤 tags 子集执行

- 参照 `--tags deploy` 只跑带标签的步骤；job 越长（build→pack→upload→远端发布），
  "只重跑后半段"越是高频需求。与 `creates` 粒度互补（tags 粗、按段；creates 细、按状态）。

### 6. 变量默认值语法

- 参照 Jinja2 `{{ var | default('x') }}`；现在 ${{ variables.X }} 未定义直接报错。
- 加 `${{ variables.X || 默认值 }}` 之类语法，共享 build.yaml 模板跨项目复用性更好。
- 改动点：`variables.rs` 解析器。

### 7. facts 缓存（优先级低）

- 参照 gather_facts + fact cache：对 target 探测远端信息（java 版本、磁盘剩余），
  缓存后供变量与条件引用，如 `when: disk_free > 1G` 的部署前守卫。
- 本机首启环境探测已有，此项是其远端版。
