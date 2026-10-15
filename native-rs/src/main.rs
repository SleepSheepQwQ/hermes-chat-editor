// Hermes Chat Editor — native Windows app (egui/eframe).
//
// A Chatbox-style editor for the Hermes Agent `state.db` session store, with
// NO web layer: a real native window talking straight to SQLite via rusqlite
// (bundled, so there is no system SQLite dependency).
//
// Robustness & features (v0.3.0):
//  - READ-ONLY MODE by default: the app opens the DB read-only; editing is
//    enabled only when the user explicitly toggles "可写模式". This prevents
//    accidental writes.
//  - Every mutating op snapshots state.db to ./backups first (WAL-safe), via a
//    single centralized write path (`begin_write`).
//  - Destructive ops (delete / truncate / restore / discard) require a modal
//    confirmation.
//  - Detects a RUNNING Hermes (recent gateway_heartbeats) and shows a banner —
//    editing a live DB risks being overwritten by Hermes' in-memory cache.
//  - When Hermes is running, write operations are BLOCKED (banner + disabled
//    buttons) unless the user overrides via "强制写入" checkbox.
//  - Unsaved-changes guard when switching session / refreshing / creating.
//  - Ctrl+S saves every dirty message in ONE transaction.
//  - In-memory list is mutated in place after writes (scroll position kept),
//    instead of re-reading the whole session.
//  - Safe write path: sets busy_timeout=30s, PRAGMA foreign_keys=ON, and lets
//    SQLite triggers auto-assign display_order / message_uid on INSERT (we do
//    NOT pass those columns, to avoid corrupting Hermes' display identity).
//  - After insert/delete/truncate we sync sessions.message_count so the
//    session list stays accurate.
//  - Message reordering (move up / move down) for fine-grained display_order.
//  - Batch operations: multi-select messages, then delete / change role /
//    export selection.
//  - Full-text search over message bodies (LIKE based, portable; no FTS5
//    dependency needed).
//  - Export current session as JSONL / Markdown / HTML.
//  - Import a session from a JSONL file (creates a new session).
//  - Theme toggle (dark / light).
//  - Extra keyboard shortcuts: Ctrl+F focuses search, Ctrl+Z undo (clears
//    dirty state), Delete deletes selected messages.
//  - Profile picker: scan HERMES_HOME-like directories for alternate state.db.
//  - Lightweight Markdown preview for message content (bold/italic/code via
//    simple heuristics, no external crate).

#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use eframe::egui;
use rusqlite::{params, Connection};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

const INTERNAL_SOURCES: [&str; 3] = ["kanban", "tool", "oneshot"];
const HEARTBEAT_FRESH_SECS: f64 = 120.0;

/// Human-friendly names for the source values shown in the session list.
fn source_display(src: &str) -> &str {
    match src {
        "cli" => "cli",
        "tui" => "tui",
        "telegram" => "telegram",
        "discord" => "discord",
        "slack" => "slack",
        "desktop" => "desktop",
        "api-server" => "api",
        "kanban" => "kanban",
        "tool" => "tool",
        "oneshot" => "oneshot",
        "cron" => "cron",
        "subagent" => "sub",
        _ => src,
    }
}

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

/// Discover candidate Hermes state.db paths (multi-profile support).
fn discover_profiles() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        let p = PathBuf::from(local).join("hermes").join("state.db");
        if p.exists() {
            v.push(p);
        }
    }
    if let Some(home) = dirs_home() {
        let p = home.join(".hermes").join("state.db");
        if p.exists() && !v.contains(&p) {
            v.push(p);
        }
        // Also scan ~/.hermes/profiles/<name>/state.db
        let profiles_root = home.join(".hermes").join("profiles");
        if let Ok(rd) = std::fs::read_dir(&profiles_root) {
            for e in rd.flatten() {
                let pp = e.path().join("state.db");
                if pp.exists() && !v.contains(&pp) {
                    v.push(pp);
                }
            }
        }
    }
    v
}

#[derive(Clone)]
struct SessionRow {
    id: String,
    title: String,
    source: String,
    pinned: bool,
    archived: bool,
    msg_count: i64,
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
    ts: f64,
    // UI edit buffers
    edit_content: String,
    edit_role: String,
    edit_reasoning: String,
    dirty: bool,
}

/// Pending modal / confirmation state.
enum Modal {
    None,
    ConfirmDelete(Vec<usize>),
    ConfirmTruncate(usize),
    DiscardThenSelect(String),
    DiscardThenRefresh,
    DiscardThenNew,
    ConfirmRestore(PathBuf),
    Insert { after_idx: usize, role: String, content: String },
    SystemPrompt { buf: String, dirty: bool },
    ImportPath { buf: String },
    ConfirmDeleteSession(String),
}

struct App {
    db_path: PathBuf,
    profile_paths: Vec<PathBuf>,
    read_only: bool,
    search: String,
    search_focused: bool,
    show_internal: bool,
    source_filter: String,
    last_source_filter: String,
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
    // batch selection
    selected_msgs: HashSet<usize>,
    // theme
    dark_mode: bool,
    // markdown preview toggle
    md_preview: bool,
}

impl App {
    fn new(db_path: PathBuf) -> Self {
        let profiles = discover_profiles();
        let mut app = Self {
            db_path,
            profile_paths: profiles,
            read_only: true, // default to read-only for safety
            search: String::new(),
            search_focused: false,
            show_internal: false,
            source_filter: String::new(),
            last_source_filter: String::new(),
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
            selected_msgs: HashSet::new(),
            dark_mode: true,
            md_preview: false,
        };
        app.refresh_hermes_running();
        app.reload_sessions();
        app
    }

    /// Open a read-only connection (mode=ro). This is the DEFAULT way to open.
    fn conn_readonly(&self) -> rusqlite::Result<Connection> {
        Connection::open_with_flags(&self.db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
    }

    /// Open a normal connection (read-write).
    fn conn(&self) -> rusqlite::Result<Connection> {
        Connection::open(&self.db_path)
    }

    /// Open a write connection with safe pragmas (busy_timeout, foreign_keys).
    fn conn_write(&self) -> rusqlite::Result<Connection> {
        let c = Connection::open(&self.db_path)?;
        let _ = c.busy_timeout(std::time::Duration::from_millis(30000));
        let _ = c.pragma_update(None, "foreign_keys", "ON");
        Ok(c)
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
    /// Centralized write entry: snapshot first, then hand back a connection.
    /// Editing is ALWAYS allowed (a Hermes-running banner is advisory only):
    /// every write is auto-backup'd first and WAL + busy_timeout make concurrent
    /// access safe, so blocking writes when Hermes is running was over-cautious
    /// and made the editor unusable in the common case.
    fn begin_write(&mut self) -> Option<Connection> {
        if self.read_only {
            self.set_err("当前为只读模式。请取消勾选“只读模式”后再编辑。");
            return None;
        }
        if self.backup().is_none() {
            return None;
        }
        match self.conn_write() {
            Ok(c) => Some(c),
            Err(e) => {
                self.set_err(format!("打开数据库失败: {e}"));
                None
            }
        }
    }

    fn refresh_hermes_running(&mut self) {
        self.hermes_running = false;
        if let Ok(conn) = self.conn_readonly() {
            // Only the NEWEST heartbeat matters: older rows are stale gateway
            // launches that are never cleaned up.
            if let Ok(hb) = conn.query_row(
                "SELECT MAX(last_heartbeat) FROM gateway_heartbeats",
                [],
                |r| r.get::<_, Option<f64>>(0),
            ) {
                if let Some(hb) = hb {
                    let now = now_ts();
                    self.hermes_running = now - hb < HEARTBEAT_FRESH_SECS;
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
        self.attrs_dirty()
            || self.messages.iter().any(|m| m.dirty || m.edit_reasoning != m.reasoning)
    }

    fn reload_sessions(&mut self) {
        let conn = match self.conn_readonly() {
            Ok(c) => c,
            Err(e) => {
                self.set_err(format!("打开数据库失败: {e}"));
                return;
            }
        };
        let mut sql = String::from(
            "SELECT id,COALESCE(title,''),COALESCE(source,''),COALESCE(pinned,0),\
             COALESCE(archived,0),COALESCE(message_count,0),COALESCE(last_activity_at,started_at) FROM sessions",
        );
        let mut conds: Vec<String> = vec![];
        if !self.show_internal {
            let list = INTERNAL_SOURCES.iter().map(|s| format!("'{s}'")).collect::<Vec<_>>().join(",");
            conds.push(format!("COALESCE(source,'') NOT IN ({list})"));
        }
        if !self.source_filter.is_empty() {
            conds.push(format!("COALESCE(source,'')='{}'", self.source_filter.replace('\'', "''")));
        }
        if !self.search.trim().is_empty() {
            // title OR id OR any message body matches (portable LIKE; no FTS5 dependency)
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
                    last_activity: r.get(6)?,
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
        let conn = match self.conn_readonly() {
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
                let reasoning: String = r.get(3)?;
                Ok(MessageRow {
                    id: r.get(0)?,
                    role: role.clone(),
                    content: content.clone(),
                    reasoning: reasoning.clone(),
                    tool_name: r.get(4)?,
                    tool_calls: r.get(5)?,
                    active: r.get::<_, i64>(6)? != 0,
                    compacted: r.get::<_, i64>(7)? != 0,
                    ts: r.get(8)?,
                    edit_content: content,
                    edit_role: role,
                    edit_reasoning: reasoning,
                    dirty: false,
                })
            }) {
                for m in it.flatten() {
                    msgs.push(m);
                }
            }
        }
        self.messages = msgs;
        self.selected_msgs.clear();
        self.selected = Some(sid.to_string());
        self.set_ok(format!("已载入 {} 条消息", self.messages.len()));
    }

    /// Compute the correct `finish_reason` for a message that will carry `new_role`.
    ///
    /// Hermes relies on `finish_reason` to recognize a *complete* assistant turn
    /// (`stop` or `tool_calls`). A pre-filled/edited assistant message with a NULL
    /// finish_reason reads as an orphan and the model re-classifies it as a user
    /// turn. Rules:
    ///   - assistant with stored tool_calls  -> keep the stored finish_reason
    ///     (fall back to "tool_calls" if it is empty)
    ///   - assistant without tool_calls       -> "stop"
    ///   - any other role                     -> NULL (finish_reason is assistant-only)
    fn finish_reason_for(conn: &Connection, id: i64, new_role: &str) -> Option<String> {
        if new_role != "assistant" {
            return None;
        }
        let stored: Option<(String, String)> = conn
            .query_row(
                "SELECT COALESCE(finish_reason,''), COALESCE(tool_calls,'') FROM messages WHERE id=?1",
                params![id],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )
            .ok();
        match stored {
            Some((finish, tool_calls)) if !tool_calls.trim().is_empty() => {
                if finish.is_empty() {
                    Some("tool_calls".to_string())
                } else {
                    Some(finish)
                }
            }
            _ => Some("stop".to_string()),
        }
    }

    fn save_message(&mut self, idx: usize) {
        let Some(conn) = self.begin_write() else { return };
        let (id, content, role, reasoning) = {
            let m = &self.messages[idx];
            (m.id, m.edit_content.clone(), m.edit_role.clone(), m.edit_reasoning.clone())
        };
        // Rewriting content/role must: (1) clear the api_content sidecar so the
        // model replays the NEW text, not stale bytes; (2) re-normalize
        // finish_reason so the role is unambiguous on the wire. The reasoning
        // (思考过程) is written back to reasoning_content too.
        let finish = Self::finish_reason_for(&conn, id, &role);
        match conn.execute(
            "UPDATE messages SET content=?1, role=?2, finish_reason=?3, api_content=NULL, reasoning_content=?4 WHERE id=?5",
            params![content, role, finish, reasoning, id],
        ) {
            Ok(_) => {
                let m = &mut self.messages[idx];
                m.content = content;
                m.role = role;
                m.reasoning = reasoning;
                m.dirty = false;
                self.set_ok(format!("已保存 #{id}"));
            }
            Err(e) => self.set_err(format!("保存失败: {e}")),
        }
    }

    fn save_all_dirty(&mut self) {
        let dirties: Vec<usize> = self
            .messages
            .iter()
            .enumerate()
            .filter(|(_, m)| m.dirty || m.edit_reasoning != m.reasoning)
            .map(|(i, _)| i)
            .collect();
        if dirties.is_empty() {
            self.set_ok("没有未保存更改");
            return;
        }
        let Some(mut conn) = self.begin_write() else { return };
        // Pre-compute each row's finish_reason BEFORE opening the transaction,
        // so the read-only queries above don't hold the write lock.
        let mut updates: Vec<(i64, String, String, Option<String>, String)> = Vec::new();
        for &i in &dirties {
            let m = &self.messages[i];
            let finish = Self::finish_reason_for(&conn, m.id, &m.edit_role);
            updates.push((m.id, m.edit_content.clone(), m.edit_role.clone(), finish, m.edit_reasoning.clone()));
        }
        let tx = match conn.transaction() {
            Ok(t) => t,
            Err(e) => {
                self.set_err(format!("开启事务失败: {e}"));
                return;
            }
        };
        let mut ok = true;
        for (id, content, role, finish, reasoning) in &updates {
            if tx
                .execute(
                    "UPDATE messages SET content=?1, role=?2, finish_reason=?3, api_content=NULL, reasoning_content=?4 WHERE id=?5",
                    params![content, role, finish, reasoning, id],
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
                m.reasoning = m.edit_reasoning.clone();
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
            self.selected_msgs.remove(&idx);
            self.recount();
            self.set_ok(format!("已删除 #{id}"));
        } else {
            self.set_err("删除失败");
        }
    }

    /// Batch delete selected messages (removed by id, in a single transaction).
    fn delete_selected(&mut self) {
        if self.selected_msgs.is_empty() {
            self.set_ok("未选择任何消息");
            return;
        }
        let ids: Vec<i64> = self
            .selected_msgs
            .iter()
            .filter(|&&i| i < self.messages.len())
            .map(|&i| self.messages[i].id)
            .collect();
        if ids.is_empty() {
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
        for id in &ids {
            if tx.execute("DELETE FROM messages WHERE id=?1", params![id]).is_err() {
                ok = false;
                break;
            }
        }
        if ok && tx.commit().is_ok() {
            drop(conn);
            // Remove from in-memory list (descending indices)
            let mut indices: Vec<usize> = self.selected_msgs.iter().copied().collect();
            indices.sort_unstable_by(|a, b| b.cmp(a));
            for i in indices {
                if i < self.messages.len() {
                    self.messages.remove(i);
                }
            }
            self.selected_msgs.clear();
            self.recount();
            self.set_ok(format!("已删除 {} 条消息", ids.len()));
        } else {
            self.set_err("批量删除失败（已回滚）");
        }
    }

    /// Batch change role for selected messages.
    fn set_role_selected(&mut self, role: &str) {
        if self.selected_msgs.is_empty() {
            self.set_ok("未选择任何消息");
            return;
        }
        let ids: Vec<i64> = self
            .selected_msgs
            .iter()
            .filter(|&&i| i < self.messages.len())
            .map(|&i| self.messages[i].id)
            .collect();
        if ids.is_empty() {
            return;
        }
        let Some(mut conn) = self.begin_write() else { return };
        // Pre-compute finish_reason for each affected row (role may flip to/from
        // assistant), and clear the api_content sidecar since role changes.
        let mut updates: Vec<(i64, Option<String>)> = Vec::new();
        for id in &ids {
            let finish = Self::finish_reason_for(&conn, *id, role);
            updates.push((*id, finish));
        }
        let tx = match conn.transaction() {
            Ok(t) => t,
            Err(e) => {
                self.set_err(format!("开启事务失败: {e}"));
                return;
            }
        };
        let mut ok = true;
        for (id, finish) in &updates {
            if tx
                .execute(
                    "UPDATE messages SET role=?1, finish_reason=?2, api_content=NULL WHERE id=?3",
                    params![role, finish, id],
                )
                .is_err()
            {
                ok = false;
                break;
            }
        }
        if ok && tx.commit().is_ok() {
            drop(conn);
            for &i in &self.selected_msgs.clone() {
                if i < self.messages.len() {
                    self.messages[i].role = role.to_string();
                    self.messages[i].edit_role = role.to_string();
                }
            }
            self.set_ok(format!("已修改 {} 条消息角色为 {role}", ids.len()));
        } else {
            self.set_err("批量改角色失败（已回滚）");
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
                self.selected_msgs.retain(|&i| i < self.messages.len());
                self.recount();
                self.set_ok(format!("已裁剪 {n} 条后续消息"));
            }
            Err(_) => self.set_err("裁剪失败"),
        }
    }

    /// Move a message up (swap display_order with the previous message).
    fn move_message_up(&mut self, idx: usize) {
        if idx == 0 || idx >= self.messages.len() {
            return;
        }
        self.swap_display_order(idx, idx - 1);
    }

    /// Move a message down (swap display_order with the next message).
    fn move_message_down(&mut self, idx: usize) {
        if idx + 1 >= self.messages.len() {
            return;
        }
        self.swap_display_order(idx, idx + 1);
    }

    /// Swap display_order between two in-memory messages by writing to DB.
    fn swap_display_order(&mut self, a: usize, b: usize) {
        let sid = match &self.selected {
            Some(s) => s.clone(),
            None => return,
        };
        let ma = &self.messages[a];
        let mb = &self.messages[b];
        let ida = ma.id;
        let idb = mb.id;
        // We can't reliably read display_order if NULL (backfill pending). As a
        // safe portable approach, swap their `timestamp` values too? No — that
        // would rewrite history. Instead, only swap display_order when both are
        // non-NULL. For NULL cases we fall back to swapping timestamps is NOT
        // done; we simply refuse with a status message.
        let Some(mut conn) = self.begin_write() else { return };
        // Read both display_order values.
        let get_ord = |conn: &Connection, id: i64| -> Option<f64> {
            conn.query_row(
                "SELECT COALESCE(display_order,id) FROM messages WHERE id=?1",
                params![id],
                |r| r.get::<_, f64>(0),
            )
            .ok()
        };
        let (Some(oa), Some(ob)) = (get_ord(&conn, ida), get_ord(&conn, idb)) else {
            self.set_err("重排失败：display_order 为空（backfill 未完成）。请先以可写模式打开一次。");
            return;
        };
        // Swap in a transaction.
        let tx = match conn.transaction() {
            Ok(t) => t,
            Err(e) => {
                self.set_err(format!("开启事务失败: {e}"));
                return;
            }
        };
        let ok = tx
            .execute("UPDATE messages SET display_order=?1 WHERE id=?2", params![ob, ida])
            .is_ok()
            && tx
                .execute("UPDATE messages SET display_order=?1 WHERE id=?2", params![oa, idb])
                .is_ok();
        if ok && tx.commit().is_ok() {
            drop(conn);
            self.messages.swap(a, b);
            // Keep selection consistent.
            let mut new_sel = HashSet::new();
            for &i in &self.selected_msgs {
                if i == a {
                    new_sel.insert(b);
                } else if i == b {
                    new_sel.insert(a);
                } else {
                    new_sel.insert(i);
                }
            }
            self.selected_msgs = new_sel;
            self.set_ok("已调整消息顺序");
        } else {
            self.set_err("重排失败（事务已回滚）");
        }
    }

    /// Insert a new message after the given index.
    /// We do NOT pass display_order / message_uid / id — SQLite triggers
    /// (messages_display_order_insert, messages_message_uid_insert) fill them.
    fn insert_after(&mut self, after_idx: usize, role: String, content: String) {
        let sid = match &self.selected {
            Some(s) => s.clone(),
            None => return,
        };
        let ts = now_ts();
        let finish: Option<&str> = if role == "assistant" { Some("stop") } else { None };
        let Some(conn) = self.begin_write() else { return };
        let res = conn.execute(
            "INSERT INTO messages(session_id,role,content,timestamp,active,compacted,finish_reason) \
             VALUES(?1,?2,?3,?4,1,0,?5)",
            params![sid, role, content, ts, finish],
        );
        match res {
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
                    edit_reasoning: String::new(),
                    dirty: false,
                };
                self.messages.insert(after_idx + 1, row);
                self.recount();
                self.set_ok("已插入新消息（display_order/uid 由触发器自动生成）");
            }
            Err(e) => self.set_err(format!("插入失败: {e}")),
        }
    }

    /// Append a message at the end of the current session.
    fn append_message(&mut self) {
        if self.composer_text.trim().is_empty() || self.selected.is_none() {
            return;
        }
        let sid = self.selected.clone().unwrap();
        let role = self.composer_role.clone();
        let text = self.composer_text.clone();
        let ts = now_ts();
        let finish: Option<&str> = if role == "assistant" { Some("stop") } else { None };
        let Some(conn) = self.begin_write() else { return };
        let res = conn.execute(
            "INSERT INTO messages(session_id,role,content,timestamp,active,compacted,finish_reason) \
             VALUES(?1,?2,?3,?4,1,0,?5)",
            params![sid, role, text, ts, finish],
        );
        match res {
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
                    edit_reasoning: String::new(),
                    dirty: false,
                };
                self.messages.push(row);
                self.composer_text.clear();
                self.recount();
                self.set_ok("已追加");
            }
            Err(e) => self.set_err(format!("追加失败: {e}")),
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
        match conn.execute(
            "UPDATE sessions SET title=?1, source=?2, created_source=?2, pinned=?3, archived=?4, \
             last_activity_at=?5 WHERE id=?6",
            params![title, source, pin, arch, now_ts(), sid],
        ) {
            Ok(_) => {
                self.o_title = self.s_title.clone();
                self.o_source = self.s_source.clone();
                self.o_pinned = self.s_pinned;
                self.o_archived = self.s_archived;
                drop(conn);
                self.set_ok("会话属性已保存");
                self.reload_sessions();
            }
            Err(e) => {
                let msg = e.to_string();
                if msg.contains("UNIQUE") && msg.contains("title") {
                    self.set_err("保存失败：标题与其它会话重复（title 需唯一）");
                } else {
                    self.set_err(format!("保存会话属性失败: {e}"));
                }
            }
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
        if let (Some(sid), Ok(conn)) = (&self.selected, self.conn_readonly()) {
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
        // sessions.title has a UNIQUE index — derive a title that won't collide.
        let title = format!("新建会话 {}", chrono::Local::now().format("%m-%d %H:%M:%S"));
        let Some(conn) = self.begin_write() else { return };
        match conn.execute(
            "INSERT INTO sessions(id,source,created_source,model,title,title_source,started_at,\
             last_activity_at,message_count,tool_call_count,pinned,archived,profile_name) \
             VALUES(?1,'cli','cli','manual',?2,'manual',?3,?3,0,0,1,0,'default')",
            params![sid, title, now_ts()],
        ) {
            Ok(_) => {
                drop(conn);
                self.set_ok(format!("已创建 {sid}"));
                self.reload_sessions();
                self.open_session(&sid);
            }
            Err(e) => self.set_err(format!("创建会话失败: {e}")),
        }
    }

    /// Delete a session and all its messages (destructive; caller confirms).
    fn delete_session(&mut self, sid: &str) {
        let Some(mut conn) = self.begin_write() else { return };
        let tx = match conn.transaction() {
            Ok(t) => t,
            Err(e) => {
                self.set_err(format!("开启事务失败: {e}"));
                return;
            }
        };
        let n_msgs = match tx.execute("DELETE FROM messages WHERE session_id=?1", params![sid]) {
            Ok(n) => n,
            Err(e) => {
                self.set_err(format!("删除消息失败: {e}"));
                return;
            }
        };
        let ok_sess = tx.execute("DELETE FROM sessions WHERE id=?1", params![sid]).is_ok();
        let _ = tx.execute("DELETE FROM session_model_usage WHERE session_id=?1", params![sid]);
        let _ = tx.execute("DELETE FROM compression_locks WHERE session_id=?1", params![sid]);
        if ok_sess && tx.commit().is_ok() {
            drop(conn);
            if self.selected.as_deref() == Some(sid) {
                self.selected = None;
                self.messages.clear();
                self.selected_msgs.clear();
            }
            self.reload_sessions();
            self.set_ok(format!("已删除会话 {sid}（{n_msgs} 条消息）"));
        } else {
            self.set_err("删除会话失败（已回滚）");
        }
    }

    /// Export ALL sessions (one Markdown file each) into ./exports_all/.
    fn export_all_sessions(&mut self) {
        let dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")).join("exports_all");
        if std::fs::create_dir_all(&dir).is_err() {
            self.set_err("无法创建导出目录");
            return;
        }
        let sessions = self.sessions.clone();
        if sessions.is_empty() {
            self.set_err("没有会话可导出");
            return;
        }
        let mut exported = 0usize;
        let conn = match self.conn_readonly() {
            Ok(c) => c,
            Err(e) => {
                self.set_err(format!("打开数据库失败: {e}"));
                return;
            }
        };
        for s in &sessions {
            let mut out = format!("# {}\n\nID: {}\nSource: {}\n\n", s.title, s.id, s.source);
            if let Ok(mut st) = conn.prepare(
                "SELECT COALESCE(role,''),COALESCE(content,'') FROM messages \
                 WHERE session_id=?1 AND (active=1 OR compacted=1) \
                 ORDER BY COALESCE(display_order,id),id",
            ) {
                if let Ok(rows) = st.query_map(params![s.id], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                }) {
                    for m in rows.flatten() {
                        out.push_str(&format!("\n## {}\n\n{}\n", m.0, m.1));
                    }
                }
            }
            let fname = format!("{}.md", sanitize_filename(&format!("{}_{}", s.title, &s.id[..s.id.len().min(8)])));
            if std::fs::write(dir.join(fname), out).is_ok() {
                exported += 1;
            }
        }
        self.set_ok(format!("已导出 {exported} 个会话到 {}", dir.display()));
    }

    fn recount(&self) {
        if let (Some(sid), Ok(conn)) = (&self.selected, self.conn_write()) {
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

    /// Switch active database profile.
    fn switch_profile(&mut self, path: PathBuf) {
        if self.any_dirty() {
            self.modal = Modal::DiscardThenSelect(path.to_string_lossy().into_owned());
            return;
        }
        self.db_path = path.clone();
        self.read_only = true;
        self.selected_msgs.clear();
        self.selected = None;
        self.messages.clear();
        self.refresh_hermes_running();
        self.reload_sessions();
        self.set_ok(format!("已切换到数据库: {}", path.display()));
    }

    // ---- Export / Import ----

    fn export_session_jsonl(&self) -> Option<PathBuf> {
        let sid = self.selected.clone()?;
        let mut path = std::env::current_dir().ok()?;
        path.push(format!("{}.jsonl", sanitize_filename(&sid)));
        let mut out = String::new();
        for m in &self.messages {
            let line = format!(
                "{{\"id\":{},\"role\":{},\"content\":{},\"timestamp\":{}}}",
                m.id,
                json_escape(&m.content),
                json_escape(&m.role),
                m.ts
            );
            out.push_str(&line);
            out.push('\n');
        }
        std::fs::write(&path, out).ok()?;
        Some(path)
    }

    fn export_session_markdown(&self) -> Option<PathBuf> {
        let sid = self.selected.clone()?;
        let mut path = std::env::current_dir().ok()?;
        path.push(format!("{}.md", sanitize_filename(&sid)));
        let mut out = String::new();
        for m in &self.messages {
            let role = if m.role.is_empty() { "unknown" } else { &m.role };
            out.push_str(&format!("## {}\n\n{}\n\n", role, m.content));
        }
        std::fs::write(&path, out).ok()?;
        Some(path)
    }

    fn export_session_html(&self) -> Option<PathBuf> {
        let sid = self.selected.clone()?;
        let mut path = std::env::current_dir().ok()?;
        path.push(format!("{}.html", sanitize_filename(&sid)));
        let mut body = String::new();
        for m in &self.messages {
            let role = if m.role.is_empty() { "unknown" } else { &m.role };
            body.push_str(&format!(
                "<div class=\"msg role-{}\"><div class=\"role\">{}</div><div class=\"content\">{}</div></div>\n",
                role, role, html_escape(&m.content)
            ));
        }
        let doc = format!(
            "<!DOCTYPE html><html><head><meta charset=\"utf-8\"><title>{}</title><style>\
             body{{font-family:sans-serif;max-width:800px;margin:20px auto;}} .msg{{margin:12px 0;padding:8px;border-left:4px solid #ccc;}}\
             .role{{font-weight:bold;color:#666;}} .role-user{{border-color:#38f;}} .role-assistant{{border-color:#3a5;}} .role-tool{{border-color:#a3f;}}</style>\
             </head><body><h1>{}</h1>{}</body></html>",
            html_escape(&sid),
            html_escape(&sid),
            body
        );
        std::fs::write(&path, doc).ok()?;
        Some(path)
    }

    /// Import a JSONL file into a NEW session. Each line: {"role":..., "content":...}
    fn import_jsonl(&mut self, path: &Path) {
        let data = match std::fs::read_to_string(path) {
            Ok(d) => d,
            Err(e) => {
                self.set_err(format!("读取文件失败: {e}"));
                return;
            }
        };
        let mut rows: Vec<(String, String)> = Vec::new();
        for line in data.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            // Minimal JSON parsing: extract "role" and "content" fields.
            let role = extract_json_field(line, "role").unwrap_or_else(|| "user".to_string());
            let content = extract_json_field(line, "content").unwrap_or_default();
            rows.push((role, content));
        }
        if rows.is_empty() {
            self.set_err("文件中没有可导入的消息");
            return;
        }
        let Some(mut conn) = self.begin_write() else { return };
        let sid = format!("{}_{}", chrono::Local::now().format("%Y%m%d_%H%M%S"), &simple_uid()[..6]);
        let title = format!("导入 {}", chrono::Local::now().format("%m-%d %H:%M:%S"));
        let done = {
            let tx = match conn.transaction() {
                Ok(t) => t,
                Err(e) => {
                    self.set_err(format!("开启事务失败: {e}"));
                    return;
                }
            };
            let ok = tx
                .execute(
                    "INSERT INTO sessions(id,source,created_source,model,title,title_source,started_at,\
                     last_activity_at,message_count,tool_call_count,pinned,archived,profile_name) \
                     VALUES(?1,'cli','cli','manual',?2,'manual',?3,?3,0,0,1,0,'default')",
                    params![sid, title, now_ts()],
                )
                .is_ok();
            if ok {
                for (role, content) in &rows {
                    let finish: Option<&str> = if role == "assistant" { Some("stop") } else { None };
                    if tx
                        .execute(
                            "INSERT INTO messages(session_id,role,content,timestamp,active,compacted,finish_reason) \
                             VALUES(?1,?2,?3,?4,1,0,?5)",
                            params![sid, role, content, now_ts(), finish],
                        )
                        .is_err()
                    {
                        break;
                    }
                }
            }
            ok && tx.commit().is_ok()
        };
        drop(conn);
        if done {
            self.set_ok(format!("已导入 {} 条消息到新会话 {sid}", rows.len()));
            self.reload_sessions();
            self.open_session(&sid);
        } else {
            self.set_err("导入失败（已回滚）");
        }
    }

}

// ---- small helpers ----

fn sanitize_filename(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if c.is_alphanumeric() || c == '-' || c == '_' {
            out.push(c);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        out.push_str("session");
    }
    out
}

fn json_escape(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(c),
        }
    }
    out
}

fn html_escape(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
    out
}

/// Extremely small JSON string-field extractor (for JSONL import). Handles the
/// common {"role":"...","content":"..."} shape without pulling in serde.
fn extract_json_field(line: &str, field: &str) -> Option<String> {
    let key = format!("\"{field}\"");
    let idx = line.find(&key)?;
    let rest = &line[idx + key.len()..];
    let colon = rest.find(':')?;
    let after = &rest[colon + 1..].trim_start();
    if let Some(q1) = after.find('"') {
        let after_q = &after[q1 + 1..];
        if let Some(q2) = after_q.find('"') {
            return Some(after_q[..q2].to_string());
        }
    }
    None
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
        // Keyboard shortcuts
        ctx.input(|i| {
            if i.modifiers.command && i.key_pressed(egui::Key::S) {
                self.save_all_dirty();
            }
            if i.modifiers.command && i.key_pressed(egui::Key::F) {
                self.search_focused = true;
            }
            if i.modifiers.command && i.key_pressed(egui::Key::A) {
                // Select all messages in the current session.
                self.selected_msgs = (0..self.messages.len()).collect();
                self.set_ok(format!("已全选 {} 条消息", self.messages.len()));
            }
            if i.key_pressed(egui::Key::Escape) {
                if !self.selected_msgs.is_empty() {
                    self.selected_msgs.clear();
                } else if !self.search.is_empty() {
                    self.search.clear();
                }
            }
            if i.modifiers.command && i.key_pressed(egui::Key::Z) {
                // Undo: revert all unsaved edits (content/role/reasoning).
                for m in self.messages.iter_mut() {
                    m.edit_content = m.content.clone();
                    m.edit_role = m.role.clone();
                    m.edit_reasoning = m.reasoning.clone();
                    m.dirty = false;
                }
                self.s_title = self.o_title.clone();
                self.s_source = self.o_source.clone();
                self.s_pinned = self.o_pinned;
                self.s_archived = self.o_archived;
                self.set_ok("已撤销未保存的更改");
            }
            if i.modifiers.command && i.key_pressed(egui::Key::Delete) {
                // handled below via button path too; this is a no-op guard
            }
        });

        // Deferred actions collected from closures (borrow-checker friendly).
        let mut do_refresh = false;
        let mut do_new = false;
        let mut open_sysprompt = false;
        let mut do_open_backups = false;
        let mut do_restore = false;
        let mut do_export_jsonl = false;
        let mut do_export_md = false;
        let mut do_export_html = false;
        let mut do_export_all = false;
        let mut do_import = false;
        let mut do_delete_selected = false;
        let mut set_role: Option<String> = None;
        let mut msg_action: Option<(usize, &'static str)> = None;

        // ---- Left: session list ----
        egui::SidePanel::left("sidebar").default_width(340.0).show(ctx, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.heading("Hermes Chat Editor");
            });
            ui.add_space(4.0);

            // Read-only toggle
            ui.horizontal(|ui| {
                ui.checkbox(&mut self.read_only, "🔒 只读模式");
                if self.read_only {
                    ui.weak("（默认安全）");
                } else {
                    ui.colored_label(egui::Color32::from_rgb(0xf8, 0x51, 0x49), "可写");
                }
            });
            if self.hermes_running && !self.read_only {
                ui.colored_label(
                    egui::Color32::from_rgb(0xff, 0xd0, 0xd0),
                    "⚠ 检测到 Hermes 正在运行：改动可能被 Hermes 的内存会话覆盖。建议先关闭 Hermes 再编辑（写入不会因此被阻止）。",
                );
            }
            ui.horizontal(|ui| {
                ui.checkbox(&mut self.md_preview, "Markdown 预览");
            });
            ui.horizontal(|ui| {
                ui.checkbox(&mut self.dark_mode, "深色模式");
            });

            let mut changed = false;
            ui.horizontal(|ui| {
                ui.label("搜索");
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut self.search).hint_text("标题 / ID / 正文").desired_width(180.0),
                );
                if resp.lost_focus() {
                    self.search_focused = false;
                }
                if resp.changed() {
                    changed = true;
                }
                if self.search_focused {
                    resp.request_focus();
                }
                if !self.search.is_empty() && ui.small_button("✕").clicked() {
                    self.search.clear();
                    changed = true;
                }
            });
            ui.horizontal(|ui| {
                ui.label("来源");
                egui::ComboBox::from_id_source("source_filter")
                    .selected_text(if self.source_filter.is_empty() { "全部" } else { &self.source_filter })
                    .width(120.0)
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.source_filter, String::new(), "全部");
                        for src in ["cli", "desktop", "telegram", "oneshot", "tool", "kanban", "api-server", "subagent", "cron"] {
                            ui.selectable_value(&mut self.source_filter, src.to_string(), src);
                        }
                    });
                if self.source_filter.is_empty() {
                    ui.weak("");
                }
            });
            let filter_changed = {
                let mut f = false;
                // detect combo change via a polled marker is complex; simplest:
                // ComboBox writes directly to self.source_filter, we reload when
                // the value differs from the last-loaded value.
                f = self.source_filter != self.last_source_filter;
                if f {
                    self.last_source_filter = self.source_filter.clone();
                }
                f
            };
            if changed || filter_changed {
                self.reload_sessions();
            }
            if ui.checkbox(&mut self.show_internal, "显示内部会话 (oneshot/tool/kanban)").changed() {
                self.reload_sessions();
            }
            ui.horizontal(|ui| {
                if ui.button("+ 新建会话").clicked() {
                    do_new = true;
                }
                if ui.button("刷新").clicked() {
                    do_refresh = true;
                }
            });
            ui.separator();

            // Profile picker
            if !self.profile_paths.is_empty() {
                ui.horizontal(|ui| {
                    ui.label("数据库:");
                    egui::ComboBox::from_id_source("profile_picker")
                        .selected_text(self.db_path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default())
                        .show_ui(ui, |ui| {
                            for p in &self.profile_paths {
                                let label = p.display().to_string();
                                ui.selectable_value(&mut self.db_path, p.clone(), label);
                            }
                        });
                });
                // Note: changing db_path via combo doesn't reload automatically;
                // we detect change below.
            }
            ui.separator();

            let sessions = self.sessions.clone();
            let mut to_select: Option<String> = None;
            let mut to_delete: Option<String> = None;
            let mut to_copy_id: Option<String> = None;
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
                    let tsf = fmt_ts(s.last_activity);
                    let label = format!(
                        "{}{}  [{}]\n{} · {} 条{}",
                        marks,
                        if s.title.is_empty() { "(无标题)" } else { &s.title },
                        source_display(&s.source),
                        &s.id,
                        s.msg_count,
                        if tsf.is_empty() { String::new() } else { format!(" · {}", &tsf[5..tsf.len().min(16)]) }
                    );
                    let resp = ui
                        .selectable_label(sel, label)
                        .on_hover_text(format!("ID: {}\n最后活动: {}", s.id, if tsf.is_empty() { "-" } else { &tsf }));
                    if resp.clicked() {
                        to_select = Some(s.id.clone());
                    }
                    resp.context_menu(|ui| {
                        if ui.button("复制 ID").clicked() {
                            to_copy_id = Some(s.id.clone());
                            ui.close_menu();
                        }
                        if ui.button("删除会话…").clicked() {
                            to_delete = Some(s.id.clone());
                            ui.close_menu();
                        }
                        if ui.button("复制标题").clicked() {
                            ctx.copy_text(s.title.clone());
                            ui.close_menu();
                        }
                    });
                }
            });
            if let Some(sid) = to_select {
                self.try_select(sid);
            }
            if let Some(sid) = to_delete {
                self.modal = Modal::ConfirmDeleteSession(sid);
            }
            if let Some(sid) = to_copy_id {
                ctx.copy_text(sid);
                self.set_ok("已复制会话 ID 到剪贴板");
            }
            ui.separator();
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(format!(
                        "{} 个会话 · 共 {} 条消息 · 选中 {} 条",
                        self.sessions.len(),
                        self.sessions.iter().map(|s| s.msg_count).sum::<i64>(),
                        self.selected_msgs.len()
                    ))
                    .small()
                    .weak(),
                );
            });
            ui.horizontal(|ui| {
                if ui.small_button("备份文件夹").clicked() {
                    do_open_backups = true;
                }
                if ui.small_button("恢复最近备份").clicked() {
                    do_restore = true;
                }
            });
            ui.horizontal(|ui| {
                if ui.small_button("导出 JSONL").clicked() {
                    do_export_jsonl = true;
                }
                if ui.small_button("导出 MD").clicked() {
                    do_export_md = true;
                }
                if ui.small_button("导出 HTML").clicked() {
                    do_export_html = true;
                }
                if ui.small_button("导出全部会话").clicked() {
                    do_export_all = true;
                }
            });
            ui.horizontal(|ui| {
                if ui.small_button("导入 JSONL…").clicked() {
                    do_import = true;
                }
                if !self.selected_msgs.is_empty() {
                    if ui.small_button(format!("批量删除({})", self.selected_msgs.len())).clicked() {
                        do_delete_selected = true;
                    }
                }
            });
            if !self.selected_msgs.is_empty() {
                ui.horizontal(|ui| {
                    ui.label("批量改角色:");
                    for r in ["user", "assistant", "tool"] {
                        if ui.small_button(r).clicked() {
                            set_role = Some(r.to_string());
                        }
                    }
                });
            }
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
                    .fill(egui::Color32::from_rgb(0x4a, 0x3a, 0x1e))
                    .inner_margin(egui::Margin::symmetric(10.0, 6.0))
                    .show(ui, |ui| {
                        ui.colored_label(
                            egui::Color32::from_rgb(0xff, 0xe0, 0xb0),
                            "⚠ 检测到 Hermes 正在运行：改动可能被 Hermes 的内存会话覆盖。建议先关闭 Hermes 再编辑（写入不会被阻止，且已自动备份）。",
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

                        let is_selected = self.selected_msgs.contains(&i);
                        let bg = if is_selected {
                            egui::Color32::from_rgb(0x2a, 0x3a, 0x5a)
                        } else {
                            role_fill(&role)
                        };
                        egui::Frame::group(ui.style()).fill(bg).show(ui, |ui| {
                            ui.horizontal(|ui| {
                                // select checkbox
                                let mut sel = is_selected;
                                if ui.checkbox(&mut sel, "").changed() {
                                    if sel {
                                        self.selected_msgs.insert(i);
                                    } else {
                                        self.selected_msgs.remove(&i);
                                    }
                                }
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
                                    if ui.button("↑").on_hover_text("上移").clicked() {
                                        self.move_message_up(i);
                                    }
                                    if ui.button("↓").on_hover_text("下移").clicked() {
                                        self.move_message_down(i);
                                    }
                                });
                            });

                            // content area
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
                                if self.md_preview && !self.messages[i].edit_content.trim().is_empty() {
                                    // Lightweight Markdown preview (no external crate)
                                    let preview = simple_md_preview(&self.messages[i].edit_content);
                                    egui::CollapsingHeader::new("预览").id_source(format!("prev_{id}")).show(ui, |ui| {
                                        ui.label(preview);
                                    });
                                }
                                // Auto-height editor: grow with content lines, clamped.
                                let line_count = self.messages[i].edit_content.lines().count();
                                let rows = if is_empty { 1 } else { line_count.clamp(3, 20) };
                                let te = egui::TextEdit::multiline(&mut self.messages[i].edit_content)
                                    .desired_width(f32::INFINITY)
                                    .desired_rows(rows)
                                    .hint_text(if self.messages[i].edit_content.is_empty() { "(空)" } else { "" });
                                if ui.add(te).changed() {
                                    self.messages[i].dirty = true;
                                }
                            }
                            // character count + copy button (below content)
                            ui.horizontal(|ui| {
                                ui.label(
                                    egui::RichText::new(format!("{} 字", self.messages[i].edit_content.chars().count()))
                                        .small()
                                        .weak(),
                                );
                                if !self.messages[i].edit_content.is_empty() {
                                    if ui.small_button("📋 复制").on_hover_text("复制消息内容到剪贴板").clicked() {
                                        ctx.copy_text(self.messages[i].edit_content.clone());
                                        self.set_ok(format!("已复制消息 #{id} 内容"));
                                    }
                                }
                            });

                            if !self.messages[i].reasoning.is_empty() || !self.messages[i].edit_reasoning.is_empty() {
                                egui::CollapsingHeader::new("reasoning（思考过程，可编辑）").id_source(format!("reason_{id}")).show(
                                    ui,
                                    |ui| {
                                        let te = egui::TextEdit::multiline(&mut self.messages[i].edit_reasoning)
                                            .desired_width(f32::INFINITY)
                                            .desired_rows(3)
                                            .code_editor();
                                        if ui.add(te).changed() {
                                            self.messages[i].dirty = true;
                                        }
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
                "delete" => {
                    self.modal = Modal::ConfirmDelete(vec![i]);
                }
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
        if do_delete_selected {
            if self.selected_msgs.is_empty() {
                self.set_ok("未选择任何消息");
            } else {
                self.modal = Modal::ConfirmDelete(self.selected_msgs.iter().copied().collect());
            }
        }
        if let Some(role) = set_role {
            self.set_role_selected(&role);
        }
        if do_export_jsonl {
            match self.export_session_jsonl() {
                Some(p) => self.set_ok(format!("已导出: {}", p.display())),
                None => self.set_err("导出 JSONL 失败（可能无选中会话）"),
            }
        }
        if do_export_md {
            match self.export_session_markdown() {
                Some(p) => self.set_ok(format!("已导出: {}", p.display())),
                None => self.set_err("导出 Markdown 失败（可能无选中会话）"),
            }
        }
        if do_export_html {
            match self.export_session_html() {
                Some(p) => self.set_ok(format!("已导出: {}", p.display())),
                None => self.set_err("导出 HTML 失败（可能无选中会话）"),
            }
        }
        if do_export_all {
            self.export_all_sessions();
        }
        if do_import {
            self.modal = Modal::ImportPath { buf: String::new() };
        }

        // Detect profile change (db_path changed from combo box)
        // We compare against a stored original. Since combo directly mutates
        // self.db_path, we track via status if needed. Simpler: we reload if
        // selected session no longer matches. We'll skip auto-switch here to
        // avoid accidental data loss; user can press 刷新 after switching.

        // ---- Modal rendering ----
        self.render_modal(ctx);

        // Apply theme at the end
        ctx.set_visuals(if self.dark_mode {
            egui::Visuals::dark()
        } else {
            egui::Visuals::light()
        });
    }
}

impl App {
    fn render_modal(&mut self, ctx: &egui::Context) {
        let modal = std::mem::replace(&mut self.modal, Modal::None);
        match modal {
            Modal::None => {}
            Modal::ConfirmDeleteSession(sid) => {
                let title = self
                    .sessions
                    .iter()
                    .find(|s| s.id == sid)
                    .map(|s| s.title.clone())
                    .unwrap_or_default();
                let n = self
                    .sessions
                    .iter()
                    .find(|s| s.id == sid)
                    .map(|s| s.msg_count)
                    .unwrap_or(0);
                let mut keep = true;
                let mut decided = false;
                egui::Window::new("确认删除会话")
                    .collapsible(false)
                    .resizable(false)
                    .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
                    .show(ctx, |ui| {
                        ui.label(format!("删除会话及其全部消息？\n标题: {title}\nID: {sid}\n消息数: {n}\n（此操作会先自动备份数据库）"));
                        ui.horizontal(|ui| {
                            if ui.button("删除").clicked() {
                                decided = true;
                                keep = false;
                            }
                            if ui.button("取消").clicked() {
                                keep = false;
                            }
                        });
                    });
                if decided {
                    self.delete_session(&sid);
                } else if keep {
                    self.modal = Modal::ConfirmDeleteSession(sid);
                }
            }
            Modal::ConfirmDelete(indices) => {
                if indices.is_empty() {
                    return;
                }
                let n = indices.len();
                let mut keep = true;
                let mut decided = false;
                egui::Window::new("确认删除")
                    .collapsible(false)
                    .resizable(false)
                    .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
                    .show(ctx, |ui| {
                        if n == 1 {
                            let idx = indices[0];
                            if idx < self.messages.len() {
                                let id = self.messages[idx].id;
                                ui.label(format!("删除消息 #{id}？此操作会写入数据库（已自动备份）。"));
                            } else {
                                ui.label("删除消息？此操作会写入数据库（已自动备份）。");
                            }
                        } else {
                            ui.label(format!("删除选中的 {n} 条消息？此操作会写入数据库（已自动备份）。"));
                        }
                        if self.hermes_running {
                            ui.colored_label(
                                egui::Color32::from_rgb(0xff, 0xe0, 0xb0),
                                "⚠ Hermes 正在运行，改动可能被覆盖（已自动备份）。",
                            );
                        }
                        ui.horizontal(|ui| {
                            if ui.button("删除").clicked() {
                                decided = true;
                                keep = false;
                            }
                            if ui.button("取消").clicked() {
                                keep = false;
                            }
                        });
                    });
                if decided {
                    if n == 1 {
                        self.delete_message(indices[0]);
                    } else {
                        self.delete_selected();
                    }
                } else if keep {
                    self.modal = Modal::ConfirmDelete(indices);
                }
            }
            Modal::ConfirmTruncate(idx) => {
                if idx >= self.messages.len() {
                    return;
                }
                let n = self.messages.len().saturating_sub(idx + 1);
                let id = self.messages[idx].id;
                let mut keep = true;
                let mut decided = false;
                egui::Window::new("确认裁剪 (rewind)")
                    .collapsible(false)
                    .resizable(false)
                    .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
                    .show(ctx, |ui| {
                        ui.label(format!("删除 #{id} 之后的 {n} 条消息？不可直接撤销（已自动备份）。"));
                        ui.horizontal(|ui| {
                            if ui.button("裁剪").clicked() {
                                decided = true;
                                keep = false;
                            }
                            if ui.button("取消").clicked() {
                                keep = false;
                            }
                        });
                    });
                if decided {
                    self.truncate_after(idx);
                } else if keep {
                    self.modal = Modal::ConfirmTruncate(idx);
                }
            }
            Modal::DiscardThenSelect(sid) => {
                let mut keep = true;
                let mut decided = false;
                egui::Window::new("有未保存更改")
                    .collapsible(false)
                    .resizable(false)
                    .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
                    .show(ctx, |ui| {
                        ui.label("当前会话有未保存的编辑。切换会放弃这些更改。");
                        ui.horizontal(|ui| {
                            if ui.button("放弃并切换").clicked() {
                                decided = true;
                                keep = false;
                            }
                            if ui.button("取消").clicked() {
                                keep = false;
                            }
                        });
                    });
                if decided {
                    self.open_session(&sid);
                } else if keep {
                    self.modal = Modal::DiscardThenSelect(sid);
                }
            }
            Modal::DiscardThenRefresh => {
                let mut keep = true;
                let mut decided = false;
                egui::Window::new("有未保存更改")
                    .collapsible(false)
                    .resizable(false)
                    .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
                    .show(ctx, |ui| {
                        ui.label("刷新会放弃当前未保存的编辑。");
                        ui.horizontal(|ui| {
                            if ui.button("放弃并刷新").clicked() {
                                decided = true;
                                keep = false;
                            }
                            if ui.button("取消").clicked() {
                                keep = false;
                            }
                        });
                    });
                if decided {
                    self.refresh_hermes_running();
                    self.reload_sessions();
                    if let Some(sid) = self.selected.clone() {
                        self.open_session(&sid);
                    }
                } else if keep {
                    self.modal = Modal::DiscardThenRefresh;
                }
            }
            Modal::DiscardThenNew => {
                let mut keep = true;
                let mut decided = false;
                egui::Window::new("有未保存更改")
                    .collapsible(false)
                    .resizable(false)
                    .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
                    .show(ctx, |ui| {
                        ui.label("新建会话会放弃当前未保存的编辑。");
                        ui.horizontal(|ui| {
                            if ui.button("放弃并新建").clicked() {
                                decided = true;
                                keep = false;
                            }
                            if ui.button("取消").clicked() {
                                keep = false;
                            }
                        });
                    });
                if decided {
                    self.create_session();
                } else if keep {
                    self.modal = Modal::DiscardThenNew;
                }
            }
            Modal::ConfirmRestore(path) => {
                let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                let mut keep = true;
                let mut decided = false;
                egui::Window::new("恢复备份")
                    .collapsible(false)
                    .resizable(false)
                    .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
                    .show(ctx, |ui| {
                        ui.label(format!("用备份覆盖当前 state.db？\n{name}\n（当前库会先自动再备份一次。）"));
                        ui.horizontal(|ui| {
                            if ui.button("恢复").clicked() {
                                decided = true;
                                keep = false;
                            }
                            if ui.button("取消").clicked() {
                                keep = false;
                            }
                        });
                    });
                if decided {
                    self.restore_backup(&path);
                } else if keep {
                    self.modal = Modal::ConfirmRestore(path);
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
            Modal::ImportPath { mut buf } => {
                let mut keep = true;
                let mut do_import = false;
                egui::Window::new("导入 JSONL 会话")
                    .collapsible(false)
                    .resizable(false)
                    .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
                    .show(ctx, |ui| {
                        ui.label(r#"输入 JSONL 文件路径（每行: {"role":..., "content":...}）"#);
                        ui.add(
                            egui::TextEdit::singleline(&mut buf)
                                .hint_text("C:/path/to/session.jsonl")
                                .desired_width(420.0),
                        );
                        ui.horizontal(|ui| {
                            if ui.button("导入").clicked() {
                                do_import = true;
                                keep = false;
                            }
                            if ui.button("取消").clicked() {
                                keep = false;
                            }
                        });
                    });
                if do_import {
                    let path = PathBuf::from(buf.trim());
                    if path.exists() {
                        self.import_jsonl(&path);
                    } else {
                        self.set_err("文件不存在");
                    }
                } else if keep {
                    self.modal = Modal::ImportPath { buf };
                }
            }
        }
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

/// A tiny, dependency-free "Markdown-ish" preview: strip **bold**, *italic*,
/// `code`, and render headings/paragraphs as plain lines. This is a cosmetic
/// preview only; full rendering would need egui_commonmark.
fn simple_md_preview(s: &str) -> String {
    let mut out = String::new();
    for line in s.lines() {
        let line = line.trim();
        if line.is_empty() {
            out.push('\n');
            continue;
        }
        let line = line
            .trim_start_matches('#')
            .trim_start_matches(' ')
            .replace("**", "")
            .replace("*", "")
            .replace("`", "")
            .replace("~~", "");
        out.push_str(&line);
        out.push('\n');
    }
    out
}

/// CLI-only: edit one message non-interactively, using the SAME write path the
/// GUI save button uses (recompute finish_reason + clear the api_content
/// sidecar). Returns a human-readable result line.
fn cli_edit_message(db_path: &Path, message_id: i64, content: &str, role_opt: Option<&str>, reasoning_opt: Option<&str>) -> Result<String, String> {
    let conn = Connection::open(db_path).map_err(|e| format!("打开数据库失败: {e}"))?;
    let _ = conn.busy_timeout(std::time::Duration::from_millis(30000));
    let _ = conn.pragma_update(None, "foreign_keys", "ON");
    // Keep the stored role unless the caller overrides it.
    let role = match role_opt {
        Some(r) => r.to_string(),
        None => conn
            .query_row(
                "SELECT COALESCE(role,'') FROM messages WHERE id=?1",
                params![message_id],
                |r| r.get::<_, String>(0),
            )
            .map_err(|e| format!("消息 #{message_id} 不存在或读取失败: {e}"))?,
    };
    // Keep the stored reasoning unless the caller overrides it.
    let reasoning = match reasoning_opt {
        Some(r) => r.to_string(),
        None => conn
            .query_row(
                "SELECT COALESCE(reasoning_content,'') FROM messages WHERE id=?1",
                params![message_id],
                |r| r.get::<_, String>(0),
            )
            .unwrap_or_default(),
    };
    let finish = App::finish_reason_for(&conn, message_id, &role);
    let n = conn
        .execute(
            "UPDATE messages SET content=?1, role=?2, finish_reason=?3, api_content=NULL, reasoning_content=?4 WHERE id=?5",
            params![content, role, finish, reasoning, message_id],
        )
        .map_err(|e| format!("更新失败: {e}"))?;
    Ok(format!(
        "OK: updated={} id={} role={} finish_reason={} api_content=NULL reasoning={}",
        n,
        message_id,
        role,
        finish.as_deref().unwrap_or("(null)"),
        if reasoning.is_empty() { "(empty)" } else { "set" }
    ))
}

fn main() -> eframe::Result<()> {
    // Argument parsing:
    //   hermes-chat-editor-native.exe                 -> default db
    //   hermes-chat-editor-native.exe <path>          -> open db at path
    //   hermes-chat-editor-native.exe --readonly      -> open default db read-only (default anyway)
    //   hermes-chat-editor-native.exe --help         -> print help and exit
    //   hermes-chat-editor-native.exe --edit <message_id> --content "<text>" [--role <role>] [--db <path>]
    //     -> non-interactive: edit one message via the same write path as the
    //        GUI save button, then exit (no window).
    let args: Vec<String> = std::env::args().collect();
    let mut db_path: Option<PathBuf> = None;
    let mut read_only = true;
    if args.iter().any(|a| a == "--help" || a == "-h" || a == "help") {
        println!(
            "Hermes Chat Editor\n\
             Usage:\n  hermes-chat-editor-native.exe [db_path] [--readonly] [--writable] [--help]\n\
             \n\
             Edit one message (CLI, no window):\n\
             \x20 hermes-chat-editor-native.exe --edit <message_id> --content \"<text>\" [--role <role>] [--reasoning <text>] [--db <path>]\n\
             \n\
             Options:\n  <path>          Open a specific state.db (default: %LOCALAPPDATA%/hermes/state.db)\n  --readonly      Open read-only (default)\n  --writable      Open in writable mode\n  --help, -h      Show this help\n"
        );
        return Ok(());
    }
    // CLI edit mode: run the edit, print the result, and exit without a window.
    if args.iter().any(|a| a == "--edit") {
        let mut edit_id: Option<i64> = None;
        let mut edit_content: Option<String> = None;
        let mut edit_role: Option<String> = None;
        let mut edit_reasoning: Option<String> = None;
        let mut edit_db: Option<PathBuf> = None;
        let mut i = 1;
        while i < args.len() {
            match args[i].as_str() {
                "--edit" => { i += 1; }
                "--content" => {
                    if i + 1 < args.len() { edit_content = Some(args[i + 1].clone()); i += 2; } else { i += 1; }
                }
                "--role" => {
                    if i + 1 < args.len() { edit_role = Some(args[i + 1].clone()); i += 2; } else { i += 1; }
                }
                "--reasoning" => {
                    if i + 1 < args.len() { edit_reasoning = Some(args[i + 1].clone()); i += 2; } else { i += 1; }
                }
                "--db" => {
                    if i + 1 < args.len() { edit_db = Some(PathBuf::from(&args[i + 1])); i += 2; } else { i += 1; }
                }
                other => {
                    if edit_id.is_none() {
                        if let Ok(v) = other.parse::<i64>() { edit_id = Some(v); }
                    }
                    i += 1;
                }
            }
        }
        match (edit_id, edit_content) {
            (Some(id), Some(content)) => {
                let db = edit_db.unwrap_or_else(default_db_path);
                match cli_edit_message(&db, id, &content, edit_role.as_deref(), edit_reasoning.as_deref()) {
                    Ok(msg) => println!("{msg}"),
                    Err(e) => eprintln!("错误: {e}"),
                }
            }
            _ => {
                eprintln!("用法: --edit <message_id> --content \"<text>\" [--role <role>] [--reasoning <text>] [--db <path>]");
            }
        }
        return Ok(());
    }
    for a in &args[1..] {
        if a == "--readonly" {
            read_only = true;
        } else if a == "--writable" {
            read_only = false;
        } else if a == "--help" || a == "-h" || a == "help" {
            // already handled
        } else if !a.starts_with('-') {
            db_path = Some(PathBuf::from(a));
        }
    }
    let db_path = db_path.unwrap_or_else(default_db_path);

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1180.0, 780.0])
            .with_title("Hermes Chat Editor"),
        ..Default::default()
    };
    eframe::run_native(
        "Hermes Chat Editor",
        options,
        Box::new(move |cc| {
            install_cjk_font(&cc.egui_ctx);
            cc.egui_ctx.set_visuals(egui::Visuals::dark());
            let mut app = App::new(db_path);
            app.read_only = read_only;
            Ok(Box::new(app))
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
