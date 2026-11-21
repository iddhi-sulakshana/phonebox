#!/usr/bin/env bash
# Usage: agent-rs/scripts/remote-ui-live.sh [playwright test args...]
# The browser suite of tools/ui-tests/live against live agents on the test server. Builds a self-contained C# agent
# (the server's SDK can publish a portable linux-x64 one) and the Rust agent there, then runs the suite in the
# Playwright container of the version tools/ui-tests/package.json pins, once per agent: C# first (it proves the suite),
# then Rust. The agents run inside the container on its bridge network, so their Wi-Fi ports (0.0.0.0) never reach the
# server's public interface. UI_LIVE_AGENTS="rust" (or "csharp") runs one leg only.
# The run uses a copy of the tree on the server (<repo>-ui-live/tree), so a sync from another session cannot change
# files under a running suite; the reports stay there (ui-test-report/live-<agent>), and a failed test's agent folder
# under tools/ui-tests/test-results/tmp.
set -euo pipefail
. "$(dirname "${BASH_SOURCE[0]}")/_sync.sh"
legs="${UI_LIVE_AGENTS:-csharp rust}"
sync_tree
ssh "$REMOTE_HOST" "bash -s -- $(quote_args "$REMOTE_DIR" "$legs" "$@")" <<'REMOTE'
set -euo pipefail
src="$1"; legs="$2"; shift 2
work="$src-ui-live"
cd "$src"
. ~/.cargo/env
version="$(sed -n 's/.*"@playwright\/test": *"\([^"]*\)".*/\1/p' tools/ui-tests/package.json)"
image="mcr.microsoft.com/playwright:v$version-noble"
mkdir -p "$work/agents"
for leg in $legs; do
  case "$leg" in
    csharp)
      dotnet publish agent/PhoneBox.Agent -c Release -f net10.0 -r linux-x64 --self-contained true \
        -p:PublishSingleFile=true -o "$work/agents/csharp" -v q -nologo </dev/null >/tmp/ui-live-dotnet-publish.log 2>&1 \
        || { tail -20 /tmp/ui-live-dotnet-publish.log; echo "the C# publish failed; UI_LIVE_AGENTS=rust runs the Rust leg alone" >&2; exit 1; }
      ;;
    rust)
      (cd agent-rs && cargo build --release --locked -q -p phonebox-agent </dev/null)
      mkdir -p "$work/agents/rust"
      cp agent-rs/target/release/PhoneBoxAgent "$work/agents/rust/"
      ;;
    *) echo "unknown agent '$leg' in UI_LIVE_AGENTS (csharp, rust)" >&2; exit 2 ;;
  esac
done
# node_modules, the reports and the kept agent folders stay in the copy between runs
rsync -a --delete --exclude .git --exclude node_modules --exclude target --exclude 'agent/**/bin' --exclude 'agent/**/obj' \
  --exclude /ui-test-report --exclude /tools/ui-tests/test-results ./ "$work/tree/"
docker image inspect "$image" >/dev/null 2>&1 || docker pull -q "$image" </dev/null
name="phonebox-ui-live-$$"
# the container this run started, by name, whatever way the run ends
trap 'docker rm -f "$name" >/dev/null 2>&1 || true' EXIT
trap 'exit 130' HUP INT TERM
status=0
docker run --rm --init --ipc=host --name "$name" \
  -v "$work/tree:/work" -v "$work/agents:/agents:ro" -w /work/tools/ui-tests \
  -e CI=1 -e LEGS="$legs" -e TMPDIR=/work/tools/ui-tests/test-results/tmp \
  "$image" bash -c '
    rm -rf "$TMPDIR" && mkdir -p "$TMPDIR"
    npm ci --no-audit --no-fund >/tmp/npm-ci.log 2>&1 || { cat /tmp/npm-ci.log; exit 1; }
    status=0
    for leg in $LEGS; do
      echo "== the live suite against the $leg agent"
      PHONEBOX_AGENT_CMD="/agents/$leg/PhoneBoxAgent" PHONEBOX_AGENT_LABEL="$leg" npx playwright test -c live "$@" || status=1
    done
    exit $status' _ "$@" </dev/null || status=$?
for leg in $legs; do echo "report ($leg): $work/tree/ui-test-report/live-$leg"; done
exit $status
REMOTE
