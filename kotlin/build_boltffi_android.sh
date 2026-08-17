#!/usr/bin/env bash
# Build the Android distribution: jniLibs for every ABI, the generated Kotlin
# bindings, and the hand-written JNI fill path.
#
# Usage: ./kotlin/build_boltffi_android.sh [extra boltffi args...]
#
# Requires the Android NDK (set ANDROID_NDK_HOME, or ANDROID_HOME/ndk).
#
# `boltffi pack android` alone is not sufficient: it emits the generated
# `Siegel.kt`, which exposes the session class but no way to fill it. The fill
# path is the hand-written `SiegelNative.kt`, which this script copies into the
# packaged output so consumers get a complete package.
#
# For host-JVM builds used by the integration tests, see build_boltffi_kotlin.sh.
set -euo pipefail

PROJECT_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CRATE_DIR="$PROJECT_ROOT/siegel-boltffi"
HANDWRITTEN="$CRATE_DIR/kotlin/dev/siegel/SiegelNative.kt"

command -v boltffi >/dev/null 2>&1 || {
    echo "boltffi CLI not found. Install with: cargo install boltffi_cli --locked" >&2
    exit 1
}
[ -f "$HANDWRITTEN" ] || { echo "missing $HANDWRITTEN" >&2; exit 1; }

# Fail with the actionable message rather than whatever a missing linker
# produces three layers down.
if [ -z "${ANDROID_NDK_HOME:-}" ] && [ -z "${ANDROID_NDK_ROOT:-}" ] \
    && [ ! -d "${ANDROID_HOME:-/nonexistent}/ndk" ] \
    && [ ! -d "${ANDROID_SDK_ROOT:-/nonexistent}/ndk" ]; then
    echo "Android NDK not found. Set ANDROID_NDK_HOME (or install it under \$ANDROID_HOME/ndk)." >&2
    exit 1
fi

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

rm -rf "${CRATE_DIR:?}/dist/android"

echo "Step 1: boltffi pack android --release"
boltffi pack android --release --cargo-arg --features --cargo-arg jvm "$@"

echo "Step 2: adding the hand-written JNI fill path"
# Locate the generated bindings rather than assuming the configured output
# path, so this keeps working if boltffi.toml's layout changes.
GENERATED="$(find "$CRATE_DIR/dist" -name 'Siegel.kt' -path '*dev/siegel/*' -print -quit)"
[ -n "$GENERATED" ] || {
    echo "could not find generated Siegel.kt under $CRATE_DIR/dist" >&2
    exit 1
}
cp "$HANDWRITTEN" "$(dirname "$GENERATED")/"
echo "  -> $(dirname "$GENERATED")/SiegelNative.kt"

# The generated loader calls System.loadLibrary with the configured package
# name; if the packed ABI libraries are named differently, every consumer gets
# an UnsatisfiedLinkError at class-init rather than a build failure.
echo "Step 3: checking the packed library name matches the generated loader"
EXPECTED="$(grep -oE 'System\.loadLibrary\("[^"]+"\)' "$GENERATED" | head -1 | sed 's/.*("\(.*\)")/\1/')"
if [ -n "$EXPECTED" ]; then
    if ! find "$CRATE_DIR/dist/android" -name "lib${EXPECTED}.so" -print -quit | grep -q .; then
        echo "No lib${EXPECTED}.so in the packed jniLibs, but the generated loader" >&2
        echo "requests exactly that name — consumers would hit UnsatisfiedLinkError" >&2
        echo "at class-init. Packed libraries:" >&2
        find "$CRATE_DIR/dist/android" -name '*.so' -exec basename {} \; | sort -u | sed 's/^/  /' >&2
        exit 1
    fi
    echo "  -> lib${EXPECTED}.so present"
fi

echo
echo "Android artifacts under $CRATE_DIR/dist/android:"
find "$CRATE_DIR/dist/android" -maxdepth 2 -mindepth 1 | sed 's|^|  |'
