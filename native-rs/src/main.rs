// Hermes Chat Editor — native Windows app (egui/eframe).
//
// A Chatbox-style editor for the Hermes Agent `state.db` session store, with
// NO web layer: a real native window talking straight to SQLite via rusqlite
// (bundled, so there is no system SQLite dependency).
//
// Robustness features:
//  - Every mutating op snapshots state.db to ./backups first (WAL-safe), via a
//    single centralized write path (`begin_write`).
//  - Destructive ops (delete / truncate / restore / discard) require a modal
//    confirmation.
//  - Detects a RUNNING Hermes (recent gateway_heartbeats) and shows a banner —
//    editing a live DB risks being overwritten by Hermes' in-memory cache.
//  - Unsaved-changes guard when switching session / refreshing / creating.
//  - Ctrl+S saves every dirty message in ONE transaction.
//  - In-memory list is mutated in place after writes (scroll position kept),
//    instead of re-reading the whole session.

#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use eframe::egui;
use rusqlite::{params, Connection};
use std::path::{Path, PathBuf};

const INTERNAL_SOURCES: [&str; 3] = ["kanban", "tool", "oneshot"];
const HEARTBEAT_FRESH_SECS: f64 = 120.0;

fn default_db_path() -> PathBuf {
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
    ts: f64,
    // UI edit buffers
    edit_content: String,
    edit_role: String,
    dirty: bool,
}

/// Pending modal / confirmation state.
enum Modal {
    None,
    ConfirmDelete(usize),
    ConfirmTruncate(usize),
    DiscardThenSelect(String),
    DiscardThenRefresh,
    DiscardThenNew,
    ConfirmRestore(PathBuf),
    Insert { after_idx: usize, role: String, content: String },
    SystemPrompt { buf: String, dirty: bool },
}

struct App {
    db_path: PathBuf,
    search: String,
    show_internal: bool,
    sessions: Vec<SessionRow>,
    selected: Option<String>,
    // selected session editable attrs + originals (for dirty detection)
    s_title: String,
    s_source: String,
    s_pinned: bool,
    s_archived: bool,
    o_title: String,
    o_source: String,
    o_pinned: bool,
    o_archived: bool,
    s_meta: String,
    messages: Vec<MessageRow>,
    composer_role: String,
    composer_text: String,
    status: String,
    status_err: bool,
    hermes_running: bool,
    modal: Modal,
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
            o_title: String::new(),
            o_source: String::new(),
            o_pinned: false,
            o_archived: false,
            s_meta: String::new(),
            messages: vec![],
            composer_role: "user".into(),
            composer_text: String::new(),
            status: String::new(),
            status_err: false,
            hermes_running: false,
            modal: Modal::None,
        };
        app.refresh_hermes_running();
        app.reload_sessions();
        app
    }

    fn conn(&self) -> rusqlite::Result<Connection> {
        Connection::open(&self.db_path)
    }

    fn set_ok(&mut self, m: impl Into<String>) {
        self.status = m.into();
        self.status_err = false;
    }
    fn set_err(&mut self, m: impl Into<String>) {
        self.status = m.into();
        self.status_err = true;
    }

    fn backup_dir(&self) -> PathBuf {
        self.db_path
            .parent()
            .map(|p| p.join("backups"))
            .unwrap_or_else(|| PathBuf::from("backups"))
    }

    /// WAL-safe snapshot via SQLite backup API. Returns the written path.
    fn backup(&mut self) -> Option<PathBuf> {
        let dir = self.backup_dir();
        if std::fs::create_dir_all(&dir).is_err() {
            self.set_err("无法创建 backups 目录");
            return None;
        }
        let stamp = chrono::Local::now().format("%Y%m%d_%H%M%S_%3f").to_string();
        let dst = dir.join(format!("state.db.{stamp}.bak"));
        match (Connection::open(&self.db_path), Connection::open(&dst)) {
            (Ok(src), Ok(mut out)) => {
                match rusqlite::backup::Backup::new(&src, &mut out) {
                    Ok(b) => {
                        if b.run_to_completion(200, std::time::Duration::from_millis(0), None).is_err() {
                            self.set_err("备份失败");
                            return None;
                        }
                    }
                    Err(_) => {
                        self.set_err("备份初始化失败");
                        return None;
                    }
                }
                if let Ok(rd) = std::fs::read_dir(&dir) {
                    let mut baks: Vec<_> = rd
                        .flatten()
                        .map(|e| e.path())
                        .filter(|p| {
                            p.file_name()
                                .map(|n| n.to_string_lossy().starts_with("state.db."))
                                .unwrap_or(false)
                                && p.extension().map(|e| e == "bak").unwrap_or(false)
                        })
                        .collect();
                    baks.sort();
                    if baks.len() > 30 {
                        for old in &baks[..baks.len() - 30] {
                            let _ = std::fs::remove_file(old);
                        }
                    }
                }
                Some(dst)
            }
            _ => {
                self.set_err("无法打开数据库进行备份（可能被占用）");
                None
            }
        }
    }

    /// Centralized write entry: snapshot first, then hand back a connection.
    fn begin_write(&mut self) -> Option<Connection> {
        if self.backup().is_none() {
            return None;
        }
        match self.conn() {
            Ok(c) => {
                let _ = c.busy_timeout(std::time::Duration::from_millis(3000));
                Some(c)
            }
            Err(e) => {
                self.set_err(format!("打开数据库失败: {e}"));
                None
            }
        }
    }

    fn refresh_hermes_running(&mut self) {
        self.hermes_running = false;
        if let Ok(conn) = self.conn() {
            if let Ok(mut st) = conn.prepare("SELECT last_heartbeat FROM gateway_heartbeats") {
                let now = now_ts();
                if let Ok(rows) = st.query_map([], |r| r.get::<_, f64>(0)) {
                    for hb in rows.flatten() {
                        if now - hb < HEARTBEAT_FRESH_SECS {
                            self.hermes_running = true;
                            return;
                        }
                    }
                }
            }
        }
    }

    fn attrs_dirty(&self) -> bool {
        self.s_title != self.o_title
            || self.s_source != self.o_source
            || self.s_pinned != self.o_pinned
            || self.s_archived != self.o_archived
    }

    fn any_dirty(&self) -> bool {
        self.attrs_dirty() || self.messages.iter().any(|m| m.dirty)
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
             COALESCE(archived,0),COALESCE(message_count,0) FROM sessions",
        );
        let mut conds: Vec<String> = vec![];
        if !self.show_internal {
            let list = INTERNAL_SOURCES.iter().map(|s| format!("'{s}'")).collect::<Vec<_>>().join(",");
            conds.push(format!("COALESCE(source,'') NOT IN ({list})"));
        }
        if !self.search.trim().is_empty() {
            // title OR id OR any message body matches
            let q = self.search.replace('\'', "''");
            conds.push(format!(
                "(COALESCE(title,'') LIKE '%{q}%' OR id LIKE '%{q}%' \
                 OR EXISTS (SELECT 1 FROM messages m WHERE m.session_id=sessions.id \
                 AND m.content LIKE '%{q}%'))"
            ));
        }
        if !conds.is_empty() {
            sql.push_str(" WHERE ");
            sql.push_str(&conds.join(" AND "));
        }
        sql.push_str(" ORDER BY pinned DESC, COALESCE(last_activity_at,started_at) DESC LIMIT 500");

        let mut rows = vec![];
        if let Ok(mut stmt) = conn.prepare(&sql) {
            if let Ok(it) = stmt.query_map([], |r| {
                Ok(SessionRow {
                    id: r.get(0)?,
                    title: r.get(1)?,
                    source: r.get(2)?,
                    pinned: r.get::<_, i64>(3)? != 0,
                    archived: r.get::<_, i64>(4)? != 0,
                    msg_count: r.get(5)?,
                })
            }) {
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
                self.s_title = title.clone();
                self.s_source = source.clone();
                self.s_pinned = pin;
                self.s_archived = arch;
                self.o_title = title;
                self.o_source = source;
                self.o_pinned = pin;
                self.o_archived = arch;
                self.s_meta = format!("ID {sid} · model {model} · profile {profile}");
            }
        }
        let mut msgs = vec![];
        if let Ok(mut st) = conn.prepare(
            "SELECT id,COALESCE(role,''),COALESCE(content,''),COALESCE(reasoning_content,''),\
             tool_name,tool_calls,COALESCE(active,1),COALESCE(compacted,0),COALESCE(timestamp,0) \
             FROM messages WHERE session_id=?1 ORDER BY COALESCE(display_order,id),id",
        ) {
            if let Ok(it) = st.query_map(params![sid], |r| {
                let role: String = r.get(1)?;
                let content: String = r.get(2)?;
                Ok(MessageRow {
                    id: r.get(0)?,
                    role: role.clone(),
                    content: content.clone(),
                    reasoning: r.get(3)?,
                    tool_name: r.get(4)?,
                    tool_calls: r.get(5)?,
                    active: r.get::<_, i64>(6)? != 0,
                    compacted: r.get::<_, i64>(7)? != 0,
                    ts: r.get(8)?,
                    edit_content: content,
                    edit_role: role,
                    dirty: false,
                })
            }) {
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
        let Some(conn) = self.begin_write() else { return };
        let (id, content, role) = {
            let m = &self.messages[idx];
            (m.id, m.edit_content.clone(), m.edit_role.clone())
        };
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

    fn save_all_dirty(&mut self) {
        let dirties: Vec<usize> =
            self.messages.iter().enumerate().filter(|(_, m)| m.dirty).map(|(i, _)| i).collect();
        if dirties.is_empty() {
            self.set_ok("没有未保存更改");
            return;
        }
        let Some(mut conn) = self.begin_write() else { return };
        let tx = match conn.transaction() {
            Ok(t) => t,
            Err(e) => {
                self.set_err(format!("开启事务失败: {e}"));
                return;
            }
        };
        let mut ok = true;
        for &i in &dirties {
            let m = &self.messages[i];
            if tx
                .execute(
                    "UPDATE messages SET content=?1, role=?2 WHERE id=?3",
                    params![m.edit_content, m.edit_role, m.id],
                )
                .is_err()
            {
                ok = false;
                break;
            }
        }
        if ok && tx.commit().is_ok() {
            for &i in &dirties {
                let m = &mut self.messages[i];
                m.content = m.edit_content.clone();
                m.role = m.edit_role.clone();
                m.dirty = false;
            }
            self.set_ok(format!("已保存 {} 条消息", dirties.len()));
        } else {
            self.set_err("批量保存失败（已回滚）");
        }
    }

    fn delete_message(&mut self, idx: usize) {
        let id = self.messages[idx].id;
        let Some(conn) = self.begin_write() else { return };
        if conn.execute("DELETE FROM messages WHERE id=?1", params![id]).is_ok() {
            drop(conn);
            self.messages.remove(idx);
            self.recount();
            self.set_ok(format!("已删除 #{id}"));
        } else {
            self.set_err("删除失败");
        }
    }

    fn truncate_after(&mut self, idx: usize) {
        let id = self.messages[idx].id;
        let sid = match &self.selected {
            Some(s) => s.clone(),
            None => return,
        };
        let Some(conn) = self.begin_write() else { return };
        let res = conn.execute(
            "DELETE FROM messages WHERE session_id=?1 AND \
             COALESCE(display_order,id) > (SELECT COALESCE(display_order,id) FROM messages WHERE id=?2)",
            params![sid, id],
        );
        match res {
            Ok(n) => {
                drop(conn);
                self.messages.truncate(idx + 1);
                self.recount();
                self.set_ok(format!("已裁剪 {n} 条后续消息"));
            }
            Err(_) => self.set_err("裁剪失败"),
        }
    }

    fn insert_after(&mut self, after_idx: usize, role: String, content: String) {
        let sid = match &self.selected {
            Some(s) => s.clone(),
            None => return,
        };
        let after_id = self.messages[after_idx].id;
        let Some(conn) = self.begin_write() else { return };
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
        let finish: Option<&str> = if role == "assistant" { Some("stop") } else { None };
        match conn.execute(
            "INSERT INTO messages(session_id,role,content,timestamp,active,compacted,finish_reason,display_order,message_uid) \
             VALUES(?1,?2,?3,?4,1,0,?5,?6,?7)",
            params![sid, role, content, ts, finish, order, uid],
        ) {
            Ok(_) => {
                let new_id = conn.last_insert_rowid();
                drop(conn);
                let row = MessageRow {
                    id: new_id,
                    role: role.clone(),
                    content: content.clone(),
                    reasoning: String::new(),
                    tool_name: None,
                    tool_calls: None,
                    active: true,
                    compacted: false,
                    ts,
                    edit_content: content,
                    edit_role: role,
                    dirty: false,
                };
                self.messages.insert(after_idx + 1, row);
                self.recount();
                self.set_ok("已插入新消息");
            }
            Err(_) => self.set_err("插入失败"),
        }
    }

    fn append_message(&mut self) {
        if self.composer_text.trim().is_empty() || self.selected.is_none() {
            return;
        }
        let sid = self.selected.clone().unwrap();
        let role = self.composer_role.clone();
        let text = self.composer_text.clone();
        let Some(conn) = self.begin_write() else { return };
        let maxord: f64 = conn
            .query_row(
                "SELECT COALESCE(MAX(COALESCE(display_order,id)),0) FROM messages WHERE session_id=?1",
                params![sid],
                |r| r.get(0),
            )
            .unwrap_or(0.0);
        let uid = simple_uid();
        let ts = now_ts();
        let finish: Option<&str> = if role == "assistant" { Some("stop") } else { None };
        match conn.execute(
            "INSERT INTO messages(session_id,role,content,timestamp,active,compacted,finish_reason,display_order,message_uid) \
             VALUES(?1,?2,?3,?4,1,0,?5,?6,?7)",
            params![sid, role, text, ts, finish, maxord + 1.0, uid],
        ) {
            Ok(_) => {
                let new_id = conn.last_insert_rowid();
                drop(conn);
                let row = MessageRow {
                    id: new_id,
                    role: role.clone(),
                    content: text.clone(),
                    reasoning: String::new(),
                    tool_name: None,
                    tool_calls: None,
                    active: true,
                    compacted: false,
                    ts,
                    edit_content: text,
                    edit_role: role,
                    dirty: false,
                };
                self.messages.push(row);
                self.composer_text.clear();
                self.recount();
                self.set_ok("已追加");
            }
            Err(_) => self.set_err("追加失败"),
        }
    }

    fn save_session_attrs(&mut self) {
        let sid = match &self.selected {
            Some(s) => s.clone(),
            None => return,
        };
        let (title, source, pin, arch) =
            (self.s_title.clone(), self.s_source.clone(), self.s_pinned as i64, self.s_archived as i64);
        let Some(conn) = self.begin_write() else { return };
        if conn
            .execute(
                "UPDATE sessions SET title=?1, source=?2, created_source=?2, pinned=?3, archived=?4, \
                 last_activity_at=?5 WHERE id=?6",
                params![title, source, pin, arch, now_ts(), sid],
            )
            .is_ok()
        {
            self.o_title = self.s_title.clone();
            self.o_source = self.s_source.clone();
            self.o_pinned = self.s_pinned;
            self.o_archived = self.s_archived;
            drop(conn);
            self.set_ok("会话属性已保存");
            self.reload_sessions();
        } else {
            self.set_err("保存会话属性失败");
        }
    }

    fn save_system_prompt(&mut self, text: &str) {
        let sid = match &self.selected {
            Some(s) => s.clone(),
            None => return,
        };
        let Some(conn) = self.begin_write() else { return };
        if conn
            .execute("UPDATE sessions SET system_prompt=?1 WHERE id=?2", params![text, sid])
            .is_ok()
        {
            self.set_ok("系统提示词已保存");
        } else {
            self.set_err("保存系统提示词失败");
        }
    }

    fn load_system_prompt(&self) -> String {
        if let (Some(sid), Ok(conn)) = (&self.selected, self.conn()) {
            if let Ok(s) = conn.query_row(
                "SELECT COALESCE(system_prompt,'') FROM sessions WHERE id=?1",
                params![sid],
                |r| r.get::<_, String>(0),
            ) {
                return s;
            }
        }
        String::new()
    }

    fn create_session(&mut self) {
        let sid = format!("{}_{}", chrono::Local::now().format("%Y%m%d_%H%M%S"), &simple_uid()[..6]);
        let Some(conn) = self.begin_write() else { return };
        if conn
            .execute(
                "INSERT INTO sessions(id,source,created_source,model,title,title_source,started_at,\
                 last_activity_at,message_count,tool_call_count,pinned,archived,profile_name) \
                 VALUES(?1,'cli','cli','manual','新建会话','manual',?2,?2,0,0,1,0,'default')",
                params![sid, now_ts()],
            )
            .is_ok()
        {
            drop(conn);
            self.set_ok(format!("已创建 {sid}"));
            self.reload_sessions();
            self.open_session(&sid);
        } else {
            self.set_err("创建会话失败");
        }
    }

    fn recount(&self) {
        if let (Some(sid), Ok(conn)) = (&self.selected, self.conn()) {
            let _ = conn.execute(
                "UPDATE sessions SET message_count=(SELECT COUNT(*) FROM messages WHERE session_id=?1) WHERE id=?1",
                params![sid],
            );
        }
    }

    fn newest_backup(&self) -> Option<PathBuf> {
        let dir = self.backup_dir();
        let rd = std::fs::read_dir(&dir).ok()?;
        let mut baks: Vec<PathBuf> = rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name().map(|n| n.to_string_lossy().starts_with("state.db.")).unwrap_or(false)
                    && p.extension().map(|e| e == "bak").unwrap_or(false)
            })
            .collect();
        baks.sort();
        baks.pop()
    }

    fn restore_backup(&mut self, path: &Path) {
        // Snapshot current first (so a restore is itself undoable).
        if self.backup().is_none() {
            return;
        }
        let wal = self.db_path.with_extension("db-wal");
        let shm = self.db_path.with_extension("db-shm");
        let _ = std::fs::remove_file(&wal);
        let _ = std::fs::remove_file(&shm);
        match std::fs::copy(path, &self.db_path) {
            Ok(_) => {
                self.set_ok(format!(
                    "已恢复备份: {}",
                    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
                ));
                let sel = self.selected.clone();
                self.reload_sessions();
                if let Some(sid) = sel {
                    self.open_session(&sid);
                }
            }
            Err(e) => self.set_err(format!("恢复失败: {e}")),
        }
    }

    fn open_backups_folder(&mut self) {
        let dir = self.backup_dir();
        let _ = std::fs::create_dir_all(&dir);
        #[cfg(windows)]
        {
            let _ = std::process::Command::new("explorer").arg(&dir).spawn();
        }
        #[cfg(not(windows))]
        {
            let _ = std::process::Command::new("xdg-open").arg(&dir).spawn();
        }
        self.set_ok(format!("已打开 {}", dir.display()));
    }

    /// Switch session, honoring the unsaved-changes guard.
    fn try_select(&mut self, sid: String) {
        if self.selected.as_deref() == Some(sid.as_str()) {
            return;
        }
        if self.any_dirty() {
            self.modal = Modal::DiscardThenSelect(sid);
        } else {
            self.open_session(&sid);
        }
    }
}

fn simple_uid() -> String {
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

fn fmt_ts(ts: f64) -> String {
    if ts <= 0.0 {
        return String::new();
    }
    use chrono::{Local, TimeZone};
    match Local.timestamp_opt(ts as i64, 0) {
        chrono::LocalResult::Single(dt) => dt.format("%Y-%m-%d %H:%M:%S").to_string(),
        _ => String::new(),
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Ctrl+S saves all dirty messages.
        let save_hotkey = ctx.input(|i| i.modifiers.command && i.key_pressed(egui::Key::S));
        if save_hotkey {
            self.save_all_dirty();
        }

        // Deferred actions collected from closures (borrow-checker friendly).
        let mut do_refresh = false;
        let mut do_new = false;
        let mut open_sysprompt = false;
        let mut do_open_backups = false;
        let mut do_restore = false;
        let mut msg_action: Option<(usize, &'static str)> = None;

        // ---- Left: session list ----
        egui::SidePanel::left("sidebar").default_width(330.0).show(ctx, |ui| {
            ui.add_space(6.0);
            ui.heading("Hermes Chat Editor");
            ui.add_space(4.0);

            let mut changed = false;
            ui.horizontal(|ui| {
                ui.label("搜索");
                if ui
                    .add(egui::TextEdit::singleline(&mut self.search).hint_text("标题 / ID / 正文"))
                    .changed()
                {
                    changed = true;
                }
            });
            if ui.checkbox(&mut self.show_internal, "显示内部会话 (oneshot/tool/kanban)").changed() {
                changed = true;
            }
            ui.horizontal(|ui| {
                if ui.button("+ 新建会话").clicked() {
                    do_new = true;
                }
                if ui.button("刷新").clicked() {
                    do_refresh = true;
                }
            });
            if changed {
                self.reload_sessions();
            }
            ui.separator();

            let sessions = self.sessions.clone();
            let mut to_select: Option<String> = None;
            egui::ScrollArea::vertical().id_source("sess_scroll").show(ui, |ui| {
                for s in &sessions {
                    let sel = self.selected.as_deref() == Some(s.id.as_str());
                    let mut marks = String::new();
                    if s.pinned {
                        marks.push_str("📌 ");
                    }
                    if s.archived {
                        marks.push_str("🗄 ");
                    }
                    let label = format!(
                        "{}{}  [{}]\n{} · {} 条",
                        marks,
                        if s.title.is_empty() { "(无标题)" } else { &s.title },
                        s.source,
                        &s.id,
                        s.msg_count
                    );
                    if ui.selectable_label(sel, label).clicked() {
                        to_select = Some(s.id.clone());
                    }
                }
            });
            if let Some(sid) = to_select {
                self.try_select(sid);
            }
            ui.separator();
            ui.horizontal(|ui| {
                ui.label(format!("{} 个会话", self.sessions.len()));
                if ui.small_button("备份文件夹").clicked() {
                    do_open_backups = true;
                }
                if ui.small_button("恢复最近备份").clicked() {
                    do_restore = true;
                }
            });
        });

        // ---- Bottom: status + composer ----
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
                    let send = ui.button("追加").clicked();
                    let resp = ui.add(
                        egui::TextEdit::singleline(&mut self.composer_text)
                            .hint_text("追加一条消息到末尾…（预填 / 伪造历史）")
                            .desired_width(f32::INFINITY),
                    );
                    let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    if send || enter {
                        self.append_message();
                    }
                });
            }
        });

        // ---- Center: messages ----
        egui::CentralPanel::default().show(ctx, |ui| {
            if self.hermes_running {
                egui::Frame::none()
                    .fill(egui::Color32::from_rgb(0x5a, 0x1e, 0x1e))
                    .inner_margin(egui::Margin::symmetric(10.0, 6.0))
                    .show(ui, |ui| {
                        ui.colored_label(
                            egui::Color32::from_rgb(0xff, 0xd0, 0xd0),
                            "⚠ 检测到 Hermes 正在运行：它可能用内存中的会话覆盖你的改动。请先关闭 gateway/desktop 再编辑。",
                        );
                    });
                ui.add_space(4.0);
            }

            if self.selected.is_none() {
                ui.centered_and_justified(|ui| ui.label("← 选择一个会话开始编辑"));
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
                if ui.button("系统提示词…").clicked() {
                    open_sysprompt = true;
                }
                if self.attrs_dirty() {
                    ui.colored_label(egui::Color32::from_rgb(0x4a, 0x84, 0xfe), "●未保存");
                }
            });
            ui.label(egui::RichText::new(&self.s_meta).small().weak());
            ui.separator();

            let count = self.messages.len();
            egui::ScrollArea::vertical()
                .id_source("msg_scroll")
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    for i in 0..count {
                        let (id, role, is_empty, is_tool, tool_summary, flags, ts) = {
                            let m = &self.messages[i];
                            let empty = m.edit_content.trim().is_empty();
                            let tool = m.role == "tool";
                            let summary = tool_call_names(m.tool_calls.as_deref());
                            let mut fl = vec![];
                            if !m.active {
                                fl.push("inactive".to_string());
                            }
                            if m.compacted {
                                fl.push("compacted".to_string());
                            }
                            if let Some(tn) = &m.tool_name {
                                fl.push(format!("tool:{tn}"));
                            }
                            (m.id, m.role.clone(), empty, tool, summary, fl.join(" · "), m.ts)
                        };

                        egui::Frame::group(ui.style()).fill(role_fill(&role)).show(ui, |ui| {
                            ui.horizontal(|ui| {
                                egui::ComboBox::from_id_source(format!("role_{id}"))
                                    .selected_text(&self.messages[i].edit_role)
                                    .width(104.0)
                                    .show_ui(ui, |ui| {
                                        for r in ["user", "assistant", "system", "tool"] {
                                            if ui
                                                .selectable_value(&mut self.messages[i].edit_role, r.to_string(), r)
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
                                let tsf = fmt_ts(ts);
                                if !tsf.is_empty() {
                                    ui.label(egui::RichText::new(tsf).small().weak());
                                }
                                ui.label(egui::RichText::new(&flags).small().weak());
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    if ui.button("🗑").on_hover_text("删除").clicked() {
                                        msg_action = Some((i, "delete"));
                                    }
                                    if ui.button("✂").on_hover_text("裁剪至此（删除其后全部）").clicked() {
                                        msg_action = Some((i, "truncate"));
                                    }
                                    if ui.button("＋").on_hover_text("在此条后插入").clicked() {
                                        msg_action = Some((i, "insert"));
                                    }
                                    if ui.button("💾").on_hover_text("保存（Ctrl+S 保存全部）").clicked() {
                                        msg_action = Some((i, "save"));
                                    }
                                });
                            });

                            if is_empty && !tool_summary.is_empty() {
                                ui.label(
                                    egui::RichText::new(format!("工具调用 → {tool_summary}")).italics().weak(),
                                );
                                let te = egui::TextEdit::multiline(&mut self.messages[i].edit_content)
                                    .desired_width(f32::INFINITY)
                                    .desired_rows(1)
                                    .hint_text("(空内容：工具调用载体)");
                                if ui.add(te).changed() {
                                    self.messages[i].dirty = true;
                                }
                            } else if is_tool {
                                let header = format!(
                                    "{} 返回结果 ({} 字符)",
                                    self.messages[i].tool_name.clone().unwrap_or_else(|| "tool".into()),
                                    self.messages[i].edit_content.len()
                                );
                                egui::CollapsingHeader::new(header).id_source(format!("tool_{id}")).show(ui, |ui| {
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
                                egui::CollapsingHeader::new("reasoning").id_source(format!("reason_{id}")).show(
                                    ui,
                                    |ui| {
                                        ui.label(egui::RichText::new(&self.messages[i].reasoning).small().weak());
                                    },
                                );
                            }
                        });
                        ui.add_space(8.0);
                    }
                });
        });

        // ---- Apply message action (open modal for destructive ones) ----
        if let Some((i, what)) = msg_action {
            match what {
                "save" => self.save_message(i),
                "delete" => self.modal = Modal::ConfirmDelete(i),
                "truncate" => self.modal = Modal::ConfirmTruncate(i),
                "insert" => {
                    self.modal = Modal::Insert { after_idx: i, role: "assistant".into(), content: String::new() }
                }
                _ => {}
            }
        }
        if do_refresh {
            if self.any_dirty() {
                self.modal = Modal::DiscardThenRefresh;
            } else {
                self.refresh_hermes_running();
                self.reload_sessions();
                if let Some(sid) = self.selected.clone() {
                    self.open_session(&sid);
                }
            }
        }
        if do_new {
            if self.any_dirty() {
                self.modal = Modal::DiscardThenNew;
            } else {
                self.create_session();
            }
        }
        if open_sysprompt {
            let buf = self.load_system_prompt();
            self.modal = Modal::SystemPrompt { buf, dirty: false };
        }
        if do_open_backups {
            self.open_backups_folder();
        }
        if do_restore {
            match self.newest_backup() {
                Some(p) => self.modal = Modal::ConfirmRestore(p),
                None => self.set_err("没有可用备份"),
            }
        }

        // ---- Modal rendering ----
        self.render_modal(ctx);
    }
}

impl App {
    fn render_modal(&mut self, ctx: &egui::Context) {
        let modal = std::mem::replace(&mut self.modal, Modal::None);
        match modal {
            Modal::None => {}
            Modal::ConfirmDelete(idx) => {
                if idx >= self.messages.len() {
                    return;
                }
                let id = self.messages[idx].id;
                let mut keep = true;
                modal_window(ctx, "确认删除", |ui, decided| {
                    ui.label(format!("删除消息 #{id}？此操作会写入数据库（已自动备份）。"));
                    ui.horizontal(|ui| {
                        if ui.button("删除").clicked() {
                            *decided = Some(true);
                            keep = false;
                        }
                        if ui.button("取消").clicked() {
                            *decided = Some(false);
                            keep = false;
                        }
                    });
                });
                if keep {
                    self.modal = Modal::ConfirmDelete(idx);
                } else if last_decision() == Some(true) {
                    self.delete_message(idx);
                }
            }
            Modal::ConfirmTruncate(idx) => {
                if idx >= self.messages.len() {
                    return;
                }
                let n = self.messages.len().saturating_sub(idx + 1);
                let id = self.messages[idx].id;
                let mut keep = true;
                modal_window(ctx, "确认裁剪 (rewind)", |ui, decided| {
                    ui.label(format!("删除 #{id} 之后的 {n} 条消息？不可直接撤销（已自动备份）。"));
                    ui.horizontal(|ui| {
                        if ui.button("裁剪").clicked() {
                            *decided = Some(true);
                            keep = false;
                        }
                        if ui.button("取消").clicked() {
                            *decided = Some(false);
                            keep = false;
                        }
                    });
                });
                if keep {
                    self.modal = Modal::ConfirmTruncate(idx);
                } else if last_decision() == Some(true) {
                    self.truncate_after(idx);
                }
            }
            Modal::DiscardThenSelect(sid) => {
                let mut keep = true;
                modal_window(ctx, "有未保存更改", |ui, decided| {
                    ui.label("当前会话有未保存的编辑。切换会放弃这些更改。");
                    ui.horizontal(|ui| {
                        if ui.button("放弃并切换").clicked() {
                            *decided = Some(true);
                            keep = false;
                        }
                        if ui.button("取消").clicked() {
                            *decided = Some(false);
                            keep = false;
                        }
                    });
                });
                if keep {
                    self.modal = Modal::DiscardThenSelect(sid);
                } else if last_decision() == Some(true) {
                    self.open_session(&sid);
                }
            }
            Modal::DiscardThenRefresh => {
                let mut keep = true;
                modal_window(ctx, "有未保存更改", |ui, decided| {
                    ui.label("刷新会放弃当前未保存的编辑。");
                    ui.horizontal(|ui| {
                        if ui.button("放弃并刷新").clicked() {
                            *decided = Some(true);
                            keep = false;
                        }
                        if ui.button("取消").clicked() {
                            *decided = Some(false);
                            keep = false;
                        }
                    });
                });
                if keep {
                    self.modal = Modal::DiscardThenRefresh;
                } else if last_decision() == Some(true) {
                    self.refresh_hermes_running();
                    self.reload_sessions();
                    if let Some(sid) = self.selected.clone() {
                        self.open_session(&sid);
                    }
                }
            }
            Modal::DiscardThenNew => {
                let mut keep = true;
                modal_window(ctx, "有未保存更改", |ui, decided| {
                    ui.label("新建会话会放弃当前未保存的编辑。");
                    ui.horizontal(|ui| {
                        if ui.button("放弃并新建").clicked() {
                            *decided = Some(true);
                            keep = false;
                        }
                        if ui.button("取消").clicked() {
                            *decided = Some(false);
                            keep = false;
                        }
                    });
                });
                if keep {
                    self.modal = Modal::DiscardThenNew;
                } else if last_decision() == Some(true) {
                    self.create_session();
                }
            }
            Modal::ConfirmRestore(path) => {
                let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                let mut keep = true;
                modal_window(ctx, "恢复备份", |ui, decided| {
                    ui.label(format!("用备份覆盖当前 state.db？\n{name}\n（当前库会先自动再备份一次。）"));
                    ui.horizontal(|ui| {
                        if ui.button("恢复").clicked() {
                            *decided = Some(true);
                            keep = false;
                        }
                        if ui.button("取消").clicked() {
                            *decided = Some(false);
                            keep = false;
                        }
                    });
                });
                if keep {
                    self.modal = Modal::ConfirmRestore(path);
                } else if last_decision() == Some(true) {
                    self.restore_backup(&path);
                }
            }
            Modal::Insert { after_idx, mut role, mut content } => {
                let mut keep = true;
                let mut commit = false;
                egui::Window::new("插入新消息")
                    .collapsible(false)
                    .resizable(true)
                    .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
                    .show(ctx, |ui| {
                        ui.horizontal(|ui| {
                            ui.label("角色");
                            egui::ComboBox::from_id_source("ins_role")
                                .selected_text(&role)
                                .show_ui(ui, |ui| {
                                    for r in ["user", "assistant", "system", "tool"] {
                                        ui.selectable_value(&mut role, r.to_string(), r);
                                    }
                                });
                        });
                        ui.add(
                            egui::TextEdit::multiline(&mut content)
                                .desired_width(420.0)
                                .desired_rows(5)
                                .hint_text("消息内容…"),
                        );
                        ui.horizontal(|ui| {
                            if ui.button("插入").clicked() {
                                commit = true;
                                keep = false;
                            }
                            if ui.button("取消").clicked() {
                                keep = false;
                            }
                        });
                    });
                if commit {
                    self.insert_after(after_idx, role, content);
                } else if keep {
                    self.modal = Modal::Insert { after_idx, role, content };
                }
            }
            Modal::SystemPrompt { mut buf, mut dirty } => {
                let mut keep = true;
                let mut do_save = false;
                egui::Window::new("系统提示词 (system_prompt)")
                    .collapsible(false)
                    .resizable(true)
                    .default_size(egui::vec2(640.0, 460.0))
                    .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
                    .show(ctx, |ui| {
                        ui.label(egui::RichText::new("直接写入会话的 system_prompt 列。").small().weak());
                        egui::ScrollArea::vertical().max_height(360.0).show(ui, |ui| {
                            if ui
                                .add(
                                    egui::TextEdit::multiline(&mut buf)
                                        .desired_width(f32::INFINITY)
                                        .desired_rows(18)
                                        .code_editor(),
                                )
                                .changed()
                            {
                                dirty = true;
                            }
                        });
                        ui.horizontal(|ui| {
                            if ui.button("保存").clicked() {
                                do_save = true;
                                keep = false;
                            }
                            if ui.button("关闭").clicked() {
                                keep = false;
                            }
                            if dirty {
                                ui.colored_label(egui::Color32::from_rgb(0x4a, 0x84, 0xfe), "●未保存");
                            }
                        });
                    });
                if do_save {
                    self.save_system_prompt(&buf);
                } else if keep {
                    self.modal = Modal::SystemPrompt { buf, dirty };
                }
            }
        }
    }
}

// --- tiny decision channel for the simple yes/no modals ---
use std::sync::atomic::{AtomicI8, Ordering};
static LAST_DECISION: AtomicI8 = AtomicI8::new(-2); // -2 none, -1 no, 1 yes

fn last_decision() -> Option<bool> {
    match LAST_DECISION.swap(-2, Ordering::SeqCst) {
        1 => Some(true),
        -1 => Some(false),
        _ => None,
    }
}

/// Render a centered yes/no modal; the closure sets `*decided` to Some(bool).
fn modal_window(ctx: &egui::Context, title: &str, add: impl FnOnce(&mut egui::Ui, &mut Option<bool>)) {
    let mut decided: Option<bool> = None;
    egui::Window::new(title)
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
        .show(ctx, |ui| {
            add(ui, &mut decided);
        });
    if let Some(d) = decided {
        LAST_DECISION.store(if d { 1 } else { -1 }, Ordering::SeqCst);
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
    let mut names = vec![];
    let bytes = s.as_bytes();
    let needle = b"\"name\"";
    let mut i = 0;
    while i + needle.len() < bytes.len() {
        if &bytes[i..i + needle.len()] == needle {
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
    let db_path = std::env::args().nth(1).map(PathBuf::from).unwrap_or_else(default_db_path);

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1120.0, 760.0])
            .with_title("Hermes Chat Editor"),
        ..Default::default()
    };
    eframe::run_native(
        "Hermes Chat Editor",
        options,
        Box::new(move |cc| {
            install_cjk_font(&cc.egui_ctx);
            cc.egui_ctx.set_visuals(egui::Visuals::dark());
            Ok(Box::new(App::new(db_path)))
        }),
    )
}

/// egui ships no CJK glyphs; pull one from the OS so Chinese renders.
fn install_cjk_font(ctx: &egui::Context) {
    let candidates = [
        "C:/Windows/Fonts/msyh.ttc",
        "C:/Windows/Fonts/simhei.ttf",
        "C:/Windows/Fonts/simsun.ttc",
    ];
    for path in candidates {
        if let Ok(bytes) = std::fs::read(path) {
            let mut fonts = egui::FontDefinitions::default();
            fonts.font_data.insert("cjk".to_owned(), egui::FontData::from_owned(bytes));
            fonts.families.entry(egui::FontFamily::Proportional).or_default().insert(0, "cjk".to_owned());
            fonts.families.entry(egui::FontFamily::Monospace).or_default().push("cjk".to_owned());
            ctx.set_fonts(fonts);
            return;
        }
    }
}
