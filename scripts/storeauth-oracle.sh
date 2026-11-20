#!/usr/bin/env bash
# Records crates/ipatool-appstore/tests/fixtures/storeauth-oracle.json from Go
# internal/storeauth at upstream/main: identity headers, anisette state JSON decoding and
# encoding, the header set and the XML plist request bodies. The recorder is a test in the
# Go package (the functions are unexported), so it runs in a temporary export.
set -euo pipefail

repo=$(cd "$(dirname "$0")/.." && pwd)
go_repo=${IPATOOL_GO_REPO:-$HOME/GolandProjects/ipatool}
rev=${IPATOOL_GO_REV:-upstream/main}

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
git -C "$go_repo" archive "$rev" | tar -x -C "$work"
cp "$repo/crates/ipatool-appstore/tests/oracle/storeauth/oracle_test.go" \
    "$work/internal/storeauth/rust_oracle_test.go"
cd "$work"
GOTOOLCHAIN=auto GOFLAGS=-mod=mod go generate ./... >/dev/null
IPATOOL_ORACLE_OUT="$repo/crates/ipatool-appstore/tests/fixtures/storeauth-oracle.json" \
    GOTOOLCHAIN=auto GOFLAGS=-mod=mod go test ./internal/storeauth -run '^TestRustOracle$' -count=1
