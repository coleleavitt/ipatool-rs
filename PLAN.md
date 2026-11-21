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
      StoreAgent decryption (need live Apple provisioning data or a purchased `.pkg`;
      `.pkg` decryption is now checked live in phase 6, byte-identical to Go).
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
- Targets, checked locally with `cargo clippy --workspace --all-targets -- -D warnings`
  per target (rustc-master std installed for these). The C dependencies (Unicorn/QEMU,
  aws-lc, lzfse) were compiled with zig as the cross C compiler where it could; this
  machine has no macOS SDK or aarch64 sysroot, so Unicorn's C build was skipped with a
  `links` override and its bindgen output, and iOS C code was compiled against macOS
  headers. So this proves the Rust code, not the C cross-builds; CI builds on native
  runners.
  - x86_64-unknown-linux-gnu: host, full build and tests.
  - aarch64-unknown-linux-gnu: clippy clean.
  - aarch64-apple-darwin: clippy clean after three fixes on this branch (dead code in
    `machine.rs` off Linux, `tokio::fs` in a test needed the `fs` feature, the keychain's
    async API without an await off Secret Service). Runtime gap: the MAC address lookup is
    ported for Linux only (`machine.rs` returns "not ported" elsewhere), so `auth login`
    and every Store command fail on macOS until Go's `mac_address_darwin.go` is ported.
  - x86_64-apple-darwin: target std not installed here; not checked (CI cross-builds it
    on the arm64 runner, as Go does).
  - x86_64-pc-windows-gnu: does not build. rustc stops in `ipatool-keychain/src/file.rs`
    (`std::os::unix` at lines 5 and 132, `mode()` at 33 and 78); the same Unix-only calls
    are in `cookies.rs`, `download_file.rs`, `download_macos.rs`, `xar.rs`
    (`FileExt::read_at`), `ipatool-cli` `state.rs` and `versions.rs`, and
    `ipatool-sap/src/assets/mod.rs`. The MAC address lookup is also Linux-only. Windows is
    left out of CI. MSVC and aarch64 Windows std are not installed here.
  - aarch64-apple-ios: clippy clean (with the header workaround above). Not releasable:
    Go's iOS build patches Unicorn for iOS W^X memory and signs with ldid
    (`tools/build-ios.sh`), uses an iOS keyring wrapper (`cmd/keyring_ios.go`; the port
    would pick Secret Service there), and the MAC lookup is Linux-only. Left out of CI.
- [ ] `profilegen` port (dev tool) — not done.
- [ ] Windows and iOS builds — see above.

Verification: gates (`cargo fmt --all --check`, `cargo clippy --workspace --all-targets --
-D warnings`, `cargo test --workspace`) pass. Offline suite: 0 differences. First live
run: 1 difference (visionOS search not ported), fixed; second live run after the fixes: 80 checks, 0 differences (about 40 Apple requests per tool, two downloads of Sony Music Center).
Untested: the CI workflows have not run on GitHub (runner images, the x86_64 macOS
cross-build of Unicorn, bindgen's libclang on the runners); text-mode output still differs
from Go (field quoting, time, colors; open question 3); the cobra `completion` command is
not ported; a flag whose value starts with `-` is taken as the value only for integer
flags (pflag does it for every flag).

## Open questions for the user

1. Shared state: decided in phase 1 — the Rust tool uses the same state directory and
   keyring entries as Go, so one login works for both.
2. Repository: publish as `coleleavitt/ipatool-rs`?
3. Keep the Go tool's exact text output, or improve text mode while keeping JSON identical?
