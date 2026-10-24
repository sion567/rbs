//! 🎯 目标管理：本项目目标定义的增删改（project.yaml 的 `targets:` 表）+ SSH 连接测试。
//!
//! v0.4 调整：目标**跟项目走**（§3.5）——不再有全局 targets.yaml；
//! 不同项目的 prod 可以是不同服务器。SSH 密码按 `{项目}/{目标}` 为键存入
//! Windows Credential Manager（§14），不落 YAML。

use crate::app::MainState;
use crate::models::{AuthMethod, Target, TargetType};
use eframe::egui;
use egui::Color32;

const KINDS: [&str; 3] = ["SSH（远端 Linux）", "Kubernetes（本地 kubectl）", "本地"];
const AUTHS: [&str; 4] = ["key（私钥文件）", "password（密码）", "agent（pageant）", "none"];

#[derive(Default)]
pub struct TargetEditorState {
    pub open: bool,
    /// 当前选中目标（project.yaml targets 表的键）
    pub selected: Option<String>,
    /// 名称输入缓冲（重命名用）
    pub name_buf: String,
    pub kind_idx: usize,
    pub host: String,
    pub port: u16,
    pub user: String,
    pub auth_idx: usize,
    pub key_path: String,
    /// 新密码输入（写时才读；留空 = 不修改已存密码）
    pub password_buf: String,
    /// Credential Manager 里是否已存密码
    pub password_saved: bool,
    pub kubeconfig: String,
    pub context: String,
    pub namespace: String,
    pub registry: String,
    pub work_root: String,
    /// 连接测试（后台线程）
    pub test_rx: Option<std::sync::mpsc::Receiver<Result<String, String>>>,
    pub test_result: Option<(bool, String)>,
    pub unsaved: bool,
}

/// 当前项目的目标表（可变）；未选项目返回 None
fn scope_map_mut(
    main: &mut MainState,
) -> Option<&mut std::collections::HashMap<String, Target>> {
    let pi = main.selected_project?;
    Some(&mut main.projects[pi].project.targets)
}

/// 当前项目的目标表（只读）
fn scope_map_ref(main: &MainState) -> Option<&std::collections::HashMap<String, Target>> {
    main.selected_project
        .and_then(|pi| main.projects.get(pi))
        .map(|p| &p.project.targets)
}

/// 凭据键：{项目名}/{目标名}（§14，不同项目的同名目标密码互不干扰）
fn cred_key_for(main: &MainState, name: &str) -> String {
    let pname = main
        .selected_project
        .and_then(|pi| main.projects.get(pi))
        .map(|p| p.project.name.clone())
        .unwrap_or_default();
    format!("{pname}/{name}")
}

pub fn show(ctx: &egui::Context, main: &mut MainState) {
    // 回收连接测试结果
    if let Some(rx) = &main.target_editor.test_rx {
        match rx.try_recv() {
            Ok(Ok(line)) => {
                main.target_editor.test_result = Some((true, line));
                main.target_editor.test_rx = None;
            }
            Ok(Err(e)) => {
                main.target_editor.test_result = Some((false, e));
                main.target_editor.test_rx = None;
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                main.target_editor.test_rx = None;
            }
        }
    }

    let has_project = main.selected_project.is_some();

    // 未选中时默认选中一个目标：优先 local，其次按名称排最前的，省得每次手动点
    if has_project && main.target_editor.selected.is_none() {
        let names: Vec<String> = scope_map_ref(main)
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default();
        let pick = names
            .iter()
            .find(|n| n.as_str() == "local")
            .cloned()
            .or_else(|| names.iter().min().cloned());
        if let Some(name) = pick {
            load_buffers(main, &name);
            main.target_editor.selected = Some(name);
        }
    }

    let mut open = main.target_editor.open;
    if open {
        crate::ui::modal_backdrop(ctx, "mask_targets");
    }
    egui::Window::new("🎯 目标管理")
        .open(&mut open)
        .order(egui::Order::Foreground)
        .default_width(720.0)
        .default_height(560.0)
        .show(ctx, |ui| {
            ui.heading("执行目标（随项目走）");
            ui.colored_label(
                Color32::from_rgb(230, 180, 80),
                "⚠ 本窗口编辑的是内存副本：如果你手动修改过 project.yaml，\
                 请先点工具栏 ⟳ 重新加载再打开本窗口，否则「保存」会用旧内容覆盖你的手改。",
            );
            ui.weak(
                "目标 = 命令在哪台机器执行：SSH（远端 Linux）、Kubernetes（本地 kubectl + kubeconfig）\
                 或本地。定义保存在**当前项目**的 project.yaml 里——不同项目的 prod 可以是不同服务器；\
                 流程的「目标」下拉读取本项目这份配置。",
            );
            ui.add_space(4.0);

            if has_project {
                target_list_and_form(ui, main);
            } else {
                ui.weak("请先在左侧选择一个项目。");
            }
        });
    main.target_editor.open = open;

    if main.target_editor.test_rx.is_some() {
        ctx.request_repaint_after(std::time::Duration::from_millis(150));
    }
}

// ---------------------------------------------------------------------------
// 目标列表 + 表单
// ---------------------------------------------------------------------------

fn target_list_and_form(ui: &mut egui::Ui, main: &mut MainState) {
    let mut names: Vec<String> = scope_map_ref(main)
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();
    names.sort();

    ui.columns(2, |columns| {
        columns[0].set_min_width(170.0);
        egui::ScrollArea::vertical()
            .id_salt("target_list")
            .max_height(280.0)
            .show(&mut columns[0], |ui| {
                for name in &names {
                    let sel = main.target_editor.selected.as_deref() == Some(name.as_str());
                    if ui.selectable_label(sel, name).clicked() {
                        main.target_editor.selected = Some(name.clone());
                        load_buffers(main, name);
                        main.target_editor.test_result = None;
                    }
                }
                if names.is_empty() {
                    ui.weak("（空，点下方「＋ 添加」）");
                }
            });

        egui::ScrollArea::vertical()
            .id_salt("target_form")
            .max_height(280.0)
            .show(&mut columns[1], |ui| {
                edit_form(ui, main);
            });
    });

    ui.add_space(6.0);
    ui.horizontal(|ui| {
        if ui.button("＋ 添加").clicked() {
            let mut n = 1;
            let mut name = format!("new-target-{n}");
            while scope_map_ref(main).map(|m| m.contains_key(&name)).unwrap_or(false) {
                n += 1;
                name = format!("new-target-{n}");
            }
            if let Some(map) = scope_map_mut(main) {
                map.insert(
                    name.clone(),
                    Target {
                        name: name.clone(),
                        target_type: TargetType::Ssh {
                            host: String::new(),
                            port: 22,
                            user: String::new(),
                            auth: AuthMethod::Password { encrypted: String::new() },
                            host_key: None,
                        },
                        work_root: None,
                        env: Default::default(),
                        cred_key: None,
                    },
                );
            }
            main.target_editor.selected = Some(name.clone());
            load_buffers(main, &name);
            main.target_editor.unsaved = true;
        }
        if ui.button("🗑 删除").clicked() {
            if let Some(name) = main.target_editor.selected.take() {
                if let Some(map) = scope_map_mut(main) {
                    map.remove(&name);
                }
                crate::secrets::delete_password(&cred_key_for(main, &name));
                main.target_editor.unsaved = true;
            }
        }
        if ui
            .button("🔌 测试连接")
            .on_hover_text(
                "SSH 连接测试 + 服务器体检：CPU / 内存 / 磁盘 / 负载（8 秒超时）。\n\
                 直接用当前表单值测试（无需先保存）；密码栏已输入未保存的密码也会参与本次测试",
            )
            .clicked()
        {
            start_test(main);
        }
        if ui.button("💾 保存 project.yaml").clicked() {
            let result = match main.selected_project {
                Some(pi) => {
                    let entry = &main.projects[pi];
                    crate::storage::files::save_yaml(
                        &entry.dir.join("project.yaml"),
                        &entry.project,
                    )
                }
                None => Err(anyhow::anyhow!("需要先在左侧选择项目")),
            };
            match result {
                Ok(()) => {
                    main.target_editor.unsaved = false;
                    main.status_msg = Some("目标配置已保存 ✅".to_string());
                }
                Err(e) => main.status_msg = Some(format!("保存失败: {e}")),
            }
        }
        if main.target_editor.unsaved {
            ui.colored_label(Color32::from_rgb(230, 180, 80), "（有未保存修改）");
        }
    });

    if let Some((ok, msg)) = &main.target_editor.test_result {
        let color = if *ok {
            Color32::from_rgb(120, 200, 120)
        } else {
            Color32::from_rgb(230, 110, 110)
        };
        let icon = if *ok { "✅" } else { "❌" };
        // 等宽字体：磁盘/内存列对齐（体检信息为多行）；
        // selectable：指纹等关键信息可直接选中复制，避免手抄 43 位出错
        ui.add(
            egui::Label::new(
                egui::RichText::new(format!("{icon} {msg}"))
                    .monospace()
                    .color(color),
            )
            .selectable(true),
        );
        if !ok && msg.contains("host key") {
            ui.weak("提示：把连接测试输出中的 SHA256 指纹（可选中复制）填入「主机键指纹」字段即可信任（TOFU）。");
        }
    }
}

fn edit_form(ui: &mut egui::Ui, main: &mut MainState) {
    let Some(selected) = main.target_editor.selected.clone() else {
        ui.weak("← 选择或添加一个目标");
        return;
    };
    if !scope_map_ref(main).map(|m| m.contains_key(&selected)).unwrap_or(false) {
        main.target_editor.selected = None;
        return;
    }

    // 凭据键提前计算（grid 内的密码保存要用，而 target 的可变借用会持续整个 grid）
    let cred_key = cred_key_for(main, &selected);

    // 重命名放在取 map 可变借用之后会失效，这里先收集、表单结束后统一应用
    let mut rename_to: Option<String> = None;

    // 限制列宽让长提示文字在单元格内换行：默认 Grid 文本不换行，提示文字会把
    // 第 1 列撑到几百 px，把同行更靠右的控件推出可视区
    let form_width = ui.available_width();
    egui::Grid::new("target_form_grid")
        .num_columns(2)
        .spacing([10.0, 4.0])
        .max_col_width(form_width)
        .show(ui, |ui| {
            ui.label("名称:");
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut main.target_editor.name_buf)
                        .desired_width(150.0),
                );
                if ui.small_button("重命名").clicked() {
                    let new = main.target_editor.name_buf.trim().to_string();
                    let exists = scope_map_ref(main)
                        .map(|m| m.contains_key(&new))
                        .unwrap_or(true);
                    if !new.is_empty() && new != selected && !exists {
                        rename_to = Some(new);
                    }
                }
            });
            ui.end_row();

            // 按作用域取目标的可变借用：match 内联以保持字段级借用，
            // 避免辅助函数把整个 &mut MainState 的生命周期带进来
            let target = main
                .selected_project
                .and_then(|pi| main.projects.get_mut(pi))
                .and_then(|p| p.project.targets.get_mut(&selected));
            let Some(target) = target else {
                return;
            };

            ui.label("类型:");
            let k_idx = main.target_editor.kind_idx;
            let mut new_k = k_idx;
            egui::ComboBox::from_id_salt("target_kind_combo")
                .selected_text(KINDS[k_idx])
                .width(220.0)
                .show_ui(ui, |ui| {
                    for (i, label) in KINDS.iter().enumerate() {
                        ui.selectable_value(&mut new_k, i, *label);
                    }
                });
            if new_k != k_idx {
                main.target_editor.kind_idx = new_k;
                apply_kind_to(target, new_k);
                if new_k == 0 {
                    main.target_editor.auth_idx = 1; // 切到 SSH 默认密码认证
                }
                main.target_editor.unsaved = true;
            }
            ui.end_row();

            match &mut target.target_type {
                TargetType::Ssh { host, port, user, auth, .. } => {
                    ui.label("主机:");
                    if text_row(ui, host, 220.0) {
                        main.target_editor.unsaved = true;
                    }
                    ui.end_row();

                    ui.label("端口:");
                    if ui.add(egui::DragValue::new(port).range(1..=65535)).changed() {
                        main.target_editor.unsaved = true;
                    }
                    ui.end_row();

                    ui.label("用户名:");
                    if text_row(ui, user, 180.0) {
                        main.target_editor.unsaved = true;
                    }
                    ui.end_row();

                    ui.label("认证:");
                    let a_idx = main.target_editor.auth_idx;
                    let mut new_a = a_idx;
                    egui::ComboBox::from_id_salt("auth_combo")
                        .selected_text(AUTHS[a_idx])
                        .width(200.0)
                        .show_ui(ui, |ui| {
                            for (i, label) in AUTHS.iter().enumerate() {
                                ui.selectable_value(&mut new_a, i, *label);
                            }
                        });
                    if new_a != a_idx {
                        main.target_editor.auth_idx = new_a;
                        *auth = auth_from_index(new_a);
                        main.target_editor.unsaved = true;
                    }
                    ui.end_row();

                    match auth {
                        AuthMethod::Key { path } => {
                            ui.label("私钥路径:");
                            if text_row(ui, path, 260.0) {
                                main.target_editor.unsaved = true;
                            }
                            ui.end_row();
                        }
                        AuthMethod::Password { .. } => {
                            ui.label("密码:");
                            ui.horizontal(|ui| {
                                // 给「保存密码」按钮留位，避免窄窗口下按钮被右缘裁掉
                                let input_w = (ui.available_width() - 90.0).max(120.0);
                                let resp = ui.add(
                                    egui::TextEdit::singleline(
                                        &mut main.target_editor.password_buf,
                                    )
                                    .password(true)
                                    .desired_width(input_w),
                                );
                                if resp.changed() {
                                    main.target_editor.unsaved = true;
                                }
                                if ui.small_button("保存密码").clicked()
                                    && !main.target_editor.password_buf.is_empty()
                                {
                                    match crate::secrets::set_password(
                                        &cred_key,
                                        &main.target_editor.password_buf,
                                    ) {
                                        Ok(()) => {
                                            main.target_editor.password_saved = true;
                                            main.target_editor.password_buf.clear();
                                            main.status_msg = Some(format!(
                                                "密码已存入 Windows Credential Manager（键: {cred_key}）✅"
                                            ));
                                        }
                                        Err(e) => {
                                            main.status_msg =
                                                Some(format!("保存密码失败: {e}"));
                                        }
                                    }
                                }
                            });
                            ui.end_row();
                            ui.label("");
                            ui.colored_label(
                                if main.target_editor.password_saved {
                                    Color32::from_rgb(120, 200, 120)
                                } else {
                                    Color32::from_rgb(230, 180, 80)
                                },
                                if main.target_editor.password_saved {
                                    "✅ 已存入系统凭据管理器（不会写入配置文件）"
                                } else {
                                    "⚠ 尚未保存密码，输入后点「保存密码」"
                                },
                            );
                            ui.end_row();
                        }
                        _ => {
                            ui.label("");
                            ui.weak("该方式不需要额外配置（pageant / 免密）");
                            ui.end_row();
                        }
                    }
                }
                TargetType::Kubernetes { kubeconfig, context, namespace, registry } => {
                    ui.label("kubeconfig:");
                    // 占满单元格剩余宽度：固定 300 在窄窗口会被右缘裁掉
                    if text_row(ui, kubeconfig, ui.available_width()) {
                        main.target_editor.unsaved = true;
                    }
                    ui.end_row();
                    if option_text_row(ui, "context:", context, 180.0) {
                        main.target_editor.unsaved = true;
                    }
                    if option_text_row(ui, "namespace:", namespace, 180.0) {
                        main.target_editor.unsaved = true;
                    }
                    if option_text_row(ui, "registry:", registry, 220.0) {
                        main.target_editor.unsaved = true;
                    }
                }
                TargetType::Local => {
                    ui.label("");
                    ui.weak("本地执行，无需配置");
                    ui.end_row();
                }
            }

            if matches!(target.target_type, TargetType::Ssh { .. }) {
                // 注意：一行只能占 Grid 的两格（label + 值控件）。此前在外面多加了
                // 「远端根目录:」label，把值控件挤到第 2 列、推出可视区（点 ＋ 后
                // 输入框只露一半），现在 label 直接由 option_text_row 渲染
                if option_text_row(ui, "远端根目录:", &mut target.work_root, 220.0) {
                    main.target_editor.unsaved = true;
                }
                ui.label("");
                ui.weak("upload 的相对路径会拼在该目录之后（绝对路径 / 开头则原样）");
                ui.end_row();

                ui.label("主机键指纹:");
                if let TargetType::Ssh { host_key, .. } = &mut target.target_type {
                    let resp = ui.add(
                        egui::TextEdit::singleline(host_key.get_or_insert_default())
                            .desired_width(ui.available_width())
                            .font(egui::TextStyle::Monospace)
                            .hint_text("SHA256:...（连接测试失败时输出会给出，复制填入）"),
                    );
                    if resp.changed() {
                        main.target_editor.unsaved = true;
                    }
                }
                ui.end_row();
            }
        });

    if let Some(new) = rename_to {
        if let Some(map) = scope_map_mut(main) {
            let mut target = map.remove(&selected).expect("checked above");
            target.name = new.clone();
            map.insert(new.clone(), target);
        }
        // 重命名后同步凭据键
        let old_key = cred_key_for(main, &selected);
        let new_key = cred_key_for(main, &new);
        if let Some(pwd) = crate::secrets::get_password(&old_key) {
            let _ = crate::secrets::set_password(&new_key, &pwd);
            crate::secrets::delete_password(&old_key);
        }
        main.target_editor.selected = Some(new);
        main.target_editor.unsaved = true;
    }
}

fn apply_kind_to(target: &mut Target, kind_idx: usize) {
    target.target_type = match kind_idx {
        0 => TargetType::Ssh {
            host: String::new(),
            port: 22,
            user: String::new(),
            auth: AuthMethod::Password { encrypted: String::new() },
            host_key: None,
        },
        1 => TargetType::Kubernetes {
            kubeconfig: String::new(),
            context: None,
            namespace: None,
            registry: None,
        },
        _ => TargetType::Local,
    };
}

fn auth_index(a: &AuthMethod) -> usize {
    match a {
        AuthMethod::Key { .. } => 0,
        AuthMethod::Password { .. } => 1,
        AuthMethod::Agent => 2,
        AuthMethod::None => 3,
    }
}

fn auth_from_index(i: usize) -> AuthMethod {
    match i {
        0 => AuthMethod::Key { path: String::new() },
        1 => AuthMethod::Password { encrypted: String::new() },
        2 => AuthMethod::Agent,
        _ => AuthMethod::None,
    }
}

fn text_row(ui: &mut egui::Ui, value: &mut String, width: f32) -> bool {
    ui.add(egui::TextEdit::singleline(value).desired_width(width)).changed()
}

/// Option<String> 字段编辑行（调用方需处于 Grid 中），返回是否修改
fn option_text_row(
    ui: &mut egui::Ui,
    label: &str,
    value: &mut Option<String>,
    width: f32,
) -> bool {
    let mut changed = false;
    ui.label(label);
    match value {
        Some(v) => {
            let mut clear = false;
            ui.horizontal(|ui| {
                changed |= ui
                    .add(egui::TextEdit::singleline(v).desired_width(width))
                    .changed();
                if ui.small_button("×").clicked() {
                    clear = true;
                }
            });
            if clear {
                *value = None;
                changed = true;
            }
        }
        None => {
            ui.horizontal(|ui| {
                ui.weak("—");
                if ui.small_button("＋").clicked() {
                    *value = Some(String::new());
                    changed = true;
                }
            });
        }
    }
    ui.end_row();
    changed
}

fn load_buffers(main: &mut MainState, name: &str) {
    main.target_editor.name_buf = name.to_string();
    let key = cred_key_for(main, name);
    // 克隆目标后在 owned 数据上操作，避免整表借用与 target_editor 写入冲突
    let Some(t) = scope_map_ref(main).and_then(|m| m.get(name)).cloned() else {
        main.target_editor.password_saved = false;
        return;
    };
    match &t.target_type {
        TargetType::Ssh { host, port, user, auth, .. } => {
            main.target_editor.kind_idx = 0;
            main.target_editor.host = host.clone();
            main.target_editor.port = *port;
            main.target_editor.user = user.clone();
            main.target_editor.auth_idx = auth_index(auth);
            main.target_editor.key_path = match auth {
                AuthMethod::Key { path } => path.clone(),
                _ => String::new(),
            };
        }
        TargetType::Kubernetes { kubeconfig, context, namespace, registry } => {
            main.target_editor.kind_idx = 1;
            main.target_editor.kubeconfig = kubeconfig.clone();
            main.target_editor.context = context.clone().unwrap_or_default();
            main.target_editor.namespace = namespace.clone().unwrap_or_default();
            main.target_editor.registry = registry.clone().unwrap_or_default();
        }
        TargetType::Local => {
            main.target_editor.kind_idx = 2;
        }
    }
    main.target_editor.work_root = t.work_root.clone().unwrap_or_default();
    main.target_editor.password_buf.clear();
    main.target_editor.password_saved = crate::secrets::get_password(&key).is_some();
}

/// 后台线程做 SSH 连接测试 + 服务器体检（复用 putty::probe_server，8 秒超时）；
/// 回填凭据键，保证密码取自本项目的条目。
/// 表单里已输入、尚未点「保存密码」的密码直接参与本次测试（DBeaver 式：
/// 首次配置即可验证连通性，不必先落凭据管理器；临时密码不落任何存储）。
fn start_test(main: &mut MainState) {
    let Some(name) = main.target_editor.selected.clone() else {
        return;
    };
    let Some(mut t) = scope_map_ref(main).and_then(|m| m.get(&name).cloned()) else {
        return;
    };
    if !matches!(t.target_type, TargetType::Ssh { .. }) {
        main.target_editor.test_result =
            Some((false, "只有 SSH 目标支持连接测试".to_string()));
        return;
    }
    if matches!(&t.target_type, TargetType::Ssh { host, .. } if host.trim().is_empty()) {
        main.target_editor.test_result = Some((false, "主机为空，请先填写主机地址".to_string()));
        return;
    }
    t.cred_key = Some(cred_key_for(main, &name));

    // 已输入未保存的密码：仅本次测试使用
    let typed_password = matches!(
        &t.target_type,
        TargetType::Ssh { auth: AuthMethod::Password { .. }, .. }
    )
    .then(|| main.target_editor.password_buf.clone())
    .filter(|p| !p.is_empty());

    main.target_editor.test_result = None;
    let (tx, rx) = std::sync::mpsc::channel();
    main.target_editor.test_rx = Some(rx);
    std::thread::spawn(move || {
        let res = crate::transport::putty::probe_server_with(&t, typed_password.as_deref())
            .map(|(info, elapsed)| {
                let mut text = info.display(elapsed);
                if typed_password.is_some() {
                    text.push_str(
                        "\n（本次使用表单输入的密码，尚未保存——点「保存密码」写入凭据管理器）",
                    );
                }
                text
            });
        let _ = tx.send(res);
    });
}
