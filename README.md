# Hermes Chat Editor

A **native Windows** editor for your **Hermes Agent** conversation history.
It opens the Hermes `state.db` SQLite session store directly and lets you view,
edit, rewind, fabricate and re-tag conversations — the "editable chat log /
prefill" workflow [Chatbox](https://github.com/Bin-Huang/chatbox) popularized,
applied to Hermes sessions.

> Built in Rust with **egui/eframe** + **rusqlite** (bundled SQLite). A real
> desktop window — no browser, no web server, no WebView, no telemetry.

## What it does

- **Browse & search** every session in `state.db` (title / id / message body).
- **Reveal internal sessions** — Hermes hides `oneshot` / `tool` / `kanban`
  sources from its own pickers. A checkbox shows them.
- **Edit a message** in place: content and role.
- **Insert** a message after any row, or **append** to the end — including
  fabricating an `assistant` turn (the manual equivalent of *prefill*).
- **Rewind / truncate** — delete everything after a chosen message.
- **Reorder messages** (move up / down).
- **Batch operations** — multi-select messages, delete them, or change role.
- **Delete** individual messages.
- **Re-tag a session**: title, `source` (e.g. flip `oneshot` → `cli` so it shows
  up in the Hermes session list), pin, archive.
- **Create a session from scratch** and populate it message by message.
- **Export** the current session as **JSONL / Markdown / HTML**.
- **Import** a session from a JSONL file (creates a new session).
- **Switch database profiles** (multiple `HERMES_HOME` / `profiles` paths).
- **Light / dark theme** toggle.
- **Lightweight Markdown preview** for message content.
- **Automatic backup** of `state.db` into `./backups/` before every write
  (keeps the 30 newest), via the SQLite backup API (WAL-safe).

## Safety features

- **Read-only mode by default.** The app opens the DB read-only; you must
  explicitly untick "只读模式" (read-only) to enable editing. This prevents
  accidental writes.
- **Write protection while Hermes is running.** If Hermes is running (recent
  `gateway_heartbeats`), writes are blocked unless you tick "强制写入"
  (force write). Editing a live DB risks being overwritten by Hermes' in-memory
  session cache.
- **Safe write path**: `busy_timeout=30s`, `PRAGMA foreign_keys=ON`, and the
  editor lets SQLite triggers auto-assign `display_order` / `message_uid` on
  insert — it does **not** pass those columns, avoiding corruption of Hermes'
  display identity. After writes it syncs `sessions.message_count`.
- **Confirm dialogs** for destructive operations (delete / truncate / restore).
- **Unsaved-changes guard** when switching sessions / refreshing / creating.

Tool-call carrier turns (empty `content` + `tool_calls`) are shown as
`工具调用 → <names>`; tool results are collapsible. Icons/visual language follow
the same Tabler set Chatbox uses.

## Getting the binary

You do **not** need Rust installed. GitHub Actions
(`.github/workflows/build-native-rust.yml`) compiles it on a Windows runner:

- Push a `v*` tag → a Release is created with `hermes-chat-editor-native.exe`.
- Or trigger the workflow manually (Actions tab) and download the artifact.

## Running

```
hermes-chat-editor-native.exe                   # auto-detects %LOCALAPPDATA%\hermes\state.db
hermes-chat-editor-native.exe C:\path\state.db  # or pass a db path
hermes-chat-editor-native.exe --readonly        # open read-only (default)
hermes-chat-editor-native.exe --writable        # open in writable mode
hermes-chat-editor-native.exe --help          # show CLI help
```

Close the Hermes gateway/desktop before editing — a running Hermes caches its
session list in memory and holds the database; reopen it afterward to see your
changes.

## Build locally (optional, needs Rust)

```
cd native-rs
cargo run --release
```

## Caveats

- Power-user tool. Editing `state.db` by hand can confuse a *running* Hermes
  process — close it first, reopen after.
- A session's `system_prompt` lives on the `sessions` row and can be edited via
  the "系统提示词…" button.

## Project layout

```
native-rs/
  Cargo.toml
  src/main.rs            egui UI + rusqlite data layer
.github/workflows/
  build-native-rust.yml  CI: cargo build --release -> exe artifact / Release
backups/                 auto-written .bak snapshots (git-ignored)
```

## License

MIT — see [LICENSE](LICENSE). Bundles icon path data from
[Tabler Icons](https://github.com/tabler/tabler-icons) (MIT).
