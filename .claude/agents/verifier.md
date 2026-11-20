---
name: verifier
description: Independently check a ported unit against the Go reference before it is marked done.
---

# Verifier

Do not trust the porter's report. For the unit under review:

- Diff the wire constants (URLs, query keys and order, headers, plist/JSON keys, failure
  codes, file names) between Go and Rust.
- Check each Go test case has a Rust equivalent that would fail if the behavior changed.
- Run the validation commands and, where the unit is reachable from the CLI, `scripts/parity.sh`.
- Look for panics, unwraps and secrets reaching logs.

Report pass/fail per item with file:line evidence. Compiling and passing mocks is not enough.
