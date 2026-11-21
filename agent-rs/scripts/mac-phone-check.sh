#!/usr/bin/env bash
# Real-phone check on the MacBook (the owner-approved laptop exception to "tests run on the server"): the Rust agent
# stands in for the installed C# agent on port 8765 for a moment, the phone on USB shows its panel, then the installed
# agent comes back.
#
#   agent-rs/scripts/mac-phone-check.sh                  # waits for Enter
#   agent-rs/scripts/mac-phone-check.sh --until FILE     # waits until FILE exists (no terminal needed; at most 10 min)
#   agent-rs/scripts/mac-phone-check.sh --wifi [--until FILE]
#                                                        # no adb reverse rule: the phone must come over Wi-Fi (HTTPS
#                                                        # on the Wi-Fi port, with the pairing it made with C#)
#
# With Wi-Fi on in the installed agent's config, the Rust agent also serves the Wi-Fi port with a copy of its
# lan-cert.pfx, and its /api/pair must match the installed agent's (pc, fingerprint, key, port, hosts).
#
# The installed agent is left as it was: its config is copied (never printed), the Rust agent keeps its log, data and
# lock in a temporary home, and the installed agent is loaded again on exit, also after Ctrl+C or an error.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
PLIST="$HOME/Library/LaunchAgents/net.oxog.phonebox.agent.plist"
CONFIG="$HOME/Library/Application Support/PhoneBox/config.json"
PORT=8765
UNTIL=""
WIFI_ONLY=""
while [ $# -gt 0 ]; do
  case "$1" in
    --until) UNTIL="${2:?--until needs a file}"; shift 2 ;;
    --wifi) WIFI_ONLY=1; shift ;;
    *) echo "unknown argument: $1"; exit 1 ;;
  esac
done

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

# what the phone stored when it paired with the installed agent (kept owner-only, never printed)
install -m 600 /dev/null "$WORK/csharp-pair.json"
curl -s --max-time 5 -H "Host: 127.0.0.1:$PORT" "http://127.0.0.1:$PORT/api/pair" >"$WORK/csharp-pair.json" || true
echo "== stopping the installed agent (pid $(port_pid))"
launchctl unload "$PLIST" # unloaded, launchd does not start it again (KeepAlive)
for _ in $(seq 1 30); do [ -z "$(port_pid)" ] && break; sleep 0.5; done
[ -z "$(port_pid)" ] || { echo "port $PORT is still in use"; exit 1; }
LANPORT="$(python3 -I -c 'import json, sys; l = json.load(open(sys.argv[1])).get("lan") or {}; print(l.get("port", 8766) if l.get("enabled") else "")' "$CONFIG")"
lan_pid() { lsof -ti "tcp:$LANPORT" -sTCP:LISTEN 2>/dev/null || true; }
if [ -n "$LANPORT" ]; then
  for _ in $(seq 1 30); do [ -z "$(lan_pid)" ] && break; sleep 0.5; done
  [ -z "$(lan_pid)" ] || { echo "Wi-Fi port $LANPORT is still in use"; exit 1; }
fi

# The adb server keeps running under the real home, whose key the phone trusts. The installed agent's reverse rule is
# dropped, so the Rust agent sets up its own and opens the panel.
"$ADB" start-server >/dev/null 2>&1
for d in $("$ADB" devices | awk 'NR > 1 && $2 == "device" { print $1 }'); do
  "$ADB" -s "$d" reverse --remove "tcp:$PORT" >/dev/null 2>&1 || true
done

install -m 600 /dev/null "$WORK/config.json"
python3 -I - "$CONFIG" "$WORK/config.json" "$WIFI_ONLY" <<'PY'
import json, sys
cfg = json.load(open(sys.argv[1], encoding="utf-8"))
if sys.argv[3]:
    cfg["deviceSerial"] = "phonebox-phone-check-wifi-only"  # no adb reverse rule: the phone has to use Wi-Fi
open(sys.argv[2], "w", encoding="utf-8").write(json.dumps(cfg))
PY
CERT="$(dirname "$CONFIG")/lan-cert.pfx"
if [ -f "$CERT" ]; then install -m 600 "$CERT" "$WORK/lan-cert.pfx"; fi # the agent reads it next to its config
mkdir -p "$WORK/home"
echo "== starting the Rust agent on port $PORT"
HOME="$WORK/home" "$ROOT/agent-rs/target/release/PhoneBoxAgent" --config "$WORK/config.json" --port "$PORT" \
  >"$WORK/rust.log" 2>&1 &
RUST_PID=$!
for _ in $(seq 1 30); do info | grep -q '"version"' && break; sleep 1; done
if [ -s "$WORK/csharp-pair.json" ]; then
  echo "== /api/pair against the installed agent's"
  curl -s --max-time 5 -H "Host: 127.0.0.1:$PORT" "http://127.0.0.1:$PORT/api/pair" >"$WORK/rust-pair.json"
  chmod 600 "$WORK/rust-pair.json"
  python3 -I - "$WORK" <<'PY'
import json, sys
from pathlib import Path
work = Path(sys.argv[1])
c = json.loads((work / "csharp-pair.json").read_text(encoding="utf-8"))
r = json.loads((work / "rust-pair.json").read_text(encoding="utf-8"))
if c != r:
    sys.exit("   DIFFERENT: " + ", ".join(k for k in c if c.get(k) != r.get(k)) + " (the phone would have to pair again)")
print("   the same " + ", ".join(c) + (f" (fingerprint {r['fingerprint'][:16]}...)" if r.get("enabled") else ""))
PY
fi
if [ -n "$WIFI_ONLY" ]; then
  WAIT_FOR="live stream opened (phone, Wi-Fi)"
  SECONDS_TO_WAIT=180
else
  WAIT_FOR="phone connected: "
  SECONDS_TO_WAIT=60
fi
for _ in $(seq 1 "$SECONDS_TO_WAIT"); do
  grep -qF "$WAIT_FOR" "$WORK/rust.log" && break
  kill -0 "$RUST_PID" 2>/dev/null || break
  sleep 1
done
show_log
grep -qF "$WAIT_FOR" "$WORK/rust.log" || { echo "no \"$WAIT_FOR\" within $SECONDS_TO_WAIT s"; exit 1; }
if [ -n "$WIFI_ONLY" ]; then
  cat <<'EOF'
== check the phone now (over Wi-Fi, without pairing again):
  - it shows the panel; volume, media and shortcut buttons work
  - the log above has "live stream opened (phone, Wi-Fi)"
EOF
else
  cat <<'EOF'
== check the phone now:
  - it shows the panel (clock, page dots; placeholders for the widgets the Rust agent does not serve yet)
  - the log above has "<serial>: adb reverse set up, panel opened"
EOF
fi
if [ -n "$UNTIL" ]; then
  for _ in $(seq 1 600); do [ -e "$UNTIL" ] && break; sleep 1; done
else
  read -r -p "Press Enter to stop the Rust agent and bring the installed one back. " _
fi
echo "== Rust agent log"
show_log
