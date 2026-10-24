//! 命令行辅助子命令（GUI 之外的无头操作）：
//!
//! - `remote-build set-password <目标名> <密码>`  写入 Windows Credential Manager
//! - `remote-build test-conn <目标名>`            SSH 连接测试
//! - `remote-build run <项目名> <流程名>`         无头执行一条流程（控制台实时输出）
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
        Some("preview") => {
            preview(args.get(1..).unwrap_or_default());
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
         remote-build preview <项目名> <流程名>        干跑预览（展示解析后的命令，不执行）"
    );
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
    match crate::transport::putty::probe_server(&t) {
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
    let history_meta = crate::storage::sqlite::RunMeta {
        run_id: format!(
            "{}-{}",
            entry.project.name,
            chrono::Local::now().format("%Y%m%d-%H%M%S")
        ),
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
            }
        }
        #[cfg(feature = "sqlite")]
        if let Some(rec) = recorder {
            rec.close().await;
        }
    });

    let (cancel_tx, cancel_rx) = broadcast::channel(4);
    let engine = Engine;
    let run_id = format!(
        "{}-{}",
        entry.project.name,
        chrono::Local::now().format("%Y%m%d-%H%M%S")
    );
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
