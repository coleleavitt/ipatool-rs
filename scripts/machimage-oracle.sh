#!/usr/bin/env bash
# Records crates/ipatool-sap/tests/fixtures/machimage-oracle.json from Go
# internal/sap/machimage at upstream/main. The Go program imports an internal package,
# so it runs inside a temporary export of the Go repository.
set -euo pipefail

repo=$(cd "$(dirname "$0")/.." && pwd)
go_repo=${IPATOOL_GO_REPO:-$HOME/GolandProjects/ipatool}
cache=${1:-${XDG_CACHE_HOME:-$HOME/.cache}/ipatool/sap/apple-assets-macos-27-26A434-arm64-v5}

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
git -C "$go_repo" archive upstream/main | tar -x -C "$work"
mkdir -p "$work/cmd/machimage-oracle"
cp "$repo/crates/ipatool-sap/tests/oracle/machimage/main.go" "$work/cmd/machimage-oracle/"
cd "$work"
GOTOOLCHAIN=auto GOFLAGS=-mod=mod go run ./cmd/machimage-oracle "$cache" \
    "$repo/crates/ipatool-sap/tests/fixtures/machimage-oracle.json"
