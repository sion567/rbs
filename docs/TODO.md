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
