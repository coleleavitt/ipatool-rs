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

## License

MIT, like the original. See [LICENSE](LICENSE).
