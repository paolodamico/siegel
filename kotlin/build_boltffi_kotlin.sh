#!/usr/bin/env bash
# Build siegel-boltffi as a host cdylib, generate Kotlin bindings, and compile
# the JNI glue for the host JVM so the integration tests can run on a desktop
# JDK.

set -euo pipefail

PROJECT_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
KOTLIN_DIR="$PROJECT_ROOT/kotlin"
CRATE_DIR="$PROJECT_ROOT/siegel-boltffi"
PACKAGE_NAME="siegel-boltffi"
LIB_NAME="siegel_boltffi"
LIBS_DIR="$KOTLIN_DIR/boltffi-libs"
TEST_MODULE="$KOTLIN_DIR/siegel-boltffi-tests"
# Generated bindings live alongside the test sources so they compile in the
# same source set without extra Gradle wiring.
GENERATED_DIR="$TEST_MODULE/src/main/kotlin/generated"

# `jvm` provides the JNI fill path; required on Android, opt-in on desktop.
FEATURES="test-utils,jvm"

command -v boltffi >/dev/null 2>&1 || {
    echo "boltffi CLI not found. Install with: cargo install boltffi_cli --locked" >&2
    exit 1
}

: "${JAVA_HOME:?JAVA_HOME must be set to a JDK (needed for jni.h)}"
[ -f "$JAVA_HOME/include/jni.h" ] || {
    echo "jni.h not found under $JAVA_HOME/include — is JAVA_HOME a JDK, not a JRE?" >&2
    exit 1
}

case "$OSTYPE" in
    darwin*) LIB_EXT="dylib"; JNI_MD="darwin"; RPATH="@loader_path" ;;
    linux*)  LIB_EXT="so";    JNI_MD="linux";  RPATH="\$ORIGIN"    ;;
    *) echo "Unsupported OS: $OSTYPE" >&2; exit 1 ;;
esac

rm -rf "$LIBS_DIR" "$GENERATED_DIR"
mkdir -p "$LIBS_DIR" "$GENERATED_DIR"

cd "$PROJECT_ROOT"

echo "Step 1: building host cdylib ($PACKAGE_NAME, features: $FEATURES)"
# The `BOLTFFI_BINDING_EXPANSION*` environment is load-bearing, not incidental.
#
# `boltffi`'s `#[export]` macro has two expansions. Without this environment it
# takes the legacy path and emits short symbols (`boltffi_siegel_session_new`).
# With it, the macro scans the whole package once and emits the current ABI.
#
# `FEATURES` must match the value passed to `boltffi generate` in step 2: the
# macro scan is cfg-sensitive, so a mismatch silently drops exports.
export BOLTFFI_BINDING_EXPANSION=1
export BOLTFFI_BINDING_EXPANSION_ROOT="$CRATE_DIR"
export BOLTFFI_BINDING_EXPANSION_SOURCE="$CRATE_DIR/src/lib.rs"
export BOLTFFI_BINDING_EXPANSION_SURFACE="native"
export BOLTFFI_BINDING_METADATA_FEATURES="$FEATURES"
cargo build --package "$PACKAGE_NAME" --features "$FEATURES" --release

RUST_LIB="$PROJECT_ROOT/target/release/lib${LIB_NAME}.${LIB_EXT}"
[ -f "$RUST_LIB" ] || { echo "cdylib missing at $RUST_LIB" >&2; exit 1; }
cp "$RUST_LIB" "$LIBS_DIR/"
echo "  -> $(basename "$RUST_LIB")"

echo "Step 2: generating Kotlin bindings + JNI glue"
# Codegen expands the crate, so the feature set must match the cdylib built in
# step 1 — otherwise `sha256_consume` is cfg'd out and the bindings won't
# expose it.
(cd "$CRATE_DIR" && boltffi generate kotlin --cargo-arg --features --cargo-arg "$FEATURES")

GEN_ROOT="$CRATE_DIR/dist/android/kotlin"
[ -f "$GEN_ROOT/jni/jni_glue.c" ] || {
    echo "expected JNI glue at $GEN_ROOT/jni/jni_glue.c" >&2
    exit 1
}
cp -R "$GEN_ROOT/dev" "$GENERATED_DIR/"

# The generated bindings expose the session class but no way to fill it: the
# fill path is the hand-written JNI entry point in `siegel-boltffi/kotlin`.
# Ship it alongside the generated sources so `dist/android` is a complete,
# copyable package, and mirror it into the test module's source set.
HANDWRITTEN="$CRATE_DIR/kotlin/dev/siegel/SiegelNative.kt"
[ -f "$HANDWRITTEN" ] || { echo "missing $HANDWRITTEN" >&2; exit 1; }
cp "$HANDWRITTEN" "$GEN_ROOT/dev/siegel/"
cp "$HANDWRITTEN" "$GENERATED_DIR/dev/siegel/"
echo "  -> $(find "$GENERATED_DIR" -name '*.kt' | wc -l | tr -d ' ') Kotlin file(s)"

echo "Step 3: compiling JNI glue for the host JVM"
# The generated Kotlin loads `siegel_boltffi_jni` first, then falls back to
# `siegel_boltffi`; the glue links against the Rust cdylib for the exported
# `boltffi_*` symbols.
CC_BIN="${CC:-cc}"
"$CC_BIN" -shared -fPIC -O2 \
    -I"$JAVA_HOME/include" \
    -I"$JAVA_HOME/include/$JNI_MD" \
    -I"$GEN_ROOT/jni" \
    "$GEN_ROOT/jni/jni_glue.c" \
    -L"$LIBS_DIR" -l"$LIB_NAME" \
    -Wl,-rpath,"$RPATH" \
    -o "$LIBS_DIR/lib${LIB_NAME}_jni.${LIB_EXT}"
echo "  -> lib${LIB_NAME}_jni.${LIB_EXT}"

echo
echo "Artifacts in $LIBS_DIR:"
ls -1 "$LIBS_DIR" | sed 's|^|  |'
