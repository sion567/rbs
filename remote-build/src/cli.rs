//! 命令行辅助子命令（GUI 之外的无头操作）：
//!
//! - `remote-build set-password <目标名> <密码>`  写入 Windows Credential Manager
//! - `remote-build test-conn <目标名>`            SSH 连接测试
//! - `remote-build run <项目名> <流程名>`         无头执行一条流程（控制台实时输出）
//! - `remote-build regen-example`                 重新生成示范工程 example（当参考文档用）
//!
//! 便于脚本化与 CI 使用；GUI 功能与 CLI 共用同一套引擎/配置。

use crate::config;
use crate::models::{Target, TargetType};
use crate::engine::{Engine, RunInput};
use crate::models::LogEvent;
use anyhow::Context as _;
use tokio::sync::{broadcast, mpsc};

pub fn dispatch(args: &[String]) -> Option<()> {
    match args.first().map(String::as_str) {
        Some("set-password") => {
            set_password(args.get(1..).unwrap_or_default());
            Some(())
        }
        Some("test-conn") => {
            test_conn(args.get(1..).unwrap_or_default());
            Some(())
        }
        Some("run") => {
            run(args.get(1..).unwrap_or_default());
            Some(())
        }
        Some("run-group") => {
            run_group(args.get(1..).unwrap_or_default());
            Some(())
        }
        Some("preview") => {
            preview(args.get(1..).unwrap_or_default());
            Some(())
        }
        Some("regen-example") => {
            regen_example();
            Some(())
        }
        Some("help" | "--help" | "-h") => {
            print_help();
            Some(())
        }
        _ => None,
    }
}

fn print_help() {
    println!(
        "用法:\n  \
         remote-build                                  启动图形界面\n  \
         remote-build set-password <目标名> <密码>     写入系统凭据管理器（不落盘）\n  \
         remote-build test-conn <目标名>               SSH 连接测试\n  \
         remote-build run <项目名> <流程名> [--yes]    无头执行一条流程（--yes 跳过确认门）\n  \
         remote-build run-group <项目名> <组名> [--yes] [--from <流程名>]\n  \
         [...]                                         组运行：组内流程串行·失败即停（--from 从指定流程续跑）\n  \
         remote-build preview <项目名> <流程名>        干跑预览（展示解析后的命令，不执行）\n  \
         remote-build regen-example                   重新生成示范工程 example（覆盖四本合集 yaml）"
    );
}

/// 重新生成示范工程：与 ⚙ 设置里的按钮同一入口，升级 / 改造示例后刷新参考
fn regen_example() {
    match config::create_example_project() {
        Ok(()) => println!(
            "✅ 已重新生成示范工程: {}（git / build / linux / deploy 四本合集）",
            config::projects_dir().join("example").display()
        ),
        Err(e) => {
            eprintln!("❌ {e:#}");
            std::process::exit(1);
        }
    }
}

fn set_password(args: &[String]) {
    if args.len() != 2 {
        eprintln!("用法: remote-build set-password <目标名> <密码>");
        std::process::exit(2);
    }
    match crate::secrets::set_password(&args[0], &args[1]) {
        Ok(()) => println!("✅ 密码已写入 Windows Credential Manager（目标: {}）", args[0]),
        Err(e) => {
            eprintln!("❌ {e:#}");
            std::process::exit(1);
        }
    }
}

fn test_conn(args: &[String]) {
    let Some(name) = args.first() else {
        eprintln!("用法: remote-build test-conn <目标名>");
        std::process::exit(2);
    };

    // 查找顺序：遍历各项目的项目级 targets（§3.5 目标随项目走）
    let mut found: Option<(String, Target)> = None;

    let (projects, _) =
        crate::storage::files::discover_with_errors(&config::projects_dir());
    for p in projects {
        if let Some(t) = p.project.targets.get(name) {
            let mut t = t.clone();
            t.name = name.clone();
            t.cred_key = Some(format!("{}/{name}", p.project.name));
            found = Some((p.project.name.clone(), t));
            break;
        }
    }

    let Some((owner, t)) = found else {
        eprintln!("❌ 未找到目标: {name}（所有项目均已查找）");
        std::process::exit(2);
    };
    if matches!(t.target_type, TargetType::Local) {
        println!("ℹ {name} 是本地目标（本机执行），无需连接测试");
        return;
    }
    let owner_note = if owner.is_empty() {
        String::new()
    } else {
        format!("（{owner}）")
    };
    println!("测试连接 {name}{owner_note} ...");
    match crate::transport::probe_target(&t, None) {
        Ok((info, elapsed)) => println!("✅ {}", info.display(elapsed)),
        Err(e) => {
            eprintln!("❌ {e}");
            std::process::exit(1);
        }
    }
}

fn run(args: &[String]) {
    let yes_flag = args.get(2).map(|s| s.as_str() == "--yes").unwrap_or(false);
    if args.len() != 2 && !(args.len() == 3 && yes_flag) {
        eprintln!("用法: remote-build run <项目名> <流程名> [--yes]");
        std::process::exit(2);
    }
    let (project_name, job_name) = (args[0].clone(), args[1].clone());

    let (projects, warnings) = crate::storage::files::discover_with_errors(&config::projects_dir());
    for w in &warnings {
        eprintln!("⚠ {w}");
    }
    let entry = projects
        .into_iter()
        .find(|p| p.project.name == project_name)
        .context(format!("未找到项目: {project_name}"));
    let entry = match entry {
        Ok(e) => e,
        Err(e) => {
            eprintln!("❌ {e:#}");
            std::process::exit(2);
        }
    };
    let Some(je) = entry.jobs.iter().find(|j| j.job.name == job_name).cloned() else {
        eprintln!(
            "❌ 项目 {project_name} 下未找到流程: {job_name}（现有: {:?}）",
            entry.jobs.iter().map(|j| j.job.name.clone()).collect::<Vec<_>>()
        );
        std::process::exit(2);
    };

    // 人工确认门（job.confirm）：交互终端输入确认词；非交互终端必须显式 --yes
    let expected = je.job.confirm.as_deref().map(str::trim).filter(|s| !s.is_empty());
    if let Some(expected) = expected {
        use std::io::IsTerminal as _;
        if yes_flag {
            println!("ℹ 已跳过确认门（--yes，确认词: {expected}）");
        } else if !std::io::stdin().is_terminal() {
            eprintln!("❌ 流程设置了确认门（需输入「{expected}」）：非交互终端请追加 --yes");
            std::process::exit(1);
        } else {
            print!("🔒 确认门：输入「{expected}」以继续执行: ");
            use std::io::Write as _;
            std::io::stdout().flush().ok();
            let mut line = String::new();
            let _ = std::io::stdin().read_line(&mut line);
            if line.trim() != expected {
                eprintln!("❌ 确认词不匹配，已取消");
                std::process::exit(1);
            }
        }
    }

    let tools = config::load_tools().map(|t| t.tools).unwrap_or_default();
    let all_targets = config::effective_targets(&entry.project);

    let rt = tokio::runtime::Runtime::new().expect("创建 tokio runtime 失败");
    let (tx, mut rx) = mpsc::channel(8192);
    let save_history = config::load_global()
        .map(|g| g.save_history)
        .unwrap_or(true);
    let steps_len = je.job.steps.len();
    let snapshot = yaml_serde::to_string(&je.job).unwrap_or_default();
    // run_id 只取一次：历史入库与引擎 ${{ run.id }} 必须一致（分两次取时间跨秒会错开）；
    // 精确到毫秒，避免同秒重跑与 GUI/CLI 并发时日志同名、SQLite UNIQUE 冲突
    let run_id = format!(
        "{}-{}",
        entry.project.name,
        chrono::Local::now().format("%Y%m%d-%H%M%S%3f")
    );
    #[cfg(feature = "sqlite")]
    let history_meta = crate::storage::sqlite::RunMeta {
        run_id: run_id.clone(),
        project: entry.project.name.clone(),
        job: je.job.name.clone(),
        target: je.job.target.clone(),
        trigger: "cli",
        log_path: None,
    };
    let printer = rt.spawn(async move {
        #[cfg(feature = "sqlite")]
        let recorder = if save_history {
            crate::storage::sqlite::RunRecorder::start(&history_meta, &snapshot, steps_len).await
        } else {
            None
        };
        while let Some(ev) = rx.recv().await {
            #[cfg(feature = "sqlite")]
            if let Some(rec) = &recorder {
                rec.on_event(&ev).await;
            }
            print_cli_event(&ev);
        }
        #[cfg(feature = "sqlite")]
        if let Some(rec) = recorder {
            rec.close().await;
        }
    });

    let (cancel_tx, cancel_rx) = broadcast::channel(4);
    let engine = Engine;
    let input = RunInput {
        project: entry.project.clone(),
        job: je.job.clone(),
        tools,
        all_targets,
        from_step: 0,
        only_step: None,
        run_id,
        default_step_timeout: config::load_global()
            .map(|g| g.default_timeout)
            .unwrap_or(crate::engine::DEFAULT_STEP_TIMEOUT),
        // 传取消发送端 = 启用同项目运行互斥（新运行自动取消旧运行）
        cancel_tx: Some(cancel_tx),
    };

    let started = std::time::Instant::now();
    let end_tx = tx.clone();
    let cli_run_id = input.run_id.clone();
    let status = rt.block_on(engine.run_job(input, cancel_rx, tx));
    // CLI 没有外层包装任务：这里补发 JobEnd（历史入库收尾 / 与 GUI 事件协议对齐）
    let _ = rt.block_on(end_tx.send(LogEvent::JobEnd {
        run_id: cli_run_id,
        status: status.clone(),
        total_duration_ms: started.elapsed().as_millis() as u64,
    }));
    drop(end_tx); // 通道关闭 → printer 循环退出
    let _ = rt.block_on(printer);
    println!("==== {} ====", status.short());
    if !status.ok() {
        std::process::exit(1);
    }
}

/// CLI 控制台事件打印（run / run-group 共用）
fn print_cli_event(ev: &LogEvent) {
    match ev {
        LogEvent::Stdout { line, .. } => println!("{line}"),
        LogEvent::Stderr { line, .. } => eprintln!("[stderr] {line}"),
        LogEvent::StepStart { step_index, name, .. } => {
            println!("── Step {} ── {name}", step_index + 1)
        }
        LogEvent::StepEnd { step_index, exit_code, duration_ms } => {
            println!("── Step {} 退出码 {exit_code} ({duration_ms}ms)", step_index + 1)
        }
        // 退避等待期已有失败告警行，不重复输出
        LogEvent::StepRetryWait { .. } => {}
        LogEvent::Error { message } => eprintln!("[error] {message}"),
        LogEvent::JobEnd { .. } => {}
        // 组运行边界（run-group）
        LogEvent::GroupJobBegin { job, index, total, .. } => {
            println!("──── 组运行 {index}/{total} ── 启动流程 {job} ────")
        }
        LogEvent::GroupJobEnd { job, status, duration_ms, .. } => {
            println!("──── 组流程 {job}: {}（{duration_ms}ms）────", status.short())
        }
    }
}

/// run-group 的组级事件出口：直接打印（历史入库与日志落盘在 runner 内）
struct CliSink;

#[async_trait::async_trait]
impl crate::group::GroupSink for CliSink {
    async fn on_event(&self, ev: LogEvent) {
        print_cli_event(&ev);
    }
}

/// `remote-build run-group <项目名> <组名> [--yes] [--from <流程名>]`：
/// 组内流程按项目树顺序串行执行，任一失败即停（金丝雀分批语义）；
/// 确认门统一收口——组内（--from 则剩余）流程的全部确认词一次输入包含才放行。
fn run_group(args: &[String]) {
    let mut yes_flag = false;
    let mut from_name: Option<String> = None;
    let mut positional: Vec<&String> = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--yes" => yes_flag = true,
            "--from" => {
                from_name = Some(it.next().context("--from 需要流程名").cloned().unwrap_or_default());
            }
            _ => positional.push(a),
        }
    }
    if positional.len() != 2 {
        eprintln!("用法: remote-build run-group <项目名> <组名> [--yes] [--from <流程名>]");
        std::process::exit(2);
    }
    let (project_name, group_name) = (positional[0].clone(), positional[1].clone());

    let (projects, warnings) = crate::storage::files::discover_with_errors(&config::projects_dir());
    for w in &warnings {
        eprintln!("⚠ {w}");
    }
    let entry = match projects.into_iter().find(|p| p.project.name == project_name) {
        Some(e) => e,
        None => {
            eprintln!("❌ 未找到项目: {project_name}");
            std::process::exit(2);
        }
    };
    let jobs: Vec<crate::group::GroupJob> = entry
        .jobs
        .iter()
        .filter(|je| je.job.group.as_deref() == Some(group_name.as_str()))
        .map(|je| crate::group::GroupJob { name: je.job.name.clone(), job: je.job.clone() })
        .collect();
    if jobs.is_empty() {
        eprintln!("❌ 项目 {project_name} 下没有分组「{group_name}」或分组为空");
        std::process::exit(2);
    }
    let from = match &from_name {
        None => 0,
        Some(n) => match jobs.iter().position(|gj| &gj.name == n) {
            Some(i) => i,
            None => {
                eprintln!(
                    "❌ --from 流程不在组内: {n}（组内现有: {:?}）",
                    jobs.iter().map(|gj| gj.name.clone()).collect::<Vec<_>>()
                );
                std::process::exit(2);
            }
        },
    };

    let tools = config::load_tools().map(|t| t.tools).unwrap_or_default();
    let all_targets = config::effective_targets(&entry.project);
    let default_timeout = config::load_global()
        .map(|g| g.default_timeout)
        .unwrap_or(crate::engine::DEFAULT_STEP_TIMEOUT);
    let save_history = config::load_global().map(|g| g.save_history).unwrap_or(true);

    // 组级预览：剩余流程干跑连展（与 GUI 确认门同一份内容）
    println!(
        "==== 组运行 {}/{}（{} 个流程，串行·失败即停{}）====",
        project_name,
        group_name,
        jobs.len() - from,
        if from > 0 { format!("，从第 {} 个流程继续", from + 1) } else { String::new() }
    );
    for line in crate::group::plan_lines(&entry.project, &jobs[from..], &tools, &all_targets, default_timeout) {
        println!("{line}");
    }

    // 确认门：剩余流程的全部确认词（非交互终端必须 --yes）
    let mut distinct: Vec<String> = Vec::new();
    for gj in &jobs[from..] {
        if let Some(w) = gj.job.confirm.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            if !distinct.iter().any(|x| x == w) {
                distinct.push(w.to_string());
            }
        }
    }
    if !distinct.is_empty() {
        use std::io::IsTerminal as _;
        if yes_flag {
            println!("ℹ 已跳过确认门（--yes，确认词: {}）", distinct.join(" "));
        } else if !std::io::stdin().is_terminal() {
            eprintln!(
                "❌ 组内流程设置了确认门（需包含: {}）：非交互终端请追加 --yes",
                distinct.join(" ")
            );
            std::process::exit(1);
        } else {
            print!("🔒 组确认门：输入确认词（需包含: {}）: ", distinct.join(" "));
            use std::io::Write as _;
            std::io::stdout().flush().ok();
            let mut line = String::new();
            let _ = std::io::stdin().read_line(&mut line);
            if !crate::group::confirm_words_match(&line, &distinct) {
                eprintln!("❌ 确认词不匹配，已取消");
                std::process::exit(1);
            }
        }
    }

    let rt = tokio::runtime::Runtime::new().expect("创建 tokio runtime 失败");
    // run_id 只取一次：成员流程 run_id = {group_id}-{序号}，历史同前缀 = 同一次组发布
    let group_id = format!(
        "{}-grp-{}",
        project_name,
        chrono::Local::now().format("%Y%m%d-%H%M%S%3f")
    );
    let agg_run_id = group_id.clone();
    let (cancel_tx, _keepalive) = broadcast::channel(4);
    let spec = crate::group::GroupSpec {
        project: entry.project.clone(),
        group: group_name.clone(),
        jobs,
        tools,
        all_targets,
        default_step_timeout: default_timeout,
        group_id,
        cancel_tx: Some(cancel_tx),
        from_job: from,
        save_history,
        log_dir: None,
        trigger: "group",
    };
    let sink = CliSink;
    let started = std::time::Instant::now();
    let outcome = rt.block_on(crate::group::run_group_serial(spec, &sink));
    let agg = outcome.status();
    rt.block_on(crate::group::GroupSink::on_event(
        &sink,
        LogEvent::JobEnd {
            run_id: agg_run_id,
            status: agg.clone(),
            total_duration_ms: started.elapsed().as_millis() as u64,
        },
    ));
    let summary: Vec<String> = outcome
        .results
        .iter()
        .map(|(name, st)| format!("{name}={}", st.short()))
        .collect();
    println!("==== 组运行结束: {}（{}）====", agg.short(), summary.join(", "));
    if !outcome.ok() {
        std::process::exit(1);
    }
}

/// `remote-build preview <项目名> <流程名>`：干跑预览（Ansible --check 思路），
/// 展示变量全量解析后的最终命令 / 上传清单 / 将跳过的步骤，不执行任何命令。
fn preview(args: &[String]) {
    if args.len() != 2 {
        eprintln!("用法: remote-build preview <项目名> <流程名>");
        std::process::exit(2);
    }
    let (project_name, job_name) = (args[0].clone(), args[1].clone());

    let (projects, warnings) = crate::storage::files::discover_with_errors(&config::projects_dir());
    for w in &warnings {
        eprintln!("⚠ {w}");
    }
    let Some(entry) = projects.into_iter().find(|p| p.project.name == project_name) else {
        eprintln!("❌ 未找到项目: {project_name}");
        std::process::exit(2);
    };
    let Some(je) = entry.jobs.iter().find(|j| j.job.name == job_name) else {
        eprintln!("❌ 项目 {project_name} 下未找到流程: {job_name}");
        std::process::exit(2);
    };

    let input = RunInput {
        project: entry.project.clone(),
        job: je.job.clone(),
        tools: config::load_tools().map(|t| t.tools).unwrap_or_default(),
        all_targets: config::effective_targets(&entry.project),
        from_step: 0,
        only_step: None,
        run_id: String::new(),
        default_step_timeout: config::load_global()
            .map(|g| g.default_timeout)
            .unwrap_or(crate::engine::DEFAULT_STEP_TIMEOUT),
        cancel_tx: None,
    };

    let previews = crate::engine::preview_job(&input);
    println!("==== 预览（干跑，不执行）: {project_name}/{job_name} ====");
    if let Some(c) = je.job.confirm.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        println!("🔒 确认门: 运行前需输入「{c}」");
    }
    for pre in &je.job.preconditions {
        let msg = if pre.message.trim().is_empty() {
            String::new()
        } else {
            format!("（失败: {}）", pre.message.trim())
        };
        println!("前置检查: {}{msg}", pre.check);
    }
    if !je.job.handlers.is_empty() {
        let names = je
            .job
            .handlers
            .iter()
            .map(|h| h.name.clone().unwrap_or_else(|| "（未命名）".into()))
            .collect::<Vec<_>>()
            .join(", ");
        println!("handlers（变更才触发）: {names}");
    }
    // 工具版本要求偏差（mise 风格）
    {
        let tools = config::load_tools().map(|t| t.tools).unwrap_or_default();
        let mut reqs: Vec<(&String, &String)> = entry.project.tools_require.iter().collect();
        reqs.sort();
        for (name, req) in reqs {
            let detected = tools.get(name).and_then(|t| t.version.clone()).unwrap_or_default();
            match crate::version::version_matches(req, &detected) {
                Ok(true) => println!("工具版本: {name} {detected} 满足 {req} ✓"),
                Ok(false) => println!(
                    "⚠ 工具版本偏差: {name} 需要 {req}，探测到 {detected}（运行将被拦截）"
                ),
                Err(e) => println!("⚠ {name}: {e}"),
            }
        }
    }
    let (mut exec_n, mut skip_n) = (0usize, 0usize);
    for p in &previews {
        let tgt = p.target.clone().unwrap_or_else(|| "local".to_string());
        if p.skipped {
            skip_n += 1;
            println!("[{}] {}  ⏭ 跳过：{}", p.index + 1, p.name, p.note.as_deref().unwrap_or(""));
        } else {
            exec_n += 1;
            println!("[{}] {}  目标: {}", p.index + 1, p.name, tgt);
            if !p.command.is_empty() {
                println!("    $ {}", p.command);
            }
            if let Some((tname, tcmd)) = &p.tool {
                println!("    🔧 工具 {tname} → {tcmd}（其目录前置 PATH，裸名命令用这个版本）");
            }
            for (l, r, exists) in &p.uploads {
                println!("    ↑ {l} -> {r}{}", if *exists { "" } else { "（本地缺失 ⚠）" });
            }
            if let Some((s, d)) = &p.pack {
                println!("    ⧉ 打包 {s} -> {d}");
            }
            for (k, pat) in &p.outputs {
                println!("    ⇩ 捕获输出 {k} ← /{pat}/");
            }
            if !p.notify.is_empty() {
                println!("    ⇮ notify: {}", p.notify.join(","));
            }
            if let Some(w) = &p.wait_note {
                println!("    ⏲ 探测: {w}");
            }
        }
        for w in &p.warnings {
            eprintln!("    ⚠ {w}");
        }
    }
    println!("==== 预览结束：{exec_n} 步执行 / {skip_n} 步跳过 ====");
}
