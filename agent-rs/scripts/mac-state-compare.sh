#!/usr/bin/env bash
# MacBook state check (the owner-approved laptop exception to "tests run on the server"): the Rust agent runs next to
# the installed C# agent on a spare port, both answer /api/state ten times a second apart, and compare-pc-ping.py checks
# that their `pc` and `ping` parts agree (fixed facts equal, live readings within its tolerances).
#
#   agent-rs/scripts/mac-state-compare.sh
#
# The installed agent keeps running untouched. The Rust agent gets a copy of its config (never printed) whose
# deviceSerial matches no phone, so it leaves the phone's adb reverse rule alone, and a temporary home for its log,
# data and lock; it is stopped by PID on exit, also after Ctrl+C or an error.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
CONFIG="$HOME/Library/Application Support/PhoneBox/config.json"
INSTALLED=8765
SAMPLES=10

[ "$(uname)" = Darwin ] || { echo "macOS only"; exit 1; }
[ -f "$CONFIG" ] || { echo "the installed agent's config was not found"; exit 1; }
state() { curl -s --max-time 3 -H "Host: 127.0.0.1:$1" "http://127.0.0.1:$1/api/state"; }
state "$INSTALLED" | grep -q '"pc"' || { echo "the installed agent does not answer on port $INSTALLED"; exit 1; }

echo "== building the Rust agent"
(cd "$ROOT/agent-rs" && cargo build --release -q)

WORK="$(mktemp -d /tmp/phonebox-state-compare.XXXXXX)"
RUST_PID=""
cleanup() {
  set +e
  if [ -n "$RUST_PID" ] && kill -0 "$RUST_PID" 2>/dev/null; then
    kill "$RUST_PID" # by PID: only the agent this script started
    wait "$RUST_PID" 2>/dev/null
  fi
  rm -rf "$WORK"
}
trap cleanup EXIT

install -m 600 /dev/null "$WORK/config.json"
python3 -I - "$CONFIG" "$WORK/config.json" <<'PY'
import json, sys
cfg = json.load(open(sys.argv[1], encoding="utf-8"))
cfg["deviceSerial"] = "phonebox-state-compare-no-phone"  # the Rust agent must not touch the phone's reverse rule
open(sys.argv[2], "w", encoding="utf-8").write(json.dumps(cfg))
PY
PORT="$(python3 -I -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])')"
mkdir -p "$WORK/home"
echo "== starting the Rust agent on port $PORT"
HOME="$WORK/home" "$ROOT/agent-rs/target/release/PhoneBoxAgent" --config "$WORK/config.json" --port "$PORT" \
  >"$WORK/rust.log" 2>&1 &
RUST_PID=$!
for _ in $(seq 1 60); do
  state "$PORT" | grep -q '"pc"' && break
  kill -0 "$RUST_PID" 2>/dev/null || { grep -v -i token "$WORK/rust.log"; echo "the Rust agent exited"; exit 1; }
  sleep 1
done
sleep 6 # the monitors' first samples (GPU, temperature, ping)

echo "== sampling /api/state from both agents, $SAMPLES times a second apart"
for _ in $(seq 1 "$SAMPLES"); do
  state "$INSTALLED" >>"$WORK/csharp.jsonl"; echo >>"$WORK/csharp.jsonl"
  state "$PORT" >>"$WORK/rust.jsonl"; echo >>"$WORK/rust.jsonl"
  sleep 1
done
python3 -I "$ROOT/agent-rs/scripts/compare-pc-ping.py" "$WORK/csharp.jsonl" "$WORK/rust.jsonl"
