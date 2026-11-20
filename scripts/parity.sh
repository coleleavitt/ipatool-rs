#!/usr/bin/env bash
# Compare JSON output of the Go and Rust ipatool for public commands.
# Usage: scripts/parity.sh [path-to-go-ipatool]
set -euo pipefail
cd "$(dirname "$0")/.."

GO_BIN=${1:-$HOME/GolandProjects/ipatool/ipatool}
RS_BIN=target/debug/ipatool
cargo build -q -p ipatool-cli

strip_time() { python3 -c 'import json,sys
for line in sys.stdin:
    d=json.loads(line); d.pop("time",None); print(json.dumps(d,sort_keys=True))'; }

status=0
check() {
  local name=$1; shift
  local go rs
  go=$("$GO_BIN" "$@" --format json 2>/dev/null | strip_time) || go="<go failed>"
  rs=$("$RS_BIN" "$@" --format json 2>/dev/null | strip_time) || rs="<rust failed>"
  if [[ "$go" == "$rs" ]]; then echo "ok    $name"; else
    echo "DIFF  $name"; diff <(echo "$go") <(echo "$rs") | head -20 || true; status=1; fi
}

# Both tools take the storefront from the logged-in account.
check "search perplexity" search perplexity --limit 3
check "search watch" search weather --limit 3 --platform watchos
check "search mac" search xcode --limit 2 --platform macos
exit $status
