# Hermes Chat Editor

A local, [Chatbox](https://github.com/Bin-Huang/chatbox)-inspired editor for your
**Hermes Agent** conversation history. It opens the Hermes `state.db` SQLite
session store directly and lets you view, edit, rewind, fabricate and re-tag
conversations from a browser UI — the "editable chat log / prefill" workflow
Chatbox popularized, applied to Hermes sessions.

> Single-file Python stdlib server + a tiny vanilla-JS front end. No pip install,
> no framework, no telemetry. Runs entirely on `127.0.0.1`.

## What it does

- **Browse & search** every session in `state.db` (title / id).
- **Reveal internal sessions** — Hermes hides `oneshot` / `tool` / `kanban`
  sources from its own pickers (`INTERNAL_LISTING_SOURCES`). A checkbox shows them.
- **Edit a message** in place: content, role, and reasoning.
- **Insert / append** a message anywhere (fractional `display_order`), including
  fabricating an `assistant` turn — the manual equivalent of *prefill*.
- **Rewind / truncate** — delete everything after a chosen message.
- **Delete** individual messages.
- **Re-tag a session**: title, `source` (e.g. flip `oneshot` → `cli` so it shows
  up in the Hermes session list), pin, archive.
- **Create a session from scratch** and populate it message by message.
- **Automatic backup** of `state.db` into `./backups/` before *every* write
  (keeps the 30 newest), plus a lock guard that refuses to write while the Hermes
  gateway/desktop holds the database (override with `--force`).

The FTS5 search index stays correct automatically — Hermes defines
`messages_fts_insert/update/delete` triggers on the `messages` table, so edits
made here are reflected in `session_search` too.

## Why it's safe to point at your real DB

- Reads are read-only connections.
- Writes run inside a transaction and update `message_count` to stay consistent.
- A fresh `.bak` snapshot (via the SQLite backup API, WAL-safe) is taken before
  each mutating request.
- Binds to loopback only; there is **no authentication** — do not expose the port.
- Verified with `PRAGMA integrity_check` after a full create/add/edit/insert/
  truncate/retag cycle on a copy of a real `state.db`.

## Requirements

- Python 3.9+ (stdlib only).
- A Hermes install with a `state.db` (default
  `%LOCALAPPDATA%\hermes\state.db` on Windows, `~/.hermes/state.db` elsewhere).

## Usage

```bash
# Stop the Hermes gateway/desktop first, or edits will be refused (DB locked).
python server.py                       # auto-detects %LOCALAPPDATA%/hermes/state.db
python server.py --db /path/to/state.db --port 8787
python server.py --no-browser          # don't auto-open a tab
python server.py --force               # write even if the DB looks locked (risky)
```

Then open <http://127.0.0.1:8787/>.

### Try it without touching your real data

```bash
cp "$LOCALAPPDATA/hermes/state.db" /tmp/test_state.db
python server.py --db /tmp/test_state.db
```

## HTTP API

All mutating calls return the path of the backup they just wrote.

| Method | Path | Body |
|---|---|---|
| GET | `/api/sessions?q=&internal=0\|1` | — |
| GET | `/api/session?id=<sid>` | — |
| POST | `/api/update_message` | `{id, fields:{role,content,reasoning_content,finish_reason}}` |
| POST | `/api/delete_message` | `{id}` |
| POST | `/api/add_message` | `{session_id, role, content, reasoning?, after_id?}` |
| POST | `/api/truncate_after` | `{session_id, id}` |
| POST | `/api/update_session` | `{id, fields:{title,source,created_source,pinned,archived}}` |
| POST | `/api/create_session` | `{title, source, model}` |

## Project layout

```
server.py            stdlib HTTP server + sqlite3 data layer
static/index.html    UI shell
static/style.css     dark theme
static/app.js        vanilla-JS front end
backups/             auto-written .bak snapshots (git-ignored)
```

## Caveats

- This is a power-user tool. Editing `state.db` by hand can confuse a *running*
  Hermes process — close the gateway/desktop before editing, and reopen after.
- A session's `system_prompt` is stored on the `sessions` row and is **not**
  edited here; this tool focuses on the conversation transcript.
- Running Hermes caches its session list in memory; restart or refresh Hermes to
  see changes made here.

## License

MIT — see [LICENSE](LICENSE).

## Native Rust app (no web layer)

`native-rs/` is a native Windows build using **egui/eframe** + **rusqlite**
(bundled SQLite) — a real desktop window, no browser, no HTTP server. It talks
straight to `state.db`. Built in CI by `.github/workflows/build-native-rust.yml`
(you do not need Rust installed locally).

Icons/visual language follow the same Tabler set used by Chatbox.
