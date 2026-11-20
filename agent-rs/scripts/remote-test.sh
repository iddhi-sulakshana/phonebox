#!/usr/bin/env bash
# Usage: agent-rs/scripts/remote-test.sh [test-name filters...]
# Runs `cargo test --workspace -- <filters>` in agent-rs/ on the test server; the exit code is cargo's.
set -euo pipefail
. "$(dirname "${BASH_SOURCE[0]}")/_sync.sh"
sync_tree
ssh "$REMOTE_HOST" "bash -c 'cd $REMOTE_DIR/agent-rs && . ~/.cargo/env && cargo test --workspace -- $(quote_args "$@")'"
