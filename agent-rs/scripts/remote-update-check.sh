#!/usr/bin/env bash
# Usage: agent-rs/scripts/remote-update-check.sh
# The self-update round trip, end to end on the test server, against releases that tools/fake-github.py serves on
# 127.0.0.1 (the agent reaches it through PHONEBOX_UPDATE_API). T is the tree's version; N is a bumped copy (x.y.z ->
# x.y.(z+1), or alpha.K -> alpha.K+1).
#   - When T is final (1.0.0 and later), a copy built as T-alpha.1 is installed first: a pre-release agent lists the
#     releases and takes the highest one, so check and apply must bring it to T (the alpha -> 1.0.0 switch).
#   - Then T (installed, or switched to) checks the way its kind does (a final agent asks for /releases/latest, a
#     pre-release agent lists the releases), and apply must bring /api/info to N from a relaunched process.
#   - A release with a wrong checksum and one whose binary exits 1 must then be refused, with the running version and
#     the installed file unchanged.
# The agents run from a scratch HOME's install folder on a spare port. No request may carry a token. Every process is
# stopped by PID.
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
running() { [ "$(info 2>/dev/null | version_of 2>/dev/null)" = "$1" ]; }
update_field() { curl -fsS --max-time 3 "http://127.0.0.1:$port/api/update" | python3 -c "import json, sys; print(json.load(sys.stdin)['status'].get('$1') or '')"; }
wait_for() { # wait_for seconds command...
  local end=$((SECONDS + $1)); shift
  until "$@"; do [ "$SECONDS" -lt "$end" ] || return 1; sleep 0.5; done
}
sha() { sha256sum "$1" | cut -d' ' -f1; }

# T, then N and the two versions after it (the refused releases), then T-alpha.1 when T is final ("-" otherwise)
tree_v="$(sed -n 's/^version = "\(.*\)"$/\1/p' "$repo_dir/agent-rs/Cargo.toml" | head -n1)"
read -r next_v bad_v broken_v pre_v < <(python3 - "$tree_v" <<'PY'
import re, sys
v = sys.argv[1]
m = re.fullmatch(r'(\d+)\.(\d+)\.(\d+)(?:-alpha\.(\d+))?', v)
assert m, f'{v} is neither x.y.z nor x.y.z-alpha.N'
x, y, z, a = m.groups()
if a is None:
    print(*[f'{x}.{y}.{int(z) + i}' for i in (1, 2, 3)], f'{v}-alpha.1')
else:
    print(*[f'{x}.{y}.{z}-alpha.{int(a) + i}' for i in (1, 2, 3)], '-')
PY
)
[ -n "$pre_v" ] || fail "could not work out the versions from $tree_v"

# build_copy <version> <folder>: the agent from a copy of the tree with that version (the copy embeds the panel from
# beside it, android/, as the tree does; the settings page and catalogs come with agent-rs/)
build_copy() {
  rm -rf "$work/src"; mkdir -p "$work/src" "$2"
  rsync -a --exclude target "$repo_dir/agent-rs/" "$work/src/agent-rs/"
  ln -s "$repo_dir/android" "$work/src/android"
  sed -i "0,/^version = \".*\"\$/s//version = \"$1\"/" "$work/src/agent-rs/Cargo.toml"
  grep -q "^version = \"$1\"\$" "$work/src/agent-rs/Cargo.toml" || fail "could not set the copy to $1"
  (cd "$work/src/agent-rs" && CARGO_TARGET_DIR="$repo_dir/agent-rs/target/update-check" cargo build --release --offline -q -p phonebox-agent)
  cp "$repo_dir/agent-rs/target/update-check/release/PhoneBoxAgent" "$repo_dir/agent-rs/THIRD-PARTY-NOTICES.txt" "$2/"
}

say "building $tree_v (the tree), $next_v$( [ "$pre_v" = - ] || echo " and $pre_v") (copies)"
cd "$repo_dir/agent-rs"
cargo build --release --locked -q -p phonebox-agent
mkdir -p "$work/tree" "$work/gh"
cp target/release/PhoneBoxAgent THIRD-PARTY-NOTICES.txt "$work/tree/"
build_copy "$next_v" "$work/next"
[ "$pre_v" = - ] || build_copy "$pre_v" "$work/pre"
rid="linux-$(uname -m | sed 's/x86_64/x64/; s/aarch64/arm64/')"

# publish <version> <folder with the zip's two files> [wrong-sum]: a release in the fake GitHub's folder (a
# pre-release when the version has a "-")
publish() {
  local v="$1" from="$2" zip sums
  zip="PhoneBoxAgent-$v-$rid.zip"; sums="SHA256SUMS-$v.txt"
  python3 "$repo_dir/tools/zip-agent.py" "$from" "$work/gh/$zip" >/dev/null
  if [ "${3:-}" = wrong-sum ]; then printf '%064d  %s\n' 0 "$zip" > "$work/gh/$sums"
  else (cd "$work/gh" && sha256sum "$zip" > "$sums"); fi
  python3 "$repo_dir/tools/fake-github.py" release "$gh_port" "v$v" "$zip" "$sums" > "$work/gh/release-$v.json"
}
listing() { # listing <versions...>: releases.json, the first listed first
  python3 - "$work/gh" "$@" <<'PY'
import json, pathlib, sys
folder = pathlib.Path(sys.argv[1])
releases = [json.loads((folder / f'release-{v}.json').read_text()) for v in sys.argv[2:]]
(folder / 'releases.json').write_text(json.dumps(releases))
PY
}
python3 "$repo_dir/tools/fake-github.py" "$work/gh" "$gh_port" &
fake_pid=$!

export HOME="$work/home"; mkdir -p "$HOME"
unset XDG_DATA_HOME XDG_CONFIG_HOME XDG_CACHE_HOME XDG_STATE_HOME GITHUB_TOKEN GH_TOKEN
export PHONEBOX_UPDATE_API="http://127.0.0.1:$gh_port"
app="$("$work/tree/PhoneBoxAgent" --print-install-dir | tail -n1)"
case "$app" in "$HOME"/*) ;; *) fail "install folder $app is outside the scratch HOME" ;; esac
cat > "$work/config.json" <<JSON
{"deviceSerial": "__update_check__", "updates": {"auto": false}, "lan": {"enabled": false}, "pages": [{"template": "home"}]}
JSON

# start_installed <folder> <version>: its program copied into the install folder and started on the spare port
start_installed() {
  say "installing $2 under a scratch HOME"
  mkdir -p "$app"; cp "$1/PhoneBoxAgent" "$app/PhoneBoxAgent"
  (cd /tmp && nohup "$app/PhoneBoxAgent" --config "$work/config.json" --port "$port" > "$work/agent-$2.log" 2>&1 &)
  wait_for 30 running "$2" || fail "$2 did not answer as $2"
  current_pid="$(agent_pid)"
}

# update_to <version> <folder>: check and apply must relaunch the agent as that version, from that folder's program
update_to() {
  local from_v from_pid="$current_pid"
  from_v="$(info | version_of)"
  say "check and apply: $from_v -> $1"
  curl -fsS -X POST "http://127.0.0.1:$port/api/update/check" >/dev/null
  [ "$(update_field latest)" = "$1" ] || fail "the check did not find $1: $(curl -fsS "http://127.0.0.1:$port/api/update")"
  [ "$(update_field agentUpdate)" = True ] || fail "$1 is not offered as an agent update"
  code="$(curl -sS -o /dev/null -w '%{http_code}' -X POST "http://127.0.0.1:$port/api/update/apply")"
  [ "$code" = 202 ] || fail "apply answered $code"
  wait_for 90 running "$1" || fail "the agent did not come back as $1"
  current_pid="$(agent_pid)"
  [ -n "$current_pid" ] && [ "$current_pid" != "$from_pid" ] || fail "no new process ($from_pid -> $current_pid)"
  ! kill -0 "$from_pid" 2>/dev/null || fail "$from_v ($from_pid) is still running"
  [ "$(sha "$app/PhoneBoxAgent")" = "$(sha "$2/PhoneBoxAgent")" ] || fail "the installed file is not $1"
  gone_old() { [ ! -e "$app/PhoneBoxAgent.old" ]; }
  wait_for 10 gone_old || fail "PhoneBoxAgent.old was not cleaned up by the new process"
  say "ok: $1 runs as PID $current_pid; $from_v ($from_pid) ended; the old file was cleaned up"
}

publish "$tree_v" "$work/tree"
if [ "$pre_v" != - ]; then
  publish "$pre_v" "$work/pre"
  listing "$tree_v" "$pre_v"
  start_installed "$work/pre" "$pre_v"
  update_to "$tree_v" "$work/tree"
  older=("$tree_v" "$pre_v")
else
  listing "$tree_v"
  start_installed "$work/tree" "$tree_v"
  older=("$tree_v")
fi

publish "$next_v" "$work/next"
listing "$next_v" "${older[@]}"
update_to "$next_v" "$work/next"
settled="$current_pid"

say "a release with a wrong checksum is refused"
mkdir -p "$work/bad"; cp "$work/next/PhoneBoxAgent" "$work/next/THIRD-PARTY-NOTICES.txt" "$work/bad/"
publish "$bad_v" "$work/bad" wrong-sum
listing "$bad_v" "$next_v" "${older[@]}"
curl -fsS -X POST "http://127.0.0.1:$port/api/update/check" >/dev/null
[ "$(update_field latest)" = "$bad_v" ] || fail "the check did not find $bad_v"
curl -fsS -o /dev/null -X POST "http://127.0.0.1:$port/api/update/apply"
has_error() { [ -n "$(update_field error)" ] && [ "$(update_field state)" = idle ]; }
wait_for 60 has_error || fail "no error after a wrong checksum"
say "status: $(update_field error)"
running "$next_v" && [ "$(agent_pid)" = "$settled" ] || fail "the agent changed after a wrong checksum"
[ "$(sha "$app/PhoneBoxAgent")" = "$(sha "$work/next/PhoneBoxAgent")" ] || fail "the installed file changed after a wrong checksum"

say "a release whose program exits 1 is refused"
mkdir -p "$work/broken"; printf '#!/bin/sh\nexit 1\n' > "$work/broken/PhoneBoxAgent"; chmod +x "$work/broken/PhoneBoxAgent"
cp "$work/next/THIRD-PARTY-NOTICES.txt" "$work/broken/"
publish "$broken_v" "$work/broken"
listing "$broken_v" "$bad_v" "$next_v" "${older[@]}"
curl -fsS -X POST "http://127.0.0.1:$port/api/update/check" >/dev/null
[ "$(update_field latest)" = "$broken_v" ] || fail "the check did not find $broken_v"
curl -fsS -o /dev/null -X POST "http://127.0.0.1:$port/api/update/apply"
does_not_start() { update_field error | grep -q 'does not start'; }
wait_for 60 does_not_start || fail "no refusal for a program that exits 1: $(update_field error)"
say "status: $(update_field error)"
running "$next_v" && [ "$(agent_pid)" = "$settled" ] || fail "the agent changed after a failing program"
[ "$(sha "$app/PhoneBoxAgent")" = "$(sha "$work/next/PhoneBoxAgent")" ] || fail "the installed file changed after a failing program"

if grep -q 'authorization=yes' "$work/gh/requests.log"; then fail "a request to the fake GitHub carried a token"; fi
grep -q '/releases/latest' "$work/gh/requests.log" || [ "$pre_v" = - ] || fail "the final agent never asked for /releases/latest"
say "requests to the fake GitHub: $(wc -l < "$work/gh/requests.log"), none with a token"
echo "update check: passed"
REMOTE
