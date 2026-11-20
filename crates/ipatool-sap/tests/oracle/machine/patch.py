"""Makes the exported Go machine package deterministic for scripts/machine-oracle.sh."""
import pathlib
import sys

path = pathlib.Path(sys.argv[1]) / "internal/sap/machine/shim_platform.go"
source = path.read_text()
replacements = [
    ("rand.Read(", "oracleRandRead("),
    ("time.Now()", "oracleNow()"),
    ('\t"crypto/rand"\n', ""),
    ('\t"time"\n', ""),
    (
        """	for name, value := range map[string]uint64{
		"_kCFBooleanFalse":               fakeHandle,
		"_kCFBooleanTrue":                fakeHandle,
		"__xpc_type_dictionary":          xpcDictionaryType,
		"__xpc_type_error":               xpcDictionaryType + 1,
		"__xpc_error_connection_invalid": xpcDictionaryType + 2,
	} {""",
        """	for _, entry := range []struct {
		name  string
		value uint64
	}{
		{"_kCFBooleanFalse", fakeHandle},
		{"_kCFBooleanTrue", fakeHandle},
		{"__xpc_type_dictionary", xpcDictionaryType},
		{"__xpc_type_error", xpcDictionaryType + 1},
		{"__xpc_error_connection_invalid", xpcDictionaryType + 2},
	} {
		name, value := entry.name, entry.value""",
    ),
]
for old, new in replacements:
    if old not in source:
        sys.exit(f"patch target not found: {old!r}")
    source = source.replace(old, new)
path.write_text(source)
