//! 新建项目向导（第一步：确定工程来源）。
//!
//! - Git 仓库：输入地址 / 账号 / 密码 / 分支 → Clone 到默认位置
//!   `~/.remote-build/projects/<项目名>`（可改 + 浏览）
//! - 本地目录：浏览选择已有代码目录，配置生成在 `~/.remote-build/projects/<项目名>/`
//! - SVN 等其他来源：当前版本不做

use crate::app::MainState;
use crate::config::sanitize_filename as sanitize_name;
use eframe::egui;
use egui::Color32;
use std::path::Path;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SourceKind {
    #[default]
    Git,
    Local,
}

#[derive(Default)]
pub struct NewProjectState {
    pub open: bool,
    pub kind: SourceKind,

    // Git 模式
    pub git_url: String,
    pub git_user: String,
    pub git_password: String,
    /// 密码明文显示开关（👁 切换）
    pub show_password: bool,
    pub git_branch: String,
    pub git_target: String,

    // 本地模式
    pub local_dir: String,

    // 共用
    pub project_name: String,

    // 克隆任务（后台线程）
    pub progress: Option<Arc<Mutex<String>>>,
    pub result_rx: Option<std::sync::mpsc::Receiver<Result<String, String>>>,
    pub message: Option<(bool, String)>,
}

/// 打开向导：清空上一次的全部输入（地址 / 账号密码 / 目录 / 名称 / 提示）。
/// 进行中的后台任务（result_rx / progress）保留，避免克隆结果无人回收。
pub fn open_new(main: &mut MainState) {
    let in_flight_rx = main.new_project.result_rx.take();
    let in_flight_progress = main.new_project.progress.take();
    main.new_project = NewProjectState::default();
    main.new_project.result_rx = in_flight_rx;
    main.new_project.progress = in_flight_progress;
    main.new_project.open = true;
}

pub fn show(ctx: &egui::Context, main: &mut MainState) {
    // 回收后台克隆结果
    if let Some(rx) = &main.new_project.result_rx {
        match rx.try_recv() {
            Ok(Ok(name)) => {
                main.new_project.result_rx = None;
                main.new_project.progress = None;
                main.new_project.message = None;
                // 克隆成功 → 生成项目配置（与端到端测试走同一 complete_clone 路径）。
                // 此前缺这一步：discover 只认有 project.yaml 的目录，
                // 克隆完成的项目一直不会出现在项目树里
                let target = main.new_project.git_target.trim().to_string();
                if let Err(e) = complete_clone(&name, &target) {
                    main.status_msg =
                        Some(format!("项目「{name}」克隆成功，但配置生成失败 ❌ {e:#}"));
                }
                main.reload();
                main.select_project_by_name(&name);
                main.status_msg = Some(format!("项目「{name}」创建成功 ✅"));
                main.new_project.open = false;
            }
            Ok(Err(e)) => {
                main.new_project.result_rx = None;
                main.new_project.progress = None;
                // 弹窗在任务提交时即已关闭，失败要落到状态栏才能被看到；
                // message 保留：若用户恰在此期间重新打开向导可直接看到详情
                main.new_project.message = Some((false, e.clone()));
                let first = e.lines().next().unwrap_or("").to_string();
                main.status_msg = Some(format!("项目创建失败 ❌ {first}"));
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                main.new_project.result_rx = None;
                main.new_project.progress = None;
            }
        }
    }

    let busy = main.new_project.result_rx.is_some();
    let mut open = main.new_project.open;
    if open {
        crate::ui::modal_backdrop(ctx, "mask_new_project");
    }

    crate::ui::centered_window(egui::Window::new("＋ 新建项目"), ctx)
        .open(&mut open)
        .order(egui::Order::Foreground)
        .default_width(600.0)
        .show(ctx, |ui| {
            // 来源选择
            ui.horizontal(|ui| {
                ui.selectable_value(&mut main.new_project.kind, SourceKind::Git, "Git 仓库");
                ui.selectable_value(&mut main.new_project.kind, SourceKind::Local, "本地目录");
                ui.separator();
                ui.weak(match main.new_project.kind {
                    SourceKind::Git => "克隆远端仓库（默认下载到 ~/.remote-build/workspace/）",
                    SourceKind::Local => "使用已有本地代码目录",
                });
            });
            ui.separator();
            ui.add_space(4.0);

            match main.new_project.kind {
                SourceKind::Git => git_form(ui, main),
                SourceKind::Local => local_form(ui, main),
            }

            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label("项目名称:");
                ui.add(
                    egui::TextEdit::singleline(&mut main.new_project.project_name)
                        .desired_width(crate::ui::input::MED),
                );
                ui.weak("（配置将创建在 ~/.remote-build/projects/<项目名>/ 下）");
            });

            ui.add_space(10.0);
            ui.horizontal(|ui| {
                let label = match main.new_project.kind {
                    SourceKind::Git => "⬇ Clone",
                    SourceKind::Local => "创建项目",
                };
                if ui.add_enabled(!busy, egui::Button::new(label)).clicked() {
                    // 校验失败留在弹窗看错误；任务提交成功即关闭弹窗
                    if start(main) {
                        main.new_project.open = false;
                    }
                }
                if busy {
                    ui.add(egui::Spinner::new().size(16.0));
                    ui.weak("处理中…");
                }
            });

            if let Some((ok, msg)) = &main.new_project.message {
                ui.add_space(6.0);
                let color = if *ok {
                    Color32::from_rgb(120, 200, 120)
                } else {
                    Color32::from_rgb(230, 110, 110)
                };
                ui.colored_label(color, msg);
            }

            // 克隆进度
            if busy {
                if let Some(progress) = &main.new_project.progress {
                    let text = progress.lock().map(|p| p.clone()).unwrap_or_default();
                    if !text.is_empty() {
                        ui.add_space(4.0);
                        egui::ScrollArea::vertical()
                            .id_salt("clone_progress")
                            .max_height(80.0)
                            .stick_to_bottom(true)
                            .show(ui, |ui| {
                                ui.monospace(&text);
                            });
                    }
                }
            }
        });
    main.new_project.open = open;

    if busy {
        ctx.request_repaint_after(std::time::Duration::from_millis(150));
    }
}

// ---------------------------------------------------------------------------
// 表单
// ---------------------------------------------------------------------------

fn git_form(ui: &mut egui::Ui, main: &mut MainState) {
    egui::Grid::new("git_form")
        .num_columns(2)
        .spacing([10.0, 4.0])
        .show(ui, |ui| {
            ui.label("仓库地址:");
            let resp = ui.add(
                egui::TextEdit::singleline(&mut main.new_project.git_url)
                    .desired_width(crate::ui::input::LONG)
                    .hint_text("https://github.com/org/repo.git 或 git@host:path/repo.git"),
            );
            if resp.changed() {
                auto_fill_from_url(main);
            }
            ui.end_row();

            ui.label("分支:");
            ui.add(
                egui::TextEdit::singleline(&mut main.new_project.git_branch)
                    .desired_width(crate::ui::input::MED)
                    .hint_text("留空 = 默认分支"),
            );
            ui.end_row();

            ui.label("账号:");
            ui.add(
                egui::TextEdit::singleline(&mut main.new_project.git_user)
                    .desired_width(crate::ui::input::MED)
                    .hint_text("https 私有仓库必填（ssh 忽略）"),
            );
            ui.end_row();

            ui.label("密码:");
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut main.new_project.git_password)
                        .password(!main.new_project.show_password)
                        .desired_width(crate::ui::input::MED)
                        .hint_text("https 填密码或 Personal Access Token"),
                );
                let (icon, tip) = if main.new_project.show_password {
                    ("🙈", "隐藏密码")
                } else {
                    ("👁", "显示密码")
                };
                if ui.small_button(icon).on_hover_text(tip).clicked() {
                    main.new_project.show_password = !main.new_project.show_password;
                }
            });
            ui.end_row();

            ui.label("目标目录:");
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut main.new_project.git_target)
                        .desired_width(crate::ui::input::LONG),
                );
                if ui.small_button("📂").clicked() {
                    if let Some(dir) = rfd::FileDialog::new()
                        .set_title("选择克隆目标目录（应为空目录）")
                        .pick_folder()
                    {
                        main.new_project.git_target = dir.to_string_lossy().to_string();
                    }
                }
            });
            ui.end_row();
        });
    ui.weak("密码仅用于本次 clone（不保存到任何文件），日志中会脱敏显示。");
}

fn local_form(ui: &mut egui::Ui, main: &mut MainState) {
    egui::Grid::new("local_form")
        .num_columns(2)
        .spacing([10.0, 4.0])
        .show(ui, |ui| {
            ui.label("本地目录:");
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut main.new_project.local_dir)
                        .desired_width(crate::ui::input::LONG)
                        .hint_text("D:\\workspace\\my-project"),
                );
                if ui.button("📂 浏览…").clicked() {
                    if let Some(dir) =
                        rfd::FileDialog::new().set_title("选择项目本地目录").pick_folder()
                    {
                        main.new_project.local_dir = dir.to_string_lossy().to_string();
                        // 目录末节直接作为项目名（覆盖旧值）
                        if let Some(name) = dir.file_name() {
                            main.new_project.project_name =
                                sanitize_name(&name.to_string_lossy());
                        }
                    }
                }
            });
            ui.end_row();
        });
    ui.weak(
        "代码与配置分离：克隆代码 → ~/.remote-build/workspace/<项目名>/，\n\
         构建配置（project.yaml + jobs/ 流程）→ ~/.remote-build/projects/<项目名>/。",
    );
}

// ---------------------------------------------------------------------------
// 动作
// ---------------------------------------------------------------------------

fn auto_fill_from_url(main: &mut MainState) {
    let derived = derive_name_from_url(&main.new_project.git_url);
    if derived.is_empty() {
        return;
    }
    // 地址末段直接作为项目名（地址再变化时以新末段覆盖）
    main.new_project.project_name = derived.clone();
    if main.new_project.git_target.is_empty() {
        main.new_project.git_target =
            crate::config::default_clone_target(&derived).to_string_lossy().to_string();
    }
}

/// 校验输入并提交任务。返回 false 表示校验未过（错误写在 message，弹窗保持打开）。
fn start(main: &mut MainState) -> bool {
    // 弹窗可能在任务进行中被重新打开，防止重复提交
    if main.new_project.result_rx.is_some() {
        main.new_project.message =
            Some((false, "已有创建任务进行中，请等待其完成".to_string()));
        return false;
    }
    let name = sanitize_name(main.new_project.project_name.trim());
    if name.is_empty() || name == "." || name == ".." {
        main.new_project.message = Some((false, "请填写合法的项目名称".to_string()));
        return false;
    }
    main.new_project.project_name = name.clone();

    match main.new_project.kind {
        SourceKind::Git => start_clone(main, name),
        SourceKind::Local => start_local(main, name),
    }
}

fn start_local(main: &mut MainState, name: String) -> bool {
    let dir = main.new_project.local_dir.trim().to_string();
    if dir.is_empty() || !Path::new(&dir).exists() {
        main.new_project.message = Some((false, format!("本地目录不存在: {dir}")));
        return false;
    }
    match crate::config::create_project_entry(&name, Path::new(&dir)) {
        Ok(_) => {
            main.new_project.message = None;
            main.reload();
            main.select_project_by_name(&name);
            main.status_msg = Some(format!("项目「{name}」创建成功 ✅"));
            true
        }
        Err(e) => {
            main.new_project.message = Some((false, format!("创建项目失败: {e}")));
            false
        }
    }
}

/// 校验并派发后台克隆。返回 false 表示校验未过。
fn start_clone(main: &mut MainState, name: String) -> bool {
    let url = main.new_project.git_url.trim().to_string();
    if url.is_empty() {
        main.new_project.message = Some((false, "请填写仓库地址".to_string()));
        return false;
    }
    let target = main.new_project.git_target.trim().to_string();
    if target.is_empty() {
        main.new_project.message = Some((false, "请填写目标目录".to_string()));
        return false;
    }
    let target_path = Path::new(&target);
    if target_path.exists() {
        let not_empty = std::fs::read_dir(target_path)
            .map(|mut it| it.next().is_some())
            .unwrap_or(true);
        if not_empty {
            main.new_project.message =
                Some((false, format!("目标目录已存在且非空: {target}")));
            return false;
        }
    }

    // git 优先用 tools.yaml 里探测到的，否则回退 PATH
    let git_cmd = main
        .tools
        .get("git")
        .map(|t| t.command.clone())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "git".to_string());

    let full_url = build_url_with_creds(
        &url,
        main.new_project.git_user.trim(),
        &main.new_project.git_password,
    );
    let branch = main.new_project.git_branch.trim().to_string();
    let password = main.new_project.git_password.clone();

    let (tx, rx) = std::sync::mpsc::channel();
    let progress: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    main.new_project.result_rx = Some(rx);
    main.new_project.progress = Some(progress.clone());
    main.new_project.message = None;

    std::thread::spawn(move || {
        let res = run_clone(&git_cmd, &full_url, &target, &branch, progress, &password);
        let _ = tx.send(res.map(|_| name));
    });
    true
}

/// 克隆成功后的收尾：生成项目配置（project.yaml + build.yaml；git 项目自动带
/// git 前置与版本/拉取起步步骤）。UI 完成分支与端到端测试共用此函数——
/// 此前克隆路径从未生成配置，克隆出的项目进不了项目树（discover 只认
/// 有 project.yaml 的目录），第一版起就存在且完全静默。
pub(crate) fn complete_clone(
    name: &str,
    target: &str,
) -> Result<std::path::PathBuf, anyhow::Error> {
    crate::config::create_project_entry(name, Path::new(target))
}

/// 后台线程执行 git clone，进度写入 progress（已脱敏），返回 Ok(())/Err(错误详情)。
/// 密码仅本次内存使用，不落盘不进日志（§14 安全策略）。
fn run_clone(
    git_cmd: &str,
    url: &str,
    target: &str,
    branch: &str,
    progress: Arc<Mutex<String>>,
    password: &str,
) -> Result<String, String> {
    let mut args: Vec<&str> = vec!["clone", "--progress"];
    if !branch.is_empty() {
        args.push("--branch");
        args.push(branch);
    }
    args.push(url);
    args.push(target);
    run_git(
        git_cmd,
        &args,
        None,
        Some(progress),
        None,
        password,
        "无输出（请检查网络 / 地址 / 账号密码）",
    )
}

/// 对已有仓库目录执行 git pull --ff-only：远端历史分叉或本地冲突时直接失败，
/// 不产生意外 merge、不覆盖本地未提交修改（设计文档 §10.2 的顾虑）。
/// 输出行经 line_tx 流式送出（进度在 stderr）。凭据用仓库里已存的 origin 配置，
/// 不再要账号密码。返回 Ok(完整输出) / Err(错误详情)。
pub(crate) fn run_pull(
    git_cmd: &str,
    dir: &Path,
    line_tx: std::sync::mpsc::Sender<String>,
) -> Result<String, String> {
    run_git(
        git_cmd,
        &["pull", "--ff-only", "--progress"],
        Some(dir),
        None,
        Some(line_tx),
        "",
        "无输出（请检查网络与远端仓库状态）",
    )
}

/// 执行 git 子命令：stderr 逐行脱敏后写入 progress（尾部 8 行，弹窗进度条用）
/// 和/或 line_tx（逐行流式，控制台用）；stdout 同步排空防子进程阻塞。
/// 返回 Ok(stderr 完整输出) / Err(子命令名 + 退出码 + stderr 尾部)。
pub(crate) fn run_git(
    git_cmd: &str,
    args: &[&str],
    cwd: Option<&Path>,
    progress: Option<Arc<Mutex<String>>>,
    line_tx: Option<std::sync::mpsc::Sender<String>>,
    password: &str,
    no_output_hint: &str,
) -> Result<String, String> {
    use std::io::{BufRead, BufReader};
    use std::process::{Command, Stdio};

    let mut cmd = Command::new(git_cmd);
    cmd.args(args);
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("git 启动失败: {e}（{git_cmd}）"))?;

    // git 的进度输出在 stderr；逐行脱敏后送往两个可选出口，并留完整输出供错误展示
    let stderr = child.stderr.take();
    let pw = password.to_string();
    let reader = std::thread::spawn(move || {
        let mut all = String::new();
        if let Some(stderr) = stderr {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                let masked = mask_secret(&line, &pw);
                all.push_str(&masked);
                all.push('\n');
                if let Some(tx) = &line_tx {
                    let _ = tx.send(masked.trim_end().to_string());
                }
                if let Some(prog) = &progress {
                    if let Ok(mut p) = prog.lock() {
                        let mut lines: Vec<&str> = p.lines().collect();
                        lines.push(masked.trim_end());
                        let n = lines.len();
                        if n > 8 {
                            lines.drain(0..n - 8);
                        }
                        *p = lines.join("\n");
                    }
                }
            }
        }
        all
    });

    // stdout 也必须排空，防止缓冲区满导致子进程阻塞
    if let Some(stdout) = child.stdout.take() {
        std::thread::spawn(move || {
            use std::io::Read;
            let mut sink = String::new();
            let _ = BufReader::new(stdout).read_to_string(&mut sink);
        });
    }

    let status = child.wait().map_err(|e| format!("git 执行失败: {e}"))?;
    let output = reader.join().unwrap_or_default();

    if status.success() {
        Ok(output)
    } else {
        let tail: Vec<&str> = output.lines().rev().take(8).collect();
        let tail = tail.into_iter().rev().collect::<Vec<_>>().join("\n");
        Err(format!(
            "git {} 失败（退出码 {}）:\n{}",
            args.first().copied().unwrap_or("?"),
            status.code().unwrap_or(-1),
            if tail.is_empty() {
                no_output_hint.to_string()
            } else {
                tail
            }
        ))
    }
}

/// 日志脱敏：密码不进任何展示文本
fn mask_secret(line: &str, password: &str) -> String {
    if password.is_empty() {
        line.to_string()
    } else {
        line.replace(password, "***")
    }
}

// ---------------------------------------------------------------------------
// 工具函数
// ---------------------------------------------------------------------------

/// 从 URL 推导项目名：取末段、去 .git
pub fn derive_name_from_url(url: &str) -> String {
    let trimmed = url.trim().trim_end_matches('/');
    let last = trimmed.rsplit(['/', ':']).next().unwrap_or("");
    sanitize_name(last.trim_end_matches(".git"))
}

/// https URL 内嵌凭证（仅用于本次 clone，不落盘）；ssh 等其他形式原样返回
fn build_url_with_creds(url: &str, user: &str, password: &str) -> String {
    if user.is_empty() && password.is_empty() {
        return url.to_string();
    }
    let enc = |s: &str| percent_encode(s);
    if let Some(rest) = url.strip_prefix("https://") {
        format!("https://{}:{}@{}", enc(user), enc(password), rest)
    } else if let Some(rest) = url.strip_prefix("http://") {
        format!("http://{}:{}@{}", enc(user), enc(password), rest)
    } else {
        url.to_string()
    }
}

/// 最小 percent-encode（RFC 3986 unreserved 之外全部转义），避免密码里
/// 的 @ / : 等字符破坏 URL 结构
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_name_from_urls() {
        assert_eq!(derive_name_from_url("https://github.com/org/payment-service.git"), "payment-service");
        assert_eq!(derive_name_from_url("https://git.internal/team/app/"), "app");
        assert_eq!(derive_name_from_url("git@host:path/repo-web.git"), "repo-web");
        assert_eq!(derive_name_from_url(""), "");
    }

    #[test]
    fn sanitizes_project_names() {
        assert_eq!(sanitize_name("a/b\\c:d"), "a_b_c_d");
        assert_eq!(sanitize_name("normal-name_1"), "normal-name_1");
    }

    #[test]
    fn embeds_creds_only_for_http() {
        assert_eq!(
            build_url_with_creds("https://git.internal/team/app.git", "dev", "p@ss:word"),
            "https://dev:p%40ss%3Aword@git.internal/team/app.git"
        );
        // 凭证为空原样返回
        assert_eq!(build_url_with_creds("https://x/y.git", "", ""), "https://x/y.git");
        // ssh 形式忽略账号密码
        assert_eq!(
            build_url_with_creds("git@host:path/repo.git", "dev", "pw"),
            "git@host:path/repo.git"
        );
    }

    #[test]
    fn run_clone_local_repo_end_to_end() {
        // 本机无 git 时跳过
        if std::process::Command::new("git").arg("--version").output().is_err() {
            return;
        }
        let base = std::env::temp_dir().join(format!("rbs_clone_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();

        // 构造源仓库
        let src = base.join("src-repo");
        std::fs::create_dir_all(&src).unwrap();
        let git = |args: &[&str], cwd: &Path| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(cwd)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {args:?} 失败: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        git(&["init", "-b", "main"], &src);
        std::fs::write(src.join("hello.txt"), "hi").unwrap();
        git(&["add", "."], &src);
        git(&["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-m", "init"], &src);

        // 正常 clone（指定分支）
        let dst = base.join("cloned");
        let progress = Arc::new(Mutex::new(String::new()));
        let res = run_clone(
            "git",
            src.to_str().unwrap(),
            dst.to_str().unwrap(),
            "main",
            progress.clone(),
            "",
        );
        assert!(res.is_ok(), "clone 应成功: {res:?}");
        assert!(dst.join("hello.txt").exists());
        assert!(!progress.lock().unwrap().is_empty(), "应有进度输出");

        // 不存在的分支 → 失败且返回错误详情
        let res = run_clone(
            "git",
            src.to_str().unwrap(),
            base.join("c2").to_str().unwrap(),
            "no-such-branch",
            progress,
            "",
        );
        assert!(res.is_err());

        let _ = std::fs::remove_dir_all(&base);
    }

    /// 端到端锁链：向导克隆路径（run_clone → complete_clone）产出的项目必须能被
    /// discover 发现，且起步配置带 git 前置 + 版本/拉取两条步骤。
    /// 此前的真 bug：完成分支没生成 project.yaml，克隆项目永远进不了项目树。
    #[test]
    fn clone_wizard_produces_discoverable_project() {
        if std::process::Command::new("git").arg("--version").output().is_err() {
            return;
        }
        let tmp = std::env::temp_dir().join(format!("rbs_wiz_e2e_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        std::env::set_var("REMOTE_BUILD_HOME", &tmp);

        // 源仓库（本地路径当远端用）
        let src = tmp.join("src-repo");
        std::fs::create_dir_all(&src).unwrap();
        let git = |args: &[&str], cwd: &Path| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(cwd)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {args:?} 失败: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        git(&["init", "-b", "main"], &src);
        std::fs::write(src.join("a.txt"), "a").unwrap();
        git(&["add", "."], &src);
        git(&["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-m", "init"], &src);

        // 向导的完整路径：克隆 → 完成分支调用的同一个 complete_clone
        let target = tmp.join("workspace").join("demo");
        let res = run_clone(
            "git",
            src.to_str().unwrap(),
            target.to_str().unwrap(),
            "main",
            Arc::new(Mutex::new(String::new())),
            "",
        );
        assert!(res.is_ok(), "{res:?}");
        complete_clone("demo", target.to_str().unwrap()).expect("克隆后配置必须生成");

        // 项目可被发现，起步配置 = 1 条 git 前置 + 版本/拉取两条步骤
        let found = crate::storage::files::discover_projects(&crate::config::projects_dir());
        let entry = found
            .iter()
            .find(|p| p.project.name == "demo")
            .expect("克隆项目应能被发现");
        let build = entry
            .jobs
            .iter()
            .find(|j| j.job.name == "build")
            .expect("应有 build 起步流程");
        assert_eq!(build.job.preconditions.len(), 1);
        assert_eq!(build.job.preconditions[0].check, "git --version");
        assert_eq!(build.job.steps.len(), 2);

        std::env::remove_var("REMOTE_BUILD_HOME");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn run_pull_picks_up_new_commits_and_rejects_diverged() {
        // 本机无 git 时跳过
        if std::process::Command::new("git").arg("--version").output().is_err() {
            return;
        }
        let base = std::env::temp_dir().join(format!("rbs_pull_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();

        // 构造源仓库
        let src = base.join("src-repo");
        std::fs::create_dir_all(&src).unwrap();
        let git = |args: &[&str], cwd: &Path| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(cwd)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {args:?} 失败: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        git(&["init", "-b", "main"], &src);
        std::fs::write(src.join("a.txt"), "1").unwrap();
        git(&["add", "."], &src);
        git(&["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-m", "init"], &src);

        // clone 到工作目录
        let work = base.join("work");
        let res = run_clone(
            "git",
            src.to_str().unwrap(),
            work.to_str().unwrap(),
            "main",
            Arc::new(Mutex::new(String::new())),
            "",
        );
        assert!(res.is_ok(), "clone 应成功: {res:?}");

        // 远端新提交 → pull 拿到（fast-forward，无需 merge 身份配置）
        std::fs::write(src.join("b.txt"), "2").unwrap();
        git(&["add", "."], &src);
        git(&["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-m", "second"], &src);
        let (line_tx, line_rx) = std::sync::mpsc::channel();
        let res = run_pull("git", &work, line_tx);
        assert!(res.is_ok(), "pull 应成功: {res:?}");
        assert!(work.join("b.txt").exists());
        assert!(line_rx.try_recv().is_ok(), "pull 应有流式输出行");

        // 非仓库目录 → pull 失败
        let not_repo = base.join("not-repo");
        std::fs::create_dir_all(&not_repo).unwrap();
        let res = run_pull("git", &not_repo, std::sync::mpsc::channel().0);
        assert!(res.is_err());

        // 本地有分叉提交时 --ff-only 必须拒绝（不产生意外 merge）
        std::fs::write(work.join("local.txt"), "x").unwrap();
        git(&["add", "."], &work);
        git(&["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-m", "local"], &work);
        std::fs::write(src.join("c.txt"), "3").unwrap();
        git(&["add", "."], &src);
        git(&["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-m", "upstream"], &src);
        let res = run_pull("git", &work, std::sync::mpsc::channel().0);
        assert!(res.is_err(), "分叉时 --ff-only 应失败: {res:?}");

        let _ = std::fs::remove_dir_all(&base);
    }
}
