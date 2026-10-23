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
         remote-build run <项目名> <流程名>            无头执行一条流程"
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
    match crate::transport::putty::test_connection(&t) {
        Ok(msg) => println!("✅ {msg}"),
        Err(e) => {
            eprintln!("❌ {e}");
            std::process::exit(1);
        }
    }
}

fn run(args: &[String]) {
    if args.len() != 2 {
        eprintln!("用法: remote-build run <项目名> <流程名>");
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

    let tools = config::load_tools().map(|t| t.tools).unwrap_or_default();
    let all_targets = config::effective_targets(&entry.project);

    let rt = tokio::runtime::Runtime::new().expect("创建 tokio runtime 失败");
    let (tx, mut rx) = mpsc::channel(8192);
    let printer = rt.spawn(async move {
        while let Some(ev) = rx.recv().await {
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
    });

    let (_cancel_tx, cancel_rx) = broadcast::channel(4);
    let engine = Engine;
    let input = RunInput {
        project: entry.project.clone(),
        job: je.job.clone(),
        tools,
        all_targets,
        from_step: 0,
        only_step: None,
    };

    let status = rt.block_on(engine.run_job(input, cancel_rx, tx));
    let _ = rt.block_on(printer);
    println!("==== {} ====", status.short());
    if !status.ok() {
        std::process::exit(1);
    }
}
