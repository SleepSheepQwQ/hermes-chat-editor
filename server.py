#!/usr/bin/env python3
"""
Hermes Chat Editor — a local, Chatbox-style editor for Hermes session history.

Reads and edits the Hermes `state.db` SQLite session store directly:
  - list / search sessions
  - view a session's messages
  - edit message content, role, reasoning
  - add / insert / delete messages (rewind-style truncation)
  - rename / pin / unpin / set-source / archive a session
  - create a brand-new session from scratch (prefill fabrication)

Zero third-party dependencies — stdlib http.server + sqlite3 only.

SAFETY
  * Every mutating request snapshots state.db into ./backups/ first.
  * Refuses to write while another process holds a write lock, unless --force.
  * Binds to 127.0.0.1 only. No auth — do not expose this port.

Usage:
  python server.py --db "C:/Users/you/AppData/Local/hermes/state.db" --port 8787
"""
from __future__ import annotations

import argparse
import json
import os
import shutil
import sqlite3
import time
import uuid
import webbrowser
from datetime import datetime, timezone
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import urlparse, parse_qs

HERE = Path(__file__).resolve().parent
STATIC = HERE / "static"
BACKUP_DIR = HERE / "backups"

# Columns we let the UI edit on a message row. The FTS triggers in state.db
# keep messages_fts in sync automatically on UPDATE/INSERT/DELETE.
EDITABLE_MSG_COLS = ("role", "content", "reasoning_content", "finish_reason")
# Internal sources Hermes hides from human session pickers.
INTERNAL_LISTING_SOURCES = ("kanban", "tool", "oneshot")


class DB:
    def __init__(self, path: str):
        self.path = path

    def connect(self, write: bool = False) -> sqlite3.Connection:
        con = sqlite3.connect(self.path, timeout=5)
        con.row_factory = sqlite3.Row
        con.execute("PRAGMA foreign_keys=ON")
        return con

    def backup(self) -> str:
        BACKUP_DIR.mkdir(exist_ok=True)
        stamp = datetime.now().strftime("%Y%m%d_%H%M%S_%f")
        dst = BACKUP_DIR / f"state.db.{stamp}.bak"
        # Use sqlite backup API so WAL content is captured consistently.
        src = sqlite3.connect(self.path, timeout=5)
        out = sqlite3.connect(str(dst))
        with out:
            src.backup(out)
        src.close()
        out.close()
        # Keep only the 30 newest backups.
        baks = sorted(BACKUP_DIR.glob("state.db.*.bak"))
        for old in baks[:-30]:
            try:
                old.unlink()
            except OSError:
                pass
        return str(dst)

    # ---- reads ----------------------------------------------------------
    def list_sessions(self, q: str | None, show_internal: bool, limit: int = 200):
        con = self.connect()
        try:
            where, args = [], []
            if not show_internal:
                qmarks = ",".join("?" * len(INTERNAL_LISTING_SOURCES))
                where.append(f"COALESCE(source,'') NOT IN ({qmarks})")
                args += list(INTERNAL_LISTING_SOURCES)
            if q:
                where.append("(COALESCE(title,'') LIKE ? OR id LIKE ?)")
                args += [f"%{q}%", f"%{q}%"]
            sql = (
                "SELECT id,title,source,pinned,archived,message_count,model,"
                "started_at,last_activity_at FROM sessions"
            )
            if where:
                sql += " WHERE " + " AND ".join(where)
            sql += " ORDER BY COALESCE(last_activity_at, started_at) DESC LIMIT ?"
            args.append(limit)
            return [dict(r) for r in con.execute(sql, args)]
        finally:
            con.close()

    def session_detail(self, sid: str):
        con = self.connect()
        try:
            s = con.execute("SELECT * FROM sessions WHERE id=?", (sid,)).fetchone()
            if not s:
                return None
            msgs = con.execute(
                "SELECT id,role,content,reasoning_content,finish_reason,tool_name,"
                "tool_calls,tool_call_id,timestamp,active,compacted FROM messages "
                "WHERE session_id=? ORDER BY COALESCE(display_order, id), id",
                (sid,),
            ).fetchall()
            return {"session": dict(s), "messages": [dict(m) for m in msgs]}
        finally:
            con.close()

    # ---- writes ---------------------------------------------------------
    def update_message(self, mid: int, fields: dict):
        sets, args = [], []
        for k, v in fields.items():
            if k not in EDITABLE_MSG_COLS:
                raise ValueError(f"field not editable: {k}")
            sets.append(f"{k}=?")
            args.append(v)
        if not sets:
            return 0
        args.append(mid)
        con = self.connect(write=True)
        try:
            with con:
                cur = con.execute(
                    f"UPDATE messages SET {', '.join(sets)} WHERE id=?", args
                )
            return cur.rowcount
        finally:
            con.close()

    def delete_message(self, mid: int):
        con = self.connect(write=True)
        try:
            with con:
                sid = con.execute(
                    "SELECT session_id FROM messages WHERE id=?", (mid,)
                ).fetchone()
                con.execute("DELETE FROM messages WHERE id=?", (mid,))
                if sid:
                    self._recount(con, sid["session_id"])
            return True
        finally:
            con.close()

    def add_message(self, sid: str, role: str, content: str,
                    reasoning: str = "", after_id: int | None = None):
        con = self.connect(write=True)
        try:
            with con:
                # display_order: place after the given row, else at the end.
                if after_id is not None:
                    base = con.execute(
                        "SELECT COALESCE(display_order,id) d FROM messages WHERE id=?",
                        (after_id,),
                    ).fetchone()
                    order = (base["d"] if base else 0) + 0.5
                else:
                    mx = con.execute(
                        "SELECT MAX(COALESCE(display_order,id)) d FROM messages "
                        "WHERE session_id=?",
                        (sid,),
                    ).fetchone()
                    order = (mx["d"] or 0) + 1
                con.execute(
                    "INSERT INTO messages(session_id,role,content,reasoning_content,"
                    "timestamp,active,compacted,finish_reason,display_order,message_uid) "
                    "VALUES(?,?,?,?,?,?,?,?,?,?)",
                    (
                        sid, role, content, reasoning or None, time.time(),
                        1, 0, "stop" if role == "assistant" else None,
                        order, uuid.uuid4().hex,
                    ),
                )
                self._recount(con, sid)
            return True
        finally:
            con.close()

    def truncate_after(self, sid: str, mid: int):
        """Rewind: drop every message ordered after `mid` in this session."""
        con = self.connect(write=True)
        try:
            with con:
                base = con.execute(
                    "SELECT COALESCE(display_order,id) d FROM messages WHERE id=?",
                    (mid,),
                ).fetchone()
                if not base:
                    return 0
                cur = con.execute(
                    "DELETE FROM messages WHERE session_id=? AND "
                    "COALESCE(display_order,id) > ?",
                    (sid, base["d"]),
                )
                self._recount(con, sid)
            return cur.rowcount
        finally:
            con.close()

    def update_session(self, sid: str, fields: dict):
        allowed = {"title", "source", "created_source", "pinned", "archived"}
        sets, args = [], []
        for k, v in fields.items():
            if k not in allowed:
                raise ValueError(f"field not editable: {k}")
            sets.append(f"{k}=?")
            args.append(v)
        if not sets:
            return 0
        sets.append("last_activity_at=?")
        args.append(time.time())
        args.append(sid)
        con = self.connect(write=True)
        try:
            with con:
                cur = con.execute(
                    f"UPDATE sessions SET {', '.join(sets)} WHERE id=?", args
                )
            return cur.rowcount
        finally:
            con.close()

    def create_session(self, title: str, source: str, model: str):
        sid = datetime.now().strftime("%Y%m%d_%H%M%S_") + uuid.uuid4().hex[:6]
        con = self.connect(write=True)
        try:
            with con:
                con.execute(
                    "INSERT INTO sessions(id,source,created_source,model,title,"
                    "title_source,started_at,last_activity_at,message_count,"
                    "tool_call_count,pinned,archived,profile_name) "
                    "VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?)",
                    (sid, source, source, model, title, "manual",
                     time.time(), time.time(), 0, 0, 1, 0, "default"),
                )
            return sid
        finally:
            con.close()

    @staticmethod
    def _recount(con, sid):
        n = con.execute(
            "SELECT COUNT(*) c FROM messages WHERE session_id=?", (sid,)
        ).fetchone()["c"]
        con.execute("UPDATE sessions SET message_count=? WHERE id=?", (n, sid))


# --------------------------------------------------------------------------
DBH: DB | None = None
ALLOW_FORCE = False


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *a):  # quiet
        pass

    def _send(self, code: int, body: bytes, ctype="application/json"):
        self.send_response(code)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        # Never let a browser serve a stale app.js/css — this tool changes fast.
        self.send_header("Cache-Control", "no-store, no-cache, must-revalidate")
        self.send_header("Pragma", "no-cache")
        self.end_headers()
        self.wfile.write(body)

    def _json(self, obj, code=200):
        self._send(code, json.dumps(obj, ensure_ascii=False).encode("utf-8"))

    def _err(self, msg, code=400):
        self._json({"error": msg}, code)

    def _read_body(self):
        n = int(self.headers.get("Content-Length") or 0)
        if not n:
            return {}
        return json.loads(self.rfile.read(n).decode("utf-8"))

    # ---- routing --------------------------------------------------------
    def do_GET(self):
        u = urlparse(self.path)
        path, qs = u.path, parse_qs(u.query)
        if path == "/" or path == "/index.html":
            return self._static("index.html", "text/html; charset=utf-8")
        if path == "/app.js":
            return self._static("app.js", "application/javascript; charset=utf-8")
        if path == "/icons.js":
            return self._static("icons.js", "application/javascript; charset=utf-8")
        if path == "/style.css":
            return self._static("style.css", "text/css; charset=utf-8")
        if path == "/api/sessions":
            q = (qs.get("q") or [None])[0]
            show = (qs.get("internal") or ["0"])[0] == "1"
            return self._json(DBH.list_sessions(q, show))
        if path == "/api/session":
            sid = (qs.get("id") or [None])[0]
            if not sid:
                return self._err("missing id")
            d = DBH.session_detail(sid)
            return self._json(d) if d else self._err("not found", 404)
        return self._err("not found", 404)

    def do_POST(self):
        u = urlparse(self.path)
        try:
            body = self._read_body()
        except Exception as e:
            return self._err(f"bad json: {e}")
        try:
            if u.path != "/api/create_session":
                bak = DBH.backup()
            else:
                bak = DBH.backup()
        except sqlite3.OperationalError as e:
            if not ALLOW_FORCE:
                return self._err(
                    f"state.db is locked by another process ({e}). "
                    "Close the Hermes gateway/desktop or restart with --force.",
                    423,
                )
            bak = None
        try:
            return self._dispatch(u.path, body, bak)
        except sqlite3.OperationalError as e:
            return self._err(f"db locked / write failed: {e}", 423)
        except ValueError as e:
            return self._err(str(e))
        except Exception as e:
            return self._err(f"server error: {e}", 500)

    def _dispatch(self, path, body, bak):
        if path == "/api/update_message":
            n = DBH.update_message(int(body["id"]), body.get("fields", {}))
            return self._json({"updated": n, "backup": bak})
        if path == "/api/delete_message":
            DBH.delete_message(int(body["id"]))
            return self._json({"ok": True, "backup": bak})
        if path == "/api/add_message":
            DBH.add_message(
                body["session_id"], body["role"], body.get("content", ""),
                body.get("reasoning", ""),
                int(body["after_id"]) if body.get("after_id") else None,
            )
            return self._json({"ok": True, "backup": bak})
        if path == "/api/truncate_after":
            n = DBH.truncate_after(body["session_id"], int(body["id"]))
            return self._json({"deleted": n, "backup": bak})
        if path == "/api/update_session":
            n = DBH.update_session(body["id"], body.get("fields", {}))
            return self._json({"updated": n, "backup": bak})
        if path == "/api/create_session":
            sid = DBH.create_session(
                body.get("title", "新建会话"),
                body.get("source", "cli"),
                body.get("model", "manual"),
            )
            return self._json({"id": sid, "backup": bak})
        return self._err("not found", 404)

    def _static(self, name, ctype):
        p = STATIC / name
        if not p.exists():
            return self._err("missing static file: " + name, 404)
        self._send(200, p.read_bytes(), ctype)


def main():
    global DBH, ALLOW_FORCE
    default_db = os.path.join(
        os.environ.get("LOCALAPPDATA", str(Path.home())), "hermes", "state.db"
    )
    ap = argparse.ArgumentParser()
    ap.add_argument("--db", default=default_db, help="path to Hermes state.db")
    ap.add_argument("--port", type=int, default=8787)
    ap.add_argument("--force", action="store_true",
                    help="write even if state.db appears locked (risky)")
    ap.add_argument("--no-browser", action="store_true")
    args = ap.parse_args()

    if not Path(args.db).exists():
        raise SystemExit(f"state.db not found: {args.db}")
    DBH = DB(args.db)
    ALLOW_FORCE = args.force

    srv = ThreadingHTTPServer(("127.0.0.1", args.port), Handler)
    url = f"http://127.0.0.1:{args.port}/"
    print(f"Hermes Chat Editor → {url}")
    print(f"  DB: {args.db}")
    print(f"  Backups: {BACKUP_DIR}")
    print("  Ctrl+C to stop.")
    if not args.no_browser:
        try:
            webbrowser.open(url)
        except Exception:
            pass
    try:
        srv.serve_forever()
    except KeyboardInterrupt:
        print("\nbye")


if __name__ == "__main__":
    main()
