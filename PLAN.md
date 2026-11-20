# ipatool → Rust port plan

Port of [majd/ipatool](https://github.com/majd/ipatool) (Go, MIT) to Rust.
Reference commit: upstream `main` at `3b02b4c55` (`3b02b4c55576562f2a55284cda9db9a4411d3067`),
checked out at `~/GolandProjects/ipatool` (remote `upstream`).

## Goal

A single `ipatool` binary with the same commands, flags, output, and on-disk
state as the Go tool, plus the fork improvements already written in Go:

- the `5005` / invalid-2FA-code handling and re-prompt (`fix/invalid-auth-code`, issue #678);
- `list-versions --with-metadata` with catalog-first names, concurrency and a local cache
  (`feat/list-versions-metadata`);
- read the stored account once per command (`fix/reuse-account-across-retries`, #677);
- skip unchanged anisette keyring writes (`fix/store-amd-redundant-save`).

"Same behavior" is checked by differential tests against the Go binary, not by
reading code side by side.

## Size and where the risk is

About 33k lines of hand-written Go plus a 55k-line generated data file, and about 19k
lines of Go tests. The ordinary App Store client (`pkg/*`, `cmd`) is about 10k lines and
ports directly. The hard part is the Apple-binary emulation (about 20k lines):

- `internal/sap`: runs Apple's own arm64 FairPlay/ADI code (`CoreFP`, `adid`,
  `fairplayd`, `commerce` from a pinned macOS 27.0.1 IPSW) inside Unicorn to produce
  SAP signatures, anisette headers and kbsync. Assets are fetched by HTTP range reads
  out of the IPSW (AEA, APFS, dyld shared cache, LZFSE).
- `internal/store`: an Objective-C/Swift runtime bridge that runs the App Store's
  `StoreServices` request engine in the emulator, driven by a precomputed profile.

Login, purchase and download all depend on these. So the plan builds the emulator
early (phase 3) and keeps public, no-login features usable from day one.

## Crate layout

```text
ipatool-cli ──> ipatool-appstore ──> ipatool-sap ──> unicorn-engine
      │                │        └──> ipatool-store ──> ipatool-sap
      └──> ipatool-keychain <──┘
```

| crate | owns |
|---|---|
| `ipatool-appstore` | catalog, bag, storefronts, login flow, purchase, download, versions; plist/JSON HTTP |
| `ipatool-keychain` | account/anisette storage: system keyring + encrypted file backend |
| `ipatool-sap` | Unicorn engine wrapper, Mach-O image loader, IPSW asset loader, SAP/ADI/kbsync machine, anisette provisioning |
| `ipatool-store` | Objective-C runtime bridge and Store profile |
| `ipatool-cli` | clap commands, prompts, output, MCP server, state directory |
| `xtask` | dev tools: parity runner, profile extraction/generation |

Only `ipatool-appstore` and `ipatool-cli` exist now. Add a crate when its phase starts.

## Package ledger

| Go package | lines | test lines | Rust owner | phase |
|---|---:|---:|---|---|
| `cmd` | 1831 | 1775 | ipatool-cli | 1–8 |
| `internal/sap` | 315 | 223 | ipatool-sap | 3 |
| `internal/sap/assets` | 3894 | 768 | ipatool-sap::assets | 3 |
| `internal/sap/machimage` | 1289 | 576 | ipatool-sap::machimage | 3 |
| `internal/sap/machine` | 4981 | 725 | ipatool-sap::machine | 3 |
| `internal/sap/unicorn` | 2036 | 1382 | ipatool-sap::engine (unicorn-engine crate) | 3 |
| `internal/store` | 324 | 128 | ipatool-store | 5 |
| `internal/store/emulator` | 6987 | 480 | ipatool-store::emulator | 5 |
| `internal/store/profile` | 55648 | 134 | ipatool-store::profile (generated data) | 5 |
| `internal/store/profile/format` | 63 | 0 | ipatool-store::profile | 5 |
| `internal/store/profilegen` | 3328 | 415 | xtask profilegen (dev tool) | 9 |
| `internal/store/profilegen/cmd/profilegen` | 146 | 0 | xtask profilegen | 9 |
| `internal/store/terms` | 37 | 0 | ipatool-store::terms | 5 |
| `internal/storeauth` | 647 | 50 | ipatool-appstore::storeauth | 4 |
| `pkg/appstore` | 7400 | 11734 | ipatool-appstore | 0–7 |
| `pkg/http` | 513 | 299 | ipatool-appstore::http | 2 |
| `pkg/keychain` | 83 | 214 | ipatool-keychain | 1 |
| `pkg/log` | 116 | 172 | ipatool-cli::output | 0 |
| `pkg/util` | 43 | 87 | (inlined) | 1 |
| `pkg/util/machine` | 353 | 547 | ipatool-appstore::machine | 2 |
| `pkg/util/operatingsystem` | 62 | 108 | (std) | 1 |

Keep a per-file status in `docs/port-ledger.md` once a package is in progress.

## Phases

Each phase ends with fmt, clippy (pedantic, `-D warnings`), offline tests, and the
parity check for the commands it touches.

### Phase 0 — scaffold and public catalog (done)

- [x] Workspace, lints, agent files, plan.
- [x] `Platform` parsing and iTunes entities, `App` with platform derivation.
- [x] iTunes search and lookup; `apps.apple.com` catalog version history (newest 25).
- [x] CLI: `search` (with `--country` until account storefronts exist) and the new
      `catalog-versions` command; zerolog-shaped JSON/text output.
- [x] `scripts/parity.sh` comparing `search` output with the Go binary (iPhone, watchOS, macOS searches match).

### Phase 1 — state, storage, account (done)

- [x] State directory: `XDG_STATE_HOME`/`XDG_DATA_HOME` then `~/.ipatool`, with the Go migration rules.
- [x] Keychain: same service name (`ipatool-auth.service`) and keys as Go, system
      keyring first, encrypted-file fallback with `--keychain-passphrase`.
      The file backend reads and writes Go's JWE files (checked against
      `byteness/keyring` v1.9.0 both ways). The Secret Service connection tries each
      `DBUS_SESSION_BUS_ADDRESS` entry without the GUID check, as godbus does.
- [x] `Account` JSON identical to Go, so a session made by one tool loads in the other.
- [x] Persistent cookie jar in the `juju/persistent-cookiejar` file format, with the
      `cookies.lock` fcntl lock and merge-on-save. Not wired into HTTP until phase 2.
- [x] `auth info`, `auth revoke`. State is shared with the Go tool (open question 1).
      `auth info` matches Go byte for byte on the live keyring; `revoke` is tested on a
      throwaway collection only.

### Phase 2 — store HTTP plumbing

- [x] Plist response handling: Apple's envelopes, HTML/429 errors (`pkg/http/client.go`).
      Requests go direct, as Go's `bag-direct` executor does (no redirects, shared cookie jar,
      30 s timeout). Requests Go sends through the Store emulator wait for phase 5.
- [x] URL bag and SAP configuration (`appstore_bag.go`), storefront discovery
      (scoped bags, available-storefronts fallback). Live check: `IPATOOL_TEST_LIVE=1`.
- [x] Machine GUID from the MAC address, Linux sysfs selection (same address as Go here).
      macOS IOKit and Windows primary-adapter lookups are not ported yet.
- [x] `search`/`catalog-versions` take the account storefront; `--country` overrides it.
- [ ] Failure types and the plist login/purchase envelopes move to phase 4 with their users.

### Phase 3 — SAP emulator (highest risk)

- [ ] Engine: `unicorn-engine` crate built from source (replaces Go's runtime download of
      a pinned `libunicorn` wheel and `purego`). Hooks, memory guard, timeouts, clock.
- [x] `machimage`: Mach-O image mapping, shared regions, preparation cache. Mach-O load
      commands come from the `object` crate; the go-macho pieces Go relies on (symbol table,
      chained-fixup walk and decoding, dyld-info bind/rebase opcodes, export trie) are ported
      with their quirks. Oracle: `scripts/machimage-oracle.sh` records every guest map and
      write (SHA-256), exports and resolved imports for the six pinned images from Go;
      `IPATOOL_TEST_SAP_CACHE=1 cargo test -p ipatool-sap --test machimage_oracle` matches it.
      Differences: an out-of-range `ARM64E_USERLAND24` bind ordinal is an error (go-macho
      panics); `strings.EqualFold` is approximated by per-character Unicode lowercasing.
      Untested: dyld-info bind/rebase/export-trie and 32/64-bit chained formats (no pinned
      image uses them; ported from go-macho by reading).
- [x] `assets`: IPSW HTTP range reader, AEA decryption, APFS reader, dyld shared cache
      extraction, LZFSE; same pinned files, sizes and SHA-256 digests; same cache directory name.
      `store.go`/`store_images.go` move to phase 5. Crates: `zip`, `hpke` + `p256` (FCS key),
      `aes`/`ctr`/`hkdf`/`hmac`, `lzfse` (Apple's reference C decoder, so Go's
      `padLZFSEStreams` workaround is not needed). Ported from Go dependencies: the sealed-APFS
      subset of `majd/go-apfs`, the LZVN decoder of `go-compressions/lzfse` (decmpfs streams
      have no end-of-stream opcode), the `go-macos-pkg` LZBITMAP decoder and the in-cache
      `File.Export` path of `blacktop/go-macho`. Not ported: the DMG fallback (both pinned
      images are raw APFS), chained-fixup parsing (unused by `Export`), context cancellation.
      Verified with `IPATOOL_TEST_SAP_REMOTE=1 cargo test -p ipatool-sap --release`: all seven
      files byte-identical to the Go cache (about 32 s and 250 MB of range reads).
- [x] `machine`: arm64 shims, IOKit/AES shims, modern ADI, kbsync, storeagent, plus
      `internal/sap` protocol/signer (`ipatool_sap::signer`). Guest state lives in the Unicorn
      data (`machine::shims::State`); one code hook dispatches the `ret` stubs, the first
      service failure wins and stops the guest, as in Go. Oracle: `scripts/machine-oracle.sh`
      runs Go in a temporary export with deterministic randomness and time (and Go's
      map-ordered registration of five data symbols fixed to source order) through SAP
      initialize, setup with Apple's live certificate and reply, three signs, teardown,
      kbsync and modern ADI open + OTP; `IPATOOL_TEST_SAP_CACHE=1 cargo test -p ipatool-sap
      matches_go_machine` replays the recorded Apple messages and matches every byte.
      Differences: no `context.Context` (Go's nil/canceled-context checks have no Rust
      equivalent); `StoreAgent::decrypt` returns the byte count only on success;
      `Signer::new` is async and not `Send` (the guest is single-threaded).
      Untested against Go: modern ADI provisioning, keybag import, XPC exchange and
      StoreAgent decryption (need live Apple provisioning data or a purchased `.pkg`).
- [ ] Oracle: a Go test harness (`xtask`) that records inputs/outputs of each guest call
      from the Go implementation; Rust must produce identical bytes for the same inputs.
      Done as scripts for `machimage` and `machine`; `assets` and `xtask` remain.

### Phase 4 — authentication

- [ ] Anisette provisioning and Store AMD state (with the skip-unchanged-write fix).
  - [x] `ipatool_appstore::storeauth`: `anisette.go`, `store_provisioning.go`, `http_client.go`
        and the state handling of `store_amd.go`. `AnisetteState` JSON, identity headers, the
        header set and the XML plist request bodies match a Go oracle
        (`scripts/storeauth-oracle.sh` records `tests/fixtures/storeauth-oracle.json`); both
        provisioning flows are tested against a fake HTTP server with a fake ADI.
        `gsa.apple.com` chains to Apple Root CA, which this machine's system store (used by
        reqwest's platform verifier) lacks, so Go's root download fallback is needed and is
        checked live by `cargo test -p ipatool-appstore gsa_needs_the_apple_root -- --ignored`.
        Differences: requests go over HTTP/1.1 (Go negotiates HTTP/2) without
        `Accept-Encoding: gzip`; response plists are XML or binary only (Go also reads
        OpenStep text); Go's nil-context checks have no equivalent. Untested: the real
        `ModernAdi` provisioning calls against Apple, and the root download itself (only its
        validation and redirect rule are unit tested).
  - [x] Store AMD headers: `EmulatorStoreAmd` (`store_amd.go`) loads the assets, opens modern
        ADI with the saved identity and database, runs `AMSAnisette`'s type-2 provider in the
        Store emulator with `com.apple.adid` XPC answered by modern ADI, and returns the headers
        with the updated state; `auth login` uses it. It runs on its own thread (the guests are
        not `Send`; a cold asset cache uses blocking HTTP). Checked against Go on a copy of a
        real `anisette-v4` state (see phase 5); nothing is sent to Apple.
- [x] SAP signer and action signing. The signer is ported in phase 3 (live setup + sign
      checked by `cargo test -p ipatool-sap signs_after_live_setup -- --ignored`). Login creates
      it from the bag's SAP configuration and the machine ID and closes it after login. Go
      sends login through the Store framework emulator; until phase 5 the Rust client encodes
      the XML plist payload itself and sends the body signature in `X-Apple-ActionSignature`,
      as Go did before the emulator (`abd86cb`). The emulator's own login parameters and
      headers are not added.
- [ ] Legacy and native login, 2FA prompt, `5005` handling and re-prompt, retries.
      Legacy login is done (`ipatool_appstore::login`, `Client::login`): 2FA code
      normalization, redirects (max 4), request retries (3 attempts, 10 s doubling to 30 s,
      `Retry-After`), dialog and machine-action errors, DSID from header or cookie,
      phone-number storefront hint, account saved under `account`, and the fork's
      `5005`/invalid-code errors. `auth login` prompts and re-prompts up to 3 attempts.
      Go's login tests are ported with a fake HTTP layer. Not covered: Go's
      `context.Canceled` cases (no Rust equivalent), the RFC 850 and ANSI C `Retry-After`
      dates only by unit test, and the signed request against Apple.
      Native login is done (`native_login.rs`, `Client::native_login`, used by `auth login`):
      Store AMD proof from a saved state, else `gsa.apple.com` anisette; on `RP` retry without
      the `X-Apple-I-MD*` headers; on `SP` generate the provisioning kbsync (DSID `^1`) with the
      phase-3 machine, finish Store provisioning, then log in with a Store AMD proof. The
      `anisette-v4` state is written only when it changes. Tested with a fake backend.
      Until phase 5 an `SP` answer ends with "prepare Store AMD proof: ... not ported yet".
- [ ] `auth login` end to end with a real account (manual, opt-in).

### Phase 5 — Store engine emulator

- [ ] Objective-C runtime bridge, Swift metadata, async tasks, network bridge.
  - [x] Runtime core (`ipatool_store::emulator`): `runtime.go`, `memory.go`, `metadata.go`,
        `profile_pages.go`, `bridge.go`, `swift.go`, `dictionary.go`, `async.go`,
        `callbacks.go`, `hashing.go`, `compression.go`, `notifications.go`, `delegate.go`,
        `accounts.go`, `session.go`, `auth.go`, `bag.go`, `anisette.go`. Guest state lives in
        the Unicorn data; Go's bridge panics are errors that the step hook records as the guest
        fault with Go's text. Go's tests are ported (memory grouping and guard, metadata cache,
        page batching, padding execution, the omitted-page strict check).
        Oracle: `scripts/store-amd-oracle.sh <state copy> <out>` runs Go's emulator with fixed
        time, counter randomness and a call-stub trace, with real modern ADI, and Go's
        `StoreAMDHeaders`. `IPATOOL_STORE_AMD_ORACLE=<out> IPATOOL_TEST_SAP_CACHE=1 cargo test
        -p ipatool-store store_amd_oracle` replays the recorded XPC replies and matches the
        headers, every call stub in order and the heap size byte for byte;
        `IPATOOL_STORE_AMD_STATE=<state copy> IPATOOL_STORE_AMD_ORACLE=<out> cargo test -p
        ipatool-appstore store_amd_matches_go` runs `EmulatorStoreAmd` and matches Go's header
        names, lengths, encoding and state changes. The record holds ADI messages derived from
        the account: keep it private and out of the repository.
        Differences: Go's randomized map iteration (Invoke field order, observers, profile
        lookups) is fixed to source or sorted order; no `context.Context`
        (`swift_task_isCancelled` is 0); guest strings are UTF-8 (invalid bytes become U+FFFD;
        `strlen`/`strptime` use the raw bytes); dates print in UTC for `%@`; profile constants
        accept property writes (Go's nil map panics); `Runtime` has no `Close` (drop frees it).
        `emulator/network.rs` is a stand-in with `requestWithURL:` until `p5-network` lands.
        Untested against Go: bridge services the Store AMD flow does not reach (they run with
        the request engine through `Execute`), `inflate` in the guest, Swift dictionary
        bridging and the strict guard's guest-side hooks.
- [x] Profile: `ipatool_store::profile` embeds Go's three zstd artifacts (extracted by
      `scripts/extract-store-profile.py`), checks Go's sizes, digests and page index, and
      decodes the metadata; re-encoding it gives the same JSON values. `profilegen` is phase 9.
- [ ] Terms acceptance adapter.

### Phase 6 — purchase and download

- [ ] `purchase` (free apps), license-exists handling, terms.
- [ ] `download`: parallel range download with resume, sinf replication, iTunesMetadata, artwork.
- [ ] macOS `.pkg` download, decryption and publish.

### Phase 7 — versions and purchases

- [ ] `list-versions`, `get-version-metadata` (partial-zip `Info.plist` read with read-ahead).
- [ ] `--with-metadata` catalog-first + cache + concurrency (fork feature).
- [ ] `list-purchases` (owned apps).
- [ ] Account read once per command (fork fix).

### Phase 8 — MCP

- [ ] `ipatool mcp` stdio server with the same tools via `rmcp`.

### Phase 9 — parity, release

- [ ] Parity suite over all commands (offline fixtures + opt-in live).
- [ ] `profilegen` port, CI (Linux, macOS, Windows), release builds, iOS build.

## Open questions for the user

1. Shared state: decided in phase 1 — the Rust tool uses the same state directory and
   keyring entries as Go, so one login works for both.
2. Repository: publish as `coleleavitt/ipatool-rs`?
3. Keep the Go tool's exact text output, or improve text mode while keeping JSON identical?
