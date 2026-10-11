// Hermes Chat Editor — native Windows app (egui/eframe).
//
// A Chatbox-style editor for the Hermes Agent `state.db` session store, with
// NO web layer: a real native window talking straight to SQLite via rusqlite
// (bundled, so there is no system SQLite dependency).
//
// Features: list/search sessions, reveal internal sources, view & edit message
// content/role, insert, delete, rewind-truncate, create session, retag
// (title/source/pin/archive). Every write snapshots state.db to ./backups first.

#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use eframe::egui;
use rusqlite::{params, Connection};
use std::path::PathBuf;

const INTERNAL_SOURCES: [&str; 3] = ["kanban", "tool", "oneshot"];

fn default_db_path() -> PathBuf {
    // %LOCALAPPDATA%\hermes\state.db on Windows; ~/.hermes/state.db elsewhere.
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        let p = PathBuf::from(local).join("hermes").join("state.db");
        if p.exists() {
            return p;
        }
    }
    if let Some(home) = dirs_home() {
        return home.join(".hermes").join("state.db");
    }
    PathBuf::from("state.db")
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .ok()
        .map(PathBuf::from)
}

#[derive(Clone)]
struct SessionRow {
    id: String,
    title: String,
    source: String,
    pinned: bool,
    archived: bool,
    msg_count: i64,
    model: String,
    last_activity: f64,
}

#[derive(Clone)]
struct MessageRow {
    id: i64,
    role: String,
    content: String,
    reasoning: String,
    tool_name: Option<String>,
    tool_calls: Option<String>,
    active: bool,
    compacted: bool,
    // UI edit buffers
    edit_content: String,
    edit_role: String,
    dirty: bool,
}

struct App {
    db_path: PathBuf,
    search: String,
    show_internal: bool,
    sessions: Vec<SessionRow>,
    selected: Option<String>,
    // selected session editable attrs
    s_title: String,
    s_source: String,
    s_pinned: bool,
    s_archived: bool,
    s_meta: String,
    messages: Vec<MessageRow>,
    composer_role: String,
    composer_text: String,
    status: String,
    status_err: bool,
}

impl App {
    fn new(db_path: PathBuf) -> Self {
        let mut app = Self {
            db_path,
            search: String::new(),
            show_internal: false,
            sessions: vec![],
            selected: None,
            s_title: String::new(),
            s_source: String::new(),
            s_pinned: false,
            s_archived: false,
            s_meta: String::new(),
            messages: vec![],
            composer_role: "user".into(),
            composer_text: String::new(),
            status: String::new(),
            status_err: false,
        };
        app.reload_sessions();
        app
    }

    fn conn(&self) -> rusqlite::Result<Connection> {
        Connection::open(&self.db_path)
    }

    fn backup(&mut self) -> bool {
        let dir = self.db_path.parent().map(|p| p.join("backups")).unwrap_or_else(|| PathBuf::from("backups"));
        if std::fs::create_dir_all(&dir).is_err() {
            self.set_err("无法创建 backups 目录");
            return false;
        }
        let stamp = chrono::Local::now().format("%Y%m%d_%H%M%S_%3f").to_string();
        let dst = dir.join(format!("state.db.{stamp}.bak"));
        // SQLite backup API via rusqlite for a WAL-consistent snapshot.
        match (Connection::open(&self.db_path), Connection::open(&dst)) {
            (Ok(src), Ok(mut out)) => {
                let bk = rusqlite::backup::Backup::new(&src, &mut out);
                match bk {
                    Ok(b) => {
                        if b.run_to_completion(100, std::time::Duration::from_millis(0), None).is_err() {
                            self.set_err("备份失败");
                            return false;
                        }
                    }
                    Err(_) => {
                        self.set_err("备份初始化失败");
                        return false;
                    }
                }
                // prune to newest 30
                if let Ok(rd) = std::fs::read_dir(&dir) {
                    let mut baks: Vec<_> = rd.flatten()
                        .map(|e| e.path())
                        .filter(|p| p.to_string_lossy().contains("state.db.") && p.extension().map(|e| e == "bak").unwrap_or(false))
                        .collect();
                    baks.sort();
                    if baks.len() > 30 {
                        for old in &baks[..baks.len() - 30] {
                            let _ = std::fs::remove_file(old);
                        }
                    }
                }
                true
            }
            _ => {
                self.set_err("无法打开数据库进行备份（可能被 Hermes 占用，先关闭 gateway/desktop）");
                false
            }
        }
    }

    fn set_ok(&mut self, m: impl Into<String>) {
        self.status = m.into();
        self.status_err = false;
    }
    fn set_err(&mut self, m: impl Into<String>) {
        self.status = m.into();
        self.status_err = true;
    }

    fn reload_sessions(&mut self) {
        let conn = match self.conn() {
            Ok(c) => c,
            Err(e) => {
                self.set_err(format!("打开数据库失败: {e}"));
                return;
            }
        };
        let mut sql = String::from(
            "SELECT id,COALESCE(title,''),COALESCE(source,''),COALESCE(pinned,0),\
             COALESCE(archived,0),COALESCE(message_count,0),COALESCE(model,''),\
             COALESCE(last_activity_at,started_at,0) FROM sessions",
        );
        let mut conds: Vec<String> = vec![];
        if !self.show_internal {
            let list = INTERNAL_SOURCES.iter().map(|s| format!("'{s}'")).collect::<Vec<_>>().join(",");
            conds.push(format!("COALESCE(source,'') NOT IN ({list})"));
        }
        if !self.search.trim().is_empty() {
            let q = self.search.replace('\'', "''");
            conds.push(format!("(COALESCE(title,'') LIKE '%{q}%' OR id LIKE '%{q}%')"));
        }
        if !conds.is_empty() {
            sql.push_str(" WHERE ");
            sql.push_str(&conds.join(" AND "));
        }
        sql.push_str(" ORDER BY COALESCE(last_activity_at,started_at) DESC LIMIT 300");

        let mut rows = vec![];
        if let Ok(mut stmt) = conn.prepare(&sql) {
            let it = stmt.query_map([], |r| {
                Ok(SessionRow {
                    id: r.get(0)?,
                    title: r.get(1)?,
                    source: r.get(2)?,
                    pinned: r.get::<_, i64>(3)? != 0,
                    archived: r.get::<_, i64>(4)? != 0,
                    msg_count: r.get(5)?,
                    model: r.get(6)?,
                    last_activity: r.get(7)?,
                })
            });
            if let Ok(it) = it {
                for row in it.flatten() {
                    rows.push(row);
                }
            }
        }
        self.sessions = rows;
    }

    fn open_session(&mut self, sid: &str) {
        let conn = match self.conn() {
            Ok(c) => c,
            Err(e) => {
                self.set_err(format!("打开失败: {e}"));
                return;
            }
        };
        // session attrs
        if let Ok(mut st) = conn.prepare(
            "SELECT COALESCE(title,''),COALESCE(source,''),COALESCE(pinned,0),\
             COALESCE(archived,0),COALESCE(model,''),COALESCE(profile_name,'default') \
             FROM sessions WHERE id=?1",
        ) {
            if let Ok((title, source, pin, arch, model, profile)) = st.query_row(params![sid], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)? != 0,
                    r.get::<_, i64>(3)? != 0,
                    r.get::<_, String>(4)?,
                    r.get::<_, String>(5)?,
                ))
            }) {
                self.s_title = title;
                self.s_source = source;
                self.s_pinned = pin;
                self.s_archived = arch;
                self.s_meta = format!("ID {sid} · model {model} · profile {profile}");
            }
        }
        // messages
        let mut msgs = vec![];
        if let Ok(mut st) = conn.prepare(
            "SELECT id,COALESCE(role,''),COALESCE(content,''),COALESCE(reasoning_content,''),\
             tool_name,tool_calls,COALESCE(active,1),COALESCE(compacted,0) FROM messages \
             WHERE session_id=?1 ORDER BY COALESCE(display_order,id),id",
        ) {
            let it = st.query_map(params![sid], |r| {
                let content: String = r.get(2)?;
                let role: String = r.get(1)?;
                Ok(MessageRow {
                    id: r.get(0)?,
                    role: role.clone(),
                    content: content.clone(),
                    reasoning: r.get(3)?,
                    tool_name: r.get(4)?,
                    tool_calls: r.get(5)?,
                    active: r.get::<_, i64>(6)? != 0,
                    compacted: r.get::<_, i64>(7)? != 0,
                    edit_content: content,
                    edit_role: role,
                    dirty: false,
                })
            });
            if let Ok(it) = it {
                for m in it.flatten() {
                    msgs.push(m);
                }
            }
        }
        self.messages = msgs;
        self.selected = Some(sid.to_string());
        self.set_ok(format!("已载入 {} 条消息", self.messages.len()));
    }

    fn save_message(&mut self, idx: usize) {
        if !self.backup() {
            return;
        }
        let (id, content, role) = {
            let m = &self.messages[idx];
            (m.id, m.edit_content.clone(), m.edit_role.clone())
        };
        match self.conn() {
            Ok(conn) => {
                match conn.execute(
                    "UPDATE messages SET content=?1, role=?2 WHERE id=?3",
                    params![content, role, id],
                ) {
                    Ok(_) => {
                        let m = &mut self.messages[idx];
                        m.content = content;
                        m.role = role;
                        m.dirty = false;
                        self.set_ok(format!("已保存 #{id}"));
                    }
                    Err(e) => self.set_err(format!("保存失败: {e}")),
                }
            }
            Err(e) => self.set_err(format!("保存失败: {e}")),
        }
    }

    fn delete_message(&mut self, idx: usize) {
        if !self.backup() {
            return;
        }
        let id = self.messages[idx].id;
        if let Ok(conn) = self.conn() {
            if conn.execute("DELETE FROM messages WHERE id=?1", params![id]).is_ok() {
                self.recount();
                self.set_ok(format!("已删除 #{id}"));
                let sid = self.selected.clone().unwrap();
                self.open_session(&sid);
                return;
            }
        }
        self.set_err("删除失败");
    }

    fn truncate_after(&mut self, idx: usize) {
        if !self.backup() {
            return;
        }
        let id = self.messages[idx].id;
        let sid = self.selected.clone().unwrap();
        if let Ok(conn) = self.conn() {
            // delete everything with display_order/id greater than this row's order
            let ok = conn.execute(
                "DELETE FROM messages WHERE session_id=?1 AND \
                 COALESCE(display_order,id) > (SELECT COALESCE(display_order,id) FROM messages WHERE id=?2)",
                params![sid, id],
            );
            if let Ok(n) = ok {
                self.recount();
                self.set_ok(format!("已裁剪 {n} 条后续消息"));
                self.open_session(&sid);
                return;
            }
        }
        self.set_err("裁剪失败");
    }

    fn insert_after(&mut self, idx: usize) {
        if !self.backup() {
            return;
        }
        let after_id = self.messages[idx].id;
        let sid = self.selected.clone().unwrap();
        if let Ok(conn) = self.conn() {
            let base: f64 = conn
                .query_row(
                    "SELECT COALESCE(display_order,id) FROM messages WHERE id=?1",
                    params![after_id],
                    |r| r.get(0),
                )
                .unwrap_or(after_id as f64);
            let order = base + 0.5;
            let uid = simple_uid();
            let ts = now_ts();
            if conn
                .execute(
                    "INSERT INTO messages(session_id,role,content,timestamp,active,compacted,display_order,message_uid) \
                     VALUES(?1,'assistant','新消息',?2,1,0,?3,?4)",
                    params![sid, ts, order, uid],
                )
                .is_ok()
            {
                self.recount();
                self.set_ok("已插入新消息");
                self.open_session(&sid);
                return;
            }
        }
        self.set_err("插入失败");
    }

    fn append_message(&mut self) {
        if self.composer_text.trim().is_empty() || self.selected.is_none() {
            return;
        }
        if !self.backup() {
            return;
        }
        let sid = self.selected.clone().unwrap();
        let role = self.composer_role.clone();
        let text = self.composer_text.clone();
        if let Ok(conn) = self.conn() {
            let maxord: f64 = conn
                .query_row(
                    "SELECT COALESCE(MAX(COALESCE(display_order,id)),0) FROM messages WHERE session_id=?1",
                    params![sid],
                    |r| r.get(0),
                )
                .unwrap_or(0.0);
            let uid = simple_uid();
            let ts = now_ts();
            if conn
                .execute(
                    "INSERT INTO messages(session_id,role,content,timestamp,active,compacted,display_order,message_uid) \
                     VALUES(?1,?2,?3,?4,1,0,?5,?6)",
                    params![sid, role, text, ts, maxord + 1.0, uid],
                )
                .is_ok()
            {
                self.recount();
                self.composer_text.clear();
                self.set_ok("已追加");
                self.open_session(&sid);
                return;
            }
        }
        self.set_err("追加失败");
    }

    fn save_session_attrs(&mut self) {
        if self.selected.is_none() {
            return;
        }
        if !self.backup() {
            return;
        }
        let sid = self.selected.clone().unwrap();
        let (title, source, pin, arch) =
            (self.s_title.clone(), self.s_source.clone(), self.s_pinned as i64, self.s_archived as i64);
        if let Ok(conn) = self.conn() {
            if conn
                .execute(
                    "UPDATE sessions SET title=?1, source=?2, created_source=?2, pinned=?3, archived=?4, \
                     last_activity_at=?5 WHERE id=?6",
                    params![title, source, pin, arch, now_ts(), sid],
                )
                .is_ok()
            {
                self.set_ok("会话属性已保存");
                self.reload_sessions();
                return;
            }
        }
        self.set_err("保存会话属性失败");
    }

    fn create_session(&mut self) {
        if !self.backup() {
            return;
        }
        let sid = format!("{}_{}", chrono::Local::now().format("%Y%m%d_%H%M%S"), &simple_uid()[..6]);
        if let Ok(conn) = self.conn() {
            if conn
                .execute(
                    "INSERT INTO sessions(id,source,created_source,model,title,title_source,started_at,\
                     last_activity_at,message_count,tool_call_count,pinned,archived,profile_name) \
                     VALUES(?1,'cli','cli','manual','新建会话','manual',?2,?2,0,0,1,0,'default')",
                    params![sid, now_ts()],
                )
                .is_ok()
            {
                self.set_ok(format!("已创建 {sid}"));
                self.reload_sessions();
                self.open_session(&sid);
                return;
            }
        }
        self.set_err("创建会话失败");
    }

    fn recount(&self) {
        if let (Some(sid), Ok(conn)) = (&self.selected, self.conn()) {
            let _ = conn.execute(
                "UPDATE sessions SET message_count=(SELECT COUNT(*) FROM messages WHERE session_id=?1) WHERE id=?1",
                params![sid],
            );
        }
    }
}

fn simple_uid() -> String {
    // Non-crypto unique-ish hex from time; adequate for message_uid.
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{:032x}", n)
}

fn now_ts() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Left: session list
        egui::SidePanel::left("sidebar")
            .default_width(320.0)
            .show(ctx, |ui| {
                ui.add_space(6.0);
                ui.heading("Hermes Chat Editor");
                ui.add_space(4.0);
                let mut changed = false;
                ui.horizontal(|ui| {
                    ui.label("搜索");
                    if ui.text_edit_singleline(&mut self.search).changed() {
                        changed = true;
                    }
                });
                if ui.checkbox(&mut self.show_internal, "显示内部会话 (oneshot/tool/kanban)").changed() {
                    changed = true;
                }
                ui.horizontal(|ui| {
                    if ui.button("+ 新建会话").clicked() {
                        self.create_session();
                    }
                    if ui.button("刷新").clicked() {
                        self.reload_sessions();
                        if let Some(sid) = self.selected.clone() {
                            self.open_session(&sid);
                        }
                    }
                });
                if changed {
                    self.reload_sessions();
                }
                ui.separator();

                let sessions = self.sessions.clone();
                let mut to_open: Option<String> = None;
                egui::ScrollArea::vertical().show(ui, |ui| {
                    for s in &sessions {
                        let sel = self.selected.as_deref() == Some(s.id.as_str());
                        let mark = if s.pinned { "📌 " } else { "" };
                        let label = format!(
                            "{}{}  [{}]\n{} · {} 条",
                            mark,
                            if s.title.is_empty() { "(无标题)" } else { &s.title },
                            s.source,
                            &s.id,
                            s.msg_count
                        );
                        if ui.selectable_label(sel, label).clicked() {
                            to_open = Some(s.id.clone());
                        }
                    }
                });
                if let Some(sid) = to_open {
                    self.open_session(&sid);
                }
                ui.separator();
                ui.label(format!("{} 个会话", self.sessions.len()));
            });

        // Bottom: status + composer
        egui::TopBottomPanel::bottom("composer").show(ctx, |ui| {
            if !self.status.is_empty() {
                let color = if self.status_err {
                    egui::Color32::from_rgb(0xf8, 0x51, 0x49)
                } else {
                    egui::Color32::from_rgb(0x4a, 0x9e, 0x5e)
                };
                ui.colored_label(color, &self.status);
            }
            if self.selected.is_some() {
                ui.horizontal(|ui| {
                    egui::ComboBox::from_id_source("composer_role")
                        .selected_text(&self.composer_role)
                        .show_ui(ui, |ui| {
                            for r in ["user", "assistant", "system", "tool"] {
                                ui.selectable_value(&mut self.composer_role, r.to_string(), r);
                            }
                        });
                    ui.add(
                        egui::TextEdit::singleline(&mut self.composer_text)
                            .hint_text("追加一条消息到末尾…（预填 / 伪造历史）")
                            .desired_width(f32::INFINITY),
                    );
                    if ui.button("追加").clicked() {
                        self.append_message();
                    }
                });
            }
        });

        // Center: messages
        egui::CentralPanel::default().show(ctx, |ui| {
            if self.selected.is_none() {
                ui.centered_and_justified(|ui| {
                    ui.label("← 选择一个会话开始编辑");
                });
                return;
            }

            // session attribute bar
            ui.horizontal_wrapped(|ui| {
                ui.label("标题");
                ui.add(egui::TextEdit::singleline(&mut self.s_title).desired_width(200.0));
                egui::ComboBox::from_id_source("s_source")
                    .selected_text(&self.s_source)
                    .show_ui(ui, |ui| {
                        for r in ["cli", "oneshot", "tool", "kanban", "telegram", "desktop", "api-server"] {
                            ui.selectable_value(&mut self.s_source, r.to_string(), r);
                        }
                    });
                ui.checkbox(&mut self.s_pinned, "置顶");
                ui.checkbox(&mut self.s_archived, "归档");
                if ui.button("保存会话属性").clicked() {
                    self.save_session_attrs();
                }
            });
            ui.label(egui::RichText::new(&self.s_meta).small().weak());
            ui.separator();

            let mut action: Option<(usize, &'static str)> = None;
            let count = self.messages.len();
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                for i in 0..count {
                    let (id, role, is_empty, is_tool, tool_summary, flags) = {
                        let m = &self.messages[i];
                        let empty = m.edit_content.trim().is_empty();
                        let tool = m.role == "tool";
                        let summary = tool_call_names(m.tool_calls.as_deref());
                        let mut fl = vec![];
                        if !m.active { fl.push("inactive".to_string()); }
                        if m.compacted { fl.push("compacted".to_string()); }
                        if let Some(tn) = &m.tool_name { fl.push(format!("tool:{tn}")); }
                        (m.id, m.role.clone(), empty, tool, summary, fl.join(" · "))
                    };

                    egui::Frame::group(ui.style())
                        .fill(role_fill(&role))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                egui::ComboBox::from_id_source(format!("role_{id}"))
                                    .selected_text(&self.messages[i].edit_role)
                                    .width(110.0)
                                    .show_ui(ui, |ui| {
                                        for r in ["user", "assistant", "system", "tool"] {
                                            if ui
                                                .selectable_value(
                                                    &mut self.messages[i].edit_role,
                                                    r.to_string(),
                                                    r,
                                                )
                                                .changed()
                                            {
                                                self.messages[i].dirty = true;
                                            }
                                        }
                                    });
                                ui.label(egui::RichText::new(format!("#{id}")).weak());
                                if self.messages[i].dirty {
                                    ui.colored_label(egui::Color32::from_rgb(0x4a, 0x84, 0xfe), "●");
                                }
                                ui.label(egui::RichText::new(&flags).small().weak());
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    if ui.button("🗑").on_hover_text("删除").clicked() {
                                        action = Some((i, "delete"));
                                    }
                                    if ui.button("✂").on_hover_text("裁剪至此（rewind）").clicked() {
                                        action = Some((i, "truncate"));
                                    }
                                    if ui.button("↓+").on_hover_text("在此条后插入").clicked() {
                                        action = Some((i, "insert"));
                                    }
                                    if ui.button("💾").on_hover_text("保存").clicked() {
                                        action = Some((i, "save"));
                                    }
                                });
                            });

                            if is_empty && !tool_summary.is_empty() {
                                ui.label(
                                    egui::RichText::new(format!("工具调用 → {tool_summary}"))
                                        .italics()
                                        .weak(),
                                );
                            } else if is_tool {
                                let header = format!(
                                    "{} 返回结果 ({} 字符)",
                                    self.messages[i].tool_name.clone().unwrap_or_else(|| "tool".into()),
                                    self.messages[i].edit_content.len()
                                );
                                egui::CollapsingHeader::new(header)
                                    .id_source(format!("tool_{id}"))
                                    .show(ui, |ui| {
                                        let te = egui::TextEdit::multiline(&mut self.messages[i].edit_content)
                                            .desired_width(f32::INFINITY)
                                            .code_editor()
                                            .desired_rows(8);
                                        if ui.add(te).changed() {
                                            self.messages[i].dirty = true;
                                        }
                                    });
                            } else {
                                let rows = if is_empty { 1 } else { 3 };
                                let te = egui::TextEdit::multiline(&mut self.messages[i].edit_content)
                                    .desired_width(f32::INFINITY)
                                    .desired_rows(rows);
                                if ui.add(te).changed() {
                                    self.messages[i].dirty = true;
                                }
                            }

                            if !self.messages[i].reasoning.is_empty() {
                                egui::CollapsingHeader::new("reasoning")
                                    .id_source(format!("reason_{id}"))
                                    .show(ui, |ui| {
                                        ui.label(egui::RichText::new(&self.messages[i].reasoning).small().weak());
                                    });
                            }
                        });
                    ui.add_space(8.0);
                }
            });

            if let Some((i, what)) = action {
                match what {
                    "save" => self.save_message(i),
                    "delete" => self.delete_message(i),
                    "truncate" => self.truncate_after(i),
                    "insert" => self.insert_after(i),
                    _ => {}
                }
            }
        });
    }
}

fn role_fill(role: &str) -> egui::Color32 {
    match role {
        "user" => egui::Color32::from_rgb(0x14, 0x24, 0x38),
        "assistant" => egui::Color32::from_rgb(0x14, 0x24, 0x1c),
        "system" => egui::Color32::from_rgb(0x24, 0x20, 0x10),
        "tool" => egui::Color32::from_rgb(0x1e, 0x18, 0x2c),
        _ => egui::Color32::from_rgb(0x1c, 0x21, 0x28),
    }
}

/// Pull function names out of the stored tool_calls JSON (best-effort, no serde).
fn tool_call_names(tc: Option<&str>) -> String {
    let Some(s) = tc else { return String::new() };
    if s.trim().is_empty() {
        return String::new();
    }
    // Cheap scan for `"name": "..."` and `"function": {"name": "..."}` patterns.
    let mut names = vec![];
    let bytes = s.as_bytes();
    let needle = b"\"name\"";
    let mut i = 0;
    while i + needle.len() < bytes.len() {
        if &bytes[i..i + needle.len()] == needle {
            // find next quote after the colon
            if let Some(colon) = s[i..].find(':') {
                let rest = &s[i + colon + 1..];
                if let Some(q1) = rest.find('"') {
                    let after = &rest[q1 + 1..];
                    if let Some(q2) = after.find('"') {
                        let name = &after[..q2];
                        if !name.is_empty() && !names.contains(&name.to_string()) {
                            names.push(name.to_string());
                        }
                    }
                }
            }
        }
        i += 1;
    }
    names.join(", ")
}

fn main() -> eframe::Result<()> {
    // CLI: optional path to state.db as first arg.
    let db_path = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(default_db_path);

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1100.0, 740.0])
            .with_title("Hermes Chat Editor"),
        ..Default::default()
    };
    eframe::run_native(
        "Hermes Chat Editor",
        options,
        Box::new(move |cc| {
            // Font that renders CJK: load a system font if present.
            install_cjk_font(&cc.egui_ctx);
            cc.egui_ctx.set_visuals(egui::Visuals::dark());
            Ok(Box::new(App::new(db_path)))
        }),
    )
}

/// egui ships no CJK glyphs; pull one from the OS so Chinese renders.
fn install_cjk_font(ctx: &egui::Context) {
    let candidates = [
        "C:/Windows/Fonts/msyh.ttc",   // Microsoft YaHei
        "C:/Windows/Fonts/simhei.ttf", // SimHei
        "C:/Windows/Fonts/simsun.ttc", // SimSun
    ];
    for path in candidates {
        if let Ok(bytes) = std::fs::read(path) {
            let mut fonts = egui::FontDefinitions::default();
            fonts.font_data.insert("cjk".to_owned(), egui::FontData::from_owned(bytes));
            fonts
                .families
                .entry(egui::FontFamily::Proportional)
                .or_default()
                .insert(0, "cjk".to_owned());
            fonts
                .families
                .entry(egui::FontFamily::Monospace)
                .or_default()
                .push("cjk".to_owned());
            ctx.set_fonts(fonts);
            return;
        }
    }
}
