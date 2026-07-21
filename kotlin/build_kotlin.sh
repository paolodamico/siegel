#!/usr/bin/env bash
# Build siegel-uniffi as a host cdylib + generate Kotlin bindings for
# Kotlin/JVM integration tests. JNA loads the cdylib at runtime from
# `kotlin/libs/`.
#
# For Android distribution you'd separately cross-compile to the four
# ABIs (aarch64/armv7/x86/x86_64) and assemble an `.aar`; that's not in
# scope here.
set -euo pipefail

PROJECT_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
KOTLIN_DIR="$PROJECT_ROOT/kotlin"
PACKAGE_NAME="siegel-uniffi"
LIB_NAME="siegel_uniffi"
LIBS_DIR="$KOTLIN_DIR/libs"
# Generated bindings live alongside the test sources so they compile
# in the same source set without extra Gradle wiring.
BINDINGS_DIR="$KOTLIN_DIR/siegel-tests/src/test/kotlin"

FEATURES="test-utils"

rm -rf "$LIBS_DIR" "$BINDINGS_DIR/uniffi"
mkdir -p "$LIBS_DIR"

cd "$PROJECT_ROOT"

echo "Building host cdylib (siegel-uniffi, features: $FEATURES)..."
cargo build --package "$PACKAGE_NAME" --features "$FEATURES" --release

case "$OSTYPE" in
    darwin*)  LIB_FILE="$PROJECT_ROOT/target/release/lib${LIB_NAME}.dylib" ;;
    linux*)   LIB_FILE="$PROJECT_ROOT/target/release/lib${LIB_NAME}.so"    ;;
    *)        echo "Unsupported OS: $OSTYPE" >&2; exit 1 ;;
esac

[ -f "$LIB_FILE" ] || { echo "cdylib missing at $LIB_FILE" >&2; exit 1; }
cp "$LIB_FILE" "$LIBS_DIR/"
echo "Copied $(basename "$LIB_FILE") to $LIBS_DIR/"

echo "Generating Kotlin bindings..."
cargo run -p uniffi-bindgen -- generate \
    "$LIB_FILE" \
    --library \
    --language kotlin \
    --no-format \
    --out-dir "$BINDINGS_DIR"

echo "Kotlin bindings written to $BINDINGS_DIR/uniffi/$LIB_NAME/"
