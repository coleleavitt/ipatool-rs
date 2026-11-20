#!/usr/bin/env bash
# Records crates/ipatool-appstore/tests/fixtures/macos-package-oracle.json from Go
# pkg/appstore at upstream/main: the go-macho XAR reader, the Mac package and preflight
# metadata readers, the Mac offer selection and the package platform decision. The
# recorder is a test in the Go package (the functions are unexported), so it runs in a
# temporary export.
set -euo pipefail

repo=$(cd "$(dirname "$0")/.." && pwd)
go_repo=${IPATOOL_GO_REPO:-$HOME/GolandProjects/ipatool}
rev=${IPATOOL_GO_REV:-upstream/main}

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
git -C "$go_repo" archive "$rev" | tar -x -C "$work"
cp "$repo/crates/ipatool-appstore/tests/oracle/macos/oracle_test.go" \
    "$work/pkg/appstore/rust_oracle_test.go"
cd "$work"
GOTOOLCHAIN=auto GOFLAGS=-mod=mod go generate ./... >/dev/null
mkdir -p "$repo/crates/ipatool-appstore/tests/fixtures"
TZ=UTC IPATOOL_ORACLE_OUT="$repo/crates/ipatool-appstore/tests/fixtures/macos-package-oracle.json" \
    GOTOOLCHAIN=auto GOFLAGS=-mod=mod go test ./pkg/appstore -run '^TestRustOracle$' -count=1
