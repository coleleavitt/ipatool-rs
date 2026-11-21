#!/usr/bin/env python3
"""Parity suite: run the Go and the Rust ipatool on the same command lines and compare.

Usage: scripts/parity.py GO_BIN RUST_BIN

Offline (default): usage and validation errors, `auth info` and the `mcp` lists and
argument errors. None of them reach Apple; some read the local keychain.
Live (IPATOOL_PARITY_LIVE=1): read-only Apple calls on apps the account already owns
(search, list-versions, get-version-metadata, --with-metadata from the cache,
list-purchases, purchase of an owned app, a download of Sony Music Center, catalog-versions
against Go's metadata, and the live `mcp` calls).

JSON lines are compared without `time`. Where Go prints a text line (pflag stopped before
--format, or an unknown command), the exit status and the error text are compared, since
text mode differs by design (PLAN.md open question 3). Downloads are compared by the IPA
entry list and the parsed Info.plist and iTunesMetadata.plist, not bytes (PLAN.md phase 6).
Exits 1 on any difference that is not listed as known.
"""

import json
import os
import plistlib
import re
import subprocess
import sys
import tempfile
import zipfile

PERPLEXITY = "1668000334"
SONY_MUSIC_CENTER = "724406878"
GOOGLE_HOME = "680819774"

# Error probes use an owned app, so a regression that skips a check cannot buy anything.
OFFLINE = [
    ["search"],
    ["search", "a", "b", "--format", "json"],
    ["search", "--bogus", "x", "--format", "json"],
    ["search", "x", "--format=json", "--bogus"],
    ["search", "-z", "x"],
    ["search", "x", "--limit", "abc"],
    ["search", "x", "--limit"],
    ["search", "x", "-l"],
    ["search", "x", "--verbose=maybe"],
    ["search", "x", "--format", "xml"],
    ["--format"],
    ["serch", "x"],
    ["list"],
    ["bogus", "--format", "json"],
    ["list-versions", "--format", "json"],
    ["list-versions", "-i", PERPLEXITY, "--platform", "bad", "--format", "json"],
    ["list-versions", "-i", PERPLEXITY, "--with-metadata", "--concurrency", "0", "--format", "json"],
    ["list-versions", "-i", PERPLEXITY, "--concurrency", "abc"],
    ["get-version-metadata", "--format", "json"],
    ["get-version-metadata", "-i", PERPLEXITY, "--format", "json"],
    ["get-version-metadata", "--external-version-id", "1", "--format", "json"],
    ["get-version-metadata", "-i", PERPLEXITY, "--external-version-id", "1", "--platform", "bad",
     "--format", "json"],
    ["--format", "json", "get-version-metadata", "-i", PERPLEXITY, "--external-version-id"],
    ["download", "--format", "json"],
    ["download", "-i", SONY_MUSIC_CENTER, "--platform", "bad", "--format", "json"],
    ["download", "-i", "abc", "--format", "json"],
    ["--format", "json", "download", "-i", SONY_MUSIC_CENTER, "--purchase=maybe"],
    ["purchase", "--format", "json"],
    ["purchase", "-i", PERPLEXITY, "--platform", "bad", "--format", "json"],
    ["list-purchases", "-p", "0", "--format", "json"],
    ["list-purchases", "-l", "0", "--format", "json"],
    ["list-purchases", "-l", "101", "--format", "json"],
    ["list-purchases", "-l", "0x10", "-p", "-1", "--format", "json"],
    ["list-purchases", "--platform", "unknown", "--format", "json"],
    ["list-purchases", "--platform", "watchos", "-p", "0", "--format", "json"],
    ["list-purchases", "extra", "--format", "json"],
    ["list-purchases", "-p", "abc", "--format", "json"],
    ["mcp", "extra", "--format", "json"],
    ["auth", "info", "--format", "json"],
    ["auth", "info", "extra", "--format", "json"],
    ["auth"],
    ["auth", "bogus"],
    [],
]

# Differences this suite reports without failing, with the reason.
KNOWN = {}


def run(binary, args, cwd=None):
    result = subprocess.run([binary, *args], capture_output=True, text=True, stdin=subprocess.DEVNULL,
                            cwd=cwd, timeout=1800)
    return result.returncode, result.stdout, result.stderr


def json_lines(text):
    lines = []
    for line in text.splitlines():
        try:
            value = json.loads(line)
        except ValueError:
            return None
        if isinstance(value, dict):
            value.pop("time", None)
        lines.append(value)
    return lines


TEXT_ERROR = re.compile(r'\berror=("(?:[^"\\]|\\.)*"|.*?)(?: success=false)?\s*$', re.S)


def text_error(stderr):
    """The error of a text-mode error line: Go quotes it, the port prints it raw."""
    plain = re.sub(r"\x1b\[[0-9;]*m", "", stderr).strip()
    match = TEXT_ERROR.search(plain)
    if not match:
        return plain
    value = match.group(1)
    return json.loads(value) if value.startswith('"') else value


def observed(result, args):
    code, stdout, stderr = result
    lines = json_lines(stdout) if stdout.strip() else []
    if lines is None:
        # Help text: cobra's and clap's layouts differ by design.
        return {"exit": code, "help": stdout.lstrip().split("\n", 1)[0]}
    if not lines and stderr.strip():
        return {"exit": code, "text error": text_error(stderr)}
    return {"exit": code, "lines": lines}


class Suite:
    def __init__(self, go, rust):
        self.go, self.rust, self.failed, self.known = go, rust, 0, 0

    def report(self, name, go, rust):
        if go == rust:
            print(f"ok     {name}")
            if name in KNOWN:
                print(f"       (listed as known: {KNOWN[name]}; remove it from KNOWN)")
            return
        if name in KNOWN:
            self.known += 1
            print(f"KNOWN  {name}: {KNOWN[name]}")
            return
        self.failed += 1
        print(f"DIFF   {name}")
        print(f"  go:   {json.dumps(go, ensure_ascii=False)[:600]}")
        print(f"  rust: {json.dumps(rust, ensure_ascii=False)[:600]}")

    def check(self, args, name=None):
        name = name or " ".join(args) or "(no arguments)"
        go = observed(run(self.go, args), args)
        rust = observed(run(self.rust, args), args)
        self.report(name, go, rust)
        return go, rust


def ipa_contents(path):
    with zipfile.ZipFile(path) as archive:
        names = archive.namelist()
        plists = {}
        for name in names:
            if re.fullmatch(r"Payload/[^/]+\.app/Info\.plist|iTunesMetadata\.plist", name):
                plists[re.sub(r"^Payload/[^/]+\.app/", "Payload/*.app/", name)] = plistlib.loads(archive.read(name))
    return names, plists


def plist_json(value):
    return json.loads(json.dumps(value, default=lambda item: item.isoformat() if hasattr(item, "isoformat")
                                 else item.hex() if isinstance(item, bytes) else str(item)))


def check_download(suite, app_id):
    with tempfile.TemporaryDirectory(prefix="ipatool-parity-") as directory:
        outputs = {}
        for tool, binary in (("go", suite.go), ("rust", suite.rust)):
            target = os.path.join(directory, tool)
            os.mkdir(target)
            result = observed(run(binary, ["download", "-i", app_id, "-o", target, "--non-interactive",
                                           "--format", "json"]), [])
            for line in result.get("lines", []):
                if "output" in line:
                    outputs[tool] = line["output"]
                    line["output"] = os.path.basename(line["output"])
            outputs[f"{tool} result"] = result
        suite.report(f"download -i {app_id}", outputs["go result"], outputs["rust result"])
        if "go" not in outputs or "rust" not in outputs:
            return
        go_names, go_plists = ipa_contents(outputs["go"])
        rust_names, rust_plists = ipa_contents(outputs["rust"])
        suite.report(f"download -i {app_id}: IPA entries", sorted(go_names), sorted(rust_names))
        suite.report(f"download -i {app_id}: IPA entry order", go_names, rust_names)
        for name in sorted(set(go_plists) | set(rust_plists)):
            suite.report(f"download -i {app_id}: {name}", plist_json(go_plists.get(name)),
                         plist_json(rust_plists.get(name)))


def check_catalog_versions(suite, app_id):
    """The port's catalog-versions has no Go command; Go's --with-metadata is the oracle."""
    _, stdout, _ = run(suite.rust, ["catalog-versions", "-i", app_id, "--format", "json"])
    rust = json_lines(stdout) or [{}]
    _, stdout, _ = run(suite.go, ["list-versions", "-i", app_id, "--with-metadata", "--format", "json"])
    go = json_lines(stdout) or [{}]
    by_id = {version["externalVersionID"]: version for version in go[-1].get("versions", [])}
    catalog = rust[-1].get("versions", [])
    expected = [{key: by_id.get(version["externalVersionID"], {}).get(key) for key in version}
                for version in catalog]
    suite.report(f"catalog-versions -i {app_id} against Go --with-metadata",
                 expected or "no Go versions", catalog or "no catalog versions")


def live(suite):
    for args in [
        ["search", "perplexity", "--limit", "3"],
        ["search", "weather", "--limit", "3", "--platform", "watchos"],
        ["search", "xcode", "--limit", "2", "--platform", "macos"],
        ["search", "music", "--limit", "2", "--platform", "visionos"],
        ["auth", "info"],
        ["list-versions", "-i", SONY_MUSIC_CENTER],
        ["list-versions", "-b", "com.google.Chromecast"],
        ["get-version-metadata", "-i", PERPLEXITY, "--external-version-id", "895516672"],
        # Both read the shared version-metadata cache the first runs filled.
        ["list-versions", "-i", PERPLEXITY, "--with-metadata"],
        ["list-purchases", "-l", "5"],
        ["list-purchases", "--platform", "macos", "-p", "2", "-l", "5"],
        ["purchase", "-i", PERPLEXITY],
        ["purchase", "-b", "jp.co.sony.audio.companion"],
    ]:
        suite.check([*args, "--format", "json"], " ".join(args))
    check_catalog_versions(suite, PERPLEXITY)
    check_download(suite, SONY_MUSIC_CENTER)


def main():
    if len(sys.argv) != 3:
        sys.exit(__doc__)
    suite = Suite(sys.argv[1], sys.argv[2])
    for args in OFFLINE:
        suite.check(args)
    is_live = os.environ.get("IPATOOL_PARITY_LIVE") == "1"
    if is_live:
        live(suite)
    mcp = [os.path.join(os.path.dirname(__file__), "mcp_parity.py"), suite.go, suite.rust]
    if subprocess.run(mcp if is_live else [*mcp, "--offline"]).returncode != 0:
        suite.failed += 1
    print(f"{suite.failed} difference(s), {suite.known} known; mode: {'live' if is_live else 'offline'}")
    return 1 if suite.failed else 0


if __name__ == "__main__":
    sys.exit(main())
