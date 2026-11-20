"""Makes the exported Go Store emulator deterministic and traced for scripts/store-amd-oracle.sh."""
import pathlib
import sys

root = pathlib.Path(sys.argv[1]) / "internal/store/emulator"
patches = {
    "bag.go": [("time.Now()", "oracleNow()")],
    "bridge.go": [
        ("time.Now()", "oracleNow()"),
        ("time.Until(date)", "date.Sub(oracleNow())"),
        ("rand.Read(", "oracleRandRead("),
        ('\t"crypto/rand"\n', ""),
    ],
    "delegate.go": [("time.Now()", "oracleNow()"), ('\t"time"\n', "")],
    "runtime.go": [
        (
            '\tif call.Selector != "" {\n\t\tr.message(r.arg(0), call.Selector, false)',
            '\tOracleTrace = append(OracleTrace, call.Symbol+call.Selector)\n'
            '\tif call.Selector != "" {\n\t\tr.message(r.arg(0), call.Selector, false)',
        )
    ],
}
for name, replacements in patches.items():
    path = root / name
    source = path.read_text()
    for old, new in replacements:
        if old not in source:
            sys.exit(f"patch target not found in {name}: {old!r}")
        source = source.replace(old, new)
    path.write_text(source)
