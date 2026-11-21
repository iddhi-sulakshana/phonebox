#!/usr/bin/env bash
# Usage: agent-rs/scripts/remote-update-check.sh
# The self-update round trip, end to end on the test server, before any real release exists: builds the agent as
# 1.0.0-alpha.1 (the tree) and as 1.0.0-alpha.2 (a bumped copy), serves both as release assets from tools/fake-github.py
# on 127.0.0.1, installs alpha.1 into a scratch HOME's install folder and starts it on a spare port with
# PHONEBOX_UPDATE_API; then POST /api/update/check and /api/update/apply must bring /api/info to 1.0.0-alpha.2 from a
# relaunched process. A release with a wrong checksum and one whose binary exits 1 must then be refused, with the
# running version and the installed file unchanged. No request may carry a token. Every process is stopped by PID.
set -euo pipefail
. "$(dirname "${BASH_SOURCE[0]}")/_sync.sh"
sync_tree
ssh "$REMOTE_HOST" "REMOTE_DIR=$REMOTE_DIR bash -s" <<'REMOTE'
set -euo pipefail
. ~/.cargo/env
repo_dir="$REMOTE_DIR"
work="$(mktemp -d /tmp/phonebox-update.XXXXXX)"
port=18871; gh_port=18872
fake_pid=""
say() { printf '== %s\n' "$*"; }
fail() { printf 'FAIL: %s\n' "$*" >&2; for f in "$work"/agent*.log; do [ -f "$f" ] && { echo "--- $f"; tail -n 40 "$f"; }; done; exit 1; }
agent_pid() { ss -ltnpH "sport = :$port" 2>/dev/null | sed -n 's/.*pid=\([0-9]*\).*/\1/p' | head -n1; }
cleanup() {
  local pid
  pid="$(agent_pid)"; [ -n "$pid" ] && kill "$pid" 2>/dev/null || true
  [ -n "$fake_pid" ] && kill "$fake_pid" 2>/dev/null || true
  sleep 1
  pid="$(agent_pid)"; [ -n "$pid" ] && kill -9 "$pid" 2>/dev/null || true
  rm -rf "$work"
}
trap cleanup EXIT
for p in "$port" "$gh_port"; do
  if ss -ltnH "sport = :$p" | grep -q .; then echo "port $p is taken" >&2; exit 1; fi
done
info() { curl -fsS --max-time 3 "http://127.0.0.1:$port/api/info"; }
version_of() { python3 -c 'import json, sys; print(json.load(sys.stdin)["version"])'; }
update_field() { curl -fsS --max-time 3 "http://127.0.0.1:$port/api/update" | python3 -c "import json, sys; print(json.load(sys.stdin)['status'].get('$1') or '')"; }
wait_for() { # wait_for seconds command...
  local end=$((SECONDS + $1)); shift
  until "$@"; do [ "$SECONDS" -lt "$end" ] || return 1; sleep 0.5; done
}
sha() { sha256sum "$1" | cut -d' ' -f1; }

say "building 1.0.0-alpha.1 (the tree) and 1.0.0-alpha.2 (a bumped copy)"
cd "$repo_dir/agent-rs"
cargo build --release --locked -q -p phonebox-agent
mkdir -p "$work/a1" "$work/a2" "$work/src" "$work/gh"
cp target/release/PhoneBoxAgent THIRD-PARTY-NOTICES.txt "$work/a1/"
# the copy embeds the panel from beside it (android/), as the tree does; the settings page and catalogs come with it
rsync -a --exclude target ./ "$work/src/agent-rs/"
ln -s "$repo_dir/android" "$work/src/android"
sed -i 's/^version = "1.0.0-alpha.1"$/version = "1.0.0-alpha.2"/' "$work/src/agent-rs/Cargo.toml"
grep -q '^version = "1.0.0-alpha.2"$' "$work/src/agent-rs/Cargo.toml" || fail "could not bump the copy"
(cd "$work/src/agent-rs" && CARGO_TARGET_DIR="$repo_dir/agent-rs/target/update-check" cargo build --release --offline -q -p phonebox-agent)
cp "$repo_dir/agent-rs/target/update-check/release/PhoneBoxAgent" THIRD-PARTY-NOTICES.txt "$work/a2/"
rid="linux-$(uname -m | sed 's/x86_64/x64/; s/aarch64/arm64/')"

# a release in the fake GitHub's folder: release <tag> <folder with the zip's two files> [sums override]
publish() {
  local tag="$1" from="$2" v="${1#v}" zip sums
  zip="PhoneBoxAgent-$v-$rid.zip"; sums="SHA256SUMS-$v.txt"
  python3 "$repo_dir/tools/zip-agent.py" "$from" "$work/gh/$zip" >/dev/null
  if [ "${3:-}" = wrong-sum ]; then printf '%064d  %s\n' 0 "$zip" > "$work/gh/$sums"
  else (cd "$work/gh" && sha256sum "$zip" > "$sums"); fi
  python3 "$repo_dir/tools/fake-github.py" release "$gh_port" "$tag" "$zip" "$sums" > "$work/gh/release-$v.json"
}
listing() { # listing <versions...>: releases.json, the first listed first
  python3 - "$work/gh" "$@" <<'PY'
import json, pathlib, sys
folder = pathlib.Path(sys.argv[1])
releases = [json.loads((folder / f'release-{v}.json').read_text()) for v in sys.argv[2:]]
(folder / 'releases.json').write_text(json.dumps(releases))
PY
}
publish v1.0.0-alpha.1 "$work/a1"
publish v1.0.0-alpha.2 "$work/a2"
listing 1.0.0-alpha.2 1.0.0-alpha.1
python3 "$repo_dir/tools/fake-github.py" "$work/gh" "$gh_port" &
fake_pid=$!

say "installing 1.0.0-alpha.1 under a scratch HOME"
export HOME="$work/home"; mkdir -p "$HOME"
unset XDG_DATA_HOME XDG_CONFIG_HOME XDG_CACHE_HOME XDG_STATE_HOME GITHUB_TOKEN GH_TOKEN
export PHONEBOX_UPDATE_API="http://127.0.0.1:$gh_port"
app="$("$work/a1/PhoneBoxAgent" --print-install-dir | tail -n1)"
case "$app" in "$HOME"/*) ;; *) fail "install folder $app is outside the scratch HOME" ;; esac
mkdir -p "$app"; cp "$work/a1/PhoneBoxAgent" "$app/PhoneBoxAgent"
cat > "$work/config.json" <<JSON
{"deviceSerial": "__update_check__", "updates": {"auto": false}, "lan": {"enabled": false}, "pages": [{"template": "home"}]}
JSON
(cd /tmp && nohup "$app/PhoneBoxAgent" --config "$work/config.json" --port "$port" > "$work/agent1.log" 2>&1 &)
up() { info >/dev/null 2>&1; }
wait_for 30 up || fail "alpha.1 did not answer"
[ "$(info | version_of)" = 1.0.0-alpha.1 ] || fail "not alpha.1"
first="$(agent_pid)"

say "check and apply: 1.0.0-alpha.1 -> 1.0.0-alpha.2"
curl -fsS -X POST "http://127.0.0.1:$port/api/update/check" >/dev/null
[ "$(update_field latest)" = 1.0.0-alpha.2 ] || fail "the check did not find alpha.2: $(curl -fsS "http://127.0.0.1:$port/api/update")"
[ "$(update_field agentUpdate)" = True ] || fail "alpha.2 is not offered as an agent update"
code="$(curl -sS -o /dev/null -w '%{http_code}' -X POST "http://127.0.0.1:$port/api/update/apply")"
[ "$code" = 202 ] || fail "apply answered $code"
new_version() { [ "$(info 2>/dev/null | version_of 2>/dev/null)" = 1.0.0-alpha.2 ]; }
wait_for 90 new_version || fail "the agent did not come back as alpha.2"
second="$(agent_pid)"
[ -n "$second" ] && [ "$second" != "$first" ] || fail "no new process ($first -> $second)"
! kill -0 "$first" 2>/dev/null || fail "alpha.1 ($first) is still running"
[ "$(sha "$app/PhoneBoxAgent")" = "$(sha "$work/a2/PhoneBoxAgent")" ] || fail "the installed file is not alpha.2"
gone_old() { [ ! -e "$app/PhoneBoxAgent.old" ]; }
wait_for 10 gone_old || fail "PhoneBoxAgent.old was not cleaned up by the new process"
say "ok: alpha.2 runs as PID $second; alpha.1 ($first) ended; the old file was cleaned up"

say "a release with a wrong checksum is refused"
mkdir -p "$work/a3"; cp "$work/a2/PhoneBoxAgent" "$work/a2/THIRD-PARTY-NOTICES.txt" "$work/a3/"
publish v1.0.0-alpha.3 "$work/a3" wrong-sum
listing 1.0.0-alpha.3 1.0.0-alpha.2 1.0.0-alpha.1
curl -fsS -X POST "http://127.0.0.1:$port/api/update/check" >/dev/null
[ "$(update_field latest)" = 1.0.0-alpha.3 ] || fail "the check did not find alpha.3"
curl -fsS -o /dev/null -X POST "http://127.0.0.1:$port/api/update/apply"
has_error() { [ -n "$(update_field error)" ] && [ "$(update_field state)" = idle ]; }
wait_for 60 has_error || fail "no error after a wrong checksum"
say "status: $(update_field error)"
[ "$(info | version_of)" = 1.0.0-alpha.2 ] && [ "$(agent_pid)" = "$second" ] || fail "the agent changed after a wrong checksum"
[ "$(sha "$app/PhoneBoxAgent")" = "$(sha "$work/a2/PhoneBoxAgent")" ] || fail "the installed file changed after a wrong checksum"

say "a release whose program exits 1 is refused"
mkdir -p "$work/a4"; printf '#!/bin/sh\nexit 1\n' > "$work/a4/PhoneBoxAgent"; chmod +x "$work/a4/PhoneBoxAgent"
cp "$work/a2/THIRD-PARTY-NOTICES.txt" "$work/a4/"
publish v1.0.0-alpha.4 "$work/a4"
listing 1.0.0-alpha.4 1.0.0-alpha.3 1.0.0-alpha.2 1.0.0-alpha.1
curl -fsS -X POST "http://127.0.0.1:$port/api/update/check" >/dev/null
[ "$(update_field latest)" = 1.0.0-alpha.4 ] || fail "the check did not find alpha.4"
curl -fsS -o /dev/null -X POST "http://127.0.0.1:$port/api/update/apply"
does_not_start() { update_field error | grep -q 'does not start'; }
wait_for 60 does_not_start || fail "no refusal for a program that exits 1: $(update_field error)"
say "status: $(update_field error)"
[ "$(info | version_of)" = 1.0.0-alpha.2 ] && [ "$(agent_pid)" = "$second" ] || fail "the agent changed after a failing program"
[ "$(sha "$app/PhoneBoxAgent")" = "$(sha "$work/a2/PhoneBoxAgent")" ] || fail "the installed file changed after a failing program"

if grep -q 'authorization=yes' "$work/gh/requests.log"; then fail "a request to the fake GitHub carried a token"; fi
say "requests to the fake GitHub: $(wc -l < "$work/gh/requests.log"), none with a token"
echo "update check: passed"
REMOTE
