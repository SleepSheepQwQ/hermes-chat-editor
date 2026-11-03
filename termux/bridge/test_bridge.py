#!/usr/bin/env python3
"""
End-to-end test for the Termux bridge, self-contained: builds a throwaway
state.db from tests/schema.sql, starts the bridge on a random port, exercises
every endpoint, then shuts everything down.

Run locally:   python3 termux/bridge/test_bridge.py -v
In CI:         same command on ubuntu-latest (stdlib only, no Termux needed).
"""
from __future__ import annotations

import argparse
import json
import os
import shutil
import socket
import sqlite3
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
from pathlib import Path

HERE = Path(__file__).resolve().parent
BRIDGE = HERE / "hermes_chat_bridge.py"
SCHEMA = HERE / "tests" / "schema.sql"


class Report:
    def __init__(self, verbose: bool):
        self.ok = 0
        self.failed = 0
        self.verbose = verbose

    def check(self, name: str, cond: bool, detail: str = "") -> None:
        if cond:
            self.ok += 1
            if self.verbose:
                print(f"  PASS  {name}")
        else:
            self.failed += 1
            print(f"  FAIL  {name}  {detail}")

    def summary(self) -> int:
        print(f"\n{self.ok} passed, {self.failed} failed")
        return 1 if self.failed else 0


def free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def build_db(path: Path) -> None:
    conn = sqlite3.connect(str(path))
    conn.executescript(SCHEMA.read_text())
    now = time.time()
    conn.execute(
        "INSERT INTO sessions(id,source,created_source,title,title_source,model,"
        "started_at,last_activity_at,message_count,tool_call_count,pinned,archived,profile_name)"
        " VALUES('seed_session','cli','cli','Seed','manual','test',?,?,2,0,0,0,'default')",
        (now, now),
    )
    for role, content in (("user", "hello"), ("assistant", "hi there")):
        conn.execute(
            "INSERT INTO messages(session_id,role,content,timestamp,active,compacted)"
            " VALUES('seed_session',?,?,?,1,0)",
            (role, content, now),
        )
    conn.commit()
    conn.close()


class Client:
    def __init__(self, base: str, token: str | None = None):
        self.base = base.rstrip("/")
        self.token = token

    def __call__(self, method: str, path: str, body=None, expect: int = 200):
        data = json.dumps(body).encode() if body is not None else None
        req = urllib.request.Request(self.base + path, data=data, method=method)
        req.add_header("Content-Type", "application/json")
        if self.token:
            req.add_header("X-Bridge-Token", self.token)
        try:
            with urllib.request.urlopen(req, timeout=10) as r:
                code, raw = r.status, r.read()
        except urllib.error.HTTPError as e:
            code, raw = e.code, e.read()
        try:
            payload = json.loads(raw) if raw else {}
        except json.JSONDecodeError:
            payload = raw.decode()
        assert code == expect, f"{method} {path} -> HTTP {code} (want {expect}) {str(payload)[:200]}"
        return payload


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("-v", "--verbose", action="store_true")
    ap.add_argument("--keep", action="store_true", help="keep the temp dir")
    ap.add_argument("--token", default=None, help="exercise token auth with this value")
    args = ap.parse_args()

    tmp = Path(tempfile.mkdtemp(prefix="hcb-"))
    db = tmp / "state.db"
    build_db(db)
    port = free_port()
    cmd = [sys.executable, str(BRIDGE), "--db", str(db), "--port", str(port),
           "--writable", "--force", "--no-token" if not args.token else "--token",
           *([] if not args.token else [args.token])]
    proc = subprocess.Popen(cmd, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
    base = f"http://127.0.0.1:{port}/api/v1"
    rep = Report(args.verbose)

    try:
        for _ in range(50):
            try:
                urllib.request.urlopen(base + "/health", timeout=1)
                break
            except Exception:
                if proc.poll() is not None:
                    print("bridge died on startup:\n" + (proc.stdout.read() or ""))
                    return 1
                time.sleep(0.1)

        api = Client(base)
        authed = Client(base, args.token) if args.token else api

        print("== health / selftest ==")
        h = api("GET", "/health")
        rep.check("health ok", h.get("ok") is True)
        rep.check("health writable", h.get("writable") is True)
        st = api("GET", "/selftest")
        rep.check("selftest ok", st.get("ok") is True, str(st)[:200])
        if args.token:
            rep.check("token required", Client(base)("GET", "/sessions", expect=401) == {"error": "bad or missing X-Bridge-Token", "status": 401})

        print("== sessions ==")
        sessions = authed("GET", "/sessions")
        rep.check("seed visible", any(s["id"] == "seed_session" for s in sessions))
        msgs = authed("GET", "/sessions/seed_session/messages")
        rep.check("seed messages", [m["content"] for m in msgs] == ["hello", "hi there"], str(msgs))

        print("== create / insert / order ==")
        created = authed("POST", "/sessions", {"title": "ci-session", "source": "cli"}, expect=201)
        sid = created["id"]
        rep.check("created", created["title"] == "ci-session")
        a = authed("POST", f"/sessions/{sid}/messages", {"role": "user", "content": "alpha"}, expect=201)
        b = authed("POST", f"/sessions/{sid}/messages", {"role": "assistant", "content": "beta"}, expect=201)
        authed("POST", f"/sessions/{sid}/messages",
               {"role": "user", "content": "gamma", "after_id": a["id"]}, expect=201)
        order = [m["content"] for m in authed("GET", f"/sessions/{sid}/messages")]
        rep.check("insert-after ordering", order == ["alpha", "gamma", "beta"], str(order))

        print("== edit keeps slot ==")
        edited = authed("PATCH", f"/messages/{b['id']}", {"content": "beta edited"})
        rep.check("edited", edited["content"] == "beta edited")
        order = [m["content"] for m in authed("GET", f"/sessions/{sid}/messages")]
        rep.check("edit kept position", order[-1] == "beta edited", str(order))

        print("== move ==")
        authed("POST", f"/messages/{b['id']}/move", {"direction": "up"})
        order = [m["content"] for m in authed("GET", f"/sessions/{sid}/messages")]
        rep.check("moved up", order.index("beta edited") < order.index("gamma"), str(order))
        authed("POST", f"/messages/{b['id']}/move", {"direction": "down"})

        print("== session patch ==")
        patched = authed("PATCH", f"/sessions/{sid}", {"title": "renamed", "pinned": 1})
        rep.check("patch title", patched["title"] == "renamed")
        rep.check("patch pinned", patched["pinned"] == 1)

        print("== export ==")
        for fmt in ("jsonl", "md", "html"):
            body = authed("GET", f"/sessions/{sid}/export?format={fmt}")
            rep.check(f"export {fmt}", isinstance(body, str) and len(body) > 30, str(body)[:80])

        print("== truncate / delete ==")
        authed("POST", f"/sessions/{sid}/truncate", {"after_id": a["id"]})
        left = authed("GET", f"/sessions/{sid}/messages")
        rep.check("truncate", len(left) == 1, f"left={len(left)}")
        authed("DELETE", f"/messages/{a['id']}")
        rep.check("delete message", authed("GET", f"/sessions/{sid}/messages") == [])
        authed("DELETE", f"/sessions/{sid}")
        rep.check("delete session", all(s["id"] != sid for s in authed("GET", "/sessions")))

        print("== backups ==")
        bkp = authed("POST", "/backups", expect=201)
        rep.check("backup listed", any(x["name"] == bkp["backup"] for x in authed("GET", "/backups")))

        print("== message_count sync ==")
        seed = [s for s in authed("GET", "/sessions") if s["id"] == "seed_session"][0]
        rep.check("count synced", seed["message_count"] == 2, str(seed["message_count"]))

        return rep.summary()
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()
        if not args.keep:
            shutil.rmtree(tmp, ignore_errors=True)
        else:
            print(f"kept: {tmp}")


if __name__ == "__main__":
    raise SystemExit(main())
