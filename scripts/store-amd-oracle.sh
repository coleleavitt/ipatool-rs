#!/usr/bin/env bash
# Records the Go Store AMD flow for `cargo test -p ipatool-store store_amd_oracle` (see
# crates/ipatool-store/tests/oracle/store_amd). It runs in a temporary export of the Go
# repository whose emulator time, randomness and call trace are patched, with the real modern
# ADI on a copy of an anisette-v4 state. No request leaves the machine.
#
# usage: scripts/store-amd-oracle.sh <anisette-v4 state file> <output file>
# The output holds ADI messages derived from the account's anisette state: keep it private
# (mode 0600, outside the repository) and never commit it.
set -euo pipefail

repo=$(cd "$(dirname "$0")/.." && pwd)
go_repo=${IPATOOL_GO_REPO:-$HOME/GolandProjects/ipatool}
go_ref=${IPATOOL_GO_REF:-upstream/main}
oracle=$repo/crates/ipatool-store/tests/oracle/store_amd
state=$(realpath "$1")
output=$(realpath -m "$2")

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
git -C "$go_repo" archive "$go_ref" | tar -x -C "$work"
python3 "$oracle/patch.py" "$work"
cp "$oracle/seam.go" "$work/internal/store/emulator/oracle_seam.go"
mkdir -p "$work/cmd/store-amd-oracle"
cp "$oracle/main.go" "$work/cmd/store-amd-oracle/"
cd "$work"
GOTOOLCHAIN=auto GOFLAGS=-mod=mod go generate ./... >/dev/null
umask 077
GOTOOLCHAIN=auto GOFLAGS=-mod=mod go run ./cmd/store-amd-oracle "$state" "$output"
