# ipatool (Rust)

A Rust port of [ipatool](https://github.com/majd/ipatool): search the App Store, sign in,
purchase free apps, and download app packages from the command line.

**Status:** every command of Go ipatool is ported (tracked against upstream commit
`3b02b4c`). The JSON output (`--format json`) matches the Go tool; text output uses this
port's own format. See [`PLAN.md`](PLAN.md) for the per-phase notes and known differences.

## Commands

| Command | What it does |
|---|---|
| `auth login` / `auth info` / `auth revoke` | Sign in (with 2FA), show, or remove the saved account |
| `search` | Search the App Store |
| `purchase` | Get a license for a free app |
| `download` | Download an IPA (iOS, iPadOS, tvOS, watchOS, visionOS) or a macOS `.pkg` |
| `list-versions` | List an app's version IDs; `--with-metadata` adds display versions and dates |
| `get-version-metadata` | Display version and release date of one version ID |
| `list-purchases` | Apps the account owns |
| `catalog-versions` | Newest 25 versions with display names, from the public catalog (no login) |
| `mcp` | The same tools as an MCP server over stdio |
| `decrypt` | Download (or read a local `.ipa`) and decrypt it offline with a 16-byte credential (opt-in `decrypt` feature; GPL-3) |

```console
$ ipatool auth login -e you@example.com
$ ipatool download -b ai.perplexity.app --purchase -o ~/Downloads
$ ipatool list-versions -b ai.perplexity.app --with-metadata --format json
```

`list-versions --with-metadata` takes the newest 25 versions from the public catalog in one
request, fetches older ones from the store (`--concurrency`, default 4), and caches them in
`version-metadata.json` in the state directory.

## Compatibility with Go ipatool

The Rust tool uses the same state directory (`$XDG_STATE_HOME/ipatool`, `$XDG_DATA_HOME/ipatool`
or `~/.ipatool`), the same keyring entries (service `ipatool-auth.service`) and the same cookie
jar, so one login works for both tools. Sign-in uses the same Store emulator as Go (Apple's
binaries run under Unicorn); the Apple framework files it needs are downloaded once from
Apple's restore image and checked against pinned SHA-256 digests.

## Build

Needs a nightly Rust toolchain, CMake and a C compiler (Unicorn is built from source).

```sh
cargo build --release
cargo test --workspace
env -u CLIPPY_CONF_DIR cargo clippy --workspace --all-targets -- -D warnings
scripts/parity.sh                       # compare with a Go ipatool binary (offline)
IPATOOL_PARITY_LIVE=1 scripts/parity.sh  # also run read-only live checks
```

Platforms: Linux (x86_64, arm64), macOS (arm64, x86_64), Windows x86_64. Linux is tested end to
end; macOS and Windows build and pass the unit tests in CI but have not been run against the
App Store. `scripts/build-ios.sh` builds a signed binary for jailbroken iOS 15+ (patched
Unicorn, `ldid`); it has not been run on a device. Windows arm64 is not supported, because
Unicorn's C build does not support it.

## Offline FairPlay decryption (opt-in, GPL-3)

The non-default `decrypt` feature adds an `ipatool decrypt` command that produces a decrypted
IPA like `ipadecrypt`: given a bundle ID, an app ID (`--app-id`), or a path to a local `.ipa`,
it downloads the app with the normal `download` path (same flags and retry/re-login logic) and
then decrypts it offline using a 16-byte credential.

```sh
cargo build --release --features ipatool-cli/decrypt
ipatool decrypt <bundle-id | --app-id N | path/to/app.ipa> -o out.ipa [--credential credential.bin] [--purchase] [--external-version-id ID]
```

The credential defaults to `credential.bin` in the ipatool state directory, then
`./credential.bin`. Only offline decryption is implemented (no credential minting, no
on-device fallback). The decryption lives in the separate crate `ipatool-decrypt`, a Rust port
of [EthanArbuckle/fairplay-decrypt](https://github.com/EthanArbuckle/fairplay-decrypt). That
crate and its port are licensed **GPL-3.0-or-later**, so a binary built with the `decrypt`
feature is GPL-3. The default build does not enable the feature and does not link the GPL
crate, so it stays MIT. The release workflow builds without the feature.

## License

MIT, like the original, except the optional `ipatool-decrypt` crate
(`crates/ipatool-decrypt`), which is GPL-3.0-or-later; see its `LICENSE`. Binaries built with
the `decrypt` feature link it and are therefore GPL-3. See [LICENSE](LICENSE).
