#!/usr/bin/env bash
# Cross-compile siegel-uniffi to Android ABIs (via cargo-ndk) into the
# :siegel-android module's jniLibs, and regenerate the uniffi bindings.
# The instrumented tests load these .so files on an emulator/device against
# Bionic libc — the real environment the Rust unit tests can't cover.
#
# Requires: Android NDK (ANDROID_NDK_HOME, or auto-detected under the SDK) and
# cargo-ndk. Rust Android targets are added automatically. CI installs these;
# see .github/workflows/ci.yml.
set -euo pipefail

PROJECT_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
KOTLIN_DIR="$PROJECT_ROOT/kotlin"
PACKAGE_NAME="siegel-uniffi"
LIB_NAME="siegel_uniffi"
JNILIBS_DIR="$KOTLIN_DIR/siegel-android/src/main/jniLibs"
BINDINGS_DIR="$KOTLIN_DIR/siegel-android/src/androidTest/kotlin"
FEATURES="test-utils"

# ABIs to build. Default covers the CI emulator (x86_64) and physical arm64
# devices; override with e.g. ANDROID_ABIS="x86_64" for a faster CI build.
read -ra ABIS <<<"${ANDROID_ABIS:-x86_64 arm64-v8a}"

# Map an Android ABI to its Rust target triple (for `rustup target add`).
abi_to_triple() {
    case "$1" in
    x86_64) echo "x86_64-linux-android" ;;
    arm64-v8a) echo "aarch64-linux-android" ;;
    armeabi-v7a) echo "armv7-linux-androideabi" ;;
    x86) echo "i686-linux-android" ;;
    *)
        echo "Unknown Android ABI: $1" >&2
        return 1
        ;;
    esac
}

if ! command -v cargo-ndk >/dev/null 2>&1; then
    echo "cargo-ndk not found. Install with: cargo install cargo-ndk --locked" >&2
    exit 1
fi

# Ensure the Rust targets are installed and assemble cargo-ndk -t flags.
ndk_targets=()
for abi in "${ABIS[@]}"; do
    rustup target add "$(abi_to_triple "$abi")" >/dev/null
    ndk_targets+=(-t "$abi")
done

cd "$PROJECT_ROOT"

rm -rf "$JNILIBS_DIR"
mkdir -p "$JNILIBS_DIR" "$BINDINGS_DIR"

echo "Cross-compiling $PACKAGE_NAME for: ${ABIS[*]} (features: $FEATURES)"
cargo ndk "${ndk_targets[@]}" -o "$JNILIBS_DIR" \
    build --release --package "$PACKAGE_NAME" --features "$FEATURES"

# Regenerate the Kotlin bindings from one built .so. The generated Kotlin is
# ABI-independent; the first ABI is picked deterministically.
SO_PATH="$JNILIBS_DIR/${ABIS[0]}/lib${LIB_NAME}.so"
[ -f "$SO_PATH" ] || {
    echo "cross-compiled .so missing at $SO_PATH" >&2
    exit 1
}

echo "Generating Kotlin bindings from $SO_PATH"
rm -rf "$BINDINGS_DIR/uniffi"
cargo run -p uniffi-bindgen -- generate \
    "$SO_PATH" \
    --library \
    --language kotlin \
    --no-format \
    --out-dir "$BINDINGS_DIR"

echo "jniLibs staged under $JNILIBS_DIR/, bindings in $BINDINGS_DIR/uniffi/"
