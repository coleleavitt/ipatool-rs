#!/usr/bin/env bash
# Records crates/ipatool-store/tests/fixtures/network-oracle.json from Go
# internal/store/emulator at upstream/main: net/url parsing and formatting, the terms
# checks, allowedURL and the query encoders. The recorder is a test in the Go package (the
# functions are unexported), so it runs in a temporary export.
set -euo pipefail

repo=$(cd "$(dirname "$0")/.." && pwd)
go_repo=${IPATOOL_GO_REPO:-$HOME/GolandProjects/ipatool}
rev=${IPATOOL_GO_REV:-upstream/main}

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
git -C "$go_repo" archive "$rev" | tar -x -C "$work"
cp "$repo/crates/ipatool-store/tests/oracle/network/oracle_test.go" \
    "$work/internal/store/emulator/rust_oracle_test.go"
cd "$work"
GOTOOLCHAIN=auto GOFLAGS=-mod=mod go generate ./... >/dev/null
mkdir -p "$repo/crates/ipatool-store/tests/fixtures"
TZ=UTC IPATOOL_ORACLE_OUT="$repo/crates/ipatool-store/tests/fixtures/network-oracle.json" \
    GOTOOLCHAIN=auto GOFLAGS=-mod=mod go test ./internal/store/emulator -run '^TestRustOracle$' -count=1
