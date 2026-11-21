#!/usr/bin/env bash
# MacBook integrations check (the owner-approved laptop exception to "tests run on the server"): the Rust agent runs
# next to the installed C# agent on a spare port, and the two are compared on what plan 6 ported:
#   - pushed values (the Rust agent starts from a copy of the installed agent's data.json),
#   - feeds and calendars (by shape and titles), the photo list, Home Assistant, OBS, Discord and Spotify status,
#   - rules, privacy and the phone's last battery report (mirrored to the Rust agent, whose copy has charge switching
#     off so its shortcuts never run),
#   - the deck state (actions, shortcuts, audio outputs, monitors, power plans),
#   - a note edited on the Rust agent's copy of the config (saved there with a new version; a stale version is a 409),
#   - a radio station (added to the copy only) that plays through ffplay or mpv and stops again. Starting a station
#     pauses whatever else plays, as the deck does.
#
#   agent-rs/scripts/mac-integrations-check.sh
#
# The installed agent keeps running untouched, and its config.json is checked unchanged at the end. The Rust agent gets
# a copy of the config (never printed) whose deviceSerial matches no phone, and a temporary home for its data, caches,
# log and lock; it is stopped by PID on exit, also after Ctrl+C or an error.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
DIR="$HOME/Library/Application Support/PhoneBox"
CONFIG="$DIR/config.json"
INSTALLED=8765
STATION="https://ice1.somafm.com/groovesalad-128-mp3"

[ "$(uname)" = Darwin ] || { echo "macOS only"; exit 1; }
[ -f "$CONFIG" ] || { echo "the installed agent's config was not found"; exit 1; }
command -v ffplay >/dev/null || command -v mpv >/dev/null || { echo "install ffplay or mpv first"; exit 1; }
fetch() { curl -s --max-time "${3:-5}" -H "Host: 127.0.0.1:$1" "http://127.0.0.1:$1$2"; }
post() { curl -s --max-time "${3:-10}" -X POST -H "Host: 127.0.0.1:$1" "http://127.0.0.1:$1$2"; }
fetch "$INSTALLED" /api/state | grep -q '"pc"' || { echo "the installed agent does not answer on port $INSTALLED"; exit 1; }
BEFORE="$(shasum -a 256 "$CONFIG" | cut -d' ' -f1)"

echo "== building the Rust agent"
(cd "$ROOT/agent-rs" && cargo build --release -q)

WORK="$(mktemp -d /tmp/phonebox-integrations-check.XXXXXX)"
RUST_PID=""
cleanup() {
  set +e
  if [ -n "$RUST_PID" ] && kill -0 "$RUST_PID" 2>/dev/null; then
    kill "$RUST_PID" # by PID: only the agent this script started (it stops its own players)
    wait "$RUST_PID" 2>/dev/null
  fi
  rm -rf "$WORK"
}
trap cleanup EXIT

free_port() { python3 -I -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])'; }
PORT="$(free_port)"
LANPORT="$(free_port)"
RUST_DIR="$WORK/home/Library/Application Support/PhoneBox"
mkdir -p "$RUST_DIR"
install -m 600 /dev/null "$WORK/config.json"
python3 -I - "$CONFIG" "$WORK/config.json" "$LANPORT" "$STATION" <<'PY'
import json, sys
cfg = json.load(open(sys.argv[1], encoding="utf-8"))
cfg["deviceSerial"] = "phonebox-integrations-check-no-phone"  # the Rust agent must not touch the phone's reverse rule
if isinstance(cfg.get("lan"), dict) and cfg["lan"].get("enabled"):
    cfg["lan"]["port"] = int(sys.argv[3])  # the installed agent keeps its own Wi-Fi port
if isinstance(cfg.get("charge"), dict):
    cfg["charge"]["enabled"] = False  # the mirrored battery report must never run the owner's charge shortcuts
controls = cfg.setdefault("controls", {})
controls.setdefault("actions", []).append({"id": "mac-check-radio", "label": "Check FM", "kind": "radio",
                                           "target": sys.argv[4], "value": 15, "enabled": True})
open(sys.argv[2], "w", encoding="utf-8").write(json.dumps(cfg, indent=2))
PY
[ -f "$DIR/data.json" ] && install -m 600 "$DIR/data.json" "$RUST_DIR/data.json"
echo "== starting the Rust agent on port $PORT"
HOME="$WORK/home" "$ROOT/agent-rs/target/release/PhoneBoxAgent" --config "$WORK/config.json" --port "$PORT" \
  >"$WORK/rust.log" 2>&1 &
RUST_PID=$!
for _ in $(seq 1 60); do
  fetch "$PORT" /api/state | grep -q '"pc"' && break
  kill -0 "$RUST_PID" 2>/dev/null || { grep -v -i token "$WORK/rust.log"; echo "the Rust agent exited"; exit 1; }
  sleep 1
done

echo "== mirroring the phone's last battery report"
fetch "$INSTALLED" /api/charge >"$WORK/csharp-charge.json"
REPORT="$(python3 -I - "$WORK/csharp-charge.json" <<'PY'
import json, sys, urllib.parse
p = (json.load(open(sys.argv[1], encoding="utf-8")) or {}).get("phone")
if p:
    q = {"level": p["level"], "plugged": str(p["plugged"]).lower()}
    q.update({k: p[k] for k in ("temp", "source", "usb") if p.get(k) is not None})
    print(urllib.parse.urlencode(q))
PY
)"
if [ -n "$REPORT" ]; then
  post "$PORT" "/api/phone/battery?$REPORT" >/dev/null
  echo "   mirrored"
else
  echo "   the installed agent has no report yet"
fi

echo "== comparing (the Rust agent's feeds, calendars, photos and connections get up to a minute)"
for attempt in $(seq 1 12); do
  for path in /api/push /api/feeds /api/calendar /api/photos /api/ha/states /api/obs /api/discord \
    /api/controls/spotify/state /api/state /api/charge /api/controls/state; do
    name="$(echo "$path" | tr '/' '_')"
    fetch "$INSTALLED" "$path" 30 >"$WORK/csharp$name.json"
    fetch "$PORT" "$path" 30 >"$WORK/rust$name.json"
  done
  if python3 -I - "$WORK" "$attempt" <<'PY'
import json, sys
from pathlib import Path
work, attempt = Path(sys.argv[1]), int(sys.argv[2])
def pair(path):
    name = path.replace("/", "_")
    load = lambda side: json.loads((work / f"{side}{name}.json").read_text(encoding="utf-8") or "null")
    return load("csharp"), load("rust")
problems, notes = [], []
def same(label, c, r):
    if c != r:
        problems.append(f"{label}: {c!r} != {r!r}")

c, r = pair("/api/push")
same("pushed keys", sorted(c["items"]), sorted(r["items"]))
for key in set(c["items"]) & set(r["items"]):
    same(f"pushed value {key}", c["items"][key], r["items"][key])
notes.append(f"{len(r['items'])} pushed values")

c, r = pair("/api/feeds")
if r["updated"] == 0 and c["updated"] != 0:
    problems.append("feeds: the Rust agent has not refreshed yet")
same("feed titles", sorted(i["title"] for i in c["items"]), sorted(i["title"] for i in r["items"]))
same("feed keys", list(c), list(r))
same("feeds canOpen", c["canOpen"], r["canOpen"])
notes.append(f"{len(r['items'])} feed items")

c, r = pair("/api/calendar")
same("calendar titles", sorted((e["title"], e["start"]) for e in c["events"]), sorted((e["title"], e["start"]) for e in r["events"]))
same("calendar keys", list(c), list(r))
notes.append(f"{len(r['events'])} events")

c, r = pair("/api/photos")
if r["scanning"] or c["scanning"]:
    problems.append("photos: still scanning")
same("photo ids", c["ids"], r["ids"])
same("photo error", c["error"], r["error"])
notes.append(f"{len(r['ids'])} photos")

c, r = pair("/api/ha/states")
same("Home Assistant", {k: c[k] for k in ("configured", "error")}, {k: r[k] for k in ("configured", "error")})
c, r = pair("/api/obs")
view = lambda o: (o["enabled"], o["control"], (o["snapshot"] or {}).get("connected"), (o["snapshot"] or {}).get("scene"))
same("OBS", view(c), view(r))
notes.append(f"OBS enabled {r['enabled']}, connected {(r['snapshot'] or {}).get('connected')}")
c, r = pair("/api/discord")
view = lambda o: (o["enabled"], o["control"], (o["snapshot"] or {}).get("connected"), (o["snapshot"] or {}).get("error"))
same("Discord", view(c), view(r))
c, r = pair("/api/controls/spotify/state")
same("Spotify", (c.get("connected"), c.get("error")), (r.get("connected"), r.get("error")))

c, r = pair("/api/state")
for key in ("rules", "rulesPaused", "privacy"):
    same(key, c.get(key), r.get(key))

c, r = pair("/api/charge")
same("charge keys", list(c), list(r))
if c.get("phone"):
    strip = lambda p: {k: v for k, v in p.items() if k != "at"}
    same("phone battery", strip(c["phone"]), strip(r.get("phone") or {}))

c, r = pair("/api/controls/state")
same("deck keys", list(c), list(r))
items = lambda s: [(i["id"], i["name"], i["kind"], i["state"], i["available"]) for i in s.get("actions", [])
                   if i["id"] != "mac-check-radio"]
same("deck actions", items(c), items(r))
shortcuts = lambda s: [(i["id"], i["name"], i["state"], i["available"]) for i in s.get("shortcuts", [])]
same("deck shortcuts", shortcuts(c), shortcuts(r))
outputs = lambda s: [(d["id"], d["name"]) for d in (s.get("audio") or {}).get("devices", [])]
same("deck outputs", outputs(c), outputs(r))
for key in ("platform", "monitors", "powerPlans", "midi", "allowListening", "allowPower"):
    same(f"deck {key}", c.get(key), r.get(key))
if problems:
    print("\n".join(f"   try {attempt}: DIFFERENT " + p for p in problems))
    sys.exit(1)
print("   the same: " + ", ".join(notes))
PY
  then break; fi
  [ "$attempt" -lt 12 ] || exit 1
  sleep 5
done

echo "== a note on the Rust agent's copy of the config"
NOTE="$(python3 -I - "$WORK/config.json" <<'PY'
import json, sys
cfg = json.load(open(sys.argv[1], encoding="utf-8"))
for page in cfg.get("pages", []):
    for w in page.get("widgets", []):
        if w.get("type") == "note":
            print(f"{page['id']}/{w['id']}")
            raise SystemExit
PY
)"
if [ -n "$NOTE" ]; then
  VERSION="$(post "$PORT" "/api/widget/$NOTE/note?text=phonebox-check" | python3 -I -c 'import json, sys; print(json.load(sys.stdin)["version"])')"
  grep -q '"phonebox-check"' "$WORK/config.json" || { echo "   the note was not saved in the copy"; exit 1; }
  STALE="$(curl -s --max-time 10 -o /dev/null -w '%{http_code}' -X POST -H "Host: 127.0.0.1:$PORT" \
    "http://127.0.0.1:$PORT/api/widget/$NOTE/note?text=again&version=1")"
  [ "$STALE" = 409 ] || { echo "   a stale version answered $STALE, not 409"; exit 1; }
  echo "   saved (version $VERSION); a stale version is a 409"
else
  echo "   skipped: no note widget"
fi

echo "== a radio station through the deck"
state_of() {
  fetch "$PORT" /api/controls/state | python3 -I -c '
import json, sys
s = json.load(sys.stdin)
print(next((i["state"] for i in s["actions"] if i["id"] == "mac-check-radio"), "missing"))'
}
players() { pgrep -P "$RUST_PID" -l | grep -E 'ffplay|mpv' || true; }
ANSWER="$(post "$PORT" "/api/controls/command/action?id=mac-check-radio" 30)"
echo "$ANSWER" | grep -q '"ok":true' || { echo "   the station did not start: $ANSWER"; exit 1; }
sleep 3
[ "$(state_of)" = Playing ] && [ -n "$(players)" ] || { echo "   not playing: $(state_of)"; exit 1; }
echo "   playing through $(players | awk '{print $2}')"
ANSWER="$(post "$PORT" "/api/controls/command/action?id=mac-check-radio&op=stop" 30)"
echo "$ANSWER" | grep -q '"ok":true' || { echo "   the station did not stop: $ANSWER"; exit 1; }
sleep 1
[ "$(state_of)" = Stopped ] && [ -z "$(players)" ] || { echo "   still playing: $(state_of)"; exit 1; }
echo "   stopped, and its player is gone"

AFTER="$(shasum -a 256 "$CONFIG" | cut -d' ' -f1)"
[ "$BEFORE" = "$AFTER" ] || { echo "the installed agent's config changed during the check"; exit 1; }
echo "integrations match; the installed agent's config is unchanged"
