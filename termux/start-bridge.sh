#!/data/data/com.termux/files/usr/bin/bash
# Start the Hermes Chat Editor bridge inside Termux.
#
#   ./start-bridge.sh              # read-only, 127.0.0.1:8765
#   ./start-bridge.sh --writable   # allow edits (app can then save changes)
#   HERMES_BRIDGE_PORT=9000 ./start-bridge.sh
#
# Any extra arguments are forwarded to hermes_chat_bridge.py.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PORT="${HERMES_BRIDGE_PORT:-8765}"

exec python3 "$HERE/bridge/hermes_chat_bridge.py" --port "$PORT" "$@"
