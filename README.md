# ipatool (Rust)

A Rust port of [ipatool](https://github.com/majd/ipatool): search the App Store and
download app packages from the command line.

**Status:** early. Public catalog commands work; login, purchase and download are
being ported. See [`PLAN.md`](PLAN.md).

```console
$ ipatool search perplexity --limit 1
INF apps=[{"id":1668000334,"bundleID":"ai.perplexity.app",...}] count=1

$ ipatool catalog-versions -b ai.perplexity.app --format json
{"level":"info","app":{...},"count":25,"versions":[{"externalVersionID":"895516672","displayVersion":"26.46.0",...}],...}
```

## Build

```sh
cargo build --release
cargo test --workspace
```

## License

MIT, like the original. See [LICENSE](LICENSE).
