#!/usr/bin/env bash
# Shared by the remote-*.sh scripts: copies the working tree (uncommitted changes included) to the test server.
# Tests and servers never run on the laptop; they run on hetzner-iddhi.
set -euo pipefail
REMOTE_HOST="${PHONEBOX_TEST_HOST:-hetzner-iddhi}"
REMOTE_DIR="/root/work/phonebox"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
sync_tree() {
  # sourced from a shell without BASH_SOURCE (zsh), ROOT would be a folder far above the repository: never sync that
  [ -f "$ROOT/agent-rs/scripts/_sync.sh" ] && [ -d "$ROOT/.git" ] || { echo "refusing to sync: $ROOT is not the repository" >&2; return 1; }
  ssh "$REMOTE_HOST" "mkdir -p $REMOTE_DIR"
  # excluded paths are also protected from --delete on the server, so its build caches survive
  rsync -az --delete \
    --exclude .git --exclude .superpowers --exclude target --exclude node_modules \
    --exclude android/.gradle --exclude android/app/build --exclude dist \
    --exclude 'agent/**/bin' --exclude 'agent/**/obj' \
    "$ROOT/" "$REMOTE_HOST:$REMOTE_DIR/"
}
quote_args() { local out="" a; for a in "$@"; do out+="$(printf '%q ' "$a")"; done; printf '%s' "$out"; }
