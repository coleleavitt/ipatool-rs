#!/usr/bin/env python3
"""Extracts the Store profile artifacts from Go's internal/store/profile/data_generated.go.

Usage: scripts/extract-store-profile.py <go-ipatool-checkout>
Writes the zstd frames Go embeds to crates/ipatool-store/profile/. The Rust loader checks
the decoded sizes and SHA-256 digests, so update them in profile/mod.rs with the files.
"""

import base64
import pathlib
import re
import sys

NAMES = {
    "generatedMetadata": "metadata.json.zst",
    "generatedPages": "pages.bin.zst",
    "generatedPlugin": "plugin.zst",
}

source = pathlib.Path(sys.argv[1], "internal/store/profile/data_generated.go").read_text()
target = pathlib.Path(__file__).resolve().parent.parent / "crates/ipatool-store/profile"
target.mkdir(parents=True, exist_ok=True)
for match in re.finditer(r"var (generated\w+) = artifact\{.*?Encoded: `([^`]*)`", source, re.S):
    name = NAMES[match.group(1)]
    (target / name).write_bytes(base64.b64decode("".join(match.group(2).split())))
    print(name)
