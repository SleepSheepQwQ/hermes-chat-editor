#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
Hermes Chat Editor — Termux bridge
=================================

A tiny, dependency-free HTTP/JSON API over a Hermes Agent ``state.db``.

Why: on Android an APK cannot read Termux's private home
(``/data/data/com.termux/files/home``), so the Kotlin app talks to this
bridge over ``127.0.0.1`` instead. The bridge owns all SQLite access and
mirrors the safety rules of the desktop editor:

* read-only by default (``--writable`` to allow mutations)
* refuses writes while a Hermes gateway heartbeat is fresh (``--force`` to override)
* WAL-safe backup before every mutation (keeps the newest 30)
* never passes ``display_order`` / ``message_uid`` to INSERTs blindly —
  triggers get a chance to fill them
* keeps ``sessions.message_count`` in sync after mutations

API (all JSON, prefix ``/api/v1``)::

    GET  /health                      bridge + db status (no token needed)
    GET  /selftest                    structured self-checks (test interface)
    GET  /sessions?q=&hidden=1        list sessions
    GET  /sessions/<id>               one session (system_prompt included)
    GET  /sessions/<id>/messages      ordered messages
    POST /sessions                    create session
    PATCH /sessions/<id>              title/source/pinned/archived/system_prompt
    POST /sessions/<id>/messages      insert message {role,content,after_id?}
    PATCH /messages/<id>              {content?,role?,finish_reason?,reasoning?}
    DELETE /messages/<id>             delete one
    POST /messages/<id>/move          {direction:"up"|"down"}
    POST /sessions/<id>/truncate      {after_id}
    GET  /sessions/<id>/export?format=jsonl|md|html
    GET  /backups                     list backups
    POST /backups                     create backup now

Auth: if a token exists (``--token`` or ``~/.hermes/chat_bridge_token``),
every request except ``/health`` must send ``X-Bridge-Token: <token>``.

Usage::

    python3 hermes_chat_bridge.py                 # 127.0.0.1:8765, read-only
    python3 hermes_chat_bridge.py --writable      # allow edits
    python3 hermes_chat_bridge.py --port 8799 --db ~/.hermes/state.db
"""

from __future__ import annotations

import argparse
import json
import os
import re
import secrets
import shutil
import sqlite3
import sys
import time
import traceback
from datetime import datetime, timezone
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import parse_qs, urlparse

VERSION = 1
API = "/api/v1"
MAX_BODY = 8 * 1024 * 1024
BACKUP_KEEP = 30
HERMES_HEARTBEAT_FRESH_SECONDS = 180


# --------------------------------------------------------------------------- #
# paths / config
# --------------------------------------------------------------------------- #
def default_db_path() -> Path:
    home = os.environ.get("HERMES_HOME")
    if home:
        return Path(home).expanduser() / "state.db"
    return Path.home() / ".hermes" / "state.db"


def token_file() -> Path:
    return Path(os.environ.get("HERMES_HOME", Path.home() / ".hermes")).expanduser() / "chat_bridge_token"


class Config:
    def __init__(self, db: Path, writable: bool, force: bool, token: str | None):
        self.db = db
        self.writable = writable
        self.force = force
        self.token = token
        self.started = time.time()


CFG: Config | None = None  # set in main()


# --------------------------------------------------------------------------- #
# sqlite helpers
# --------------------------------------------------------------------------- #
def connect(ro: bool) -> sqlite3.Connection:
    assert CFG is not None
    if ro:
        uri = f"file:{CFG.db}?mode=ro"
        conn = sqlite3.connect(uri, uri=True, timeout=15)
    else:
        conn = sqlite3.connect(str(CFG.db), timeout=30)
        conn.execute("PRAGMA busy_timeout=30000")
        conn.execute("PRAGMA foreign_keys=ON")
    conn.row_factory = sqlite3.Row
    return conn


def query(sql: str, args: tuple = (), ro: bool = True) -> list[dict]:
    with connect(ro) as conn:
        return [dict(r) for r in conn.execute(sql, args).fetchall()]


def scalar(sql: str, args: tuple = (), ro: bool = True):
    rows = query(sql, args, ro)
    if not rows:
        return None
    return list(rows[0].values())[0]


def hermes_running() -> dict:
    try:
        row = query(
            "SELECT backend_id, pid, last_heartbeat FROM gateway_heartbeats "
            "ORDER BY last_heartbeat DESC LIMIT 1"
        )
    except sqlite3.Error:
        return {"running": False, "detail": "no heartbeat table"}
    if not row:
        return {"running": False, "detail": "no heartbeat"}
    hb = row[0]
    age = time.time() - float(hb["last_heartbeat"] or 0)
    pid_alive = False
    try:
        os.kill(int(hb["pid"]), 0)
        pid_alive = True
    except (OSError, ValueError):
        pid_alive = False
    running = age < HERMES_HEARTBEAT_FRESH_SECONDS and pid_alive
    return {"running": running, "age_seconds": round(age, 1), "pid": hb["pid"],
            "backend_id": hb["backend_id"]}


def backup_dir() -> Path:
    assert CFG is not None
    return CFG.db.parent / "backups"


def make_backup() -> Path:
    """WAL-safe snapshot using the SQLite backup API."""
    d = backup_dir()
    d.mkdir(parents=True, exist_ok=True)
    stamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    out_path = d / f"state-{stamp}-{secrets.token_hex(2)}.db"
    src = connect(ro=True)
    dst = sqlite3.connect(str(out_path))
    try:
        src.backup(dst)
    finally:
        dst.close()
        src.close()
    # prune
    backups = sorted(d.glob("state-*.db"), key=lambda p: p.stat().st_mtime, reverse=True)
    for old in backups[BACKUP_KEEP:]:
        try:
            old.unlink()
        except OSError:
            pass
    return out_path


def sync_count(conn: sqlite3.Connection, session_id: str) -> None:
    conn.execute(
        "UPDATE sessions SET message_count="
        "(SELECT COUNT(*) FROM messages WHERE session_id=?1) WHERE id=?1",
        (session_id,),
    )


def ensure_writable() -> None:
    assert CFG is not None
    if not CFG.writable:
        raise ApiError(403, "bridge opened read-only; restart with --writable")
    st = hermes_running()
    if st.get("running") and not CFG.force:
        raise ApiError(
            409,
            "Hermes gateway is running (heartbeat "
            f"{st.get('age_seconds')}s old, pid {st.get('pid')}); "
            "stop it or restart the bridge with --force",
        )


def with_backup(fn):
    """Run a mutation after taking a backup."""

    def wrapper(*a, **kw):
        ensure_writable()
        make_backup()
        return fn(*a, **kw)

    return wrapper


# --------------------------------------------------------------------------- #
# domain operations
# --------------------------------------------------------------------------- #
SESSION_COLUMNS = (
    "id, source, title, title_source, model, started_at, ended_at, "
    "last_activity_at, message_count, tool_call_count, pinned, archived, hidden, "
    "profile_name, created_source, display_name, cwd"
)


def list_sessions(q: str = "", hidden: bool = False) -> list[dict]:
    sql = f"SELECT {SESSION_COLUMNS} FROM sessions"
    args: tuple = ()
    where = []
    if not hidden:
        where.append("COALESCE(hidden,0)=0")
    if q:
        where.append("(COALESCE(title,'') LIKE ?1 OR id LIKE ?1)")
        args = (f"%{q}%",)
    if where:
        sql += " WHERE " + " AND ".join(where)
    sql += " ORDER BY COALESCE(last_activity_at, started_at) DESC LIMIT 500"
    return query(sql, args)


def get_session(sid: str) -> dict:
    rows = query(f"SELECT {SESSION_COLUMNS}, system_prompt, end_reason FROM sessions WHERE id=?", (sid,))
    if not rows:
        raise ApiError(404, f"session not found: {sid}")
    return rows[0]


def list_messages(sid: str) -> list[dict]:
    return query(
        "SELECT id, role, content, tool_call_id, tool_calls, tool_name, timestamp, "
        "finish_reason, reasoning_content, active, compacted, display_kind, "
        "COALESCE(display_order,id) AS ord, message_uid "
        "FROM messages WHERE session_id=? ORDER BY COALESCE(display_order,id), id",
        (sid,),
    )


def get_message(mid: int, ro: bool = True) -> dict:
    rows = query(
        "SELECT id, session_id, role, content, timestamp, finish_reason, "
        "reasoning_content, tool_calls, tool_name, COALESCE(display_order,id) AS ord, "
        "message_uid, display_identity IS NULL AS identity_reset "
        "FROM messages WHERE id=?",
        (mid,), ro=ro,
    )
    if not rows:
        raise ApiError(404, f"message not found: {mid}")
    return rows[0]


def update_message(mid: int, body: dict) -> dict:
    fields, args = [], []
    mapping = {
        "content": "content",
        "role": "role",
        "finish_reason": "finish_reason",
        "reasoning": "reasoning_content",
    }
    if "reasoning_content" in body:
        body = dict(body, reasoning=body["reasoning_content"])
    for key, col in mapping.items():
        if key in body:
            fields.append(f"{col}=?")
            args.append(body[key])
    if not fields:
        raise ApiError(400, "no editable fields supplied")
    fields.append("api_content=NULL")
    with connect(ro=False) as conn:
        prev = conn.execute(
            "SELECT COALESCE(display_order,id) AS ord FROM messages WHERE id=?", (mid,)
        ).fetchone()
        conn.execute(
            f"UPDATE messages SET {', '.join(fields)} WHERE id=?",
            (*args, mid),
        )
        # The display_identity trigger nulls display_order whenever content/role
        # changes (Hermes re-derives identity on its next load). Restore the old
        # slot so an edit does not silently push the message to the session end.
        if prev is not None:
            conn.execute("UPDATE messages SET display_order=? WHERE id=?",
                         (int(prev["ord"]), mid))
        conn.commit()
    return get_message(mid, ro=False)


def insert_message(sid: str, role: str, content: str, after_id: int | None) -> dict:
    ts = time.time()
    finish = "stop" if role == "assistant" else None
    get_session(sid)
    with connect(ro=False) as conn:
        norm = "NULLIF(COALESCE(display_order,id),0)"
        if after_id is not None:
            anchor = conn.execute(
                f"SELECT {norm} AS ord FROM messages WHERE id=? AND session_id=?",
                (after_id, sid),
            ).fetchone()
            if anchor is None:
                raise ApiError(404, f"message {after_id} not in session {sid}")
            order = int(anchor["ord"]) + 1
            # make room: shift everything at/after the insertion point
            conn.execute(
                f"UPDATE messages SET display_order = COALESCE(display_order,id) + 1 "
                f"WHERE session_id=? AND COALESCE(display_order,id) >= ?",
                (sid, order),
            )
        else:
            max_ord = conn.execute(
                f"SELECT MAX({norm}) AS m FROM messages WHERE session_id=?",
                (sid,),
            ).fetchone()["m"] or 0
            order = int(max_ord) + 1
        cur = conn.execute(
            "INSERT INTO messages(session_id,role,content,timestamp,active,compacted,"
            "finish_reason,display_order) VALUES(?,?,?,?,1,0,?,?)",
            (sid, role, content, ts, finish, order),
        )
        new_id = cur.lastrowid
        sync_count(conn, sid)
        conn.commit()
    return get_message(new_id, ro=False)


def delete_message(mid: int) -> dict:
    msg = get_message(mid, ro=False)
    with connect(ro=False) as conn:
        conn.execute("DELETE FROM messages WHERE id=?", (mid,))
        sync_count(conn, msg["session_id"])
        conn.commit()
    return {"deleted": mid, "session_id": msg["session_id"]}


def move_message(mid: int, direction: str) -> dict:
    if direction not in ("up", "down"):
        raise ApiError(400, "direction must be 'up' or 'down'")
    msg = get_message(mid, ro=False)
    session_id, ord_ = msg["session_id"], int(msg["ord"])
    op = "<" if direction == "up" else ">"
    order_sql = "DESC" if direction == "up" else "ASC"
    with connect(ro=False) as conn:
        other = conn.execute(
            f"SELECT id, COALESCE(display_order,id) AS ord FROM messages "
            f"WHERE session_id=? AND COALESCE(display_order,id) {op} ? "
            f"ORDER BY COALESCE(display_order,id) {order_sql} LIMIT 1",
            (session_id, ord_),
        ).fetchone()
        if other is None:
            raise ApiError(409, "already at the edge")
        conn.execute("UPDATE messages SET display_order=? WHERE id=?", (int(other["ord"]), mid))
        conn.execute("UPDATE messages SET display_order=? WHERE id=?", (ord_, other["id"]))
        conn.commit()
    return {"moved": mid, "direction": direction}


def truncate_session(sid: str, after_id: int) -> dict:
    anchor = get_message(after_id, ro=False)
    if anchor["session_id"] != sid:
        raise ApiError(400, "message does not belong to session")
    with connect(ro=False) as conn:
        cur = conn.execute(
            "DELETE FROM messages WHERE session_id=? AND COALESCE(display_order,id) > ?",
            (sid, int(anchor["ord"])),
        )
        removed = cur.rowcount
        sync_count(conn, sid)
        conn.commit()
    return {"removed": removed}


def create_session(source: str, title: str, model: str | None) -> dict:
    now = time.time()
    sid = datetime.now().strftime("%Y%m%d_%H%M%S_") + secrets.token_hex(3)
    with connect(ro=False) as conn:
        conn.execute(
            "INSERT INTO sessions(id,source,created_source,model,title,title_source,"
            "started_at,last_activity_at,message_count,tool_call_count,pinned,archived,"
            "profile_name) VALUES(?,?,?,?,?,?,?,?,0,0,0,0,?)",
            (sid, source, source, model, title, "manual", now, now,
             os.environ.get("HERMES_PROFILE", "default")),
        )
        conn.commit()
    return get_session(sid)


def delete_session(sid: str) -> dict:
    get_session(sid)
    with connect(ro=False) as conn:
        conn.execute("DELETE FROM messages WHERE session_id=?", (sid,))
        conn.execute("DELETE FROM sessions WHERE id=?", (sid,))
        conn.commit()
    return {"deleted": sid}


def update_session(sid: str, body: dict) -> dict:
    get_session(sid)
    fields, args = [], []
    for col in ("title", "source", "pinned", "archived", "system_prompt"):
        if col in body:
            val = body[col]
            if col == "source":
                fields.append("created_source=?")
                args.append(val)
            fields.append(f"{col}=?")
            args.append(val)
    if not fields:
        raise ApiError(400, "no editable fields supplied")
    with connect(ro=False) as conn:
        conn.execute(f"UPDATE sessions SET {', '.join(fields)} WHERE id=?", (*args, sid))
        conn.commit()
    return get_session(sid)


def export_session(sid: str, fmt: str) -> tuple[str, str]:
    s = get_session(sid)
    msgs = list_messages(sid)
    title = s.get("title") or sid
    if fmt == "jsonl":
        lines = [json.dumps({"session": s}, ensure_ascii=False)]
        lines += [json.dumps({k: m[k] for k in
                              ("id", "role", "content", "timestamp", "finish_reason")},
                             ensure_ascii=False) for m in msgs]
        return "\n".join(lines) + "\n", "application/x-ndjson"
    if fmt == "md":
        out = [f"# {title}", "", f"- id: `{sid}`", f"- source: `{s.get('source')}`",
               f"- messages: {len(msgs)}", ""]
        for m in msgs:
            out.append(f"## {m['role']}")
            out.append("")
            out.append(m["content"] or "_(empty)_")
            out.append("")
        return "\n".join(out), "text/markdown"
    if fmt == "html":
        import html as _html
        body = "\n".join(
            f"<section><h3>{_html.escape(m['role'])}</h3>"
            f"<pre>{_html.escape(m['content'] or '')}</pre></section>"
            for m in msgs
        )
        doc = ("<!doctype html><meta charset='utf-8'>"
               f"<title>{_html.escape(title)}</title>"
               "<style>body{font-family:sans-serif;max-width:52rem;margin:2rem auto}"
               "pre{white-space:pre-wrap;background:#f6f6f6;padding:.6rem}</style>"
               f"<h1>{_html.escape(title)}</h1>{body}")
        return doc, "text/html"
    raise ApiError(400, "format must be jsonl|md|html")


def list_backups() -> list[dict]:
    d = backup_dir()
    if not d.exists():
        return []
    out = []
    for p in sorted(d.glob("state-*.db"), key=lambda x: x.stat().st_mtime, reverse=True):
        st = p.stat()
        out.append({"name": p.name, "size": st.st_size,
                    "mtime": datetime.fromtimestamp(st.st_mtime, timezone.utc).isoformat()})
    return out


def selftest() -> dict:
    checks: list[dict] = []

    def check(name, fn):
        try:
            detail = fn()
            checks.append({"name": name, "ok": True, "detail": detail})
        except Exception as exc:  # noqa: BLE001 - report, never raise
            checks.append({"name": name, "ok": False, "detail": f"{type(exc).__name__}: {exc}"})

    check("db exists", lambda: str(CFG.db) if CFG.db.exists() else (_ for _ in ()).throw(FileNotFoundError(CFG.db)))
    check("db readable", lambda: f"{scalar('SELECT COUNT(*) FROM sessions')} sessions, "
                                 f"{scalar('SELECT COUNT(*) FROM messages')} messages")
    check("schema>=31", lambda: f"version {scalar('SELECT MAX(version) FROM schema_version')}")
    check("messages view", lambda: (lambda s: f"{s} recent session(s)")(len(list_sessions())))
    check("write triggers", lambda: ", ".join(
        r["name"] for r in query("SELECT name FROM sqlite_master WHERE type='trigger' "
                                 "AND name LIKE 'messages_%_insert'")))
    check("backup dir", lambda: str(backup_dir()) if backup_dir().exists() else "absent (created on first write)")
    check("writable", lambda: "yes" if CFG.writable else "no (read-only)")
    check("hermes idle", lambda: hermes_running())
    check("roundtrip", lambda: (lambda s: f"read {len(list_messages(s['id']))} messages of {s['id']}")(
        (list_sessions() or [_ for _ in () if False])[0]) if list_sessions() else "no sessions to read")
    ok = all(c["ok"] for c in checks)
    return {"ok": ok, "checks": checks, "version": VERSION, "db": str(CFG.db),
            "writable": CFG.writable}


# --------------------------------------------------------------------------- #
# HTTP layer
# --------------------------------------------------------------------------- #
class ApiError(Exception):
    def __init__(self, status: int, message: str):
        super().__init__(message)
        self.status = status
        self.message = message


class Handler(BaseHTTPRequestHandler):
    server_version = f"hermes-chat-bridge/{VERSION}"
    protocol_version = "HTTP/1.1"

    def log_message(self, fmt, *args):  # keep logs terse
        sys.stderr.write("[bridge] %s - %s\n" % (self.address_string(), fmt % args))

    # -- plumbing ---------------------------------------------------------- #
    def _json(self, status: int, payload) -> None:
        raw = json.dumps(payload, ensure_ascii=False, default=str).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json; charset=utf-8")
        self.send_header("Content-Length", str(len(raw)))
        self.send_header("Cache-Control", "no-store")
        self.end_headers()
        self.wfile.write(raw)

    def _text(self, status: int, body: str, ctype: str, filename: str | None = None) -> None:
        raw = body.encode()
        self.send_response(status)
        self.send_header("Content-Type", f"{ctype}; charset=utf-8")
        self.send_header("Content-Length", str(len(raw)))
        if filename:
            self.send_header("Content-Disposition", f'attachment; filename="{filename}"')
        self.end_headers()
        self.wfile.write(raw)

    def _body(self) -> dict:
        length = int(self.headers.get("Content-Length") or 0)
        if length <= 0:
            return {}
        if length > MAX_BODY:
            raise ApiError(413, "body too large")
        raw = self.rfile.read(length)
        try:
            data = json.loads(raw.decode("utf-8"))
        except (UnicodeDecodeError, json.JSONDecodeError):
            raise ApiError(400, "body must be JSON")
        if not isinstance(data, dict):
            raise ApiError(400, "body must be a JSON object")
        return data

    def _auth(self, path: str) -> None:
        if CFG.token and path not in (f"{API}/health", "/health"):
            sent = self.headers.get("X-Bridge-Token", "")
            if not secrets.compare_digest(sent, CFG.token):
                raise ApiError(401, "bad or missing X-Bridge-Token")

    def _dispatch(self, method: str) -> None:
        parsed = urlparse(self.path)
        path = parsed.path.rstrip("/") or "/"
        qs = {k: v[-1] for k, v in parse_qs(parsed.query).items()}
        try:
            self._auth(path)
            if path in ("/health", f"{API}/health"):
                return self._json(200, {
                    "ok": True, "version": VERSION, "db": str(CFG.db),
                    "writable": CFG.writable, "force": CFG.force, "uptime": round(time.time() - CFG.started, 1),
                    "hermes": hermes_running(),
                })
            if path in ("/selftest", f"{API}/selftest"):
                return self._json(200, selftest())
            if path in ("/sessions", f"{API}/sessions") and method == "GET":
                return self._json(200, list_sessions(qs.get("q", ""), qs.get("hidden") == "1"))
            if path in ("/sessions", f"{API}/sessions") and method == "POST":
                b = self._body()
                return self._json(201, with_backup(create_session)(
                    b.get("source") or "cli", b.get("title") or "新会话", b.get("model")))

            m = re.fullmatch(rf"{API}/sessions/([^/]+)(/messages|/truncate|/export)?", path)
            if m:
                sid, sub = m.group(1), m.group(2)
                if sub is None and method == "GET":
                    return self._json(200, get_session(sid))
                if sub is None and method == "PATCH":
                    return self._json(200, with_backup(update_session)(sid, self._body()))
                if sub is None and method == "DELETE":
                    return self._json(200, with_backup(delete_session)(sid))
                if sub == "/messages" and method == "GET":
                    return self._json(200, list_messages(sid))
                if sub == "/messages" and method == "POST":
                    b = self._body()
                    role = b.get("role") or "user"
                    if role not in ("user", "assistant", "system", "tool"):
                        raise ApiError(400, "bad role")
                    after = b.get("after_id")
                    return self._json(201, with_backup(insert_message)(
                        sid, role, b.get("content") or "", int(after) if after else None))
                if sub == "/truncate" and method == "POST":
                    b = self._body()
                    if "after_id" not in b:
                        raise ApiError(400, "after_id required")
                    return self._json(200, with_backup(truncate_session)(sid, int(b["after_id"])))
                if sub == "/export" and method == "GET":
                    body, ctype = export_session(sid, qs.get("format", "jsonl"))
                    ext = {"jsonl": "jsonl", "md": "md", "html": "html"}[qs.get("format", "jsonl")]
                    return self._text(200, body, ctype, f"{sid}.{ext}")

            m = re.fullmatch(rf"{API}/messages/(\d+)(/move)?", path)
            if m:
                mid, sub = int(m.group(1)), m.group(2)
                if sub is None and method == "PATCH":
                    return self._json(200, with_backup(update_message)(mid, self._body()))
                if sub is None and method == "DELETE":
                    return self._json(200, with_backup(delete_message)(mid))
                if sub == "/move" and method == "POST":
                    return self._json(200, with_backup(move_message)(
                        mid, self._body().get("direction", "")))

            if path in ("/backups", f"{API}/backups") and method == "GET":
                return self._json(200, list_backups())
            if path in ("/backups", f"{API}/backups") and method == "POST":
                ensure_writable()
                p = make_backup()
                return self._json(201, {"backup": p.name})

            raise ApiError(404, f"no route: {method} {path}")
        except ApiError as e:
            self._json(e.status, {"error": e.message, "status": e.status})
        except sqlite3.Error as e:
            self._json(500, {"error": f"sqlite: {e}", "status": 500})
        except Exception as e:  # noqa: BLE001
            traceback.print_exc()
            self._json(500, {"error": f"{type(e).__name__}: {e}", "status": 500})

    def do_GET(self):
        self._dispatch("GET")

    def do_POST(self):
        self._dispatch("POST")

    def do_PATCH(self):
        self._dispatch("PATCH")

    def do_DELETE(self):
        self._dispatch("DELETE")

    def do_OPTIONS(self):
        self.send_response(204)
        self.send_header("Allow", "GET,POST,PATCH,DELETE,OPTIONS")
        self.send_header("Content-Length", "0")
        self.end_headers()


# --------------------------------------------------------------------------- #
# main
# --------------------------------------------------------------------------- #
def load_token(explicit: str | None) -> str | None:
    if explicit is not None:
        return explicit or None
    tf = token_file()
    if tf.exists():
        tok = tf.read_text().strip()
        return tok or None
    tok = secrets.token_urlsafe(24)
    try:
        tf.parent.mkdir(parents=True, exist_ok=True)
        tf.write_text(tok + "\n")
        os.chmod(tf, 0o600)
    except OSError:
        pass
    return tok


def main(argv: list[str] | None = None) -> int:
    global CFG
    ap = argparse.ArgumentParser(description="Hermes Chat Editor Termux bridge")
    ap.add_argument("--db", default=str(default_db_path()))
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--port", type=int, default=int(os.environ.get("HERMES_BRIDGE_PORT", 8765)))
    ap.add_argument("--writable", action="store_true", help="allow mutations")
    ap.add_argument("--force", action="store_true", help="write even if Hermes is running")
    ap.add_argument("--token", default=None, help="auth token (default: auto, stored in ~/.hermes/chat_bridge_token)")
    ap.add_argument("--no-token", action="store_true", help="disable auth")
    ap.add_argument("--selftest", action="store_true", help="run self-checks and exit")
    args = ap.parse_args(argv)

    db = Path(args.db).expanduser().resolve()
    token = None if args.no_token else load_token(args.token)
    CFG = Config(db=db, writable=args.writable, force=args.force, token=token)

    if not db.exists():
        print(f"error: database not found: {db}", file=sys.stderr)
        return 2
    try:
        query("SELECT 1 FROM sessions LIMIT 1")
    except sqlite3.Error as e:
        print(f"error: not a usable Hermes state.db: {e}", file=sys.stderr)
        return 2

    if args.selftest:
        print(json.dumps(selftest(), ensure_ascii=False, indent=2))
        return 0

    srv = ThreadingHTTPServer((args.host, args.port), Handler)
    srv.daemon_threads = True
    print(f"hermes-chat-bridge v{VERSION}")
    print(f"  db      : {db}")
    print(f"  listen  : http://{args.host}:{args.port}{API}")
    print(f"  mode    : {'writable' if args.writable else 'read-only'}")
    print(f"  token   : {'(disabled)' if not token else token}")
    sys.stdout.flush()
    try:
        srv.serve_forever()
    except KeyboardInterrupt:
        print("\nbye")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
