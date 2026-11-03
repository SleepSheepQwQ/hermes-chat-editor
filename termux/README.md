# Hermes Chat Editor · Termux + Android

The desktop app in `native-rs/` edits `state.db` directly. On Android that is
impossible from an APK: Termux's home (`/data/data/com.termux/files/home`) is
private to the Termux uid, so a normal app cannot open the database — and this
project deliberately does **not** use root.

So the port is split in two:

```
Termux (python3, no deps)                Android APK (Kotlin, no deps)
┌──────────────────────────────┐         ┌────────────────────────────┐
│ hermes_chat_bridge.py        │  HTTP   │ MainActivity  (会话列表)    │
│  · owns all SQLite access    │◄───────►│ MessagesActivity (消息编辑) │
│  · 127.0.0.1:8765/api/v1     │  JSON   │ SelfTestActivity (自检)     │
│  · read-only by default      │         │ 纯 framework，无 AndroidX   │
└──────────────────────────────┘         └────────────────────────────┘
```

Both ends speak plain JSON over loopback, so the phone never needs a PC, adb,
or root — the APK and the bridge live on the same device.

## 1. Start the bridge (inside Termux)

```bash
cd termux
./start-bridge.sh                 # read-only (recommended first run)
./start-bridge.sh --writable      # allow edits
```

First start prints a token and stores it in `~/.hermes/chat_bridge_token`.
Paste that token into the app's 设置 dialog. The bridge binds `127.0.0.1` only —
it is not reachable from the LAN.

If Hermes itself is running (fresh `gateway_heartbeats`), writes are refused
with HTTP 409 unless the bridge was started with `--force` — same rule as the
desktop app's 「强制写入」.

## 2. Install the APK

CI builds it (see `.github/workflows/build-termux-apk.yml`):

* push to `termux` branch or run the workflow manually → artifact
  `hermes-chat-termux-debug`
* or `gh run download -n hermes-chat-termux-debug`

Install from Termux without root (Shizuku):

```bash
RISH_APPLICATION_ID=com.termux rish -c 'pm install -r /sdcard/Download/app-debug.apk'
```

or just tap the APK in a file manager.

## 3. What the app can do

| Screen | Actions |
|---|---|
| 会话列表 | 搜索、刷新、含隐藏会话、置顶/归档标记、会话设置、导出 JSONL、删除会话 |
| 消息列表 | 点击编辑（角色 + 内容）、长按：**在此后插入** / 上移 / 下移 / 删除 / 截断到此 |
| 底部栏 | 刷新、插入到末尾、导出 JSONL/MD/HTML |
| 自检 | 一键跑通 bridge + 读写回路，并写 `files/selftest/result.json` |

写操作一律先由 bridge 用 SQLite backup API 生成备份（`~/.hermes/backups/`，
保留最近 30 份）。编辑消息后保持原 `display_order`，不会因为触发器把
`display_order` 清空而让消息跳到会话末尾。

## 4. Test interfaces (预留测试接口)

Three layers, all usable from a normal shell (rish) without root:

**a. Bridge self-check**

```bash
curl -s http://127.0.0.1:8765/api/v1/selftest | python3 -m json.tool
```

**b. App self-test via intent extra**

```bash
RISH_APPLICATION_ID=com.termux rish -c \
  "am start -n com.sleepsheep.hermeschat/.MainActivity --ez selftest true"
# result:
RISH_APPLICATION_ID=com.termux rish -c \
  "cat /sdcard/Android/data/com.sleepsheep.hermeschat/files/selftest/result.json"
# logcat:
RISH_APPLICATION_ID=com.termux rish -c "logcat -d -s SELFTEST"
```

The write probe creates its own throwaway session, exercises
insert/edit/move/truncate/export, then deletes it — a passing run leaves no
residue.

**c. Bridge end-to-end test (CI + local, no device needed)**

```bash
python3 termux/bridge/test_bridge.py -v      # builds a fixture DB from tests/schema.sql
```

## 5. API surface

`GET /api/v1/...` — everything is JSON; writes need `X-Bridge-Token`.

| Method | Path | Notes |
|---|---|---|
| GET | `/health` | no token; db path, writable, Hermes heartbeat |
| GET | `/selftest` | structured checks |
| GET | `/sessions?q=&hidden=1` | 500 newest |
| GET/PATCH/DELETE | `/sessions/{id}` | title/source/pinned/archived/system_prompt |
| POST | `/sessions` | create `{title,source}` |
| GET/POST | `/sessions/{id}/messages` | list / insert `{role,content,after_id?}` |
| PATCH/DELETE | `/messages/{id}` | edit / delete |
| POST | `/messages/{id}/move` | `{direction:"up"\|"down"}` |
| POST | `/sessions/{id}/truncate` | `{after_id}` |
| GET | `/sessions/{id}/export?format=jsonl\|md\|html` | text download |
| GET/POST | `/backups` | list / create snapshot |

## 6. Build locally (optional)

Needs JDK 17 + Android SDK 34; Termux alone cannot build an APK.

```bash
cd termux/android
gradle :app:assembleDebug     # or ./gradlew if you add a wrapper
```

## Caveats

* Editing a live DB can confuse a *running* Hermes process (it caches sessions
  in memory). Close Hermes, edit, reopen.
* `messages.content` edits null `display_identity` by design (Hermes re-derives
  it); the bridge restores `display_order` so ordering does not jump.
* The app is intentionally dependency-free (no AndroidX/Compose) so the CI
  build stays reproducible on a bare runner.
