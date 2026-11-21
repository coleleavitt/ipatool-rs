# ipatool (Rust port): agent guide

Read `PLAN.md` first: it has the crate layout, the Go-package ledger and the current phase.
`CLAUDE.md` holds the same rules for Claude Code.

## What this is

A Rust port of the Go `ipatool` (`~/GolandProjects/ipatool`, remote `upstream` =
`majd/ipatool`, reference commit in `PLAN.md`). The Go code is the specification.
When Go and Rust disagree, the Go binary's observable behavior wins unless `PLAN.md`
lists the change as a deliberate fork improvement.

## Porting rules

- Port one Go package (or file group) at a time. Read the Go source and its tests at the
  reference commit (`git -C ~/GolandProjects/ipatool show upstream/main:<path>`), not the
  working tree, which may be on a feature branch.
- Port the Go tests' cases with the code. A Go test with no Rust equivalent is a gap; list it.
- Keep wire formats exact: URLs, query order, headers, plist/JSON keys, user agents,
  failure codes, file names, cache directory names, keyring keys.
- Keep on-disk state compatible with Go (account JSON, cookie jar, caches) unless `PLAN.md` says otherwise.
- Do not guess Apple behavior. If the Go code does not show it and no capture proves
  it, leave it out and note it in `PLAN.md`.
- Update `PLAN.md` checkboxes and the ledger in the same commit as the code.

## Code rules

- Edition 2024, built with the machine default `rustc-master` toolchain (the global cargo
  config passes nightly `-Z` flags, so do not pin a stable toolchain).
- Clippy `all` + `pedantic` are deny-level. No `unwrap`/`expect`/panics in library code
  except on values that are infallible by construction (constant URLs), with a message.
- `unsafe` is denied workspace-wide. The SAP crate may allow it in one module for the
  emulator boundary, `ipatool-appstore/src/sys.rs` for the OS calls Go makes through
  `x/sys` and `purego` (adapters, IOKit, process check), and the CLI's Windows console
  module in `prompt.rs`; every block has a `// SAFETY:` comment.
- Typed errors (`thiserror`) in libraries; the CLI turns them into output lines.
- Async with `tokio` + `reqwest` (rustls).
- Comments explain why, not what.

## Validation

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
scripts/parity.sh            # compares output with the Go binary (offline; IPATOOL_PARITY_LIVE=1 adds Apple calls)
```

Default tests are offline and use fixtures. Live tests are `#[ignore]` and run only when
asked (`cargo test -- --ignored`).

Optional cross-checks, skipped unless their variable is set:

```sh
# Go keyring file format, both directions
(cd crates/ipatool-keychain/tests/go_helper && GOTOOLCHAIN=auto go build -o /tmp/kr .)
IPATOOL_GO_KEYRING=/tmp/kr cargo test -p ipatool-keychain --test go_interop
# live Secret Service, throwaway collection `ipatool-rust-test.service`
IPATOOL_TEST_SECRET_SERVICE=1 cargo test -p ipatool-keychain --test go_interop
# a copy of a real Go cookie jar (the original is not written)
IPATOOL_GO_COOKIES=~/.ipatool/cookies cargo test -p ipatool-appstore cookies
# the macOS restore-image extraction against Apple's CDN (about 250 MB of range reads)
IPATOOL_TEST_SAP_REMOTE=1 cargo test -p ipatool-sap --release extracts_the_pinned_frameworks
```

## Apple services and accounts

- Live requests go to real Apple services with the user's own account. Keep them few:
  Apple rate-limits (HTTP 429) and per-version metadata calls are slow.
- Never write passwords, 2FA codes, tokens, cookies, keyring contents or anisette state
  into logs, fixtures, commits or chat. Fixtures must be sanitized.
- Never run `auth login`, `purchase` or anything that changes account state unless the
  user asked for that run.
- Downloaded `.ipa`/`.pkg` files and HAR captures stay untracked (`.gitignore`).
