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
for arg in "$@"; do
    case "$arg" in
        --sim-only) SIM_ONLY=1 ;;
        --help|-h) sed -n '2,10p' "$0"; exit 0 ;;
        *) echo "Unknown argument: $arg" >&2; exit 1 ;;
    esac
done

PROJECT_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CRATE_DIR="$PROJECT_ROOT/siegel-boltffi"

# Build with the test-utils feature so the foreign test suite can reach
# `sha256_consume` + the guard-page helper. Production consumers must rebuild
# without it.
FEATURES="test-utils"

command -v boltffi >/dev/null 2>&1 || {
    echo "boltffi CLI not found. Install with: cargo install boltffi_cli --locked" >&2
    exit 1
}

cd "$CRATE_DIR"

PACK_ARGS=(pack apple --release --cargo-arg --features --cargo-arg "$FEATURES")
if [ "$SIM_ONLY" = "1" ]; then
    PACK_ARGS+=(--overlay boltffi.ci.toml)
fi

echo "Running: boltffi ${PACK_ARGS[*]}"
boltffi "${PACK_ARGS[@]}"

OUTPUT_DIR="$CRATE_DIR/dist/apple"
echo
echo "Apple artifacts in $OUTPUT_DIR:"
ls -1 "$OUTPUT_DIR" | sed 's|^|  |'
