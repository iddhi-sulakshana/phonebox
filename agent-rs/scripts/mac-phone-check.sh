#!/usr/bin/env bash
# Real-phone check on the MacBook (the owner-approved laptop exception to "tests run on the server"): the Rust agent
# stands in for the installed C# agent on port 8765 for a moment, the phone on USB shows its panel, then the installed
# agent comes back.
#
#   agent-rs/scripts/mac-phone-check.sh                  # waits for Enter
#   agent-rs/scripts/mac-phone-check.sh --until FILE     # waits until FILE exists (no terminal needed; at most 10 min)
#
# The installed agent is left as it was: its config is copied (never printed), the Rust agent keeps its log, data and
# lock in a temporary home, and the installed agent is loaded again on exit, also after Ctrl+C or an error.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
PLIST="$HOME/Library/LaunchAgents/net.oxog.phonebox.agent.plist"
CONFIG="$HOME/Library/Application Support/PhoneBox/config.json"
PORT=8765
UNTIL=""
if [ "${1:-}" = "--until" ]; then UNTIL="${2:?--until needs a file}"; fi

[ "$(uname)" = Darwin ] || { echo "macOS only"; exit 1; }
[ -f "$PLIST" ] && [ -f "$CONFIG" ] || { echo "the installed agent was not found"; exit 1; }
ADB="$(command -v adb || true)"
[ -n "$ADB" ] || { echo "adb is not on PATH"; exit 1; }

info() { curl -s --max-time 2 -H "Host: 127.0.0.1:$PORT" "http://127.0.0.1:$PORT/api/info" || true; }
port_pid() { lsof -ti "tcp:$PORT" -sTCP:LISTEN 2>/dev/null || true; }
show_log() { grep -v -i token "$WORK/rust.log" | sed 's/^/  /' || true; } # the agent logs no keys; filter anyway

echo "== building the Rust agent"
(cd "$ROOT/agent-rs" && cargo build --release -q)

WORK="$(mktemp -d /tmp/phonebox-phone-check.XXXXXX)"
RUST_PID=""
restore() {
  set +e
  if [ -n "$RUST_PID" ] && kill -0 "$RUST_PID" 2>/dev/null; then
    kill "$RUST_PID" # by PID: only the agent this script started
    wait "$RUST_PID" 2>/dev/null
  fi
  echo "== loading the installed agent again"
  launchctl load "$PLIST" 2>/dev/null
  for _ in $(seq 1 60); do info | grep -q '"connected":true' && break; sleep 1; done
  info | python3 -I -c 'import json, sys
d = json.load(sys.stdin)
print("installed agent:", d["version"], "| connected:", d["connected"], "| via:", d["via"])' \
    || echo "the installed agent did not answer on port $PORT"
  rm -rf "$WORK"
}
trap restore EXIT

echo "== stopping the installed agent (pid $(port_pid))"
launchctl unload "$PLIST" # unloaded, launchd does not start it again (KeepAlive)
for _ in $(seq 1 30); do [ -z "$(port_pid)" ] && break; sleep 0.5; done
[ -z "$(port_pid)" ] || { echo "port $PORT is still in use"; exit 1; }

# The adb server keeps running under the real home, whose key the phone trusts. The installed agent's reverse rule is
# dropped, so the Rust agent sets up its own and opens the panel.
"$ADB" start-server >/dev/null 2>&1
for d in $("$ADB" devices | awk 'NR > 1 && $2 == "device" { print $1 }'); do
  "$ADB" -s "$d" reverse --remove "tcp:$PORT" >/dev/null 2>&1 || true
done

install -m 600 "$CONFIG" "$WORK/config.json"
mkdir -p "$WORK/home"
echo "== starting the Rust agent on port $PORT"
HOME="$WORK/home" "$ROOT/agent-rs/target/release/PhoneBoxAgent" --config "$WORK/config.json" --port "$PORT" \
  >"$WORK/rust.log" 2>&1 &
RUST_PID=$!
for _ in $(seq 1 60); do
  grep -q "phone connected: " "$WORK/rust.log" && break
  kill -0 "$RUST_PID" 2>/dev/null || break
  sleep 1
done
show_log
grep -q "phone connected: " "$WORK/rust.log" || { echo "no phone connected within 60 s"; exit 1; }
cat <<'EOF'
== check the phone now:
  - it shows the panel (clock, page dots; placeholders for the widgets the Rust agent does not serve yet)
  - the log above has "<serial>: adb reverse set up, panel opened"
EOF
if [ -n "$UNTIL" ]; then
  for _ in $(seq 1 600); do [ -e "$UNTIL" ] && break; sleep 1; done
else
  read -r -p "Press Enter to stop the Rust agent and bring the installed one back. " _
fi
echo "== Rust agent log"
show_log
