# ipatool (Rust port)

Read `AGENTS.md` (rules, validation) and `PLAN.md` (phases, ledger, open questions) first.
They are the project policy; this file only adds Claude Code specifics.

- Go reference: `~/GolandProjects/ipatool`, read through `git show upstream/main:<path>`.
  The Go build there needs `GOTOOLCHAIN=auto` and `go generate ./...` before tests.
- Use `.claude/agents/porter.md` for porting one Go package and `.claude/agents/verifier.md`
  for an independent check before a phase checkbox is ticked.
- `/port <go-package>` and `/parity` are in `.claude/commands/`.
- Commit per finished unit with a conventional message (`feat:`, `fix:`, `test:`, `docs:`).
  Ask before pushing.
