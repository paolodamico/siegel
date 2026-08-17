#!/usr/bin/env bash
# Cross-compile siegel-boltffi for iOS, generate Swift bindings, and assemble
# an XCFramework via `boltffi pack apple`.
#
# Usage: ./swift/build_boltffi_swift.sh [--sim-only]
#
#   --sim-only   Build only the arm64 simulator slice (applies
#                `boltffi.ci.toml`). Intended for running the XCTest suite —
#                release/distribution builds must omit this.
#
# Unlike the UniFFI script this does not drive cargo/lipo/xcodebuild by hand:
# `boltffi pack apple` builds every configured slice, generates the Swift
# sources, and assembles the XCFramework in one step. Layout is controlled by
# `siegel-boltffi/boltffi.toml`.
set -euo pipefail

SIM_ONLY=0
TEST_UTILS=0
for arg in "$@"; do
    case "$arg" in
        --sim-only) SIM_ONLY=1 ;;
        --test-utils) TEST_UTILS=1 ;;
        --help|-h) sed -n '2,10p' "$0"; exit 0 ;;
        *) echo "Unknown argument: $arg" >&2; exit 1 ;;
    esac
done

PROJECT_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CRATE_DIR="$PROJECT_ROOT/siegel-boltffi"

# Test-only helpers are OFF by default. They export `sha256_consume` and
# `unsafe_test_only_siegel_front_guard_bolt`
FEATURES=""
if [ "$TEST_UTILS" = "1" ]; then
    FEATURES="test-utils"
fi

command -v boltffi >/dev/null 2>&1 || {
    echo "boltffi CLI not found. Install with: cargo install boltffi_cli --locked" >&2
    exit 1
}


# Ensure the CLI is using the same version as the dependency
cargo metadata --format-version 1 >/dev/null
RESOLVED="$(cargo pkgid boltffi 2>/dev/null | sed 's/.*[@#]//')"
INSTALLED="$(boltffi --version | awk '{print $NF}')"
if [ -n "$RESOLVED" ] && [ "$INSTALLED" != "$RESOLVED" ]; then
    echo "boltffi CLI is $INSTALLED but cargo resolved boltffi $RESOLVED." >&2
    echo "Install the matching CLI: cargo install boltffi_cli --version $RESOLVED --locked" >&2
    exit 1
fi

cd "$CRATE_DIR"

# Pack into a clean tree. A previous --sim-only run leaves a simulator-only
# XCFramework here, and nothing downstream distinguishes it from a release
# artifact.
rm -rf "${CRATE_DIR:?}/dist/apple"

PACK_ARGS=(pack apple --release)
if [ -n "$FEATURES" ]; then
    PACK_ARGS+=(--cargo-arg --features --cargo-arg "$FEATURES")
fi
if [ "$SIM_ONLY" = "1" ]; then
    PACK_ARGS+=(--overlay boltffi.ci.toml)
fi
if [ "${VERBOSE:-0}" = "1" ]; then
    PACK_ARGS+=(-v)
fi

echo "Running: boltffi ${PACK_ARGS[*]}"
boltffi "${PACK_ARGS[@]}"

OUTPUT_DIR="$CRATE_DIR/dist/apple"
echo
echo "Apple artifacts in $OUTPUT_DIR:"
ls -1 "$OUTPUT_DIR" | sed 's|^|  |'
