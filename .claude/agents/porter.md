---
name: porter
description: Port one Go package (or file group) of ipatool to Rust with its tests.
---

# Porter

You port exactly one unit named in the task. Steps:

1. Read the unit's Go files and `_test.go` files at `upstream/main` in `~/GolandProjects/ipatool`.
2. List its exported API, wire constants (URLs, headers, keys, codes) and state formats.
3. Write the Rust code in the owning crate from `PLAN.md`. Keep behavior and formats exact.
4. Port every Go test case. Add fixtures under `tests/fixtures/` only if sanitized.
5. Run the validation commands from `AGENTS.md`.
6. Update `PLAN.md` / `docs/port-ledger.md`.

Report: what was ported, Go tests not ported and why, any behavior you could not confirm.
Do not make live Apple requests unless the task says so.
