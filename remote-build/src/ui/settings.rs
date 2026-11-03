//! 设置窗口：工具管理（全局 tools.yaml / 项目级 project.yaml 双作用域）
//! + PuTTY 状态 + 全局参数展示。
//!
//! 工具分层（§6.5）：全局放机器级工具（mvn/node/git，探测自动生成）；
//! 项目级放工程级覆盖（pnpm/yarn 等，键相同整体覆盖全局）。
//! 步骤下拉读取两者的合并结果，并以「来源」徽标标注。

use crate::app::MainState;
use crate::models::{Tool, ToolType, ToolsFile};
use anyhow::anyhow;
use eframe::egui;
use egui::Color32;

const TOOL_TYPES: [&str; 4] = [
    "system（系统 PATH）",
    "archive（下载解压）",
    "custom（自定义路径）",
    "script（脚本）",
];

fn tool_type_index(t: &ToolType) -> usize {
    match t {
        ToolType::System => 0,
        ToolType::Archive => 1,
        ToolType::Custom => 2,
        ToolType::Script => 3,
    }
}

fn tool_type_from_index(i: usize) -> ToolType {
    match i {
        0 => ToolType::System,
        1 => ToolType::Archive,
        2 => ToolType::Custom,
        _ => ToolType::Script,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ToolScope {
    /// 全局 tools.yaml：机器级工具（mvn / node / git…）
    #[default]
    Global,
    /// 本项目 project.yaml 的 tools 段：工程级覆盖（pnpm / yarn…）
    Project,
}

/// 工具编辑器的会话状态（不落盘，保存即写对应作用域的配置文件）
#[derive(Default)]
pub struct ToolEditorState {
    pub scope: ToolScope,
    /// Project 作用域绑定的项目名（切换项目时重置编辑状态）
    pub bound_project: Option<String>,
    /// 当前选中工具（作用域配置中的键）
    pub selected: Option<String>,
    /// 名称输入缓冲（重命名用）
    pub name_buf: String,
    /// env 编辑缓冲（"KEY=VALUE" 每行一条）
    pub env_buf: String,
    /// 验证结果 (是否成功, 展示文本)
    pub verify: Option<(bool, String)>,
    /// 后台验证线程回传（工具名, 结果）——名随结果走，切换选中后仍能归位
    pub verify_rx: Option<std::sync::mpsc::Receiver<(String, Option<String>)>>,
    /// 本会话内每个工具的最近验证结果（列表 ✓/✗ 标记；会话态不落盘）
    pub verify_results: std::collections::HashMap<String, (bool, String)>,
    pub unsaved: bool,
}

fn reset_editor(main: &mut MainState) {
    main.tool_editor.selected = None;
    main.tool_editor.name_buf.clear();
    main.tool_editor.env_buf.clear();
    main.tool_editor.verify = None;
    main.tool_editor.verify_results.clear();
    main.tool_editor.unsaved = false;
}

/// 从作业编辑器一键打开「本项目」工具覆盖页
pub fn open_project_scope(main: &mut MainState) {
    main.show_settings = true;
    // 作用域和窗口 tab 要一起切：只切作用域不切 tab 时，窗口停在上次的
    // 「运行历史」等页，用户点「工具覆盖」看到的却是不相干页面
    main.settings_tab = SettingsTab::Tools;
    if main.tool_editor.scope != ToolScope::Project {
        main.tool_editor.scope = ToolScope::Project;
        reset_editor(main);
    }
}

/// 当前作用域对应的工具表（可变）
fn scope_map_mut(
    main: &mut MainState,
) -> Option<&mut std::collections::HashMap<String, Tool>> {
    match main.tool_editor.scope {
        ToolScope::Global => Some(&mut main.tools),
        ToolScope::Project => {
            let pi = main.selected_project?;
            Some(&mut main.projects[pi].project.tools)
        }
    }
}

/// 当前作用域对应的工具表（只读）
fn scope_map_ref(main: &MainState) -> Option<&std::collections::HashMap<String, Tool>> {
    match main.tool_editor.scope {
        ToolScope::Global => Some(&main.tools),
        ToolScope::Project => main
            .selected_project
            .and_then(|pi| main.projects.get(pi))
            .map(|p| &p.project.tools),
    }
}

/// 设置窗口的分区 tab
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SettingsTab {
    #[default]
    Tools,
    Global,
    History,
}

/// 文件夹样式的 tab 行：选中项填充高亮 + 粗体并压在下方分隔线上。
/// 返回被点击的 tab 下标（点击当前项也会返回，调用方赋值幂等）。
pub(crate) fn tab_row(ui: &mut egui::Ui, tabs: &[String], selected: usize, enabled: &[bool]) -> Option<usize> {
    let mut clicked = None;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 2.0;
        for (i, label) in tabs.iter().enumerate() {
            let active = i == selected;
            let mut text = egui::RichText::new(label.as_str());
            text = if active {
                text.strong()
            } else {
                text.color(ui.visuals().weak_text_color())
            };
            let frame = egui::Frame::new()
                .fill(if active {
                    ui.visuals().selection.bg_fill
                } else {
                    egui::Color32::TRANSPARENT
                })
                .corner_radius(egui::CornerRadius { nw: 6, ne: 6, sw: 0, se: 0 })
                .inner_margin(egui::Margin::symmetric(10, 4));
            let inner = frame.show(ui, |ui| {
                let r = if enabled[i] {
                    ui.add(egui::Label::new(text).sense(egui::Sense::click()))
                        .on_hover_cursor(egui::CursorIcon::PointingHand)
                } else {
                    ui.add(egui::Label::new(text.weak()).sense(egui::Sense::hover()))
                };
                r.clicked() && enabled[i]
            });
            if inner.inner {
                clicked = Some(i);
            }
        }
    });
    ui.separator();
    clicked
}

pub fn show(ctx: &egui::Context, main: &mut MainState) {
    // 回收后台验证结果
    if let Some(rx) = &main.tool_editor.verify_rx {
        match rx.try_recv() {
            Ok((name, Some(first_line))) => {
                main.tool_editor
                    .verify_results
                    .insert(name.clone(), (true, first_line.clone()));
                main.tool_editor.verify = Some((true, first_line));
                main.tool_editor.verify_rx = None;
            }
            Ok((name, None)) => {
                let msg = "无输出（超时 3s / 命令不存在 / 启动失败）".to_string();
                main.tool_editor
                    .verify_results
                    .insert(name.clone(), (false, msg.clone()));
                main.tool_editor.verify = Some((false, msg));
                main.tool_editor.verify_rx = None;
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                main.tool_editor.verify_rx = None;
            }
        }
    }

    // Project 作用域：切换了所选项目则重置编辑状态
    if main.tool_editor.scope == ToolScope::Project {
        let current = main
            .selected_project
            .and_then(|pi| main.projects.get(pi))
            .map(|p| p.project.name.clone());
        if main.tool_editor.bound_project != current {
            main.tool_editor.bound_project = current;
            reset_editor(main);
        }
    }

    let mut open = main.show_settings;
    if open {
        crate::ui::modal_backdrop(ctx, "mask_settings");
    }
    crate::ui::centered_window(egui::Window::new("⚙ 设置"), ctx)
    .collapsible(false)
        .open(&mut open)
        .order(egui::Order::Foreground)
        .default_width(780.0)
        .default_height(560.0)
        .show(ctx, |ui| {
            // ── 窗口级 tab：工具管理 / 全局配置 / 运行历史 ──
            //（PuTTY 不再单开 tab：套件目录配置并入工具管理的 putty 条目表单）
            let tab_idx = match main.settings_tab {
                SettingsTab::Tools => 0,
                SettingsTab::Global => 1,
                SettingsTab::History => 2,
            };
            let section_tabs = vec![
                "🔧 工具管理".to_string(),
                "⚙ 全局配置".to_string(),
                "🕘 运行历史".to_string(),
            ];
            if let Some(i) = tab_row(ui, &section_tabs, tab_idx, &[true, true, true]) {
                main.settings_tab = match i {
                    1 => SettingsTab::Global,
                    2 => SettingsTab::History,
                    _ => SettingsTab::Tools,
                };
            }
            ui.add_space(6.0);

            match main.settings_tab {
                SettingsTab::Tools => {
                    ui.colored_label(
                        Color32::from_rgb(230, 180, 80),
                        "⚠ 本窗口编辑的是内存副本：如果你手动修改过 tools.yaml / project.yaml，\
                         请先点工具栏 ⟳ 重新加载再打开本窗口，否则「保存」会用旧内容覆盖你的手改。",
                    );
                    // ── 作用域 tab：全局 / 本项目 ──
                    let has_project = main.selected_project.is_some();
                    let scope_idx = match main.tool_editor.scope {
                        ToolScope::Global => 0,
                        ToolScope::Project => 1,
                    };
                    let project_name = main
                        .selected_project
                        .and_then(|pi| main.projects.get(pi))
                        .map(|p| p.project.name.clone());
                    let scope_tabs = vec![
                        "🌐 全局 (tools.yaml)".to_string(),
                        match &project_name {
                            Some(name) => format!("📦 本项目[{name}] (project.yaml)"),
                            None => "📦 本项目 (project.yaml)".to_string(),
                        },
                    ];
                    if let Some(i) = tab_row(ui, &scope_tabs, scope_idx, &[true, has_project]) {
                        let new_scope = if i == 0 { ToolScope::Global } else { ToolScope::Project };
                        if new_scope != main.tool_editor.scope {
                            main.tool_editor.scope = new_scope;
                            reset_editor(main);
                        }
                    }
                    ui.weak(match main.tool_editor.scope {
                        ToolScope::Global => {
                            "机器级工具（mvn / node / git…），来自首次启动探测；对应步骤下拉的「全局」分组。"
                        }
                        ToolScope::Project => {
                            "工程级工具（pnpm / yarn…），只对当前项目生效，键相同整体覆盖全局；\
                             对应步骤下拉的「本项目」分组。保存写入当前项目的 project.yaml。"
                        }
                    });
                    // 加载状态：让"内存已加载 N 个"与磁盘文件可对照，外部手改后点 ⟳ 重新加载
                    let (count, file) = match main.tool_editor.scope {
                        ToolScope::Global => (
                            main.tools.len(),
                            crate::config::tools_path().to_string_lossy().to_string(),
                        ),
                        ToolScope::Project => {
                            let n = main
                                .selected_project
                                .and_then(|pi| main.projects.get(pi))
                                .map(|p| p.project.tools.len())
                                .unwrap_or(0);
                            let f = main
                                .selected_project
                                .and_then(|pi| main.projects.get(pi))
                                .map(|p| p.dir.join("project.yaml").to_string_lossy().to_string())
                                .unwrap_or_default();
                            (n, f)
                        }
                    };
                    ui.weak(format!("已加载 {count} 个工具 ← {file}"));
                    ui.add_space(4.0);
                    tool_editor(ui, main);
                }
                SettingsTab::Global => global_section(ctx, ui, main),
                SettingsTab::History => history_section(ui, main),
            }
        });
    // 每帧把窗口抬回遮罩之上（遮罩被点击/首帧会在这带内置顶）
    main.show_settings = open;

    if main.tool_editor.verify_rx.is_some() {
        ctx.request_repaint_after(std::time::Duration::from_millis(100));
    }
}

// ---------------------------------------------------------------------------
// 工具管理
// ---------------------------------------------------------------------------

/// 常用工具预设：（名称, version_check）。一键添加后 command 可再按需改成完整路径。
const TOOL_PRESETS: &[(&str, &str)] = &[
    ("pnpm", "pnpm -v"),
    ("yarn", "yarn -v"),
    ("npm", "npm -v"),
    ("node", "node -v"),
    ("python", "python --version"),
    ("pip", "pip --version"),
    ("docker", "docker --version"),
    ("kubectl", "kubectl version --client"),
    ("gradle", "gradle -v"),
    ("maven", "mvn -v"),
    ("git", "git --version"),
    // 数据库客户端（备份 / 迁移：mysqldump / pg_dump 等裸名命令经工具 PATH 解析）
    ("mysql", "mysql --version"),
    ("mysqldump", "mysqldump --version"),
    ("psql", "psql --version"),
    ("pg_dump", "pg_dump --version"),
    ("pg_restore", "pg_restore --version"),
    // 压缩 / 其他常用 CLI
    ("7z", "7z i"),
    ("sqlite3", "sqlite3 --version"),
    ("redis-cli", "redis-cli --version"),
    // putty 是套件（plink + pscp）：预设给裸名入口，选中条目后用专用表单配套件目录
    // （传输层直接读探测结果，preset 条目会在重新探测时自动更新为真实路径）
    ("putty", "plink -V"),
];

fn tool_editor(ui: &mut egui::Ui, main: &mut MainState) {
    let mut names: Vec<String> = scope_map_ref(main)
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();
    names.sort();

    // 2:8 布局：工具名不长，左窄列足够；右侧表单（command 路径、env 多行）需要宽度。
    // 与 🎯 目标管理的分栏同款（ui.columns 只会 1:1 均分，此处手动分配）
    let total = ui.available_width();
    let spacing = ui.spacing().item_spacing.x;
    let left_w = ((total - spacing) * 0.2).clamp(150.0, 220.0);
    let right_w = (total - spacing - left_w).max(200.0);
    ui.horizontal(|ui| {
        // 必须用 allocate_ui_with_layout 显式传 top_down 布局：allocate_ui 会继承
        // 外层 ui.horizontal 的横向布局（egui 0.37），工具名列表被横排成一行
        // （targets 分栏同款问题与修法）
        ui.allocate_ui_with_layout(
            egui::vec2(left_w, 240.0),
            egui::Layout::top_down_justified(egui::Align::LEFT),
            |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("tool_list")
                    .max_height(240.0)
                    .show(ui, |ui| {
                        for name in &names {
                            let sel =
                                main.tool_editor.selected.as_deref() == Some(name.as_str());
                            // 「最近验证」标记：本会话验证过的工具带 ✓/✗（运行期引擎自检的
                            // 手动对应物；悬停看验证输出详情）
                            let mut text = name.clone();
                            if let Some((ok, _)) = main.tool_editor.verify_results.get(name) {
                                text.push_str(if *ok { " ✓" } else { " ✗" });
                            }
                            // 长工具名截断 + 悬停看全（左列收窄后防折行把行高撑乱）
                            let shown = crate::ui::truncate_label(&text, 18);
                            let mut resp = ui.selectable_label(sel, egui::RichText::new(&shown));
                            if shown != text {
                                resp = resp.on_hover_text(&text);
                            }
                            if let Some((_, msg)) = main.tool_editor.verify_results.get(name) {
                                resp = resp.on_hover_text(format!("最近验证：{msg}"));
                            }
                            if resp.clicked() {
                                main.tool_editor.selected = Some(name.clone());
                                load_buffers(main, name);
                                main.tool_editor.verify = None;
                            }
                        }
                        if names.is_empty() {
                            ui.weak("（空，在下方添加）");
                        }
                    });
            },
        );

        ui.allocate_ui_with_layout(
            egui::vec2(right_w, 240.0),
            egui::Layout::top_down_justified(egui::Align::LEFT),
            |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("tool_form")
                    .max_height(240.0)
                    .show(ui, |ui| {
                        edit_form(ui, main);
                    });
            },
        );
    });

    ui.add_space(6.0);
    ui.horizontal(|ui| {
        let scope = main.tool_editor.scope;
        if ui.button("＋ 添加").clicked() {
            let mut n = 1;
            let mut name = format!("new-tool-{n}");
            while scope_map_ref(main).map(|m| m.contains_key(&name)).unwrap_or(false) {
                n += 1;
                name = format!("new-tool-{n}");
            }
            if let Some(map) = scope_map_mut(main) {
                map.insert(
                    name.clone(),
                    Tool { name: name.clone(), tool_type: ToolType::System, ..Default::default() },
                );
            }
            main.tool_editor.selected = Some(name.clone());
            load_buffers(main, &name);
            main.tool_editor.verify = None;
            main.tool_editor.unsaved = true;
        }

        // 常用工具预设：一键添加，免手写条目
        egui::ComboBox::from_id_salt("tool_preset_combo")
            .selected_text("＋ 常用工具…")
            .width(140.0)
            .show_ui(ui, |ui| {
                for (name, version_check) in TOOL_PRESETS {
                    let exists =
                        scope_map_ref(main).map(|m| m.contains_key(*name)).unwrap_or(false);
                    let label = if exists {
                        egui::RichText::new(*name).weak()
                    } else {
                        egui::RichText::new(*name)
                    };
                    if ui.selectable_label(!exists, label).clicked() && !exists {
                        if let Some(map) = scope_map_mut(main) {
                            map.insert(
                                name.to_string(),
                                Tool {
                                    name: name.to_string(),
                                    tool_type: ToolType::System,
                                    command: name.to_string(),
                                    version_check: Some(version_check.to_string()),
                                    detected_from: Some("preset".to_string()),
                                    ..Default::default()
                                },
                            );
                        }
                        main.tool_editor.selected = Some(name.to_string());
                        load_buffers(main, name);
                        main.tool_editor.verify = None;
                        main.tool_editor.unsaved = true;
                    }
                }
            });

        if ui.button("🗑 删除").clicked() {
            if let Some(name) = main.tool_editor.selected.take() {
                if let Some(map) = scope_map_mut(main) {
                    map.remove(&name);
                }
                main.tool_editor.unsaved = true;
            }
        }
        if ui.button("🔍 验证 version_check").clicked() {
            start_verify(main);
        }

        let save_label = match scope {
            ToolScope::Global => "💾 保存 tools.yaml",
            ToolScope::Project => "💾 保存 project.yaml",
        };
        if ui.button(save_label).clicked() {
            let result = match scope {
                ToolScope::Global => crate::config::save_tools(&ToolsFile {
                    tools: main.tools.clone(),
                }),
                ToolScope::Project => {
                    match main.selected_project {
                        Some(pi) => {
                            let entry = &main.projects[pi];
                            crate::storage::files::save_yaml(
                                &entry.dir.join("project.yaml"),
                                &entry.project,
                            )
                        }
                        None => Err(anyhow!("项目作用域需要先在左侧选择项目")),
                    }
                }
            };
            match result {
                Ok(()) => {
                    main.tool_editor.unsaved = false;
                    main.status_msg = Some("工具配置已保存 ✅".to_string());
                }
                Err(e) => main.status_msg = Some(format!("保存失败: {e}")),
            }
        }
        if main.tool_editor.unsaved {
            ui.colored_label(Color32::from_rgb(230, 180, 80), "（有未保存修改）");
        }
    });

    if let Some((ok, msg)) = &main.tool_editor.verify {
        let color = if *ok {
            Color32::from_rgb(120, 200, 120)
        } else {
            Color32::from_rgb(230, 110, 110)
        };
        let icon = if *ok { "✅" } else { "❌" };
        ui.colored_label(color, format!("{icon} {msg}"));
    }
}

fn edit_form(ui: &mut egui::Ui, main: &mut MainState) {
    let Some(selected) = main.tool_editor.selected.clone() else {
        ui.weak("← 选择或添加一个工具");
        return;
    };
    if !scope_map_ref(main).map(|m| m.contains_key(&selected)).unwrap_or(false) {
        main.tool_editor.selected = None;
        return;
    }

    // putty 特例：它是"套件"不是单个命令行工具——传输层（plink / pscp）直接读取
    // 套件目录的探测结果，通用 command / 版本字段对它无意义。原「🔌 PuTTY」tab
    // 并入此处：选中 putty 条目即显示套件目录配置（原 tab 内容同款）。
    // tools.yaml 的 putty 条目由探测自动生成 / 更新，仅供展示。
    if selected == "putty" {
        ui.weak(
            "putty 是套件（plink + pscp + ...），传输层直接读取下方目录的探测结果；\
             此条目由探测自动生成 / 更新，通用字段不生效。",
        );
        ui.add_space(4.0);
        putty_section(ui, &mut main.global);
        return;
    }

    // 重命名放在取 map 可变借用之后会失效，这里先收集、表单结束后统一应用
    let mut rename_to: Option<String> = None;

    egui::Grid::new("tool_form_grid")
        .num_columns(2)
        .spacing([10.0, 4.0])
        .show(ui, |ui| {
            ui.label("名称:");
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut main.tool_editor.name_buf)
                        .desired_width(150.0),
                );
                if ui.small_button("重命名").clicked() {
                    let new = main.tool_editor.name_buf.trim().to_string();
                    let exists = scope_map_ref(main)
                        .map(|m| m.contains_key(&new))
                        .unwrap_or(true);
                    if !new.is_empty() && new != selected && !exists {
                        rename_to = Some(new);
                    }
                }
            });
            ui.end_row();

            // 按作用域取工具的可变借用：match 内联以保持字段级借用，
            // 避免辅助函数把整个 &mut MainState 的生命周期带进来
            let tool = match main.tool_editor.scope {
                ToolScope::Global => main.tools.get_mut(&selected),
                ToolScope::Project => main
                    .selected_project
                    .and_then(|pi| main.projects.get_mut(pi))
                    .and_then(|p| p.project.tools.get_mut(&selected)),
            };
            let Some(tool) = tool else {
                return;
            };

            ui.label("command:");
            ui.horizontal(|ui| {
                let r = ui.add(
                    egui::TextEdit::singleline(&mut tool.command).desired_width(300.0),
                );
                if r.changed() {
                    main.tool_editor.unsaved = true;
                }
                if ui.small_button("📂").clicked() {
                    if let Some(p) = rfd::FileDialog::new()
                        .set_title("选择工具可执行文件")
                        .pick_file()
                    {
                        tool.command = p.to_string_lossy().to_string();
                        main.tool_editor.unsaved = true;
                    }
                }
            });
            ui.end_row();

            ui.label("类型:");
            let t_idx = tool_type_index(&tool.tool_type);
            let mut new_idx = t_idx;
            egui::ComboBox::from_id_salt("tool_type_combo")
                .selected_text(TOOL_TYPES[t_idx])
                .width(180.0)
                .show_ui(ui, |ui| {
                    for (i, label) in TOOL_TYPES.iter().enumerate() {
                        ui.selectable_value(&mut new_idx, i, *label);
                    }
                });
            if new_idx != t_idx {
                tool.tool_type = tool_type_from_index(new_idx);
                main.tool_editor.unsaved = true;
            }
            ui.end_row();

            if option_text_row(ui, "版本:", &mut tool.version, 180.0) {
                main.tool_editor.unsaved = true;
            }
            if option_text_row(ui, "版本检查:", &mut tool.version_check, 220.0) {
                main.tool_editor.unsaved = true;
            }
            if option_text_row(ui, "探测来源:", &mut tool.detected_from, 180.0) {
                main.tool_editor.unsaved = true;
            }

            ui.label("env（KEY=VALUE）:");
            let resp = ui.add(
                egui::TextEdit::multiline(&mut main.tool_editor.env_buf)
                    .desired_rows(3)
                    .desired_width(300.0)
                    .font(egui::TextStyle::Monospace)
                    .hint_text("JAVA_HOME=C:/Program Files/Java/jdk-17\nPIP_INDEX_URL=https://nexus.internal/repository/pypi/simple"),
            );
            if resp.changed() {
                tool.env = parse_env_lines(&main.tool_editor.env_buf);
                main.tool_editor.unsaved = true;
            }
            ui.end_row();
        });

    if let Some(new) = rename_to {
        if let Some(map) = scope_map_mut(main) {
            let tool = map.remove(&selected).expect("checked above");
            map.insert(new.clone(), tool);
        }
        main.tool_editor.selected = Some(new);
        main.tool_editor.unsaved = true;
    }
}

/// Option<String> 字段的编辑行（有值显示输入框 + × 清除；无值显示 — + ＋ 添加）
/// 返回是否发生了修改。调用方需处于 Grid 中。
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
                if ui.small_button("🗑").clicked() {
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
    main.tool_editor.name_buf = name.to_string();
    let env = scope_map_ref(main)
        .and_then(|m| m.get(name))
        .map(|tool| {
            let mut lines: Vec<String> =
                tool.env.iter().map(|(k, v)| format!("{k}={v}")).collect();
            lines.sort();
            lines.join("\n")
        })
        .unwrap_or_default();
    main.tool_editor.env_buf = env;
}

fn parse_env_lines(text: &str) -> std::collections::HashMap<String, String> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            line.split_once('=')
                .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        })
        .collect()
}

/// 后台线程跑 version_check（≤3s），避免阻塞 UI
fn start_verify(main: &mut MainState) {
    let Some(name) = main.tool_editor.selected.clone() else {
        return;
    };
    // putty 特例：套件版本经探测到的 plink -V 验证，不走 version_check——
    // putty.exe 本体是 GUI 程序（--version 弹窗、不往 stdout 打）；
    // plink 在 PuTTY 安装器默认不进 PATH，裸名验证只会"无输出"，
    // 必须用套件探测（自定义目录 → PATH → Program Files）拿到的完整路径
    if name == "putty" {
        let detection = crate::transport::putty::detect_putty_tools();
        let result = if !detection.found {
            Err(
                "未找到 PuTTY 套件（自定义目录 / PATH / Program Files 均未命中）——\
                 请先安装 PuTTY，或在此表单指定套件目录"
                    .to_string(),
            )
        } else {
            let plink = detection.plink.to_string_lossy().to_string();
            match crate::env_detect::probe(&plink, &["-V"]) {
                Some(line) => Ok(line),
                None => Err(format!("plink -V 无输出（套件目录缺 plink.exe？）: {plink}")),
            }
        };
        main.tool_editor.verify = Some(match &result {
            Ok(line) => (true, line.clone()),
            Err(msg) => (false, msg.clone()),
        });
        match result {
            Ok(line) => {
                main.tool_editor
                    .verify_results
                    .insert(name, (true, line));
            }
            Err(msg) => {
                main.tool_editor
                    .verify_results
                    .insert(name, (false, msg));
            }
        }
        return;
    }
    let Some(check) = scope_map_ref(main)
        .and_then(|m| m.get(&name))
        .and_then(|t| t.version_check.clone())
        .filter(|s| !s.trim().is_empty())
    else {
        main.tool_editor.verify = Some((
            false,
            "未配置 version_check（如 java -version / mvn -v）".to_string(),
        ));
        return;
    };
    // 连同工具本体一起带过去：验证要按工具的 env / command 所在目录解析，
    // 否则裸名命令解析到系统 PATH 上的同名程序，结果与工具实际版本不符
    let tool = scope_map_ref(main).and_then(|m| m.get(&name)).cloned();
    main.tool_editor.verify = None;
    let (tx, rx) = std::sync::mpsc::channel();
    main.tool_editor.verify_rx = Some(rx);
    std::thread::spawn(move || {
        let result = match tool {
            Some(t) => crate::env_detect::run_version_check_for_tool(&check, &t),
            None => crate::env_detect::run_version_check(&check),
        };
        let _ = tx.send((name, result));
    });
}

// ---------------------------------------------------------------------------
// PuTTY：自定义目录设置（探测在执行时按「custom → PATH → Program Files」进行）
// ---------------------------------------------------------------------------

fn putty_section(ui: &mut egui::Ui, global: &mut crate::models::GlobalConfig) {
    ui.heading("PuTTY");
    ui.weak(
        "transport: putty 的目标执行 / 上传依赖 plink / pscp。不设置时按「系统 PATH → \
         Program Files」自动探测；自定义目录优先级最高，需包含 plink.exe / pscp.exe。",
    );
    ui.add_space(4.0);

    ui.label("PuTTY 目录:");
    let mut changed = false;
    ui.horizontal(|ui| {
        match &mut global.putty.custom_path {
            Some(path) => {
                changed |= ui
                    .add(
                        egui::TextEdit::singleline(path)
                            .desired_width(430.0)
                            .hint_text("D:/tools/putty"),
                    )
                    .changed();
                if ui
                    .small_button("🗑")
                    .on_hover_text("清除，恢复自动探测")
                    .clicked()
                {
                    global.putty.custom_path = None;
                    changed = true;
                }
            }
            None => {
                ui.weak("（未设置，自动探测）");
            }
        }
        if ui.button("📂 浏览…").clicked() {
            // 起点 = 输入框当前路径（所有浏览按钮统一约定）
            let starting = global.putty.custom_path.clone().unwrap_or_default();
            if let Some(dir) = crate::ui::pick_folder(
                &starting,
                "选择 PuTTY 所在文件夹（含 plink.exe / pscp.exe）",
            ) {
                global.putty.custom_path = Some(dir.to_string_lossy().to_string());
                changed = true;
            }
        }
    });
    if changed {
        if let Err(e) = crate::config::save_global(global) {
            eprintln!("保存 PuTTY 目录配置失败: {e}");
        }
    }

    // 只对自定义目录本身做存在性校验（不做全量探测展示），给出能否直接生效的即时反馈
    if let Some(path) = &global.putty.custom_path {
        let dir = std::path::PathBuf::from(path);
        if dir.join("plink.exe").exists() && dir.join("pscp.exe").exists() {
            ui.colored_label(
                Color32::from_rgb(120, 200, 120),
                format!("✅ 目录有效（已找到 plink.exe / pscp.exe）: {path}"),
            );
        } else {
            ui.colored_label(
                Color32::from_rgb(230, 180, 80),
                format!("⚠ {path} 下未找到 plink.exe / pscp.exe，执行时将回退自动探测"),
            );
        }
    }
}

// ---------------------------------------------------------------------------
// 全局参数（只读展示，P0）
// ---------------------------------------------------------------------------

fn global_section(
    ctx: &egui::Context,
    ui: &mut egui::Ui,
    main: &mut MainState,
) {
    let global = &mut main.global;
    ui.heading("全局配置（config.yaml）");
    egui::Grid::new("settings_global")
        .num_columns(2)
        .spacing([16.0, 4.0])
        .show(ui, |ui| {
            ui.label("配置目录:");
            ui.monospace(crate::config::app_root().to_string_lossy().to_string());
            ui.end_row();
            ui.label("项目目录 (projects_root):");
            ui.monospace(&global.projects_root);
            ui.end_row();
            ui.label("默认超时:");
            ui.label(format!("{}s", global.default_timeout));
            ui.end_row();
            ui.label("日志保留:");
            ui.label(format!("{} 天（日志文件与运行历史共用）", global.log_retention_days));
            ui.end_row();
            ui.label("保留运行历史:");
            if ui
                .checkbox(&mut global.save_history, "存 SQLite（~/.remote-build/data/history.db）")
                .changed()
                && crate::config::save_global(global).is_err()
            {
                eprintln!("保存 config.yaml 失败");
            }
            ui.end_row();
            ui.label("桌面通知:");
            if ui
                .checkbox(&mut global.desktop_notify, "作业结束时弹 Windows 通知（toast）")
                .changed()
                && crate::config::save_global(global).is_err()
            {
                eprintln!("保存 config.yaml 失败");
            }
            ui.end_row();
            ui.label("主题:");
            let theme_names = ["明亮", "暗黑"];
            let mut idx = match global.theme {
                crate::models::Theme::Light => 0,
                crate::models::Theme::Dark => 1,
                crate::models::Theme::System => 2,
            };
            egui::ComboBox::from_id_salt("theme_combo")
                .selected_text(theme_names[idx.min(1)])
                .width(120.0)
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut idx, 0, "明亮");
                    ui.selectable_value(&mut idx, 1, "暗黑");
                });
            let new_theme = match idx {
                0 => crate::models::Theme::Light,
                _ => crate::models::Theme::Dark,
            };
            if new_theme != global.theme {
                global.theme = new_theme;
                crate::app::apply_theme(ctx, &global.theme);
                match crate::config::save_global(global) {
                    Ok(()) => {} // 已持久化到 config.yaml
                    Err(e) => eprintln!("保存主题失败: {e}"),
                }
            }
            ui.end_row();
        });
    ui.weak(
        "提示：PuTTY 自定义目录在「🔧 工具管理」选中 putty 条目后设置（putty.custom_path）；\
         tools.yaml 的 putty 条目由探测自动生成，仅供展示。控制台配色跟随主题（深浅两套色板各自调校）。",
    );

    notify_section(ui, main);

    ui.add_space(8.0);
    ui.separator();
    ui.heading("示范工程");
    ui.horizontal(|ui| {
        if ui
            .button("重新生成 example（五本示范合集）")
            .on_hover_text(
                "覆盖 projects/example 下的 project.yaml 与 git/build/linux/deploy/db 五本合集 yaml。\n\
                 只影响示范工程，你自己的项目不动",
            )
            .clicked()
        {
            match crate::config::create_example_project() {
                Ok(()) => {
                    main.status_msg = Some(
                        "示范工程已重新生成（Git / 构建 / Linux / 发布 / 数据库 五本合集）"
                            .to_string(),
                    );
                    main.reload();
                }
                Err(e) => main.status_msg = Some(format!("重新生成失败: {e}")),
            }
        }
        ui.weak("Git / 构建 / Linux 远端 / 发布 / 数据库 五本合集，点「👁 预览」当活文档看");
    });
}

// ---------------------------------------------------------------------------
// 消息通知（Mattermost / 钉钉 / 邮箱）：全局触发开关 + 三渠道配置。
// 凭据字段改动即写凭据管理器（不落 YAML）；「发送测试」同步实发一条。
// ---------------------------------------------------------------------------

fn notify_section(ui: &mut egui::Ui, main: &mut MainState) {
    ui.add_space(8.0);
    ui.separator();
    ui.heading("消息通知（作业结束推送）");
    let mut changed = false;
    let mut test_msg: Option<String> = None;

    {
        let notify = &mut main.global.notify;
        ui.horizontal(|ui| {
            changed |= ui.checkbox(&mut notify.on_failure, "失败时推送").changed();
            changed |= ui.checkbox(&mut notify.on_success, "成功时推送").changed();
        });

        // ── Mattermost：webhook URL 存凭据管理器（键 notify:mattermost）──
        ui.add_space(4.0);
        let mut mm_url = crate::secrets::get_password(crate::notify::KEY_MATTERMOST)
            .ok()
            .flatten()
            .unwrap_or_default();
        ui.horizontal(|ui| {
            changed |= ui
                .checkbox(&mut notify.mattermost.enabled, "📨 Mattermost")
                .changed();
            ui.label("Webhook URL:");
            let resp = ui
                .add(egui::TextEdit::singleline(&mut mm_url).password(true).desired_width(220.0))
                .on_hover_text("incoming webhook 地址（含 token，只存本机凭据管理器）");
            if resp.changed() {
                if let Err(e) = save_notify_secret(crate::notify::KEY_MATTERMOST, &mm_url) {
                    test_msg = Some(format!("❌ 保存凭据失败: {e}"));
                }
            }
            if ui.small_button("发送测试").clicked() {
                let ctx = notify_test_ctx();
                test_msg = Some(match crate::notify::send_mattermost(&notify.mattermost.clone(), &ctx) {
                    Ok(m) => format!("✅ Mattermost 测试通知{m}"),
                    Err(e) => format!("❌ Mattermost 发送失败: {e:#}"),
                });
            }
        });

        // ── 钉钉：access_token + 加签 secret 存凭据管理器 ──
        let mut dd_token = crate::secrets::get_password(crate::notify::KEY_DINGTALK_TOKEN)
            .ok()
            .flatten()
            .unwrap_or_default();
        let mut dd_secret = crate::secrets::get_password(crate::notify::KEY_DINGTALK_SECRET)
            .ok()
            .flatten()
            .unwrap_or_default();
        ui.horizontal(|ui| {
            changed |= ui
                .checkbox(&mut notify.dingtalk.enabled, "🔔 钉钉机器人")
                .changed();
            ui.label("access_token:");
            let resp = ui
                .add(
                    egui::TextEdit::singleline(&mut dd_token)
                        .password(true)
                        .desired_width(180.0),
                )
                .on_hover_text("机器人 webhook 的 access_token（或整条 URL）");
            if resp.changed() {
                if let Err(e) = save_notify_secret(crate::notify::KEY_DINGTALK_TOKEN, &dd_token) {
                    test_msg = Some(format!("❌ 保存凭据失败: {e}"));
                }
            }
            ui.label("加签secret:");
            let resp = ui
                .add(
                    egui::TextEdit::singleline(&mut dd_secret)
                        .password(true)
                        .desired_width(140.0),
                )
                .on_hover_text("安全设置选「加签」时填；留空 = 不加签");
            if resp.changed() {
                if let Err(e) = save_notify_secret(crate::notify::KEY_DINGTALK_SECRET, &dd_secret) {
                    test_msg = Some(format!("❌ 保存凭据失败: {e}"));
                }
            }
            if ui.small_button("发送测试").clicked() {
                let ctx = notify_test_ctx();
                test_msg = Some(match crate::notify::send_dingtalk(&notify.dingtalk.clone(), &ctx) {
                    Ok(m) => format!("✅ 钉钉测试通知{m}"),
                    Err(e) => format!("❌ 钉钉发送失败: {e:#}"),
                });
            }
        });

        // ── 邮箱：服务器参数落 YAML，密码存凭据管理器 ──
        ui.add_space(4.0);
        let mut smtp_password = crate::secrets::get_password(crate::notify::KEY_SMTP_PASSWORD)
            .ok()
            .flatten()
            .unwrap_or_default();
        ui.horizontal(|ui| {
            changed |= ui
                .checkbox(&mut notify.email.enabled, "✉ 邮箱（SMTP）")
                .changed();
            ui.label("服务器:");
            changed |= ui
                .add(
                    egui::TextEdit::singleline(&mut notify.email.smtp_host)
                        .desired_width(120.0)
                        .hint_text("smtp.example.com"),
                )
                .changed();
            ui.label("端口:");
            changed |= ui
                .add(egui::DragValue::new(&mut notify.email.smtp_port).range(1..=65535))
                .changed();
            ui.label("加密:");
            let modes = ["starttls", "ssl", "none"];
            let hints = ["587 STARTTLS", "465 隐式SSL", "25 明文(内网中继)"];
            let cur = modes
                .iter()
                .position(|m| *m == notify.email.ssl)
                .unwrap_or(0);
            let combo = egui::ComboBox::from_id_salt("notify_ssl_combo")
                .selected_text(hints[cur])
                .width(110.0)
                .show_ui(ui, |ui| {
                    for (i, h) in hints.iter().enumerate() {
                        ui.selectable_value(&mut notify.email.ssl, modes[i].to_string(), *h);
                    }
                });
            changed |= combo.response.changed();
        });
        ui.horizontal(|ui| {
            ui.label("用户名:");
            changed |= ui
                .add(
                    egui::TextEdit::singleline(&mut notify.email.username)
                        .desired_width(140.0)
                        .hint_text("一般即邮箱地址"),
                )
                .changed();
            ui.label("密码:");
            let resp = ui
                .add(
                    egui::TextEdit::singleline(&mut smtp_password)
                        .password(true)
                        .desired_width(120.0),
                )
                .on_hover_text("SMTP 密码/授权码，只存本机凭据管理器");
            if resp.changed() {
                if let Err(e) = save_notify_secret(crate::notify::KEY_SMTP_PASSWORD, &smtp_password) {
                    test_msg = Some(format!("❌ 保存凭据失败: {e}"));
                }
            }
            ui.label("发件人:");
            changed |= ui
                .add(
                    egui::TextEdit::singleline(&mut notify.email.from)
                        .desired_width(130.0)
                        .hint_text("缺省用用户名"),
                )
                .changed();
        });
        ui.horizontal(|ui| {
            ui.label("收件人:");
            // 多收件人单行编辑：分号/逗号分隔，存 Vec<String>
            let mut to_text = notify.email.to.join(";");
            let resp = ui
                .add(
                    egui::TextEdit::singleline(&mut to_text)
                        .desired_width(240.0)
                        .hint_text("a@x.com;b@x.com"),
                )
                .on_hover_text("多个收件人用分号或逗号分隔");
            if resp.changed() {
                let to: Vec<String> = to_text
                    .split([',', ';', '，', '；'])
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect();
                if to != notify.email.to {
                    notify.email.to = to;
                    changed = true;
                }
            }
            if ui.small_button("发送测试").clicked() {
                let ctx = notify_test_ctx();
                test_msg = Some(match crate::notify::send_email(&notify.email.clone(), &ctx) {
                    Ok(m) => format!("✅ 测试邮件{m}"),
                    Err(e) => format!("❌ 邮件发送失败: {e:#}"),
                });
            }
        });
        ui.weak(
            "凭据（webhook URL / token / 密码）只存 Windows 凭据管理器，不写 config.yaml；\
             测试按钮会实际发送一条（最长等 15 秒）。",
        );
    }

    // 配置改动统一落盘（凭据字段不走这里——它们只进凭据管理器）
    if changed {
        if let Err(e) = crate::config::save_global(&main.global) {
            test_msg = Some(format!("保存 config.yaml 失败: {e}"));
        }
    }
    if let Some(m) = test_msg {
        main.status_msg = Some(m);
    }
}

/// 凭据字段改动即存凭据管理器；清空 = 删除该凭据
fn save_notify_secret(key: &str, value: &str) -> Result<(), String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        crate::secrets::delete_password(key);
        Ok(())
    } else {
        crate::secrets::set_password(key, trimmed).map_err(|e| format!("{key}: {e:#}"))
    }
}

fn notify_test_ctx() -> crate::notify::NotifyCtx {
    crate::notify::NotifyCtx {
        label: "测试通知/测试流程".into(),
        status_text: "success（来自设置窗口的测试消息）".into(),
        ok: true,
        duration_ms: 0,
        run_id: "test".into(),
    }
}

// ---------------------------------------------------------------------------
// 运行历史（SQLite，§11.2）：最近运行列表（kamal audit 最小形态——先有列表，再谈详情）
// ---------------------------------------------------------------------------

fn history_section(ui: &mut egui::Ui, main: &mut MainState) {
    ui.heading("运行历史（SQLite）");
    if !main.global.save_history {
        ui.colored_label(
            Color32::from_rgb(230, 180, 80),
            "⚠ 「保留运行历史」未开启（全局配置页可勾选）——当前运行不入库。",
        );
    }
    ui.weak(format!("库文件: {}", history_db_display()));
    ui.add_space(4.0);

    ui.horizontal(|ui| {
        if ui.button("⟳ 刷新").clicked() {
            main.history_rows = None; // 清缓存 → 下方重新加载
        }
        if ui
            .button("🗑 清空历史")
            .on_hover_text("删除全部运行记录（日志文件不受影响）")
            .clicked()
        {
            #[cfg(feature = "sqlite")]
            {
                let _ = main.runtime.block_on(async {
                    crate::storage::sqlite::clear_all().await
                });
                main.history_rows = None;
            }
        }
        if let Some(n) = &main.history_rows {
            ui.weak(format!("{} 条", n.len()));
        }
    });
    ui.separator();

    // 懒加载：首次进入 / 刷新时读一次（本地 SQLite，毫秒级；block_on 可接受）
    if main.history_rows.is_none() {
        #[cfg(feature = "sqlite")]
        {
            main.history_rows = Some(
                main.runtime
                    .block_on(async { crate::storage::sqlite::recent_runs(100).await })
                    .unwrap_or_default(),
            );
        }
        #[cfg(not(feature = "sqlite"))]
        {
            main.history_rows = Some(Vec::new());
        }
    }

    let rows = main.history_rows.clone().unwrap_or_default();
    if rows.is_empty() {
        ui.weak("（暂无运行记录）");
        return;
    }
    egui::ScrollArea::vertical()
        .id_salt("history_scroll")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            egui::Grid::new("history_grid")
                .num_columns(6)
                .spacing([14.0, 3.0])
                .show(ui, |ui| {
                    ui.strong("开始时间");
                    ui.strong("项目/流程");
                    ui.strong("目标");
                    ui.strong("触发");
                    ui.strong("状态");
                    ui.strong("耗时");
                    ui.end_row();
                    for r in &rows {
                        // RFC3339(UTC) → 本地时区 yyyy-MM-dd HH:mm:ss
                        let started = chrono::DateTime::parse_from_rfc3339(&r.started_at)
                            .map(|t| {
                                t.with_timezone(&chrono::Local)
                                    .format("%Y-%m-%d %H:%M:%S")
                                    .to_string()
                            })
                            .unwrap_or_else(|_| r.started_at.clone());
                        ui.monospace(&started).on_hover_text(format!(
                            "run_id: {}
日志: {}",
                            r.run_id,
                            r.log_path.as_deref().unwrap_or("—")
                        ));
                        ui.label(format!("{}/{}", r.project, r.job));
                        ui.label(r.target.clone().unwrap_or_else(|| "—".into()));
                        ui.weak(match r.trigger.as_str() {
                            "watch" => "👀 监听",
                            "cli" => "终端",
                            _ => "手动",
                        });
                        let (icon, color) = match r.status.as_str() {
                            "success" => ("✅ 成功", Color32::from_rgb(120, 200, 120)),
                            "success-warn" => ("⚠ 成功(有跳过)", Color32::from_rgb(230, 180, 80)),
                            "failed" => ("❌ 失败", Color32::from_rgb(230, 110, 110)),
                            "cancelled" => ("🚫 取消", Color32::GRAY),
                            "timeout" => ("⏱ 超时", Color32::from_rgb(230, 110, 110)),
                            _ => ("… 运行中", Color32::GRAY),
                        };
                        ui.colored_label(color, icon);
                        ui.monospace(match r.duration_ms {
                            Some(ms) => format!("{:.1}s", ms as f64 / 1000.0),
                            None => "—".into(),
                        });
                        ui.end_row();
                    }
                });
        });
}

#[cfg(feature = "sqlite")]
fn history_db_display() -> String {
    crate::storage::sqlite::db_path().to_string_lossy().to_string()
}

#[cfg(not(feature = "sqlite"))]
fn history_db_display() -> String {
    "（编译未启用 sqlite feature）".to_string()
}
