#!/usr/bin/env bash
# Usage: agent-rs/scripts/remote-resources.sh [--quick] [--runs N] [--out DIR]
# The Linux cross-check of the resource comparison (survey-c 4.5) on the test server. It backs the Linux claim in the
# release notes and proves the tooling; the sign-off measurement is the MacBook's (mac-resources.sh).
#   1. syncs the tree and copies this script and tools/resources/*.py to a run folder under /root/work/resource/runs,
#   2. in the tmux session phonebox-resource: builds the C# agent as it ships (dotnet publish, self-contained single
#      file, linux-x64, from its own copy of the sources so it never shares obj/ with the parity runs) and the Rust
#      agent (cargo build --release); stages each as installed (C#: the publish folder without .pdb; Rust:
#      PhoneBoxAgent and THIRD-PARTY-NOTICES.txt) and zips it for the download size,
#   3. per run (the second starts the agents in the other order): both agents under `taskset -c 0-9` (so the .NET
#      thread pool and tokio size themselves for 10 cores, as on the M4), each with its own HOME/XDG_CONFIG_HOME/
#      XDG_DATA_HOME, spare ports and the same config (config.default.linux.json with deviceSerial
#      "phonebox-resource-no-phone", updates.auto false, charge.enabled false, Wi-Fi on a spare port with the parity
#      fixture's certificate); sample.py every second and `pidstat -h -u -r -d 5` beside it, client.py's
#      timeline (both on cores 10-19); then every process it started is stopped by PID,
#   4. report.py writes summary.json and table.md; the results are copied back to DIR (default: the plan workspace,
#      resources/server-<date>, or resources/server-quick with --quick).
# --quick: warm-up 20 s, 60 s per scenario and pages of 8 s (every default page is shown once): proves the tooling,
# it is not a measurement. A full run is about 2 x 34 min. Ctrl+C here kills the tmux session, whose script stops
# the agents by PID. Never run on the laptop.
# shellcheck disable=SC2029 # the ssh commands below are meant to expand on this side
set -euo pipefail
SESSION=phonebox-resource

# ---------------------------------------------------------------- on the server, inside the tmux session
# globals, not locals: the EXIT trap must see them however the script ends
on_server() {
  run_dir="$1" quick="$2" runs="$3" commit="$4"
  src=/root/work/phonebox build=/root/work/resource/build res="$run_dir/results" work="$run_dir/work"
  if [ "$quick" = 1 ]; then phases="warmup=20,S0=60,S1=60,S2=60"; page=8; else phases="warmup=120,S0=600,S1=600,S2=720"; page=30; fi
  mkdir -p "$res" "$work"
  # shellcheck disable=SC1090,SC1091
  . ~/.cargo/env
  declare -gA PID=() PORT=() LANPORT=()
  helpers=()
  # shellcheck disable=SC2317 # called by the EXIT trap
  cleanup() {
    set +e
    local p
    for p in "${helpers[@]}" "${PID[@]}"; do
      if [ -n "$p" ] && kill -0 "$p" 2>/dev/null; then kill "$p"; fi # by PID: only what this script started
    done
    for p in "${helpers[@]}" "${PID[@]}"; do
      [ -n "$p" ] && wait "$p" 2>/dev/null
    done
    rm -rf "$work"
  }
  trap cleanup EXIT
  trap 'exit 129' HUP
  trap 'exit 130' INT
  trap 'exit 143' TERM
  free_port() { python3 -I -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])'; }
  fetch() { curl -s --max-time 5 -H "Host: 127.0.0.1:$1" "http://127.0.0.1:$1$2"; }
  listening() { [ -n "$(ss -Htln "sport = :$1")" ]; }

  echo "== building (C#: dotnet publish, self-contained single file, linux-x64; Rust: cargo build --release)"
  mkdir -p "$build/src/agent-rs" "$build/src/android/app/src/main/assets"
  rsync -a --delete --exclude bin --exclude obj "$src/agent/" "$build/src/agent/"
  rsync -a --delete "$src/agent-rs/assets/" "$build/src/agent-rs/assets/"
  rsync -a --delete "$src/android/app/src/main/assets/www/" "$build/src/android/app/src/main/assets/www/"
  rm -rf "$build/cs-out"
  dotnet publish "$build/src/agent/PhoneBox.Agent" -c Release -f net10.0 -r linux-x64 --self-contained true \
    -p:PublishSingleFile=true -o "$build/cs-out" --nologo -v q >"$res/dotnet-publish.log" 2>&1 \
    || { tail -30 "$res/dotnet-publish.log"; exit 1; }
  (cd "$src/agent-rs" && cargo build --release -q)
  rm -rf "$build/cs" "$build/rs"
  mkdir -p "$build/cs" "$build/rs"
  find "$build/cs-out" -maxdepth 1 -type f ! -name '*.pdb' -exec cp -p {} "$build/cs/" \;
  cp -p "$src/agent-rs/target/release/PhoneBoxAgent" "$src/agent-rs/THIRD-PARTY-NOTICES.txt" "$build/rs/"
  for label in cs rs; do
    python3 -I - "$build/$label" "$build/$label.zip" <<'PY'
import pathlib, sys, zipfile
folder = pathlib.Path(sys.argv[1])
with zipfile.ZipFile(sys.argv[2], "w", zipfile.ZIP_DEFLATED) as z:
    for p in sorted(folder.iterdir()):
        z.write(p, p.name)
PY
  done
  cs_installed=$(( $(du -sk "$build/cs" | cut -f1) * 1024 ))
  rs_installed=$(( $(du -sk "$build/rs" | cut -f1) * 1024 ))
  cs_zip=$(stat -c %s "$build/cs.zip")
  rs_zip=$(stat -c %s "$build/rs.zip")
  echo "   installed: C# $cs_installed bytes, Rust $rs_installed bytes; zips: C# $cs_zip, Rust $rs_zip"

  csv=()
  cs_version="?" rs_version="?"
  for run in $(seq 1 "$runs"); do
    if [ $((run % 2)) = 1 ]; then order="cs rs"; else order="rs cs"; fi
    out="$res/run$run"
    mkdir -p "$out"
    echo "== run $run of $runs (started in the order: $order)"
    for label in $order; do
      dir="$work/run$run/$label"
      mkdir -p "$dir/home"
      PORT[$label]="$(free_port)"
      LANPORT[$label]="$(free_port)"
      cp "$src/tools/parity/fixtures/lan-cert.pfx" "$dir/lan-cert.pfx"
      python3 -I - "$src/agent-rs/assets/config.default.linux.json" "$src/tools/parity/fixtures/lan.json" \
        "${LANPORT[$label]}" "$dir/config.json" <<'PY'
import json, re, sys

def strip_jsonc(text):
    out, i, n, in_str = [], 0, len(text), False
    while i < n:
        c = text[i]
        if in_str:
            out.append(text[i:i + 2] if c == "\\" else c)
            i += 2 if c == "\\" else 1
            in_str = c != '"'
            continue
        if c == '"':
            in_str = True
        elif text.startswith("//", i):
            j = text.find("\n", i)
            i = n if j < 0 else j
            continue
        elif text.startswith("/*", i):
            j = text.find("*/", i + 2)
            i = n if j < 0 else j + 2
            continue
        out.append(c)
        i += 1
    return re.sub(r",(\s*[}\]])", r"\1", "".join(out))

cfg = json.loads(strip_jsonc(open(sys.argv[1], encoding="utf-8").read()))
cfg["deviceSerial"] = "phonebox-resource-no-phone"
cfg.setdefault("updates", {})["auto"] = False
cfg.setdefault("charge", {})["enabled"] = False
cfg["lan"] = dict(json.load(open(sys.argv[2], encoding="utf-8"))["lan"], port=int(sys.argv[3]))
open(sys.argv[4], "w", encoding="utf-8").write(json.dumps(cfg, indent=2) + "\n")
PY
      taskset -c 0-9 env HOME="$dir/home" XDG_CONFIG_HOME="$dir/home" XDG_DATA_HOME="$dir/home" \
        "$build/$label/PhoneBoxAgent" --config "$dir/config.json" --port "${PORT[$label]}" >"$dir/agent.log" 2>&1 &
      PID[$label]=$!
    done
    for label in $order; do
      ready=""
      for _ in $(seq 1 90); do
        if fetch "${PORT[$label]}" /api/state | grep -q '"pc"'; then ready=1; break; fi
        kill -0 "${PID[$label]}" 2>/dev/null || break
        sleep 1
      done
      [ -n "$ready" ] || { tail -20 "$work/run$run/$label/agent.log"; echo "the $label agent did not answer"; exit 1; }
      echo "   $label: pid ${PID[$label]}, port ${PORT[$label]}, Wi-Fi ${LANPORT[$label]}"
    done
    agents=() pids=()
    for label in $order; do agents+=(--agent "$label=${PORT[$label]}"); pids+=(--pid "$label=${PID[$label]}"); done
    taskset -c 10-19 python3 -I "$run_dir/sample.py" --out "$out/samples.csv" --phase-file "$out/phase" "${pids[@]}" &
    sampler=$!
    helpers+=("$sampler")
    pidstat -h -u -r -d -p "${PID[cs]},${PID[rs]}" 5 >"$out/pidstat.txt" 2>&1 &
    pidstat_pid=$!
    helpers+=("$pidstat_pid")
    taskset -c 10-19 python3 -I "$run_dir/client.py" "${agents[@]}" --phases "$phases" --page-seconds "$page" \
      --phase-file "$out/phase" --log "$out/schedule.log" &
    client=$!
    helpers+=("$client")
    echo "   timeline $phases (pages of $page s) from $(date +%T)"
    wait "$client" || { echo "the client failed"; exit 1; }
    kill "$sampler" "$pidstat_pid" 2>/dev/null || true
    wait "$sampler" "$pidstat_pid" 2>/dev/null || true
    helpers=()
    for label in cs rs; do
      kill -0 "${PID[$label]}" 2>/dev/null || { tail -20 "$work/run$run/$label/agent.log"; echo "the $label agent died during the run"; exit 1; }
      version="$(fetch "${PORT[$label]}" /api/info | python3 -I -c 'import json, sys; print(json.load(sys.stdin).get("version", "?"))')"
      home_kb=$(du -sk "$work/run$run/$label/home" | cut -f1)
      log_bytes=$(stat -c %s "$work/run$run/$label/agent.log")
      echo "$label version=$version pid=${PID[$label]} home_kb=$home_kb stdout_bytes=$log_bytes" >>"$out/meta.txt"
      if [ "$label" = cs ]; then cs_version="$version"; else rs_version="$version"; fi
    done
    for label in $order; do
      kill "${PID[$label]}" # by PID: only the agent this run started
      for _ in $(seq 1 30); do kill -0 "${PID[$label]}" 2>/dev/null || break; sleep 0.5; done
      if kill -0 "${PID[$label]}" 2>/dev/null; then kill -9 "${PID[$label]}"; fi
      wait "${PID[$label]}" 2>/dev/null || true
      if listening "${PORT[$label]}" || listening "${LANPORT[$label]}"; then echo "a port of the $label agent is still open"; exit 1; fi
      PID[$label]=""
    done
    echo "   both agents stopped; rows: $(($(wc -l <"$out/samples.csv") - 1)), requests: $(grep -vc '^#' "$out/schedule.log")"
    grep -P '\tERR' "$out/schedule.log" | head -5 || true
    csv+=("$out/samples.csv")
  done

  echo "== report"
  quick_note=""
  [ "$quick" = 1 ] && quick_note=" QUICK RUN (proves the tooling; not a measurement)."
  python3 -I "$run_dir/report.py" "${csv[@]}" --out-dir "$res" --platform linux \
    --title "hetzner-iddhi, Linux x86_64, taskset -c 0-9, same config" \
    --label "cs=C# $cs_version" --label "rs=Rust $rs_version" \
    --installed-size "cs=$cs_installed" --installed-size "rs=$rs_installed" \
    --download-size "cs=$cs_zip" --download-size "rs=$rs_zip" --download-label "Download (linux-x64 zip)" \
    --note "Method: survey-c 4.5. Both agents side by side under taskset -c 0-9, own HOME/XDG, config.default.linux.json (dummy deviceSerial, updates and charge off, Wi-Fi on spare ports); /proc samples every 1 s, client.py's headless phone; $phases, pages $page s; $runs run(s), start order swapped. C#: dotnet publish self-contained single file linux-x64; Rust: cargo build --release; tree at $commit; $(date +%F).$quick_note"
}

if [ "${1:-}" = "--on-server" ]; then
  shift
  on_server "$@"
  exit
fi

# ---------------------------------------------------------------- on the laptop: sync, start, follow, copy back
# shellcheck disable=SC1091
. "$(dirname "${BASH_SOURCE[0]}")/_sync.sh"
QUICK=0
RUNS=2
OUT=""
while [ $# -gt 0 ]; do
  case "$1" in
    --quick) QUICK=1; shift ;;
    --runs) RUNS="$2"; shift 2 ;;
    --out) OUT="$2"; shift 2 ;;
    *) echo "unknown argument: $1"; exit 1 ;;
  esac
done
case "$RUNS" in '' | *[!0-9]* | 0) echo "--runs takes a whole number above 0"; exit 1 ;; esac
WORKSPACE="$ROOT/.superpowers/sdd/2026-11-22-rust-agent-plan-8-sign-off/resources"
if [ -z "$OUT" ]; then
  if [ "$QUICK" = 1 ]; then OUT="$WORKSPACE/server-quick"; else OUT="$WORKSPACE/server-$(date +%F)"; fi
fi
COMMIT="$(git -C "$ROOT" rev-parse --short HEAD)"
[ -z "$(git -C "$ROOT" status --porcelain)" ] || COMMIT="$COMMIT+uncommitted"
RUN_DIR="/root/work/resource/runs/$(date +%Y%m%d-%H%M%S)"

sync_tree
ssh "$REMOTE_HOST" bash -s -- "$RUN_DIR" "$QUICK" "$RUNS" "$COMMIT" <<'REMOTE'
set -euo pipefail
run_dir=$1 quick=$2 runs=$3 commit=$4
if tmux has-session -t phonebox-resource 2>/dev/null; then
  echo "a resource run is already going (tmux session phonebox-resource); stop it with: tmux kill-session -t phonebox-resource" >&2
  exit 1
fi
mkdir -p "$run_dir"
cp /root/work/phonebox/agent-rs/scripts/remote-resources.sh /root/work/phonebox/tools/resources/sample.py \
  /root/work/phonebox/tools/resources/client.py /root/work/phonebox/tools/resources/report.py "$run_dir/"
cmd="$(printf '%q ' bash "$run_dir/remote-resources.sh" --on-server "$run_dir" "$quick" "$runs" "$commit")"
tmux new-session -d -s phonebox-resource "$cmd >$(printf %q "$run_dir/run.log") 2>&1; echo \$? >$(printf %q "$run_dir/exit")"
REMOTE
echo "== started in tmux session $SESSION on $REMOTE_HOST ($RUN_DIR)"
stop_remote() {
  echo
  echo "== stopping the run (tmux session $SESSION)"
  ssh "$REMOTE_HOST" "tmux kill-session -t $SESSION" 2>/dev/null || true # its script stops the agents by PID
  exit 130
}
trap stop_remote INT TERM
CHUNK="$(mktemp /tmp/phonebox-resource-log.XXXXXX)"
OFFSET=0
running=1
while [ "$running" = 1 ]; do
  ssh "$REMOTE_HOST" "tmux has-session -t $SESSION 2>/dev/null" || running=0
  ssh "$REMOTE_HOST" "tail -c +$((OFFSET + 1)) $RUN_DIR/run.log 2>/dev/null" >"$CHUNK" || true
  OFFSET=$((OFFSET + $(wc -c <"$CHUNK")))
  cat "$CHUNK"
  [ "$running" = 0 ] || sleep 10
done
rm -f "$CHUNK"
trap - INT TERM
CODE="$(ssh "$REMOTE_HOST" "cat $RUN_DIR/exit 2>/dev/null" || echo '?')"
[ "$CODE" = 0 ] || { echo "the run failed (exit $CODE); its folder on the server: $RUN_DIR"; exit 1; }
mkdir -p "$OUT"
COPY="$(mktemp -d /tmp/phonebox-resource-copy.XXXXXX)"
scp -rq "$REMOTE_HOST:$RUN_DIR/results" "$COPY/"
scp -q "$REMOTE_HOST:$RUN_DIR/run.log" "$COPY/results/"
cp -R "$COPY/results/." "$OUT/"
rm -rf "$COPY"
echo "== results in $OUT"
