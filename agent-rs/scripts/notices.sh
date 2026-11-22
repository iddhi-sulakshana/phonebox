#!/usr/bin/env bash
# Usage: agent-rs/scripts/notices.sh
# Regenerates agent-rs/THIRD-PARTY-NOTICES.txt (shipped in every agent zip) with cargo-about on the test server, then
# copies it back (LF line ends: some crates' license files use CRLF); tools/check-notices.py checks it. Run it after
# any dependency change.
set -euo pipefail
. "$(dirname "${BASH_SOURCE[0]}")/_sync.sh"
sync_tree
ssh "$REMOTE_HOST" "bash -c 'set -e; cd $REMOTE_DIR/agent-rs; . ~/.cargo/env
  cargo about --version >/dev/null 2>&1 || cargo install --locked --features cli cargo-about@0.9.3
  cargo fetch --locked -q
  cargo about generate --locked --fail -c about.toml -o THIRD-PARTY-NOTICES.txt about.hbs
  sed -i \"s/\\r\$//\" THIRD-PARTY-NOTICES.txt'"
scp -q "$REMOTE_HOST:$REMOTE_DIR/agent-rs/THIRD-PARTY-NOTICES.txt" "$ROOT/agent-rs/THIRD-PARTY-NOTICES.txt"
echo "notices: agent-rs/THIRD-PARTY-NOTICES.txt"
