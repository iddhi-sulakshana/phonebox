#!/usr/bin/env bash
# MacBook resource comparison (survey-c 4.1-4.4): the sign-off measurement, run by the owner at gate 1 (an
# owner-approved laptop exception to "tests run on the server"). C# 0.9.23 and the Rust agent run side by side,
# beside the installed agent, which stays untouched on 8765; both get the same config and the same timeline.
#
#   agent-rs/scripts/mac-resources.sh               # prints what it does and its side effects; starts nothing
#   agent-rs/scripts/mac-resources.sh --yes         # the measurement: 2 runs of about 34 min, plus the builds
#
# Options:
#   --quick              warm-up 20 s, 60 s per scenario, pages of 8 s: a try-out of the tooling, not a measurement
#   --runs N             runs (2); even runs start the agents in the other order
#   --cs-bin PATH        a C# PhoneBoxAgent to use instead of publishing one (for example a copy of the 0.9.23 app
#                        folder); the files next to it, without .pdb, are its installed size
#   --rs-bin PATH        a Rust PhoneBoxAgent to use instead of `cargo build --release` (for example the release
#                        candidate from CI)
#   --cs-zip PATH        the C# download (default: the size of upstream's PhoneBoxAgent-0.9.23-osx-arm64.zip,
#                        44,322,227 bytes)
#   --rs-zip PATH        the Rust download (default: tools/zip-agent.py of PhoneBoxAgent and THIRD-PARTY-NOTICES.txt)
#   --out DIR            results (default: the plan workspace, resources/<date>, or resources/<date>-quick)
#
# Per run: a temporary folder (/tmp/phonebox-res.XXXXXX) with a home per agent; each agent gets a copy of the owner's
# config.json (never printed: deviceSerial "phonebox-resource-no-phone", Wi-Fi on its own spare port with a copy of
# lan-cert.pfx, updates.auto false, charge.enabled false), copies of data.json and claude-quota*.json, links to
# ~/.claude, ~/.codex, ~/.workbuddy, ~/.config/gh and ~/Library/Keychains (read only by the agents) and the PATH of the
# installed agent's launchd job, then its own spare port. Both start, the warm-up runs, then S0 (idle), S1 (a headless
# phone: the stream and the panel's loops) and S2 (every enabled page in turn) from tools/resources/client.py, while
# tools/resources/sample.py samples both every second (libproc) and `top` every 5 s. Everything this script starts is
# stopped by PID, also after Ctrl+C or an error; the temporary folder is removed. report.py writes summary.json and
# table.md (the release-notes table and the pass rule) next to each run's samples.csv, schedule.log, top.txt and
# pmset.txt.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
PLIST="$HOME/Library/LaunchAgents/net.oxog.phonebox.agent.plist"
APPDIR="$HOME/Library/Application Support/PhoneBox"
CONFIG="$APPDIR/config.json"
INSTALLED=8765
TOOLS="$ROOT/tools/resources"
CS_UPSTREAM_ZIP=44322227 # gh release view v0.9.23 -R ersinkoc/phonebox: PhoneBoxAgent-0.9.23-osx-arm64.zip
YES=""
QUICK=""
RUNS=2
CS_BIN=""
RS_BIN=""
CS_ZIP=""
RS_ZIP=""
OUT=""
while [ $# -gt 0 ]; do
  case "$1" in
    --yes) YES=1; shift ;;
    --quick) QUICK=1; shift ;;
    --runs) RUNS="$2"; shift 2 ;;
    --cs-bin) CS_BIN="$2"; shift 2 ;;
    --rs-bin) RS_BIN="$2"; shift 2 ;;
    --cs-zip) CS_ZIP="$2"; shift 2 ;;
    --rs-zip) RS_ZIP="$2"; shift 2 ;;
    --out) OUT="$2"; shift 2 ;;
    *) echo "unknown argument: $1"; exit 1 ;;
  esac
done
case "$RUNS" in '' | *[!0-9]* | 0) echo "--runs takes a whole number above 0"; exit 1 ;; esac
if [ -n "$QUICK" ]; then
  PHASES="warmup=20,S0=60,S1=60,S2=60"; PAGE=8; MINUTES=4
else
  PHASES="warmup=120,S0=600,S1=600,S2=720"; PAGE=30; MINUTES=34
fi
WORKSPACE="$ROOT/.superpowers/sdd/2026-11-22-rust-agent-plan-8-sign-off/resources"
[ -n "$OUT" ] || OUT="$WORKSPACE/$(date +%F)${QUICK:+-quick}"

cat <<EOF
mac-resources.sh: the resource comparison of survey-c 4.1-4.4 (C# 0.9.23 against the Rust agent)

What it does, for about $((RUNS * MINUTES)) minutes plus the builds:
  - builds the Rust agent (cargo build --release) and the C# agent as it ships (dotnet publish, self-contained
    single file, osx-arm64, into /storage/builds/phonebox-res/cs), unless --rs-bin / --cs-bin are given;
  - runs both beside your installed agent, which stays untouched on port $INSTALLED: each on spare ports, with its
    own temporary HOME and a copy of your config.json (deviceSerial "phonebox-resource-no-phone", Wi-Fi on a spare
    port, updates.auto false, charge.enabled false; never printed), your data.json and quota files, links to
    ~/.claude, ~/.codex, ~/.workbuddy, ~/.config/gh and ~/Library/Keychains, and the PATH of your launchd job;
  - $RUNS run(s), the second with the agents started in the other order: warm-up, idle, a headless phone (the
    stream and the panel's loops), every enabled page in turn ($PHASES; pages of $PAGE s);
    libproc samples every second and top every 5 s; the Mac is kept awake (caffeinate -i) meanwhile.
Side effects while it runs:
  - two more quota and GitHub pollers (CodexBar, gh), and two more clients of Home Assistant, OBS, Discord and
    Spotify if your config uses them (Discord may ask you to authorize the new clients);
  - your rules run in both copies too: whatever a rule does when it fires can happen up to three times;
  - macOS may ask once per new binary for Automation (Music, Spotify, System Events) or Local Network access;
  - no phone is involved: the copies match no phone, and the panel's battery report is never sent, so no charge
    shortcut can run.
Keep the Mac otherwise idle, on AC power, with music off (or the same) for the whole run.
Results: $OUT
EOF
if [ -z "$YES" ]; then
  echo
  echo "Nothing was started. Run again with --yes to start."
  exit 2
fi

[ "$(uname)" = Darwin ] || { echo "macOS only"; exit 1; }
if [ ! -f "$PLIST" ] || [ ! -f "$CONFIG" ]; then echo "the installed agent was not found"; exit 1; fi
fetch() { curl -s --max-time "${3:-5}" -H "Host: 127.0.0.1:$1" "http://127.0.0.1:$1$2"; }
port_pid() { lsof -ti "tcp:$1" -sTCP:LISTEN 2>/dev/null || true; }
free_port() { python3 -I -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])'; }
fetch "$INSTALLED" /api/state | grep -q '"pc"' || { echo "the installed agent does not answer on port $INSTALLED"; exit 1; }
BEFORE="$(shasum -a 256 "$CONFIG" | cut -d' ' -f1)"

echo
echo "== checking the sampler"
python3 -I "$TOOLS/sample.py" --check

echo "== the agents"
if [ -z "$RS_BIN" ]; then
  (cd "$ROOT/agent-rs" && cargo build --release -q)
  RS_BIN="$ROOT/agent-rs/target/release/PhoneBoxAgent"
fi
if [ -z "$CS_BIN" ]; then
  mkdir -p /storage/builds/phonebox-res
  rm -rf /storage/builds/phonebox-res/cs
  dotnet publish "$ROOT/agent/PhoneBox.Agent" -c Release -f net10.0 -r osx-arm64 --self-contained true \
    -p:PublishSingleFile=true -o /storage/builds/phonebox-res/cs --nologo -v q
  CS_BIN=/storage/builds/phonebox-res/cs/PhoneBoxAgent
fi
if [ ! -x "$RS_BIN" ] || [ ! -x "$CS_BIN" ]; then echo "an agent binary is missing: $CS_BIN, $RS_BIN"; exit 1; fi

BASE="$(mktemp -d /tmp/phonebox-res.XXXXXX)"
declare -a STARTED=() # every PID this script started, stopped by PID on exit
CS_PID=""
RS_PID=""
cleanup() {
  set +eu # bash 3.2 calls an empty array unbound
  local p
  for p in "${STARTED[@]}"; do
    if kill -0 "$p" 2>/dev/null; then kill "$p"; fi
  done
  for p in "${STARTED[@]}"; do
    wait "$p" 2>/dev/null
  done
  rm -rf "$BASE" # the config copies and the homes
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
caffeinate -i -w $$ &
STARTED+=("$!")

# what an install holds: C# the publish folder without .pdb, Rust the executable and its notices
mkdir -p "$BASE/stage/cs" "$BASE/stage/rs"
find "$(dirname "$CS_BIN")" -maxdepth 1 -type f ! -name '*.pdb' -exec cp -p {} "$BASE/stage/cs/" \;
cp -p "$RS_BIN" "$BASE/stage/rs/PhoneBoxAgent"
cp -p "$ROOT/agent-rs/THIRD-PARTY-NOTICES.txt" "$BASE/stage/rs/"
CS_INSTALLED=$(($(du -sk "$BASE/stage/cs" | cut -f1) * 1024))
RS_INSTALLED=$(($(du -sk "$BASE/stage/rs" | cut -f1) * 1024))
if [ -n "$CS_ZIP" ]; then CS_DOWNLOAD="$(stat -f %z "$CS_ZIP")"; else CS_DOWNLOAD=$CS_UPSTREAM_ZIP; fi
if [ -n "$RS_ZIP" ]; then
  RS_DOWNLOAD="$(stat -f %z "$RS_ZIP")"
else
  python3 -I "$ROOT/tools/zip-agent.py" "$BASE/stage/rs" "$BASE/rs.zip" >/dev/null
  RS_DOWNLOAD="$(stat -f %z "$BASE/rs.zip")"
fi
echo "   C#: $CS_BIN ($CS_INSTALLED bytes installed, $CS_DOWNLOAD download)"
echo "   Rust: $RS_BIN ($RS_INSTALLED bytes installed, $RS_DOWNLOAD download)"
AGENT_PATH="$(plutil -extract EnvironmentVariables.PATH raw -o - "$PLIST" 2>/dev/null || echo /usr/bin:/bin:/usr/sbin:/sbin)"

# start_agent LABEL BIN DIR: a home and a config copy, then the agent; sets START_PID, START_PORT, START_LAN
start_agent() {
  local label="$1" bin="$2" dir="$3"
  local h="$dir/home"
  START_PORT="$(free_port)"
  START_LAN="$(free_port)"
  mkdir -p "$h/Library/Application Support/PhoneBox" "$h/.config"
  install -m 600 /dev/null "$dir/config.json"
  python3 -I - "$CONFIG" "$dir/config.json" "$START_LAN" <<'PY'
import json, sys
cfg = json.load(open(sys.argv[1], encoding="utf-8"))
cfg["deviceSerial"] = "phonebox-resource-no-phone"  # matches no phone: the phone's adb reverse rule stays alone
if isinstance(cfg.get("lan"), dict) and cfg["lan"].get("enabled"):
    cfg["lan"]["port"] = int(sys.argv[3])  # the installed agent keeps its own Wi-Fi port
updates = cfg.get("updates") if isinstance(cfg.get("updates"), dict) else {}
cfg["updates"] = dict(updates, auto=False)
charge = cfg.get("charge") if isinstance(cfg.get("charge"), dict) else {}
cfg["charge"] = dict(charge, enabled=False)  # no battery report may ever run the owner's charge shortcuts
open(sys.argv[2], "w", encoding="utf-8").write(json.dumps(cfg, indent=2))
PY
  if [ -f "$APPDIR/lan-cert.pfx" ]; then install -m 600 "$APPDIR/lan-cert.pfx" "$dir/lan-cert.pfx"; fi
  local f d
  for f in "$APPDIR/data.json" "$APPDIR"/claude-quota*.json; do
    if [ -f "$f" ]; then install -m 600 "$f" "$h/Library/Application Support/PhoneBox/"; fi
  done
  for d in .claude .codex .workbuddy .config/gh Library/Keychains; do
    if [ -e "$HOME/$d" ]; then ln -s "$HOME/$d" "$h/$d"; fi
  done
  HOME="$h" PATH="$AGENT_PATH" "$bin" --config "$dir/config.json" --port "$START_PORT" >"$dir/agent.log" 2>&1 &
  START_PID=$!
  STARTED+=("$START_PID")
  echo "   $label: pid $START_PID, port $START_PORT"
}

wait_ready() { # LABEL PID PORT DIR
  for _ in $(seq 1 90); do
    fetch "$3" /api/state | grep -q '"pc"' && return 0
    kill -0 "$2" 2>/dev/null || break
    sleep 1
  done
  grep -v -i token "$4/agent.log" | tail -20 # the agents log no keys; filter anyway
  echo "the $1 agent did not answer"
  exit 1
}

stop_agent() { # LABEL PID PORT LANPORT
  kill "$2" # by PID: only the agent this script started
  for _ in $(seq 1 30); do kill -0 "$2" 2>/dev/null || break; sleep 0.5; done
  if kill -0 "$2" 2>/dev/null; then kill -9 "$2"; fi
  wait "$2" 2>/dev/null || true
  if [ -n "$(port_pid "$3")" ] || [ -n "$(port_pid "$4")" ]; then echo "a port of the $1 agent is still open"; exit 1; fi
}

power_state() {
  echo "## $(date '+%F %T') $1"
  pmset -g batt
  pmset -g therm
  pmset -g assertions | grep -iE 'display|UserIsActive' || true
}

CSVS=()
CS_VERSION="?"
RS_VERSION="?"
for run in $(seq 1 "$RUNS"); do
  if [ $((run % 2)) = 1 ]; then ORDER="cs rs"; else ORDER="rs cs"; fi
  R="$OUT/run$run"
  W="$BASE/run$run"
  mkdir -p "$R" "$W/cs" "$W/rs"
  echo "== run $run of $RUNS (started in the order: $ORDER)"
  power_state start >"$R/pmset.txt"
  for label in $ORDER; do
    if [ "$label" = cs ]; then
      start_agent cs "$CS_BIN" "$W/cs"; CS_PID=$START_PID; CS_PORT=$START_PORT; CS_LAN=$START_LAN
    else
      start_agent rs "$RS_BIN" "$W/rs"; RS_PID=$START_PID; RS_PORT=$START_PORT; RS_LAN=$START_LAN
    fi
  done
  wait_ready cs "$CS_PID" "$CS_PORT" "$W/cs"
  wait_ready rs "$RS_PID" "$RS_PORT" "$W/rs"
  if [ "$ORDER" = "cs rs" ]; then
    AGENTS=(--agent "cs=$CS_PORT" --agent "rs=$RS_PORT"); PIDS=(--pid "cs=$CS_PID" --pid "rs=$RS_PID")
  else
    AGENTS=(--agent "rs=$RS_PORT" --agent "cs=$CS_PORT"); PIDS=(--pid "rs=$RS_PID" --pid "cs=$CS_PID")
  fi
  python3 -I "$TOOLS/sample.py" --out "$R/samples.csv" --phase-file "$W/phase" "${PIDS[@]}" &
  SAMPLER=$!
  STARTED+=("$SAMPLER")
  top -l 0 -s 5 -pid "$CS_PID" -pid "$RS_PID" -stats pid,cpu,mem,threads,idlew,power >"$R/top.txt" 2>&1 &
  TOP=$!
  STARTED+=("$TOP")
  python3 -I "$TOOLS/client.py" "${AGENTS[@]}" --phases "$PHASES" --page-seconds "$PAGE" --phase-file "$W/phase" \
    --log "$R/schedule.log" &
  CLIENT=$!
  STARTED+=("$CLIENT")
  echo "   timeline $PHASES (pages of $PAGE s) from $(date +%T)"
  wait "$CLIENT" || { echo "the client failed"; exit 1; }
  kill "$SAMPLER" "$TOP" 2>/dev/null || true
  wait "$SAMPLER" "$TOP" 2>/dev/null || true
  for label in cs rs; do
    if [ "$label" = cs ]; then pid=$CS_PID; port=$CS_PORT; else pid=$RS_PID; port=$RS_PORT; fi
    kill -0 "$pid" 2>/dev/null || { grep -v -i token "$W/$label/agent.log" | tail -20; echo "the $label agent died during the run"; exit 1; }
    version="$(fetch "$port" /api/info | python3 -I -c 'import json, sys; print(json.load(sys.stdin).get("version", "?"))')"
    if [ "$label" = cs ]; then CS_VERSION=$version; else RS_VERSION=$version; fi
    phonebox_kb=$(du -sk "$W/$label/home/Library/Application Support/PhoneBox" | cut -f1)
    echo "$label version=$version pid=$pid phonebox_folder_kb=$phonebox_kb stdout_bytes=$(stat -f %z "$W/$label/agent.log")" >>"$R/meta.txt"
  done
  for label in $ORDER; do
    if [ "$label" = cs ]; then stop_agent cs "$CS_PID" "$CS_PORT" "$CS_LAN"; else stop_agent rs "$RS_PID" "$RS_PORT" "$RS_LAN"; fi
  done
  power_state end >>"$R/pmset.txt"
  echo "   both agents stopped; rows: $(($(wc -l <"$R/samples.csv") - 1)), requests: $(grep -vc '^#' "$R/schedule.log")"
  rm -rf "$W"
  CSVS+=("$R/samples.csv")
done

AFTER="$(shasum -a 256 "$CONFIG" | cut -d' ' -f1)"
[ "$BEFORE" = "$AFTER" ] || echo "WARNING: the installed agent's config.json changed during the run"
echo "== report"
python3 -I "$TOOLS/report.py" "${CSVS[@]}" --out-dir "$OUT" --platform macos \
  --title "MacBook Air M4, macOS $(sw_vers -productVersion), same config" \
  --label "cs=C# $CS_VERSION" --label "rs=Rust $RS_VERSION" \
  --installed-size "cs=$CS_INSTALLED" --installed-size "rs=$RS_INSTALLED" \
  --download-size "cs=$CS_DOWNLOAD" --download-size "rs=$RS_DOWNLOAD" --download-label "Download (osx-arm64 zip)" \
  --note "Method: survey-c 4.1-4.4. Both agents side by side beside the installed one, own HOME, the owner's config (no phone, updates and charge off); libproc samples every 1 s with child tools, client.py's headless phone; $PHASES, pages $PAGE s; $RUNS run(s), start order swapped. C#: $CS_BIN; Rust: $RS_BIN at $(git -C "$ROOT" rev-parse --short HEAD); $(date +%F).${QUICK:+ QUICK RUN (a try-out; not a measurement).}"
echo "== results in $OUT"
