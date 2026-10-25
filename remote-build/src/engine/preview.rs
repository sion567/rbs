//! 干跑预览（Ansible --check 思路）：变量全量解析 + when/creates 判定，不执行
//! 任何命令、不发起任何网络请求。顺带当变量拼错检查器用：未解析的占位符原样
//! 保留，预览里以 ⚠ 标出。

use crate::models::*;
use crate::variables::{eval_condition, resolve};
use std::path::PathBuf;

use super::{
    auto_corepack_wrap, build_var_context, corepack_available, dangerous_command_hits,
    is_remote, resolve_job_working_dir, resolve_remote_step_dir, resolve_step_working_dir,
    step_display_name, RunInput,
};

/// 单个步骤的预览结果（preview_job 产出；UI 控制台 / CLI 直接渲染）
#[derive(Debug, Clone)]
pub struct StepPreview {
    pub index: usize,
    pub name: String,
    pub skipped: bool,
    /// 跳过原因 / 备注（未启用 / 条件不满足 / creates 已存在 / 远端 creates 未校验）
    pub note: Option<String>,
    /// ⚠ 警告（条件无法解析、变量未解析、上传源缺失、目标未定义…）
    pub warnings: Vec<String>,
    /// 有效目标名（None = 本机）
    pub target: Option<String>,
    /// 步骤选择的工具（工具名, command 路径）：其目录会前置到 PATH，
    /// 命令里的裸名将解析到这个版本——预览里明确展示，选择才有可见后果
    pub tool: Option<(String, String)>,
    /// 变量解析后的最终命令（含 corepack 自动包装后的形态）
    pub command: String,
    /// 解析后的本地工作目录
    pub working_dir: PathBuf,
    /// upload 步骤：解析后的 (本地路径, 远端路径, 本地是否存在)
    pub uploads: Vec<(String, String, bool)>,
    /// pack 步骤：解析后的 (源, 目标 zip)
    pub pack: Option<(String, String)>,
    /// step outputs：捕获声明（键, 正则）
    pub outputs: Vec<(String, String)>,
    /// notify 的 handler 名
    pub notify: Vec<String>,
    /// wait 探测描述（模式 目标 间隔）
    pub wait_note: Option<String>,
}

/// 干跑预览：按真实执行路径解析每个步骤（变量替换、when 条件、creates 守卫、
/// upload/pack 命令格式），但不执行。同步无副作用，可在 UI 线程直接调用。
pub fn preview_job(input: &RunInput) -> Vec<StepPreview> {
    let ctx = build_var_context(input);
    let workspace = PathBuf::from(&input.project.local_path);
    let job_wd = resolve_job_working_dir(&input.job, &workspace, &ctx);

    // 与 run_job 同一判定：前端工程 corepack 钉版（预览展示包装后的最终命令）
    let pinned_frontend = {
        let pj = workspace.join("package.json");
        std::fs::read_to_string(&pj)
            .map(|t| t.contains("\"packageManager\""))
            .unwrap_or(false)
    };
    let corepack_ok = pinned_frontend && corepack_available();

    // 与 run_job 同一合并规则（§6.5）：项目级 tools 覆盖全局
    let mut merged_tools = input.tools.clone();
    merged_tools.extend(input.project.tools.clone());

    let mut out = Vec::new();
    for (index, step) in input.job.steps.iter().enumerate() {
        let step_name = step_display_name(step);
        // wait 探测目标与执行路径同批变量替换（引擎 run_single_step 里同一 resolve）；
        // 解析后仍含占位符 → 记录下来在构造完 p 后补 ⚠
        let wait_resolved = step.wait.as_ref().map(|wp| {
            let interval = if wp.interval_seconds == 0 {
                crate::transport::WAIT_POLL_SECONDS
            } else {
                wp.interval_seconds.max(1)
            };
            let target = resolve(&wp.target, &ctx);
            (format!("{} {} 每 {interval}s", wp.mode.as_str(), target), target.contains("${{"))
        });
        let mut p = StepPreview {
            index,
            name: step_name,
            skipped: false,
            note: None,
            warnings: Vec::new(),
            target: None,
            tool: (!step.tool.is_empty())
                .then(|| {
                    merged_tools
                        .get(&step.tool)
                        .map(|t| (step.tool.clone(), t.command.clone()))
                })
                .flatten(),
            command: String::new(),
            working_dir: resolve_step_working_dir(step, &job_wd, &ctx),
            uploads: Vec::new(),
            pack: None,
            outputs: step.outputs.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            notify: step.notify.clone(),
            wait_note: wait_resolved.as_ref().map(|(s, _)| s.clone()),
        };
        if let Some((_, unresolved)) = &wait_resolved {
            if *unresolved {
                p.warnings.push("wait 探测目标仍含未解析的变量占位符（检查变量名拼写）".to_string());
            }
        }

        if !step.enabled {
            p.skipped = true;
            p.note = Some("未启用".to_string());
            out.push(p);
            continue;
        }

        // when / condition
        if let Some(cond) = &step.condition {
            match eval_condition(cond, &ctx) {
                Some(false) => {
                    p.skipped = true;
                    p.note = Some(format!("条件不满足: {cond}"));
                    out.push(p);
                    continue;
                }
                None => p.warnings.push(format!("条件无法解析，将按真值执行: {cond}")),
                Some(true) => {}
            }
        }

        // 有效目标（§3.3）
        let eff_name = step.target.clone().or_else(|| input.job.target.clone());
        let eff_target = eff_name.as_deref().and_then(|n| {
            let t = input.all_targets.get(n)?;
            let mut t = t.clone();
            if t.name.is_empty() {
                t.name = n.to_string();
            }
            Some(t)
        });
        p.target = eff_name.clone();
        if let Some(n) = &eff_name {
            if eff_target.is_none() {
                p.warnings
                    .push(format!("目标「{n}」未定义（project.yaml 的 targets 表）"));
            }
        }
        // 引用了不存在的工具：引擎会静默跳过注入，预览里说破
        if !step.tool.is_empty() && p.tool.is_none() {
            p.warnings
                .push(format!("工具「{}」未定义（tools.yaml / 项目级 tools / ⚙ 设置）", step.tool));
        }
        let remote = is_remote(eff_target.as_ref());
        let remote_base = eff_target
            .as_ref()
            .and_then(|t| t.work_root.clone())
            .unwrap_or_default();
        let remote_dir = resolve_remote_step_dir(step, &remote_base, &ctx);

        // creates：本地实际校验；远端路径干跑不发起 SSH 往返，仅标注
        if let Some(creates) = &step.creates {
            let resolved = resolve(creates, &ctx);
            if remote {
                p.note =
                    Some(format!("creates={resolved}（远端路径，干跑不校验，按执行处理）"));
            } else if super::steps::local_creates_exists(&workspace, &resolved) {
                p.skipped = true;
                p.note = Some(format!("creates 已存在: {resolved}"));
                out.push(p);
                continue;
            }
        }

        // 命令解析（与 run_job 同一替换与 corepack 包装）
        let mut command = resolve(&step.command, &ctx);
        if pinned_frontend
            && corepack_ok
            && matches!(step.step_type, StepType::Exec | StepType::K8sApply)
            && !remote
        {
            command = auto_corepack_wrap(&command);
        }
        if command.contains("${{") || command.contains("${") {
            p.warnings
                .push("命令仍含未解析的变量占位符（检查变量名拼写 / 变量组）".to_string());
        }
        // 危险命令静态检查（§14）：预览 ⚠，防手滑（执行前引擎也会再提醒一次）
        if matches!(step.step_type, StepType::Exec | StepType::K8sApply) {
            let hits = dangerous_command_hits(&command);
            if !hits.is_empty() {
                p.warnings
                    .push(format!("⚠ 危险命令模式: {}（请确认是有意为之）", hits.join(" / ")));
            }
        }
        p.command = command.clone();

        match step.step_type {
            StepType::Upload if remote => {
                match super::steps::parse_upload_mappings(&command) {
                    Ok(mappings) => {
                        for (local_part, remote_part) in mappings {
                            let local_path = {
                                let lp = std::path::Path::new(&local_part);
                                if lp.is_absolute() {
                                    lp.to_path_buf()
                                } else {
                                    workspace.join(lp)
                                }
                            };
                            let exists = local_path.exists();
                            if !exists {
                                p.warnings
                                    .push(format!("上传源不存在: {}", local_path.display()));
                            }
                            let remote_path = if remote_part.starts_with('/')
                                || remote_dir.is_empty()
                            {
                                remote_part.clone()
                            } else {
                                format!("{}/{}", remote_dir.trim_end_matches('/'), remote_part)
                            };
                            p.uploads
                                .push((local_path.to_string_lossy().to_string(), remote_path, exists));
                        }
                    }
                    Err(e) => p.warnings.push(e.to_string()),
                }
            }
            StepType::Upload => {
                p.warnings
                    .push("upload 需要 SSH 目标，当前目标不支持上传".to_string());
            }
            StepType::Download if remote => {
                match super::steps::parse_upload_mappings(&command) {
                    Ok(mappings) => {
                        for (remote_part, local_part) in mappings {
                            let remote_path = if remote_part.starts_with('/')
                                || remote_dir.is_empty()
                            {
                                remote_part.clone()
                            } else {
                                format!("{}/{}", remote_dir.trim_end_matches('/'), remote_part)
                            };
                            let local_path = {
                                let lp = std::path::Path::new(&local_part);
                                if lp.is_absolute() { lp.to_path_buf() } else { workspace.join(lp) }
                            };
                            p.uploads.push((
                                remote_path,
                                local_path.to_string_lossy().to_string(),
                                true,
                            ));
                        }
                    }
                    Err(e) => p.warnings.push(e.to_string()),
                }
            }
            StepType::Download => {
                p.warnings
                    .push("download 需要 SSH 目标，当前目标不支持下载".to_string());
            }
            StepType::Pack => match command.split_once("->") {
                Some((s, d)) => {
                    let rp = |v: &str| {
                        let pp = std::path::Path::new(v.trim());
                        if pp.is_absolute() {
                            pp.to_path_buf()
                        } else {
                            workspace.join(pp)
                        }
                    };
                    let src = rp(s);
                    let dst = rp(d);
                    if !dst.to_string_lossy().to_ascii_lowercase().ends_with(".zip") {
                        p.warnings.push("pack 目标必须是 .zip 文件".to_string());
                    }
                    p.pack =
                        Some((src.to_string_lossy().to_string(), dst.to_string_lossy().to_string()));
                }
                None => p
                    .warnings
                    .push("pack 命令格式应为: <目录或通配> -> <目标.zip>".to_string()),
            },
            _ => {}
        }

        out.push(p);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::super::{preview_job, RunInput, DEFAULT_STEP_TIMEOUT};
    use crate::models::*;
    use std::collections::HashMap;

    fn preview_input(steps: Vec<Step>) -> (RunInput, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("rbs_preview_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("exists_marker.txt"), "x").unwrap();
        let input = RunInput {
            project: Project {
                name: "p".into(),
                local_path: dir.to_string_lossy().to_string(),
                variables: HashMap::new(),
                ..Default::default()
            },
            job: Job {
                name: "j".into(),
                project: "p".into(),
                steps,
                ..Default::default()
            },
            tools: HashMap::new(),
            all_targets: HashMap::new(),
            from_step: 0,
            only_step: None,
            run_id: String::new(),
            default_step_timeout: DEFAULT_STEP_TIMEOUT,
            cancel_tx: None,
        };
        (input, dir)
    }

    #[test]
    fn preview_flags_unresolved_vars_missing_sources_and_creates() {
        // 干跑预览：变量拼错 ⚠ / 上传源缺失 ⚠ / creates 已存在 → 跳过；不发任何网络请求
        let upload = Step {
            name: Some("上传".into()),
            command: "missing/nope.jar -> app.jar".to_string(),
            step_type: StepType::Upload,
            enabled: true,
            target: Some("prod".into()),
            ..Default::default()
        };
        let typo = Step {
            name: None,
            command: "echo ${{ variables.TYPO }}".to_string(),
            step_type: StepType::Exec,
            enabled: true,
            ..Default::default()
        };
        let done = Step {
            name: Some("已完成步骤".into()),
            command: "echo done".to_string(),
            step_type: StepType::Exec,
            enabled: true,
            creates: Some("exists_marker.txt".to_string()),
            ..Default::default()
        };

        let (mut input, dir) = preview_input(vec![upload, typo, done]);
        input.all_targets.insert(
            "prod".to_string(),
            Target {
                name: "prod".to_string(),
                target_type: TargetType::Ssh {
                    host: "1.2.3.4".to_string(),
                    port: 22,
                    user: "u".to_string(),
                    auth: crate::models::AuthMethod::None,
                    host_key: None,
                },
                work_root: Some("/srv".to_string()),
                ..Default::default()
            },
        );

        let previews = preview_job(&input);
        assert_eq!(previews.len(), 3);
        // 上传源缺失 → 警告；相对远端路径应拼上 work_root
        assert!(
            previews[0].warnings.iter().any(|w| w.contains("上传源不存在")),
            "{:?}",
            previews[0].warnings
        );
        assert_eq!(previews[0].uploads.len(), 1);
        assert_eq!(previews[0].uploads[0].1, "/srv/app.jar");
        // 变量拼写错误 → 警告，占位符原样展示
        assert!(
            previews[1].warnings.iter().any(|w| w.contains("未解析")),
            "{:?}",
            previews[1].warnings
        );
        assert_eq!(previews[1].command, "echo ${{ variables.TYPO }}");
        // creates 已存在 → 跳过
        assert!(previews[2].skipped, "{:?}", previews[2]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
