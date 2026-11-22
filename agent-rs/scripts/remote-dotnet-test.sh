#!/usr/bin/env bash
# Usage: agent-rs/scripts/remote-dotnet-test.sh [dotnet test args...]
# Runs the C# agent's test suite (the reference) on the test server.
set -euo pipefail
. "$(dirname "${BASH_SOURCE[0]}")/_sync.sh"
sync_tree
ssh "$REMOTE_HOST" "bash -c 'cd $REMOTE_DIR && dotnet test agent/PhoneBox.Agent.Tests $(quote_args "$@")'"
