#!/usr/bin/env bash
# Usage: agent-rs/scripts/remote-cross-check.sh [target...]
# Runs `cargo clippy --workspace --all-targets -- -D warnings` on the test server for the Windows and macOS targets
# (or the ones given), so platform code is compile-checked before CI sees it. Nothing is linked or run. Without the
# default `tls` feature: rustls's crypto (ring) builds C with the target platform's toolchain, which this server lacks;
# the TLS code is the same on every platform and remote-check.sh checks it.
set -euo pipefail
. "$(dirname "${BASH_SOURCE[0]}")/_sync.sh"
targets=("$@")
[ ${#targets[@]} -gt 0 ] || targets=(x86_64-pc-windows-msvc aarch64-apple-darwin x86_64-apple-darwin)
sync_tree
ssh "$REMOTE_HOST" "bash -c 'set -e; cd $REMOTE_DIR/agent-rs; . ~/.cargo/env
  rustup target add $(quote_args "${targets[@]}") >/dev/null 2>&1
  for t in $(quote_args "${targets[@]}"); do
    echo \"== \$t\"
    cargo clippy --workspace --all-targets --no-default-features --target \"\$t\" -q -- -D warnings
  done
  echo cross-check: clean'"
