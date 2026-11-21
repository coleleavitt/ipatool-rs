#!/bin/sh
set -eu

# Build a standalone executable for jailbroken arm64 devices running iOS 15+.
ROOT=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
OUTPUT=${1:-ipatool-ios-arm64}
TARGET=aarch64-apple-ios

for TOOL in ldid cmake; do
  command -v "$TOOL" >/dev/null 2>&1 || {
    echo "$TOOL is required; install it with 'brew install ldid cmake'." >&2
    exit 1
  }
done

SDKROOT=$(xcrun --sdk iphoneos --show-sdk-path)
CC_aarch64_apple_ios=$(xcrun --sdk iphoneos --find clang)
export SDKROOT CC_aarch64_apple_ios
export IPHONEOS_DEPLOYMENT_TARGET=15.0
# Unicorn's CMakeLists builds for x86_64 and arm64 on Apple platforms unless ARCHFLAGS
# names one architecture.
export ARCHFLAGS='-arch arm64'
# Never link a system Unicorn found through pkg-config: only the patched sources may be used.
export PKG_CONFIG_ALLOW_CROSS=0

BUILD_DIR=$(mktemp -d "${TMPDIR:-/tmp}/ipatool-ios.XXXXXX")
trap 'rm -rf "$BUILD_DIR"' EXIT HUP INT TERM

cd "$ROOT"
# The patch drops PROT_EXEC from Unicorn's translation buffer, which only works with the
# iOS W^X toggle it adds, so it must not reach other targets. Patch a copy of the locked
# unicorn-engine-sys crate (its bundled C sources are Unicorn 2.1.4, as in Go) and point
# only this build at it; the copied lock file keeps Cargo.lock unchanged.
VERSION=$(sed -n '/^name = "unicorn-engine-sys"$/{n;s/^version = "\(.*\)"$/\1/p;}' Cargo.lock)
CHECKSUM=$(sed -n '/^name = "unicorn-engine-sys"$/,/^$/s/^checksum = "\(.*\)"$/\1/p' Cargo.lock)
cargo fetch --locked --target "$TARGET"
set -- "${CARGO_HOME:-$HOME/.cargo}"/registry/cache/*/"unicorn-engine-sys-$VERSION.crate"
CRATE=$1
echo "$CHECKSUM  $CRATE" | shasum -a 256 -c -
tar -xzf "$CRATE" -C "$BUILD_DIR"
UNICORN="$BUILD_DIR/unicorn-engine-sys-$VERSION"
# Use iOS W^X memory protection for Unicorn, including nested callbacks.
patch -d "$UNICORN" -p1 < "$ROOT/scripts/patches/unicorn-ios.patch"
cp Cargo.lock "$BUILD_DIR/Cargo.lock"

cargo build --release -p ipatool-cli --target "$TARGET" \
  --config "patch.crates-io.unicorn-engine-sys.path=\"$UNICORN\"" \
  --config "resolver.lockfile-path=\"$BUILD_DIR/Cargo.lock\""

TARGET_DIR=$(cargo metadata --format-version 1 --no-deps | sed 's/.*"target_directory":"\([^"]*\)".*/\1/')
cp "$TARGET_DIR/$TARGET/release/ipatool" "$OUTPUT"
ldid -S"$ROOT/resources/ios-entitlements.plist" "$OUTPUT"
