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
| `ipatool-profilegen` | dev tool: regenerates the Store profile (`profilegen` binary) |
| `xtask` | dev tools: parity runner, profile extraction |

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
| `internal/store/profilegen` | 3328 | 415 | ipatool-profilegen (dev tool) | 9 |
| `internal/store/profilegen/cmd/profilegen` | 146 | 0 | ipatool-profilegen (bin `profilegen`) | 9 |
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
      The macOS IOKit and Windows `GetIfEntry2Ex` lookups were ported in phase 9.
- [x] `search`/`catalog-versions` take the account storefront; `--country` overrides it.
- [x] Failure types and the plist login/purchase envelopes move to phase 4 with their users (done in phases 4 and 6).

### Phase 3 — SAP emulator (highest risk)

- [x] Engine: `unicorn-engine` crate built from source (replaces Go's runtime download of
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
      StoreAgent decryption (need live Apple provisioning data or a purchased `.pkg`;
      `.pkg` decryption is now checked live in phase 6, byte-identical to Go).
- [x] Oracle: a Go test harness (`xtask`) that records inputs/outputs of each guest call
      from the Go implementation; Rust must produce identical bytes for the same inputs.
      Done as per-module scripts (`machimage`, `machine`, macOS package, MCP) instead of one
      `xtask`; `assets` is checked by the pinned SHA-256 of every extracted file, and
      `profilegen --check` covers the Store profile.

### Phase 4 — authentication

- [x] Anisette provisioning and Store AMD state (with the skip-unchanged-write fix; Store AMD in phase 5).
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
- [x] Legacy and native login, 2FA prompt, `5005` handling and re-prompt, retries.
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
- [x] `auth login` end to end with a real account (manual, opt-in): done 2026-11-20 through the Store AMD emulator.

### Phase 5 — Store engine emulator

- [x] Objective-C runtime bridge, Swift metadata, async tasks, network bridge.
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
        Untested against Go: bridge services the Store AMD flow does not reach (they run with
        the request engine through `Execute`), `inflate` in the guest, Swift dictionary
        bridging and the strict guard's guest-side hooks.
  - [x] Network bridge and request engine: `network.go`, `client.go` (`Runtime::execute`,
        `encodeRequest`, `purchaseRequest`, `allowedURL`), `purchase_adapter.go`,
        `terms_adapter.go` and `internal/store/backend.go` (`ipatool_store::backend`:
        `Backend::new`, `client(operation)` with `send`/`execute_http`, `bag-direct` and `sap`
        straight to the transport, hardware identity, bag caching, gzip, the shared and
        authentication pools). Go's `net/url` (`gourl`), `http.Header`, cookie parsing and
        `AddCookie` (`cookie`), howett plist XML/binary and `encoding/json` (`emulator::codec`)
        are ported because the guest sees their exact output. `plist_body` moved here from
        `ipatool-appstore`. Go's tests are ported: `backend_test.go` (pool reuse, separate auth
        connections, redirects returned, explicit cookies, per-request and bag timeouts, stream
        drop), `terms_adapter_test.go` and `generated_profile_test.go`.
        Oracle: `scripts/network-oracle.sh` records `tests/fixtures/network-oracle.json` from Go:
        `net/url` parse/format/resolve/join/escape, terms URLs, `allowedURL`, Set-Cookie parsing,
        the codecs (XML and binary plist bytes, JSON) and 13 `Execute` runs through a fake
        transport (plain, encoded POST, JSON, raw PUT, free and paid purchase, bad host, bad
        method, followed/unfollowed/foreign redirects, stream, authenticate with a fake signer).
        `IPATOOL_TEST_SAP_CACHE=1 cargo test -p ipatool-store` replays them; every outgoing
        request (method, URL, headers, body) and every response matches Go byte for byte, error
        text included (purchase bodies differ only in the random `clientCorrelationKey`).
        Differences: the transport is `reqwest::blocking` over HTTP/1.1 (Go negotiates HTTP/2),
        reqwest adds `Accept: */*`, and there is no response-header timeout (Go: 30 s); Go's
        implicit `User-Agent: Go-http-client/1.1`, `Accept-Encoding: gzip` with transparent
        decoding and `Connection: close` on the authentication pool are reproduced; no
        `context.Context`; `close_idle_connections` rebuilds the pools; `execute_http` treats an
        empty body as Go's nil body; property lists parse as XML or binary only (Go also reads
        OpenStep text); JSON dates are written in UTC (Go writes the local zone); cookie
        `Expires` ignores local-zone abbreviations (Go applies the local offset for them).
        Untested against Go: a finance `FrameworkError` returning the original response, the
        `AKAppleIDSession` anisette challenge, `cookiesWithResponseHeaderFields:` and the other
        `NSHTTPCookie` selectors (unit-tested parsing only), binary plists with dates or
        non-ASCII keys, live Apple traffic (none sent).
- [x] Profile: `ipatool_store::profile` embeds Go's three zstd artifacts (extracted by
      `scripts/extract-store-profile.py`), checks Go's sizes, digests and page index, and
      decodes the metadata; re-encoding it gives the same JSON values. `profilegen` is phase 9.
- [x] Terms acceptance adapter (`emulator::terms_adapter`, `ipatool_store::terms`).

### Phase 6 — purchase and download

- [x] `purchase` (free apps), license-exists handling, terms. `purchase.rs`: `Purchase`
      (`STDQ`, then `GAME` after `2059` outside macOS), `purchaseWithParams` (one
      country-mismatch storefront correction saved to the keychain, one terms acceptance),
      Go's failure mapping (`5002` and HTTP 500 are `license already exists`), and
      `appstore_terms.go` (terms info/check endpoints from the bag, pod host, raw JSON
      version). Requests go through the Store `purchase` and `request` executors
      (`commerce.rs`, `Client::store_*`; `StoreTransport` is the seam tests replace).
      `StoreSession` (`session.rs`) holds what Go's `appstore` keeps beside the client: the
      keychain, the Store AMD proof (`storeAMDProof`, state written only when it changes) and
      the `kbsync-v1` cache. CLI `purchase` prints `alreadyOwned`/`success` like Go and logs
      in again once after an expired token. Live: `purchase -b ai.perplexity.app` reports
      `alreadyOwned: true` (the account owns it; nothing was bought).
- [x] `download`: parallel range download with resume, sinf replication, iTunesMetadata,
      artwork. `download_product.rs` (`sendDownloadProduct`: ent/download with a kbsync blob
      and its cache, volumeStoreDownloadProduct, redownload, updateProduct, the delisted-app
      chain), `version_lookup.rs` (MDM catalogs, the App Store catalog API for Arcade apps,
      the visionOS product page), `download.rs` (`Download`, package platform check,
      `iTunesArtwork`, binary `iTunesMetadata.plist` with sorted keys), `download_file.rs`
      (resume from the `.tmp` length, four ranges from 128 MiB with a strong `ETag`, staged
      parts, single-connection fallback with `If-Range`), `replicate_sinf.rs`, and
      `zipfile.rs`: the parts of Go's `archive/zip` the rewrite depends on (central-directory
      reader with zip64 and prefixed archives, `CreateRaw`/`Create`/`Close` framing, local
      extras kept beside central ones), because entries are copied compressed and must keep
      Go's framing. Bag and storefront-bag requests now use the `bag-direct` executor, as Go.
      CLI `download` (all Go flags, `--purchase`, three attempts, progress bar when
      interactive). `ipatool_store::gourl` is public with `ParseRequestURI`, `ParseQuery` and
      `Values`. Go's tests are ported (`commerce_tests`, CLI `commerce/tests.rs`) against fake
      executors. Live: `download -b ai.perplexity.app` (26.46.0, 146 MB, parallel path)
      matches Go's IPA: same 7485 entries in the same order with identical local headers and
      compressed bytes for every package entry, the same `Info.plist` version, decoded
      `iTunesMetadata.plist` equal to Go's; the sinf differs between any two downloads (Go
      vs Go too).
      Differences: no `context.Context` (Go's cancellation cases and the killed-process
      resume test are not ported; a failing range stops the others between reads, and
      ent/download has no overall two-minute bound); deflated entries the tool adds
      (`iTunesArtwork`, `iTunesMetadata.plist`, sinfs) are compressed by flate2, so their
      bytes differ from Go's compress/flate; the binary plist encoder differs from howett's
      (same decoded content); the replicate temporary file is truncated (Go opens it without
      `O_TRUNC`); a package without `Info.plist` and manifest is an error (Go panics); plist
      fields of an unexpected type read as empty (Go fails the decode); the progress bar
      only approximates `schollz/progressbar`. Untested against Apple: terms acceptance,
      storefront correction, the GAME retry, updateProduct and the delisted chain (all
      covered by fake executors only), visionOS/tvOS/watchOS downloads.
- [x] macOS `.pkg` download, decryption and publish. `download_package.rs`
      (`downloadPackagePlatform`), `download_macos.rs` (`downloadMacPackage`, the
      `StoreAgent` decrypter adapter, staging, XAR validation, backup/rollback publish),
      `macos_version_lookup.rs` (`lookupLatestMacOSExternalVersionID`, `serializedServerData`),
      `macos_version_metadata.rs` and `macos_preflight_metadata.rs` (the Mac readers that
      `get-version-metadata` uses in phase 7), and `xar.rs`, a port of go-macho's `pkg/xar`
      reader (v1.1.282). Go's tests are ported; `sendDownloadProduct`'s Mac pinning cases
      (`externalVersionId`, redownload `appExtVrsId`) are in `commerce_tests::version_lookup`.
      Oracle: `scripts/macos-package-oracle.sh` records `tests/fixtures/macos-package-oracle.json`
      from Go (43 XAR archives, 33 metadata cases, 72 offer selections, 189 platform
      decisions); `macos_oracle_tests` matches all of them. Live: `download --platform macos`
      of two owned native apps (TestFlight 4.4.0, Calculate84 2.6.4) writes `.pkg` files
      byte-identical to Go's (SHA-256), including a second run over an existing package.
      Differences: XAR signatures are not verified (Go stores the result and nobody reads
      it); files are kept sorted by ID (Go's map order is random, so only the first-reported
      error can differ); the TOC and package XML are parsed with `roxmltree` (strict, whole
      document) where Go's decoder is non-strict and stops after the root element, and XML,
      `time.Parse` and JSON decode error details differ after Go's prefix; close errors of
      the staging files are not reported (Rust drops files); the TOC is decompressed fully
      before parsing. Cancellation is dropping the future: staging is removed, but a running
      decryption thread finishes first. Go quirk kept: current Mac App Store packages nest
      `bundle` under `bundle-version` in `PackageInfo`, so Go's (and this) package metadata
      reader reports "does not contain metadata for the requested bundle" for them (checked
      on TestFlight 4.4.0 with Go). Untested live: preflight packages (old builds without
      `dpInfo`), iOS apps returned for a macOS request, and a failing rollback.

### Phase 7 — versions and purchases

- [x] `list-versions`, `get-version-metadata` (partial-zip `Info.plist` read with read-ahead),
      branch `phase7-versions`. `list_versions.rs` and `get_version_metadata.rs` port
      `appstore_list_versions.go` and `appstore_get_version_metadata.go` on `StoreSession`
      (platform pin first: Mac offer, tvOS/watchOS/visionOS lookup; shared failure mapping in
      `version_item`). `partial_zip.rs` ports `appstore_partial_zip.go`: `HttpRangeReader`
      (Go's `httpRangeReaderAt`: `bytes=0-0` size probe, `Content-Range` checks, 1 MiB
      read-ahead window, a partial body still serves the read, the window is replaced only by
      complete responses), requests through the Store `transfer` executor with
      `Accept-Encoding: identity`, and the `Info.plist` reader (display version keys, release
      date keys and value types, `Monday, January 2, 2006` and `2006-01-02` layouts, entry
      time fallback). `zipfile::Archive` now reads any `ReadAt` source through a 4 KiB
      buffered directory reader as Go's, checks the data descriptor CRC, and gives Go's
      `Modified` (NTFS, Unix, extended timestamp extra fields, else normalized MS-DOS time).
      A macOS package result goes through `read_version_metadata_from_mac_package`
      (preflight first, then the decrypted package), so the phase 6 readers are no longer
      dead code; the decrypter and OS seam is `StoreSession::mac_package`. The iOS/platform
      version lookups were already ported in phase 6 (`version_lookup.rs`,
      `macos_version_lookup.rs`). CLI `versions.rs` ports both commands with Go's flags and
      JSON (`externalVersionIdentifiers`, `bundleID`, `success`; `externalVersionID`,
      `displayVersion`, `releaseDate` in whole-second RFC 3339) and fork fix `aee638e`
      (account read once, re-login result reused). Tests: Go's
      `appstore_list_versions_test.go`, `appstore_get_version_metadata_test.go`,
      `appstore_partial_zip_test.go`, the `GetVersionMetadata` cases of the macOS metadata
      tests, `cmd/list_versions_test.go` and `cmd/get_version_metadata_test.go`
      (`commerce_tests::{list_versions, get_version_metadata, partial_zip}`, `versions/tests.rs`).
      Live (read-only, same output as the Go binary except `time`): `list-versions` for
      Perplexity (227 IDs), `get-version-metadata -i 1668000334 --external-version-id
      895516672` (26.46.0, `2026-11-18T08:16:56Z`, the package date; the catalog says
      19:21:02), and the three error lines (missing app, `--concurrency 0`).
- [x] `--with-metadata` catalog-first + cache + concurrency (fork feature, Go branch
      `feat/list-versions-metadata` 8a75934, aaaa3c6, 241230f). `list-versions
      --with-metadata --concurrency N` (default 4, below 1 is Go's error): the catalog
      version history (phase 0 `catalog_version_history`, newest 25, one unauthenticated
      request in the account's storefront), then `version-metadata.json` in the state
      directory, then per-version store requests at most N at once; the first failure stops
      the rest and the versions fetched before it are cached. Each entry has `source`
      (`catalog` or `store`). The cache has Go's shape and bytes (sorted keys, RFC 3339 with
      trimmed nanoseconds, mode 0600, `.tmp` then rename), so both tools share it. The fork's
      tests are ported (`versions/tests.rs`, plus a Go-written cache file read and rewritten
      byte for byte, and the catalog test for non-iOS platforms). Live: Perplexity from the
      Go-written cache (227 entries, no per-version requests; the catalog answered 429 and the
      cache covered it) gives the Go output and leaves the file byte-identical. Sony Music
      Center uncached: 85 versions, 25 from the catalog and 60 from the store; the Go fork
      binary run afterwards reads the Rust-written cache, prints the same JSON and rewrites
      the file byte-identical; four store entries (1.0.0, 3.3.1, 5.15.0, 6.7.0) match Go
      `get-version-metadata` run against Apple. Differences: a catalog failure's verbose
      line for network or decode errors uses this port's error text (HTTP status errors use
      Go's `catalog version history returned HTTP N`). Untested live: the macOS paths of
      both commands (offline tests only; no owned Mac app in the allowed set), tvOS/watchOS/
      visionOS pins, and the expired-token re-login (fake store only).
- [x] `list-purchases` (owned apps), branch `phase7-purchases`. `owned_apps.rs` ports
      `appstore_owned_apps.go`: the Configurator purchase history (DAAP `login`, signed
      `update`, signed `databases/<revision>/items` on `pd.itunes.apple.com`) for storefront
      `,34` then `,13`, the DMAP walker and parser, the platform mask, the merge by ID (all
      platforms, latest date, `unknown` dropped when another platform is known), the stable
      newest-first sort and the page. Requests go through the Store `request` executor with
      Go's headers; the SAP signer from the bag config signs each body before the request
      moves to the guest thread (`CommerceRequest::signature`; the signer is not `Send`, and
      the guest signs the raw body unchanged). `App` gained `purchaseDate` (printed only when
      set, RFC 3339 UTC, before `platforms`, as zerolog). The machine, bag and signer seam of
      login is now `SapBackend` (login keeps `LoginBackend: SapBackend`). CLI
      `list-purchases` has Go's flags (`-l/--max-results` 10, `-p/--page` 1, `--platform`,
      checked before the account is read), two attempts with one login after an expired
      token, and prints `count`, `totalCount`, `page`, `apps`. Go's `appstore_owned_apps_test.go`
      and `cmd/purchases_test.go` are ported (`owned_apps/tests.rs`, `commerce/tests.rs`).
      Found live: Apple's purchase history answers HTTP 411 to the bodiless login `POST`
      without `Content-Length`; the Store transport now sends `Content-Length: 0` for empty
      `POST`/`PUT`/`PATCH`, as Go's `net/http` does (`backend.rs`, with a test).
      Live (read-only): `list-purchases -l 100 --format json` and `--platform macos -p 2 -l 5`
      print the same JSON as the Go binary byte for byte except `time` (734 apps owned);
      `--platform watchos`, `-p 0`, `-l 101` and `--platform unknown` give Go's errors.
      Differences: `X-Apple-I-TimeZone` follows Go's `time.Local` name rules for `TZ` but
      does not check that a named zone loads (Go falls back to `UTC`); `X-Apple-TZ` is 0 when
      the local offset cannot be read; a signer error is reported by the guest with the
      signer's text only. Text mode keeps this port's field order without colors (Go's
      console writer sorts fields; open question 3). Untested live: the expired-token
      re-login (fake store only), DAAP `mstt` errors, and a Mac storefront failure.
- [x] Account read once per command (fork fix `aee638e`) for `purchase` and `list-purchases`
      (`download` already did): the account is read before the attempts and the account
      from the re-login is reused. `list-versions` and `get-version-metadata` do the same
      (branch `phase7-versions`).
- [x] Error lines carry `"success": false`, as Go's `Execute` logs them (all commands).

### Phase 8 — MCP

- [x] `ipatool mcp` (branch `phase8-mcp`): Go `cmd/mcp.go`, `cmd/mcp_tools.go` and
      `cmd/mcp_app.go` as `crates/ipatool-cli/src/mcp.rs`, `mcp/tools.rs` and `mcp/app.rs`,
      on `rmcp` 3.11 (stdio, `stdio_pipes`) with `schemars` 1.2 schemas. Same server name,
      instructions, capabilities (`logging`, `tools.listChanged`), protocol versions (Go SDK
      v1.8.0's four; an unknown one gets 2025-11-25), the six tools with their descriptions,
      annotations (`readOnlyHint`, `idempotentHint: false`), input/output field names and
      schema descriptions, listed in name order with `ttlMs`/`cacheScope`, and the empty
      prompt/resource lists Go's SDK answers. `mcp/schema.rs` ports what the tools rely on
      from `go-sdk` and `jsonschema-go`: the schema shape of `jsonschema.For` (inline, no
      titles or formats, `null` first for pointers and slices), the argument validation
      (`type`, `properties`, `additionalProperties`, `required`, with Go's error texts and
      `%v` value formatting; whole floats count as integers), and the result text
      (`encoding/json` of the result decoded into `any`: sorted keys, `\u0026`-style
      escapes, whole floats as integers). Tool calls are queued to one runner that owns the
      Store session (Go's one-at-a-time gate); `withMCPAccount` (one re-login with the saved
      password, Go's three error texts), the validation errors, `resolveApp` (delisted
      fallback for downloads), `completeApp` and the `"<tool> failed: ..."` wrapping are
      Go's. Interactive mode is off under `mcp` (keychain passphrase prompt), the error log
      line is JSON on stderr whatever `--format` says (Go's `initWithCommand`), an interrupt
      ends with "MCP server failed: context canceled" (exit 1), EOF before or after
      `initialize` exits 0. The `Store` seam gained `search`, and `lookup` takes a
      `LookupQuery` (app ID lookups for `completeApp`).
      Tests: every case of `cmd/mcp_test.go` and `cmd/mcp_app_test.go` (`mcp/tests.rs`,
      over an in-memory pipe with a scripted store), plus `tools/list` equal to the Go
      server's (`tests/fixtures/mcp_tools_list.json`), 21 validation texts recorded from the
      Go binary, the Go JSON escaping of the result text and Go `%v` float formatting.
      Live (Go binary vs Rust, `scripts/mcp_parity.py`, run by `scripts/parity.sh`):
      initialize, `tools/list`, the lists, validation errors, `search_apps` (with and without
      platform), `list_app_versions` (Perplexity), `get_version_metadata` (Perplexity
      895516672, by ID and by bundle ID), `list_purchases` (5 per page; page 2 with a
      platform) are equal JSON-RPC replies, result text included. `download_app` of Sony
      Music Center (`purchase` false) returns the same result object as Go (paths aside);
      the IPAs have the same 743 entries and differ only in the sinf and the plist encoding
      (as in phase 6). `purchase_app` on Perplexity (owned) answers `alreadyOwned: true`
      (Rust only, one call).
      Differences: `serverInfo.version` is the crate version (Go prints its build's
      `version`, "dev" without ldflags); non-object `arguments` are rejected by rmcp as a
      JSON-RPC error (-32601), where Go returns an `isError` result; numbers outside int64
      fail with serde's text instead of Go's `encoding/json` text; with several invalid
      properties Go reports one in map order (random), Rust the first in schema order; an
      invalid JSON line is skipped (Go's server stops with an error); a request before
      `initialize`, `resources/read`, `prompts/get` and `completion/complete` get rmcp's
      error codes and texts. Untested: cancellation (`notifications/cancelled` drops a
      queued or running call and answers "tool call canceled: context canceled"; Go passes
      its context into the Store calls instead), `download_app` with `purchase` true and
      the expired-token refresh (fake store only), macOS packages over MCP.

### Phase 9 — parity, release

Branch `phase9-parity`.

- [x] Parity suite over every command (`scripts/parity.sh` runs `scripts/parity.py` and
      `scripts/mcp_parity.py`). Offline by default: 43 command lines (usage and validation
      errors of every command, `auth info`, help of the command groups) and the 14 `mcp`
      calls that never reach Apple. `IPATOOL_PARITY_LIVE=1` adds read-only calls on owned
      apps: `search` (iPhone, watchOS, macOS, visionOS), `auth info`, `list-versions` by ID
      and bundle ID, `get-version-metadata`, `list-versions --with-metadata` from the shared
      cache, `list-purchases` (two pages), `purchase` of Perplexity and Sony Music Center
      (already owned), `catalog-versions` checked against Go's `--with-metadata` versions
      (no Go command), a download of Sony Music Center, and the 4 live `mcp` calls. JSON
      lines are compared without `time`; where Go prints a text line (pflag stopped before
      `--format`, unknown command) the exit status and the error text are compared, since
      text mode differs (open question 3); help text is compared by exit status and first
      line only. Downloads are compared by the IPA entry list (names and order) and the
      parsed `Info.plist` and `iTunesMetadata.plist`, not bytes. Exit status 1 on any
      difference. Error probes use owned app IDs, so a regression that skips a check cannot
      buy anything. The suite never runs `auth login`, `auth revoke` or a purchase of an
      app the account does not own.
- [x] Port differences the suite found, fixed on this branch:
      - usage errors: clap's own messages (exit 2) became cobra/pflag's error line with
        exit 1 — `accepts 1 arg(s), received N`, `required flag(s) "…" not set`,
        `unknown command "x" for "ipatool"` with cobra's suggestions, `unknown flag`,
        `unknown shorthand flag`, `flag needs an argument`, `invalid argument "v" for
        "-l, --limit" flag: strconv.ParseInt: …`, and `NoArgs` for `list-purchases` and
        `mcp`. The line is JSON only when pflag had parsed `--format json` before it
        stopped, as in Go. `auth`, `auth <unknown>` and no arguments print help, exit 0;
      - integer flags parse like `strconv.ParseInt(v, 0, 64)` (`0x10`, `010`, `1_000`,
        negative values, a value that starts with `-`); bool flags take `=value` like
        pflag (`--purchase=false`); leaf commands without an `Args` check ignore extra
        arguments (`auth info extra`), as cobra does;
      - `--platform` is parsed where Go parses it (after the app check, before the
        account), not by clap; `search` takes a negative `--limit`; `get-version-metadata`
        checks its required flag before the app;
      - keychain errors carry Go's `failed to get item: …` / `failed to set item: …` /
        `failed to remove item: …` (`pkg/keychain`);
      - visionOS search was not ported; now it is (`vision_search.rs`: Go's
        `searchVisionOS` and `storefrontVisionApps`, with their tests);
      - iTunes search and lookup URLs now use Go's sorted query order (`url.Values.Encode`);
      - `--version` prints `ipatool version <crate version>` (Go prints `dev` without
        release ldflags).
- [x] CI: `.github/workflows/` `lint.yml` (fmt, clippy on Linux and macOS), `unit-tests.yml`
      (Linux x86_64 and arm64, macOS), `dry-build.yml` (release builds for the four
      targets on pull requests), `release.yml` (on `v*` tags: the tag must match the
      workspace version, tests, then `ipatool-<version>-<os>-<arch>.tar.gz` with `bin/<file>`
      and a `.sha256sum`, as the Go release, uploaded to the GitHub release). The Go
      workflow's Homebrew tap update is left out. CI uses the nightly toolchain, since the
      port builds with rustc-master. `actionlint` 1.7.12 (with shellcheck) reports nothing.
      Nothing is pushed and no repository is created (open question 2).
- [x] Per-OS gaps closed, branch `phase9-platforms`:
      - `machine.rs` ports `mac_address_darwin.go` (IOKit `IOMACAddress` of the interface
        or its parents, `en0` first, unredacted network address as fallback),
        `mac_address_windows.go` (`GetIfEntry2Ex` permanent address, hardware/filter bits,
        Bluetooth PAN and interface type checks; the entry point is looked up at run time
        like Go's lazy DLL proc, so older Windows 10 falls back) and
        `mac_address_default.go` (iOS and the BSDs: the interface address as a fallback),
        with Go's `net.Interfaces` for each OS (`getifaddrs` `AF_LINK` entries on Apple and
        the BSDs; `GetAdaptersAddresses` on Windows, with Go's index, name and
        loopback/point-to-point rules). The selection, the once-per-process cache and the
        Linux sysfs path are unchanged. The OS calls live in `ipatool-appstore/src/sys.rs`,
        the crate's one `unsafe` module (`AGENTS.md` updated); Go's `macAddressAPI` is the
        `IoKit` trait, so its IOKit release-order tests run on every host. Go links IOKit and
        CoreFoundation with `purego.Dlopen`; the port links the frameworks.
      - `cmd/keyring_ios.go`: iOS uses the Keychain backend (it picked Secret Service
        before), and `set` updates first with a class/service/account query (no
        `kSecMatchLimit`/`kSecReturnAttributes`), adding the item with its label when it
        does not exist; update errors read `update iOS keyring item: …`. Secret Service is
        now used on Linux (and Android) only, as keyring's `linux` build tag; the BSDs use
        the file backend like Go.
      - Windows builds: file modes are applied on Unix only (Go's modes are ignored there),
        `xar.rs` reads with `seek_read`, the state directory home is `%HOMEDRIVE%` joined
        with `%HOMEPATH%` (Go's `machine.HomeDirectory`), masked password input uses x/term's
        Windows `MakeRaw` (console mode without echo, line and processed input, with VT
        input), and the cookie-jar lock follows `go4/lock`: fcntl only where Go uses it
        (linux/amd64, linux/arm, darwin/amd64, freebsd), else Go's portable owner-PID file
        (`{"OwnerPID":N}`, stale when the owner is gone, closed before removal). That
        changes the lock on Apple silicon, arm64 Linux and Windows to match Go there.
      - The SAP cache directory is `~/Library/Caches` on iOS too, as `os.UserCacheDir`.
      - CI: Windows (`windows-latest`, `windows-11-arm`) added to lint, unit tests, dry
        build and release (`ipatool-<version>-windows-<arch>.tar.gz` with
        `bin/<file>.exe`, as Go). `actionlint` 1.7.12 reports nothing.
- Targets, checked locally with `cargo clippy --workspace --all-targets --target <t> --
  -D warnings` for every target whose rustc-master std is installed. This proves the Rust
  code; the C dependencies were built where a cross compiler exists, otherwise Unicorn's
  build script found a stub `unicorn.pc` (pkg-config, so no QEMU build; clippy does not
  link) and bindgen read zig's libc headers:
  - x86_64-unknown-linux-gnu: host, full build and tests; the MAC address equals Go's
    (`machine.New(...).MacAddress()` from `/tmp/ipa-ref`; same address).
  - aarch64-unknown-linux-gnu, x86_64-unknown-linux-musl, armv7-unknown-linux-musleabihf:
    clippy clean (zig cc as the C compiler).
  - x86_64-pc-windows-gnu: clippy clean, and `cargo build --workspace --all-targets`
    links (mingw-w64 gcc 15 for C, a real Unicorn cmake build; this mingw toolchain has no
    `libgcc_eh.a`, its unwinder is in `libgcc.a`, so an empty `libgcc_eh.a` was put on the
    link path). `ipatool.exe` imports only `GetAdaptersAddresses` from `iphlpapi.dll`.
    Not run: no Windows machine or wine here. MSVC and aarch64 Windows std are not
    installed; CI builds those natively.
  - aarch64-apple-darwin: clippy clean (clang with zig's macOS headers, no SDK). Not
    linked or run here.
  - aarch64-apple-ios-sim: clippy clean (macOS headers, an empty `SDKROOT` and a
    declarations-only `CommonCrypto/CommonRandom.h` stub for aws-lc).
  - aarch64-apple-ios: release build through `scripts/build-ios.sh` (see the iOS item
    below); built, linked and signed here with an iPhoneOS 16.5 SDK. In CI.
  - x86_64-apple-darwin: target std not installed here; not checked (CI builds it).
  - wasm32-unknown-unknown, wasm32-wasip2: std installed, not a target of the tool (no
    Go equivalent); not checked.
- [x] `profilegen` port (branch `phase9-profilegen`): Go `internal/store/profilegen` and its
      `cmd/profilegen` as the `ipatool-profilegen` crate (binary `profilegen`, not built into
      `ipatool`). One module per Go file (`source`, `symbols`, `discover`, `objc`, `vm`,
      `realize`, `swift_metadata`, `features`, `collect`, `references`, `classes`, `validate`,
      `emit`). Go's `assets/store.go` and `store_images.go` are `ipatool_sap::assets::StoreCache`
      (`assets/remote/store.rs`) on the existing dyld, APFS, AEA and HTTP-range readers:
      `open_local` (cache files plus host `/System` feature plists) and `with_remote` (Apple's
      restore image), `page` with per-page slide memo, `contains`, `address_range`, `images`,
      `open_image` (load commands, sections, segments, dependency kinds and dylib-use flags,
      UUID) and `dyld_exports` (export trie). The bootstrap VM is `ipatool_sap::engine` with its
      state in the Unicorn data slot; Go's panics are an `Error::Panic` variant that context
      wrappers pass through and `generate`/`validate` turn into Go's one-prefix error.
      The binary writes the three zstd files the Rust profile embeds (`--output`, default
      `crates/ipatool-store/profile`), `--check` compares, and an artifact whose decoded
      bytes already match is not rewritten, so the committed frames from Go stay.
      Oracle (same local inputs: the 80 arm64e cache members and the three FeatureFlags
      plists, copied from the pinned restore image with Go's own extractor; Go's run had
      the plists at `/System` through a `bwrap` bind mount): Go's generator reproduced the
      committed `data_generated.go` byte for byte (2 min 21 s), and the Rust generator's
      decoded artifacts are byte-identical to it (35 s): metadata 4 807 917 bytes
      `5e92717743272aa84343bd25f6bab6401155f88ade51b704ec535d3e6b5bd421`, pages 20 893 696
      bytes `4fd8cf193ca6e71d52a2d6e68d9ca76907332ff33dade332351c97efe5f0caed`, plugin
      138 088 bytes `e33fff05a6e6593dd91e33fd103b5d3068b62a787c680df33b859f19b3acc4ea`.
      `profilegen --check` against the committed profile reports it up to date. The zstd
      frames differ: Go uses klauspost's best level, `ruzstd` only has its fastest level, so
      a regenerated changed artifact would be larger (metadata 1.26 MB vs 0.72 MB, pages
      6.9 MB vs 4.2 MB, plugin 55 KB vs 32 KB) but decodes to the same bytes.
      Tests: every `It` of the Go suite except "honors cancellation before encoding" (Rust
      has no context argument; a Ctrl-C ends the process) — `classes`, `references`
      (9 cases), `discover` (records, GOT references, witness and foreign metadata
      returns), `emit`, `validate`, and the VM cases (ADRP, RCpc lowering, heap overflow,
      ranges, CAS, CASP, 32-bit LDADD) on a synthetic one-mapping cache; plus mangled-name
      lengths, zone allocations with options, Go JSON escaping, the `StoreCache` reads and
      `version()` equal to `PINNED_INPUTS`.
      Differences: no `context` cancellation (Go checks it every 4096 guest steps; each guest
      call keeps Go's 30 s and 20 M-instruction bounds); the output is the three `.zst` files,
      not Go source. Remote mode (no `--cache`: cache members and feature plists read from
      Apple's restore image over HTTP ranges, no account traffic) produced the same three
      decoded SHA-256 values (5 min 4 s, 750 MB RSS). Untested: `validate` against a
      native-exported profile (Go has no caller in the repository either), and the Windows
      `seek_read` path of the local cache reader.
- [x] iOS release build, branch `ios-release`. Go's `tools/build-ios.sh`,
      `tools/patches/unicorn-ios.patch` and `resources/ios-entitlements.plist` are
      `scripts/build-ios.sh`, `scripts/patches/unicorn-ios.patch` and
      `resources/ios-entitlements.plist` (the entitlements file is Go's, byte for byte).
      - Patch scope: the patch drops `PROT_EXEC` from Unicorn's translation buffer, so it
        must reach only this build. The script extracts the locked `unicorn-engine-sys`
        `.crate` from the cargo cache (checked against the `Cargo.lock` checksum, as Go
        checks its tarball's SHA-256), patches the copy and builds `ipatool-cli` for
        `aarch64-apple-ios` with `--config patch.crates-io.unicorn-engine-sys.path=<copy>`
        and `--config resolver.lockfile-path=<copy of Cargo.lock>`. Nothing in the
        repository names the patched crate, so other targets and `Cargo.lock` never see
        it. The patch target is `unicorn-engine-sys` (the C sources and `build.rs`), not
        `unicorn-engine` (Rust API only). The output is signed with `ldid -S<entitlements>`.
      - Sources: `unicorn-engine-sys` 2.1.5 bundles Unicorn 2.1.4's C sources. They equal
        the 2.1.4 tag Go pins, except for an MSVC-only block in `qemu/util/guest-random.c`.
        So Go's hunks apply unchanged (no offset, no fuzz).
      - One hunk changed, for a duplicate symbol: Go builds one QEMU target
        (`UNICORN_ARCH=x86`), while the crate builds `arm` and `aarch64` (`arch_aarch64`
        implies `arch_arm`). Go's global `ipatool_jit_write_protect` in `translate-all.c` is
        compiled once per target, so the iOS link failed with a duplicate symbol. The
        function is now `static` with the same body. Each target's `tcg_exec_init` stores
        it in a new `uc_struct` field (`include/uc_priv.h`). The header's
        `jit_write_protect` calls that field, so the target-independent `uc.c` reaches the
        running target's buffer. It does nothing before `tcg_exec_init`, as Go's null
        check does. The adapted patch also applies to the 2.1.4 tarball.
      - Go flags and the crate's `build.rs`: `CMAKE_SYSTEM_NAME=iOS` comes from the cmake
        crate (cross build). arm64 only comes from `ARCHFLAGS='-arch arm64'`, read by
        Unicorn's CMakeLists, as in Go. iOS 15.0 comes from `IPHONEOS_DEPLOYMENT_TARGET`
        (cc-rs puts `--target=arm64-apple-ios15.0` into `CMAKE_C_FLAGS`; rustc uses it for
        the link). `Release` comes from the cargo profile, and `UNICORN_BUILD_TESTS=OFF` is
        already set. These Go flags are not taken:
        - `BUILD_SHARED_LIBS=OFF`: CMake also builds an unused dylib, and the static
          `libunicorn.a` is linked.
        - `UNICORN_INSTALL=OFF`: the crate installs to find the library.
        - `UNICORN_ARCH=x86`: the crate's features decide.
        - `-force_load`/`-export_dynamic`: Rust links the Unicorn symbols directly; Go
          needed them for purego's `dlsym`.
        The script exports Go's `SDKROOT`, sets `CC_aarch64_apple_ios` instead of a global
        `CC` (host build scripts keep the host compiler), and sets
        `PKG_CONFIG_ALLOW_CROSS=0`, so a system Unicorn found by pkg-config is never linked
        instead of the patched one. The version is the crate version, not an ldflag.
      - Go finding (not ported): Go's script builds Unicorn with `UNICORN_ARCH=x86`, but its
        engine opens `UC_ARCH_ARM64` (`internal/sap/unicorn/engine.go`, `archARM64 = 2`).
        So the Go iOS binary's `uc_open` should fail with `UC_ERR_ARCH`. This is from
        reading the code; it was not run.
      - Keychain: the Rust iOS backend (`ipatool-keychain/src/macos.rs`) queries, updates
        and adds with class, service (`ipatool-auth.service`) and account only. It sets no
        `kSecAttrAccessGroup` and no data-protection key, as keyring v1.9.0's
        `keychain_ios.go` and Go's `cmd/keyring_ios.go`. Items therefore go to the default
        access group, the first `keychain-access-groups` entry (`dev.majd.ipatool`, equal
        to `application-identifier`). Queries search the groups the entitlement grants.
        This is the same as Go.
      - CI: `release.yml` has a `build_ios` job on `macos-latest` (`brew install ldid
        cmake`, then the script). It packages `ipatool-<version>-ios-arm64.tar.gz` with
        `bin/<file>` and a `.sha256sum`, as the other jobs do, and the release job waits for
        it. `dry-build.yml` has Go's `build_ios` job. `actionlint` 1.7.12 and `shellcheck`
        report nothing.
      - Checked on Linux (no Xcode):
        - The script ran end to end with a stand-in `xcrun` that returns a real iPhoneOS
          16.5 SDK (from the `ios-sys` git checkout in `~/.cargo/git`) and a clang 22
          wrapper with `ld64.lld`. CMake built the patched Unicorn for iOS, cargo linked
          `ipatool` (Mach-O arm64, `LC_BUILD_VERSION` platform ios, minos 15.0), and `ldid`
          signed it. Here `ldid` is a wrapper around `rcodesign`. The signature carries the
          three entitlements.
        - Disassembly of that binary: the translation buffer is mapped with `PROT_READ |
          PROT_WRITE` (3) and `MAP_PRIVATE | MAP_ANON`, without `MAP_JIT`. Each target's
          `ipatool_jit_write_protect` switches between 3 and 5 (`PROT_READ | PROT_EXEC`).
          Configure set `CONFIG_DARWIN` and not `HAVE_PTHREAD_JIT_PROTECT`.
        - `cargo clippy --workspace --all-targets --target aarch64-apple-ios -- -D warnings`
          is clean with the patched sources and the real SDK (no stub `unicorn.pc`).
          `cargo test --workspace --no-run --target aarch64-apple-ios` links.
        - Run-time check of the W^X path on the Linux host: the patched sources built with
          `-D__ENVIRONMENT_IPHONE_OS_VERSION_MIN_REQUIRED__=150000` turn on the iOS branch.
          The `ipatool-sap` and `ipatool-store` tests pass (141, Apple code included). The
          negative control, the same build without the `tcg_exec_init` assignment, crashes
          `engine::tests::runs_arm64_with_go_configuration` with SIGSEGV.
        - Host: gates pass (645 tests). Without the `--config` options cargo resolves the
          registry `unicorn-engine-sys`, the host `alloc_code_gen_buffer` maps with
          `PROT_EXEC` (7), and the host archive has no `ipatool_jit_write_protect`.
      - Untested: everything on a device. That covers the W^X toggle under the iOS kernel
        with `no-container`, the keychain with the entitlement, and any command. Also
        untested: a build on macOS with Xcode. There, configure checks with the host macOS
        target and the iPhoneOS sysroot, as in Go; here the wrapper compiled them for iOS.
        Apple `ar` is also used there; this Linux build warned that `libunicorn.a` is
        GNU-format. The CI jobs have not run.

Verification of `phase9-platforms`: host gates pass (`cargo fmt --all --check`,
`cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`: 612
tests); `scripts/parity.sh` offline: 0 differences. Go's tests ported:
`mac_address_test.go` (address rules, selection, the once-per-process identity, the
interface-error prefix, the real machine), `mac_address_linux_test.go`,
`mac_address_darwin_test.go` (both tables, through the `IoKit` trait),
`mac_address_windows_test.go` (row classification; `OperStatus` is not read, so that case
is one check), `keyring_ios_test.go` (compiled for iOS, runs only on a device), and
`go4/lock`'s portable lock cases. No live Apple calls were made for this work.
Untested on a real machine: everything macOS, Windows and iOS at run time (IOKit,
`getifaddrs`, `GetAdaptersAddresses`/`GetIfEntry2Ex`, the Windows console mode, the
portable lock against a running Go process, the iOS keychain update), and the Windows CI
jobs have never run (test path assumptions on Windows may still fail there).

Verification of `phase9-parity`: gates (`cargo fmt --all --check`, `cargo clippy --workspace --all-targets --
-D warnings`, `cargo test --workspace`) pass. Offline suite: 0 differences. First live
run: 1 difference (visionOS search not ported), fixed; second live run after the fixes: 80 checks, 0 differences (about 40 Apple requests per tool, two downloads of Sony Music Center).
Untested: the CI workflows have not run on GitHub (runner images, the x86_64 macOS
cross-build of Unicorn, bindgen's libclang on the runners); text-mode output still differs
from Go (field quoting, time, colors; open question 3); the cobra `completion` command is
not ported; a flag whose value starts with `-` is taken as the value only for integer
flags (pflag does it for every flag).

### Offline FairPlay decryption (fork feature, opt-in, GPL-3)

Branch `decrypt`. A new crate `crates/ipatool-decrypt` (`license = "GPL-3.0-or-later"`, its own
`LICENSE`) is a port of [EthanArbuckle/fairplay-decrypt](https://github.com/EthanArbuckle/fairplay-decrypt)
(`core.py` + `decrypt.py`, GPL-3 "or any later version"). It derives per-page keys from a
16-byte credential and an app's SINF and SUPF records, decrypts the encrypted arm64 Mach-O
pages (AES-128-CBC) and clears `cryptid`:

- `core.rs`: `arm64_encryption_infos` (thin `cffaedfe`, FAT32 `cafebabe`, FAT64 `cafebabf`),
  the SINF atom walk and `derive_work16` (448- and 440-byte `priv`; the 440 record ends in 8
  zero bytes), SUPF v3 and v5 offsets (formats `507`/`510`/`612`), the page-key mixing
  (`finish`, `block_transform`, `derive_page_key`) and `decrypt_binary`. Error texts match
  `core.py`.
- `ipa.rs`: `decrypt.py`'s plan (`supf_paths`, `enclosing_app_sinf`, v5→v4 SINF fallback),
  decrypting each binary and writing a new IPA. It reuses `ipatool-appstore`'s zip reader and
  writer through the new public facade `ipatool_appstore::IpaReader` (`open`, `names`, `read`,
  `rewrite`) instead of new zip code; the GPL crate depends on the MIT crates, never the other
  way round.
- CLI: a non-default `decrypt` feature on `ipatool-cli` adds `ipatool decrypt
  <bundle-id | --app-id N | path/to/app.ipa> [-o OUTPUT] [--credential PATH] [--purchase]
  [--external-version-id ID]`. For a bundle/app ID it downloads with the existing `download`
  path (shared `commerce::download_run`, same flags and retry/re-login), then decrypts. The
  credential defaults to `credential.bin` in the state directory, then `./credential.bin`.
  Output JSON is `{output, decrypted, success}`. The default build does not enable the feature
  and does not link the GPL crate (checked with `cargo tree -p ipatool-cli -e normal`); the
  release workflow keeps building without it. Binaries built with the feature are GPL-3.
- Tests: the Python tool has no tests; Rust tests are built from its logic. Synthetic fixtures
  (`tests/fixtures/decrypt_binary*.json`, generated by the reference `core.py`, synthetic
  credentials only) check `decrypt_binary` and `derive_work16` byte-for-byte for a thin arm64
  Mach-O (448-byte `priv`, SUPF v5) and a FAT32 slice (440-byte `priv`, SUPF v3); unit tests
  cover the parser error texts.
- Verified against the Python oracle's real output (`~/Downloads/ipas`, decrypted with
  `fairplay-decrypt` and its credential): the Rust `decrypt` of the encrypted Chromecast
  (4.39.23) and Sony Music Center (7.7.0) IPAs gives the same entry list and order, and every
  entry's decompressed content (all 17687 and 743 entries, every Mach-O) is byte-identical to
  the Python output. Zip framing differs: unchanged entries are raw-copied with the app's
  original compressed bytes and the 658/658 deflated entries get a trailing data descriptor,
  where the Python tool re-deflates with no descriptor (Sony: 54.70 MB vs 54.07 MB); per-entry
  compression methods are identical. One live run `ipatool decrypt jp.co.sony.audio.companion`
  (owned, no `--purchase`) downloaded 7.7.0/112 from Apple and decrypted it: same 743 entries,
  742 byte-identical to the reference including every Mach-O; only the per-download `.sinf`
  differs (decrypt copies it through unchanged; the sinf differs between any two downloads).
- Not ported (from `ipadecrypt`'s `internal/fairplay`, read only for comparison, out of scope
  per the task): SUPF v4 (`supp`) and the older `319`/`322`/`502`/`611` formats, arm64e,
  arm64_32, armv7, Arcade, on-device decryption and credential minting. Untested: SINF `priv`
  records other than the 448/440 shapes, SUPF format `612`, and Mach-O inputs with multiple
  arm64 slices beyond the thin/FAT fixtures (the multi-slice code path is ported from `core.py`
  but only the synthetic single-slice-per-file fixtures exercise it live).

### Upstream issues (fork fixes)

Branch `upstream-issues`. Fixes for open majd/ipatool issues, following the design in each
issue or its linked fork branch where it is sound. Without the new flags, the commands,
keychain entries and JSON output stay as before.

- [x] majd/ipatool#679, Secret Service collection on oo7-daemon (Fedora 45).
  - Problem: the spec leaves collection paths to the daemon. GNOME Keyring escapes `-` and
    `.` (`ipatool_2dauth_2eservice`), but oo7-daemon replaces them with `_`
    (`ipatool_auth_service`). Go's `byteness/keyring` (and this port) only decoded the path,
    so on oo7 every `get` failed with "not found" and every `set` created a new collection.
  - Fix (`secret_service.rs`): match the decoded path first, as Go does. Else match the
    collection's `Label`, compared without ASCII case. The case rule comes from reading
    oo7-daemon 0.7.2: after a daemon restart it reloads a collection from
    `keyrings/v1/<label lowercased>.keyring` and sets the label to the capitalized file name
    (`Ipatool-auth.service`). An exact label match (the issue's suggestion) would fail after
    every reboot.
  - Verified:
    - oo7-daemon 0.7.2 (`cargo install oo7-daemon`) on a private D-Bus bus
      (`dbus-run-session --config-file` with one `unix:dir` listen address), with a
      pre-created `ipatool-rust-test.service` keyring unlocked by `--login`. That is the
      after-restart state: path `.../ipatool_rust_test_service`, label
      `Ipatool-rust-test.service`. The old code did not find the collection, tried
      `CreateCollection` and waited on a prompt nobody answers (killed after 20 s). The
      fixed `IPATOOL_TEST_SECRET_SERVICE=1` round trip passed twice in a row.
    - The same live test passes on GNOME Keyring (Cole's session, throwaway collection).
    - Unit test for the path rule.
  - Untested: a collection created through oo7's `CreateCollection` (that needs a prompter,
    which a private bus does not have; oo7 then keeps the exact label), and KWallet's
    Secret Service.
  - Warning for local testing: `/etc/dbus-1/session-local.conf` on this machine adds
    `unix:runtime=yes`, so a plain `dbus-run-session` binds `/run/user/1000/bus` and takes
    over the real session bus path. Always pass a private `--config-file`.

- [x] majd/ipatool#671, FCS key download retried. Go's `resolveAEASymmetricKey`
  (`aea.rs` `resolve_aea_symmetric_key`) fetched `wkms-public.apple.com` once, so one
  transient EOF ended the whole download, while every range read of the same image is
  retried. Now up to 5 attempts, waiting 1, 2, 3 and 4 s, as the issue's patch does. A
  transport error, a non-200 status or a body read error is retried. The last attempt's
  error text is returned unchanged. The issue's patch also sets `User-Agent: Mozilla/5.0`
  on this request. That is not taken: the reporter got the key with and without it, so it
  is not part of the fix, and Go's request has no such header.
  - Verified: unit tests with an injected sleep (success on the third attempt; five
    failures give the wait sequence 1, 2, 3, 4 and the last error). Not tested against a
    failing network: the key URL must be `https://wkms-public.apple.com`, so a local fake
    server cannot stand in for it.

- [x] majd/ipatool#669, `download --no-parallel`, from chrismessina's
  `feat/no-parallel-download` (f54a90d). Same flag name and help text. The flag sets
  `DownloadInput.disable_parallel_download` (Go: `DisableParallelDownload`).
  `Client::download_file` and `download_file_blocking` take `parallel` (Go:
  `downloadFileWithOptions`). macOS packages pass it through `download_mac_package`.
  `get-version-metadata` and MCP keep parallel ranges, as in the patch. The fork's test is
  ported: a 256 MiB fixture with the flag off makes one `bytes=0-` request, assembles the
  right bytes, leaves no `.ipatool-parts`, and counts all bytes.
- [x] majd/ipatool#649, download progress modes, from NSExceptional's
  `download-progress-modes` (d8088ad, 1867a8e, f35f080). `--progress
  auto|bar|json|plain|none` on `download`, or `IPATOOL_PROGRESS` (the flag wins). `auto`
  gives `none` with `--format json`, `bar` when interactive and stdout is a terminal,
  else `plain`. `json` and `plain` write to stderr with the fork's throttling (JSON 250 ms;
  plain every 1 % at least 30 ms apart, a 15 s heartbeat). They also use the fork's line
  shapes and byte format (SI, one decimal).
  - Phases: `resolving` at each attempt and in `Download`, `purchasing`, `downloading`,
    `patching` (iOS), `decrypting` (macOS, after the encrypted package download).
  - `Progress` (`ipatool-appstore`) gained `phase`. The fork replaced the progress bar type
    with `Phase`/`Bytes`. Here the existing `set_total`/`set`/`add` stay, because the
    parallel ranges already add to one shared counter. The reporters turn them into the
    fork's `Bytes(current, total)`.
  - The fork's branch predates parallel ranges, so its `downloadFile` changes were applied to
    the current download code. One addition from the fork is kept: a finished resume (416)
    reports its size.
  - The bar moved to `progress.rs` and starts at the `downloading` phase, as the fork's
    `barProgress` does.
  - The fork's tests are ported (`progress/tests.rs`: mode table, unknown mode, plain and
    JSON throttling with a fake clock, unknown totals, byte format; `commerce/tests.rs`:
    phases in plain mode, also with the purchase retry, the invalid mode checked after the
    app check and before the account read, `--no-parallel` reaching `DownloadInput`; the
    resume-complete case in `commerce_tests/download_file.rs`).
  - Behavior change, as in the fork: with `auto`, a non-interactive text run, or a run whose
    stdout is not a terminal, now prints plain progress on stderr. `--progress none` gives
    the old output. JSON output is unchanged unless `--progress json` is asked for.
  - The fork's second branch, `no-color-off-tty` (1eac401), is already satisfied: this
    port's text lines never contain ANSI colors (open question 3), so nothing changed. The
    bar is the only output with control characters, and `auto` picks it only for a
    terminal.
  - Live: `download -b jp.co.sony.audio.companion --no-parallel --progress plain
    --non-interactive --format json` (7.7.0, 73.9 MB) printed 104 stderr lines (`resolving`,
    `downloading`, 0 % to 100 % one per percent, `patching`) and one JSON line on stdout. The
    IPA has 743 entries, passes CRC checks, and its `Info.plist` says 7.7.0. Sony is below the
    128 MiB threshold, so this run used one connection either way. The large-package path
    with the flag is covered by the fixture test only.
  - Untested: `--progress bar` and `auto` in a real terminal (the bar code itself did not
    change), the `decrypting` phase live (no owned Mac app in the allowed set), and the
    `json` mode live.

- [x] majd/ipatool#235, several accounts. Design from the issue comments (dvcrn:
  "specify the keychain entry ... `--keychain-id`").
  - The closed PR majd/ipatool#420 is not followed: it rewrote the `account` entry into an
    account list, which breaks Go-compatible entries.
  - A global `--keychain-id <ID>` makes `ipatool_keychain::Config::id` store every key as
    `<key>/<ID>`: `account/<ID>`, `kbsync-v1/<ID>`, `anisette-v4/<ID>`. These are
    ordinary Go-format entries under other names. The file backend percent-encodes the `/`
    as Go's does.
  - The cookie jar is `cookies-<ID>` in the state directory. Apple's session cookies (pod,
    DSID) must not mix between accounts; #420 cleared the jar on every switch for the same
    reason.
  - Scoping the anisette state as well means a new ID provisions its own anisette at its
    first login, which is the normal first-login path. Sharing it would also work (Go keeps
    it across `auth revoke` and the next login), but then one ID's keychain would depend on
    another's.
  - The ID allows ASCII letters, digits and `.` `_` `@` `+` `-`, and must not start with `.`
    (it is part of a file name). An email address works. Without the flag, keys, cookie
    file and output are unchanged. `version-metadata.json` is shared, since version data
    does not depend on the account.
  - Tests: key mapping and isolation on the file backend; Go's `byteness/keyring` helper
    reads `account/work` written by the port (`IPATOOL_GO_KEYRING`); the flag is global
    and validated (`main.rs` tests).
  - Live (read-only): `auth info` still reads the default entry, `--keychain-id
    nobody@example.com auth info` gives Go's "not found" error line, and a bad ID is
    rejected.
  - Untested: a second real login (account-changing; not run), and two accounts'
    downloads side by side.

Verification of `upstream-issues`: `cargo fmt --all --check`; `env -u CLIPPY_CONF_DIR cargo
clippy --workspace --all-targets -- -D warnings` with and without `--features
ipatool-cli/decrypt`; `cargo test --workspace` with and without the feature (674 tests
pass, 3 ignored); `IPATOOL_TEST_SECRET_SERVICE=1` and `IPATOOL_GO_KEYRING` keychain tests;
`scripts/parity.sh` offline: 0 differences. Live Apple traffic: one Sony Music Center
download and one `auth info`.

## Open questions for the user

1. Shared state: decided in phase 1 — the Rust tool uses the same state directory and
   keyring entries as Go, so one login works for both.
2. Repository: decided 2026-11-21 — publish as public `coleleavitt/ipatool-rs`.
3. Text mode: decided 2026-11-21 — keep this port's text format; only JSON output is
   guaranteed identical to Go.
