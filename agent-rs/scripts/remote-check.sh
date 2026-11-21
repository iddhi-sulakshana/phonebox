#!/usr/bin/env bash
# Usage: agent-rs/scripts/remote-check.sh [test-name filters...]
# On the test server: `cargo fmt --check`, `cargo clippy -D warnings` (host target) and `cargo test` (with the
# filters); the exit code is the first failure's.
set -euo pipefail
. "$(dirname "${BASH_SOURCE[0]}")/_sync.sh"
sync_tree
ssh "$REMOTE_HOST" "bash -c 'set -eo pipefail; cd $REMOTE_DIR/agent-rs && . ~/.cargo/env
  cargo fmt --all --check
  cargo clippy --workspace --all-targets -q -- -D warnings
  cargo test --workspace -q -- $(quote_args "$@") 2>&1 | grep -E \"^test result: .* [1-9][0-9]* passed|FAILED|panicked|^error\" '"
