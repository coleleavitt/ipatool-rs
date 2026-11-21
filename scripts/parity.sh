#!/usr/bin/env bash
# Parity suite: compares the Go and the Rust ipatool on the same command lines.
# Usage: scripts/parity.sh [path-to-go-ipatool]
# Offline by default; IPATOOL_PARITY_LIVE=1 adds read-only Apple calls on owned apps.
set -euo pipefail
cd "$(dirname "$0")/.."

GO_BIN=${1:-$HOME/GolandProjects/ipatool/ipatool}
cargo build -q -p ipatool-cli
# Unbuffered, so a live run shows each check as it finishes.
PYTHONUNBUFFERED=1 exec scripts/parity.py "$GO_BIN" target/debug/ipatool
