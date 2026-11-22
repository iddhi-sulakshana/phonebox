#!/usr/bin/env bash
# Usage: agent-rs/scripts/remote-parity.sh <corpus.json> [parity.py args...]
# Builds the C# reference agent and the Rust agent on the test server and runs tools/parity/parity.py there.
# PARITY_RUST_CMD replaces the Rust command (for example a second C# agent, to prove the harness itself).
set -euo pipefail
. "$(dirname "${BASH_SOURCE[0]}")/_sync.sh"
corpus="$1"; shift
sync_tree
CSHARP_CMD="agent/PhoneBox.Agent/bin/Release/net10.0/PhoneBoxAgent"
RUST_CMD="${PARITY_RUST_CMD:-agent-rs/target/release/PhoneBoxAgent}"
ssh "$REMOTE_HOST" "bash -c 'set -e; cd $REMOTE_DIR; . ~/.cargo/env
  dotnet build -c Release agent/PhoneBox.Agent -f net10.0 -v q -nologo >/tmp/parity-dotnet-build.log 2>&1 || { tail -20 /tmp/parity-dotnet-build.log; exit 1; }
  (cd agent-rs && cargo build --release -q)
  python3 -I tools/parity/parity.py --csharp $(quote_args "$CSHARP_CMD") --rust $(quote_args "$RUST_CMD") --corpus $(quote_args "$corpus") $(quote_args "$@")'"
