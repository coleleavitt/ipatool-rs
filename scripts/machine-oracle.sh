#!/usr/bin/env bash
# Records crates/ipatool-sap/tests/fixtures/machine-oracle.json from Go internal/sap/machine at
# upstream/main. It runs in a temporary export of the Go repository whose randomness, wall time
# and one map-ordered registration are patched to be deterministic (tests/oracle/machine).
# Needs network: it fetches Apple's SAP setup certificate and runs one SAP setup exchange.
set -euo pipefail

repo=$(cd "$(dirname "$0")/.." && pwd)
go_repo=${IPATOOL_GO_REPO:-$HOME/GolandProjects/ipatool}
oracle=$repo/crates/ipatool-sap/tests/oracle/machine

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
git -C "$go_repo" archive upstream/main | tar -x -C "$work"
python3 "$oracle/patch.py" "$work"
cp "$oracle/seam.go" "$work/internal/sap/machine/oracle_seam.go"
mkdir -p "$work/cmd/machine-oracle"
cp "$oracle/main.go" "$work/cmd/machine-oracle/"
cd "$work"
GOTOOLCHAIN=auto GOFLAGS=-mod=mod go generate ./... >/dev/null
GOTOOLCHAIN=auto GOFLAGS=-mod=mod go run ./cmd/machine-oracle \
    "$repo/crates/ipatool-sap/tests/fixtures/machine-oracle.json"
