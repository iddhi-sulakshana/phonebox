#!/usr/bin/env bash
# MacBook desk-controls check (the owner-approved laptop exception to "tests run on the server"): the Rust agent runs
# next to the installed C# agent on a spare port, and the two are compared on what the desk widgets read:
#   - `media`, `audio` and `toggles` in /api/state (positions within 3 s; cover ids only by presence),
#   - /api/apps (support, the open permission, and every app's id and name, in order),
#   - one app icon from the Rust agent (a PNG),
#   - /api/volume set to the current level when not muted (no audible change), answered with that level,
#   - /api/shortcuts/test with /usr/bin/true on both agents,
#   - Wi-Fi (when the installed agent has it on): the Rust agent reads the installed agent's lan-cert.pfx (copied) and
#     serves it on a spare port; /api/pair matches the installed agent's (pc, fingerprint, key, hosts), the served
#     certificate has that fingerprint, and the phone key opens /api/state over HTTPS.
# It never controls media, sends keys or opens apps.
#
#   agent-rs/scripts/mac-desk-check.sh
#
# The installed agent keeps running untouched. The Rust agent gets a copy of its config (never printed) whose
# deviceSerial matches no phone, so it leaves the phone's adb reverse rule alone, and a temporary home for its log,
# data and lock; it is stopped by PID on exit, also after Ctrl+C or an error.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
CONFIG="$HOME/Library/Application Support/PhoneBox/config.json"
INSTALLED=8765

[ "$(uname)" = Darwin ] || { echo "macOS only"; exit 1; }
[ -f "$CONFIG" ] || { echo "the installed agent's config was not found"; exit 1; }
fetch() { curl -s --max-time "${3:-5}" -H "Host: 127.0.0.1:$1" "http://127.0.0.1:$1$2"; }
fetch "$INSTALLED" /api/state | grep -q '"pc"' || { echo "the installed agent does not answer on port $INSTALLED"; exit 1; }

echo "== building the Rust agent"
(cd "$ROOT/agent-rs" && cargo build --release -q)

WORK="$(mktemp -d /tmp/phonebox-desk-check.XXXXXX)"
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

free_port() { python3 -I -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])'; }
PORT="$(free_port)"
LANPORT="$(free_port)"
install -m 600 /dev/null "$WORK/config.json"
python3 -I - "$CONFIG" "$WORK/config.json" "$LANPORT" <<'PY'
import json, sys
cfg = json.load(open(sys.argv[1], encoding="utf-8"))
cfg["deviceSerial"] = "phonebox-desk-check-no-phone"  # the Rust agent must not touch the phone's reverse rule
if isinstance(cfg.get("lan"), dict) and cfg["lan"].get("enabled"):
    cfg["lan"]["port"] = int(sys.argv[3])  # the installed agent keeps its own Wi-Fi port
open(sys.argv[2], "w", encoding="utf-8").write(json.dumps(cfg))
PY
CERT="$(dirname "$CONFIG")/lan-cert.pfx"
WIFI=""
if python3 -I -c 'import json, sys; sys.exit(0 if (json.load(open(sys.argv[1])).get("lan") or {}).get("enabled") else 1)' "$CONFIG" \
  && [ -f "$CERT" ]; then
  WIFI=1
  install -m 600 "$CERT" "$WORK/lan-cert.pfx" # next to the copied config, where the agent looks for it
fi
mkdir -p "$WORK/home"
echo "== starting the Rust agent on port $PORT"
HOME="$WORK/home" "$ROOT/agent-rs/target/release/PhoneBoxAgent" --config "$WORK/config.json" --port "$PORT" \
  >"$WORK/rust.log" 2>&1 &
RUST_PID=$!
for _ in $(seq 1 60); do
  fetch "$PORT" /api/state | grep -q '"pc"' && break
  kill -0 "$RUST_PID" 2>/dev/null || { grep -v -i token "$WORK/rust.log"; echo "the Rust agent exited"; exit 1; }
  sleep 1
done
sleep 4 # the media and audio monitors' first readings

echo "== comparing media, audio, toggles and the app list"
# both agents cache live readings (the volume, the position) and refresh them in the background: a difference is only
# reported if it is still there after a few tries
for attempt in 1 2 3 4; do
fetch "$INSTALLED" /api/state >"$WORK/csharp-state.json"
fetch "$PORT" /api/state >"$WORK/rust-state.json"
fetch "$INSTALLED" /api/apps 60 >"$WORK/csharp-apps.json"
fetch "$PORT" /api/apps 60 >"$WORK/rust-apps.json"
NOWPLAYING="$(command -v nowplaying-cli || true)"
{ [ -n "$NOWPLAYING" ] && "$NOWPLAYING" get-raw 2>/dev/null; } >"$WORK/now-playing.json" || true
if python3 -I - "$WORK" "$HOME" "$attempt" <<'PY'
import hashlib, json, sys
from pathlib import Path
work, real_home, attempt = Path(sys.argv[1]), Path(sys.argv[2]), int(sys.argv[3])
load = lambda name: json.loads((work / name).read_text(encoding="utf-8"))
cs, rs = load("csharp-state.json"), load("rust-state.json")
problems = []

def media_view(m):
    if m is None:
        return None
    return {k: m.get(k) for k in ("app", "title", "artist", "album", "playing", "durMs", "canSeek")} | {
        "hasArt": (m.get("artId") or 0) > 0}

def position_comparable():
    """A source that publishes no Timestamp (and is not Spotify or Music, which are asked for their position) gives
    only an ElapsedTime that does not move: each agent counts from when it first saw the track, so an agent started
    later shows less. Positions are compared only when the source dates them."""
    try:
        raw = json.loads((work / "now-playing.json").read_text(encoding="utf-8") or "{}")
    except ValueError:
        raw = {}
    if not isinstance(raw, dict) or not raw:
        return True  # the AppleScript fallback: the player itself reports the position
    prefix = "kMRMediaRemoteNowPlayingInfo"
    return prefix + "Timestamp" in raw or raw.get(prefix + "ClientBundleIdentifier") in ("com.spotify.client", "com.apple.Music")

cm, rm = cs.get("media"), rs.get("media")
if media_view(cm) != media_view(rm):
    problems.append(f"media: {media_view(cm)} != {media_view(rm)}")
elif cm is not None and position_comparable() and abs((cm.get("posMs") or 0) - (rm.get("posMs") or 0)) > 3000:
    problems.append(f"media position: {cm.get('posMs')} vs {rm.get('posMs')} ms")
ca, ra = cs.get("audio"), rs.get("audio")
if (ca is None) != (ra is None):
    problems.append(f"audio: {ca} != {ra}")
elif ca is not None:
    if ca.get("muted") != ra.get("muted") or ca.get("canControl") != ra.get("canControl"):
        problems.append(f"audio: {ca} != {ra}")
    elif abs((ca.get("volume") or 0) - (ra.get("volume") or 0)) > 1:
        problems.append(f"audio volume: {ca.get('volume')} vs {ra.get('volume')}")
if cs.get("toggles") != rs.get("toggles"):
    problems.append(f"toggles: {cs.get('toggles')} != {rs.get('toggles')}")
capps, rapps = load("csharp-apps.json"), load("rust-apps.json")
for key in ("supported", "canOpen"):
    if capps.get(key) != rapps.get(key):
        problems.append(f"apps {key}: {capps.get(key)} != {rapps.get(key)}")
# the Rust agent runs with a temporary home, so it cannot see ~/Applications: leave those apps out of the C# list
def home_ids():
    root, ids = real_home / "Applications", set()
    if not root.is_dir():
        return ids
    for d in root.iterdir():
        if d.name.startswith(".") or not d.is_dir():
            continue
        found = [d] if d.name.lower().endswith(".app") else [
            a for a in (d.iterdir() if d.is_dir() else []) if a.is_dir() and a.name.lower().endswith(".app")
            and not a.name.startswith(".")]
        ids |= {hashlib.sha256(str(a).encode()).hexdigest()[:16] for a in found}
    return ids
skipped = home_ids()
capps["apps"] = [a for a in capps.get("apps", []) if a["id"] not in skipped]
pairs = lambda a: [(x["id"], x["name"]) for x in a.get("apps", [])]
if pairs(capps) != pairs(rapps):
    only_c = sorted(set(pairs(capps)) - set(pairs(rapps)))[:5]
    only_r = sorted(set(pairs(rapps)) - set(pairs(capps)))[:5]
    problems.append(f"apps differ ({len(pairs(capps))} vs {len(pairs(rapps))}; only C#: {only_c}; only Rust: {only_r})")
if problems:
    print("\n".join(f"   try {attempt}: DIFFERENT " + p for p in problems))
    sys.exit(1)
print(f"   media: {media_view(rm)}")
print(f"   audio: {ra}; toggles: {len(rs.get('toggles') or {})}; apps: {len(pairs(rapps))} "
      f"(and {len(skipped)} in ~/Applications, which the Rust agent's temporary home hides)")
PY
then break; fi
[ "$attempt" -lt 4 ] || exit 1
sleep 3
done

echo "== one app icon"
APP_ID="$(python3 -I - "$WORK/rust-apps.json" <<'PY'
import json, sys
apps = json.load(open(sys.argv[1], encoding="utf-8")).get("apps", [])
preferred = [a for a in apps if a["name"] in ("Safari", "Calculator", "Notes", "System Settings")]
print((preferred or apps or [{"id": ""}])[0]["id"])
PY
)"
[ -n "$APP_ID" ] || { echo "   no apps listed"; exit 1; }
CODE="$(curl -s --max-time 30 -o "$WORK/icon.png" -w '%{http_code}' -H "Host: 127.0.0.1:$PORT" \
  "http://127.0.0.1:$PORT/api/apps/icon?id=$APP_ID")"
MAGIC="$(head -c 8 "$WORK/icon.png" | od -An -tx1 | tr -d ' \n')"
[ "$CODE" = 200 ] && [ "$MAGIC" = 89504e470d0a1a0a ] || { echo "   the icon was not a PNG (HTTP $CODE)"; exit 1; }
echo "   $(wc -c <"$WORK/icon.png" | tr -d ' ') bytes of PNG"

echo "== volume to its current level"
LEVEL="$(python3 -I - "$WORK/rust-state.json" <<'PY'
import json, sys
a = json.load(open(sys.argv[1], encoding="utf-8")).get("audio") or {}
print(a["volume"] if a.get("canControl") and not a.get("muted") and a.get("volume") is not None else "")
PY
)"
if [ -n "$LEVEL" ]; then
  ANSWER="$(curl -s --max-time 10 -X POST -H "Host: 127.0.0.1:$PORT" "http://127.0.0.1:$PORT/api/volume?level=$LEVEL")"
  python3 -I -c '
import json, sys
a, level = json.loads(sys.argv[1]), int(sys.argv[2])
assert a is not None and abs(a["volume"] - level) <= 1 and not a["muted"], f"answered {a} for {level}"
print(f"   {level} -> {a}")' "$ANSWER" "$LEVEL"
else
  echo "   skipped (muted, or the volume cannot be set here)"
fi

echo "== shortcut test with /usr/bin/true"
test_run() {
  curl -s --max-time 10 -o /dev/null -w '%{http_code}' -X POST -H "Host: 127.0.0.1:$1" \
    -H 'Content-Type: application/json' --data '{"cmd":"/usr/bin/true"}' "http://127.0.0.1:$1/api/shortcuts/test"
}
C_CODE="$(test_run "$INSTALLED")"
R_CODE="$(test_run "$PORT")"
[ "$C_CODE" = "$R_CODE" ] && [ "$R_CODE" = 200 ] || { echo "   C# answered $C_CODE, Rust $R_CODE"; exit 1; }
echo "   both 200"

if [ -n "$WIFI" ]; then
  echo "== Wi-Fi: the installed agent's certificate and key, served by the Rust agent on port $LANPORT"
  fetch "$INSTALLED" /api/pair >"$WORK/csharp-pair.json"
  fetch "$PORT" /api/pair >"$WORK/rust-pair.json"
  chmod 600 "$WORK/csharp-pair.json" "$WORK/rust-pair.json"
  python3 -I - "$WORK" "$LANPORT" <<'PY'
import json, sys
from pathlib import Path
work, lanport = Path(sys.argv[1]), int(sys.argv[2])
c = json.loads((work / "csharp-pair.json").read_text(encoding="utf-8"))
r = json.loads((work / "rust-pair.json").read_text(encoding="utf-8"))
problems = [f"{k} differs" for k in ("enabled", "pc", "fingerprint", "token", "hosts") if c.get(k) != r.get(k)]
if list(c.keys()) != list(r.keys()):
    problems.append(f"keys {list(c.keys())} != {list(r.keys())}")
if r.get("port") != lanport:
    problems.append(f"port {r.get('port')} != {lanport}")
if problems or not r.get("enabled"):
    sys.exit("   DIFFERENT /api/pair: " + "; ".join(problems or ["Wi-Fi is off in the Rust agent"]))
# the key goes to curl in a header file, never on a command line or in the output
(work / "auth").write_text(f"Authorization: Bearer {r['token']}\n", encoding="utf-8")
(work / "auth").chmod(0o600)
(work / "fingerprint").write_text(r["fingerprint"], encoding="utf-8")
print(f"   /api/pair matches: pc {r['pc']}, fingerprint {r['fingerprint'][:16]}..., the same key, hosts {r['hosts']}")
PY
  SERVED="$(openssl s_client -connect "127.0.0.1:$LANPORT" -servername phonebox </dev/null 2>/dev/null \
    | openssl x509 -outform der 2>/dev/null | shasum -a 256 | cut -d' ' -f1)"
  [ "$SERVED" = "$(cat "$WORK/fingerprint")" ] || { echo "   the served certificate is not the installed one ($SERVED)"; exit 1; }
  echo "   the Wi-Fi port serves that certificate"
  CODE="$(curl -sk --max-time 10 -o /dev/null -w '%{http_code}' -H @"$WORK/auth" "https://127.0.0.1:$LANPORT/api/state")"
  NOKEY="$(curl -sk --max-time 10 -o /dev/null -w '%{http_code}' "https://127.0.0.1:$LANPORT/api/state")"
  [ "$CODE" = 200 ] && [ "$NOKEY" = 403 ] || { echo "   over HTTPS: $CODE with the phone key, $NOKEY without"; exit 1; }
  echo "   over HTTPS: 200 with the phone key, 403 without"
fi

echo "desk controls match"
