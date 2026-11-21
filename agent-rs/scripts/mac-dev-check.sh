#!/usr/bin/env bash
# MacBook AI and dev check (the owner-approved laptop exception to "tests run on the server"): the Rust agent stands in
# for the installed C# agent on port 8765 for a moment, then the installed agent comes back.
#
#   agent-rs/scripts/mac-dev-check.sh            # the script answers the permission prompt itself
#   agent-rs/scripts/mac-dev-check.sh --phone    # the phone shows the prompt and the owner answers Allow there
#
# The Rust agent runs with the owner's config (copied, never printed) and a temporary home where ~/.claude, ~/.codex,
# ~/.workbuddy, ~/.config/gh and ~/Library/Keychains are links to the real ones (the agent only reads them: this
# script never installs or removes the hook) and whose PhoneBox folder holds copies of the quota files, and with the
# PATH the installed agent's launchd job sets (the tools it finds depend on it). Then:
#   - /api/work lists the same sessions as the installed agent did just before it stopped (id, tool, name, project),
#     and GitHub as the same signed-in user,
#   - /api/quotas lists the same providers (id, name, plan, limit labels) and the same overview sections,
#   - a Claude Code permission prompt: the hook command from ~/.claude/settings.json runs with a PermissionRequest for
#     `echo phonebox-dev-check` on stdin, as Claude Code runs it. With --phone the card reaches the phone and the owner
#     answers Allow there; otherwise the script stands in for the phone (a live stream with the app's header), sees the
#     approval event, finds the card in /api/approvals and answers it with a POST. Either way the hook must print the
#     allow decision. Any other prompt that reaches the stand-in meanwhile is handed back to its terminal (skip).
# The installed agent is loaded again on exit, also after Ctrl+C or an error.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
PLIST="$HOME/Library/LaunchAgents/net.oxog.phonebox.agent.plist"
APPDIR="$HOME/Library/Application Support/PhoneBox"
CONFIG="$APPDIR/config.json"
SETTINGS="$HOME/.claude/settings.json"
PORT=8765
PHONE=""
while [ $# -gt 0 ]; do
  case "$1" in
    --phone) PHONE=1; shift ;;
    *) echo "unknown argument: $1"; exit 1 ;;
  esac
done

[ "$(uname)" = Darwin ] || { echo "macOS only"; exit 1; }
[ -f "$PLIST" ] && [ -f "$CONFIG" ] || { echo "the installed agent was not found"; exit 1; }
[ -f "$SETTINGS" ] || { echo "~/.claude/settings.json was not found"; exit 1; }

fetch() { curl -s --max-time "${2:-5}" -H "Host: 127.0.0.1:$PORT" "http://127.0.0.1:$PORT$1"; }
post() { curl -s --max-time 5 -o /dev/null -w '%{http_code}' -X POST -H "Host: 127.0.0.1:$PORT" "http://127.0.0.1:$PORT$1"; }
info() { fetch /api/info 2 || true; }
port_pid() { lsof -ti "tcp:$1" -sTCP:LISTEN 2>/dev/null || true; }
show_log() { grep -v -i token "$WORK/rust.log" | sed 's/^/  /' || true; } # the agent logs no keys; filter anyway

info | grep -q '"version"' || { echo "the installed agent does not answer on port $PORT"; exit 1; }
# the hook as Claude Code runs it: the PermissionRequest command that posts to the agent's endpoint
HOOK="$(python3 -I - "$SETTINGS" "$PORT" <<'PY'
import json, sys
settings, port = json.load(open(sys.argv[1], encoding="utf-8")), sys.argv[2]
for entry in (settings.get("hooks") or {}).get("PermissionRequest") or []:
    for hook in entry.get("hooks") or []:
        command = hook.get("command") or ""
        if "/api/hooks/claude/permission" in command:
            if f"127.0.0.1:{port}/" not in command:
                sys.exit(f"the installed hook does not post to port {port}")
            print(command)
            sys.exit(0)
sys.exit("the PhoneBox hook is not installed in ~/.claude/settings.json")
PY
)"

echo "== building the Rust agent"
(cd "$ROOT/agent-rs" && cargo build --release -q)

WORK="$(mktemp -d /tmp/phonebox-dev-check.XXXXXX)"
RUST_PID=""
STREAM_PID=""
HOOK_PID=""
restore() {
  set +e
  for pid in "$HOOK_PID" "$STREAM_PID"; do
    [ -n "$pid" ] && kill "$pid" 2>/dev/null # by PID: only what this script started
  done
  if [ -n "$RUST_PID" ] && kill -0 "$RUST_PID" 2>/dev/null; then
    kill "$RUST_PID"
    wait "$RUST_PID" 2>/dev/null
  fi
  echo "== loading the installed agent again"
  launchctl load "$PLIST" 2>/dev/null
  for _ in $(seq 1 60); do info | grep -q '"version"' && break; sleep 1; done
  info | python3 -I -c 'import json, sys
d = json.load(sys.stdin)
print("installed agent:", d["version"], "| connected:", d["connected"], "| via:", d["via"])' \
    || echo "the installed agent did not answer on port $PORT"
  rm -rf "$WORK"
}
trap restore EXIT

echo "== what the installed agent lists"
fetch /api/work >"$WORK/csharp-work.json"
fetch /api/quotas 30 >"$WORK/csharp-quotas.json"
chmod 600 "$WORK/csharp-work.json" "$WORK/csharp-quotas.json"
echo "== stopping the installed agent (pid $(port_pid "$PORT"))"
launchctl unload "$PLIST" # unloaded, launchd does not start it again (KeepAlive)
for _ in $(seq 1 30); do [ -z "$(port_pid "$PORT")" ] && break; sleep 0.5; done
[ -z "$(port_pid "$PORT")" ] || { echo "port $PORT is still in use"; exit 1; }
LANPORT="$(python3 -I -c 'import json, sys; l = json.load(open(sys.argv[1])).get("lan") or {}; print(l.get("port", 8766) if l.get("enabled") else "")' "$CONFIG")"
if [ -n "$LANPORT" ]; then
  for _ in $(seq 1 30); do [ -z "$(port_pid "$LANPORT")" ] && break; sleep 0.5; done
  [ -z "$(port_pid "$LANPORT")" ] || { echo "Wi-Fi port $LANPORT is still in use"; exit 1; }
fi

install -m 600 "$CONFIG" "$WORK/config.json"
if [ -f "$APPDIR/lan-cert.pfx" ]; then install -m 600 "$APPDIR/lan-cert.pfx" "$WORK/lan-cert.pfx"; fi
H="$WORK/home"
mkdir -p "$H/Library/Application Support/PhoneBox" "$H/.config"
for d in .claude .codex .workbuddy .config/gh Library/Keychains; do
  if [ -e "$HOME/$d" ]; then ln -s "$HOME/$d" "$H/$d"; fi
done
for f in "$APPDIR"/claude-quota*.json; do
  if [ -f "$f" ]; then install -m 600 "$f" "$H/Library/Application Support/PhoneBox/"; fi
done
# the tools the agent finds (gh, docker, CodexBar, agy) depend on PATH: the one launchd gives the installed agent, not
# this terminal's
AGENT_PATH="$(plutil -extract EnvironmentVariables.PATH raw -o - "$PLIST" 2>/dev/null || echo /usr/bin:/bin:/usr/sbin:/sbin)"
echo "== starting the Rust agent on port $PORT"
HOME="$H" PATH="$AGENT_PATH" "$ROOT/agent-rs/target/release/PhoneBoxAgent" --config "$WORK/config.json" \
  --port "$PORT" >"$WORK/rust.log" 2>&1 &
RUST_PID=$!
for _ in $(seq 1 30); do info | grep -q '"version"' && break; sleep 1; done
info | grep -q '"version"' || { show_log; echo "the Rust agent does not answer"; exit 1; }

echo "== /api/work and /api/quotas against the installed agent's"
# the session list every 2 s, GitHub after 3 s, the quotas' first round at once (CodexBar can take a while): a
# difference is only reported if it is still there after 30 s
for attempt in $(seq 1 10); do
  sleep 3
  fetch /api/work >"$WORK/rust-work.json"
  fetch /api/quotas 30 >"$WORK/rust-quotas.json"
  chmod 600 "$WORK/rust-work.json" "$WORK/rust-quotas.json"
  if python3 -I - "$WORK" "$attempt" <<'PY'
import json, sys
from pathlib import Path
work, attempt = Path(sys.argv[1]), int(sys.argv[2])
load = lambda name: json.loads((work / name).read_text(encoding="utf-8"))
cw, rw, cq, rq = load("csharp-work.json"), load("rust-work.json"), load("csharp-quotas.json"), load("rust-quotas.json")
problems = []
session = lambda s: (s.get("id"), s.get("tool"), s.get("name"), s.get("project"))
cs, rs = sorted(map(session, cw.get("sessions") or [])), sorted(map(session, rw.get("sessions") or []))
if cs != rs:
    problems.append(f"sessions: only C# {sorted(set(cs) - set(rs))}; only Rust {sorted(set(rs) - set(cs))}")
cg, rg = cw.get("github") or {}, rw.get("github") or {}
if cg.get("user") and (cg.get("user") != rg.get("user") or rg.get("error")):
    problems.append(f"github: {'the same user' if cg.get('user') == rg.get('user') else 'not the same user'}, "
                    f"error {rg.get('error')!r}")
provider = lambda p: (p.get("id"), p.get("name"), p.get("plan"), [l.get("label") for l in p.get("limits") or []])
cp, rp = list(map(provider, cq.get("providers") or [])), list(map(provider, rq.get("providers") or []))
if cp != rp:
    problems.append(f"providers: {cp} != {rp}")
sections = lambda o: None if o is None else list(o.keys())
if sections(cq.get("overview")) != sections(rq.get("overview")):
    problems.append(f"overview: {sections(cq.get('overview'))} != {sections(rq.get('overview'))}")
if problems:
    print("\n".join(f"   try {attempt}: DIFFERENT " + p for p in problems))
    sys.exit(1)
print(f"   sessions: {len(rs)} the same ({', '.join(s[3] or s[2] for s in rs)})")
print(f"   github: {'signed in as the same user' if rg.get('user') else 'not signed in on either'}; "
      f"{rg.get('notifications', 0)} notifications, {len(rg.get('myPrs') or [])} own PRs")
print(f"   quotas: {[p[0] for p in rp] or 'no providers on either'}; overview: {sections(rq.get('overview'))}")
for p in rq.get("providers") or []:
    if p.get("error"):
        print(f"     {p['id']}: {p['error']}")
PY
  then break; fi
  [ "$attempt" -lt 10 ] || { show_log; exit 1; }
done

echo "== a Claude Code permission prompt through the installed hook"
python3 -I - "$ROOT" >"$WORK/prompt.json" <<'PY'
import json, sys
print(json.dumps({
    "session_id": "phonebox-dev-check", "transcript_path": "", "cwd": sys.argv[1], "permission_mode": "default",
    "hook_event_name": "PermissionRequest", "tool_name": "Bash",
    "tool_input": {"command": "echo phonebox-dev-check", "description": "PhoneBox dev check"},
}))
PY
if [ -z "$PHONE" ]; then
  # the phone, as far as the agent can tell: a live stream with the app's header
  curl -sN -H "Host: 127.0.0.1:$PORT" -H "X-PhoneBox-App: dev-check" "http://127.0.0.1:$PORT/api/stream" \
    >"$WORK/stream.txt" 2>/dev/null &
  STREAM_PID=$!
  sleep 2
else
  python3 -I -c 'import json, sys; d = json.load(sys.stdin); sys.exit(0 if d.get("connected") else "the phone is not connected")' \
    <<<"$(info)"
fi
sh -c "$HOOK" <"$WORK/prompt.json" >"$WORK/hook-out.json" 2>/dev/null &
HOOK_PID=$!
ID=""
for _ in $(seq 1 20); do
  ID="$(fetch /api/approvals | python3 -I -c '
import json, sys
items = [a for a in json.load(sys.stdin).get("items", []) if a.get("session") == "phonebox-dev-check"]
print(items[0]["id"] if items else "")')"
  [ -n "$ID" ] && break
  sleep 0.5
done
[ -n "$ID" ] || { show_log; echo "   the prompt never reached /api/approvals"; exit 1; }
fetch /api/approvals >"$WORK/approvals.json"
python3 -I - "$WORK/approvals.json" "$ID" <<'PY'
import json, sys
a = [a for a in json.load(open(sys.argv[1], encoding="utf-8"))["items"] if a["id"] == sys.argv[2]][0]
left = (a["expiresAt"] - a["createdAt"]) // 1000
print(f"   listed: {a['tool']} in {a['project']}: {a['detail']} ({a['kind']}, expires in {left} s)")
PY
if [ -n "$PHONE" ]; then
  echo "== answer Allow on the phone (the card says \"echo phonebox-dev-check\")"
else
  grep -q '"kind":"approval"' "$WORK/stream.txt" || { echo "   the stream had no approval event"; exit 1; }
  echo "   the stream sent the approval event"
  CODE="$(post "/api/approvals/$ID?decision=allow")"
  [ "$CODE" = 200 ] || { echo "   POST /api/approvals/$ID answered $CODE"; exit 1; }
  echo "   answered Allow with a POST"
fi
for _ in $(seq 1 120); do kill -0 "$HOOK_PID" 2>/dev/null || break; sleep 1; done
wait "$HOOK_PID" 2>/dev/null || true
HOOK_PID=""
python3 -I -c '
import json, sys
out = open(sys.argv[1], encoding="utf-8").read()
d = json.loads(out) if out.strip() else {}
behavior = ((d.get("hookSpecificOutput") or {}).get("decision") or {}).get("behavior")
sys.exit(0 if behavior == "allow" else f"   the hook printed {out!r}")' "$WORK/hook-out.json"
echo "   the hook printed the allow decision"
# anything else that reached the stand-in goes back to its terminal
for other in $(fetch /api/approvals | python3 -I -c 'import json, sys; print(" ".join(a["id"] for a in json.load(sys.stdin).get("items", [])))'); do
  post "/api/approvals/$other?decision=skip" >/dev/null
done
grep -qF "approval $ID: allow from the phone" "$WORK/rust.log" || { show_log; echo "   the log has no decision line"; exit 1; }

echo "== Rust agent log"
show_log
echo "AI and dev match"
