#!/usr/bin/env bash
# Cross-compile siegel-uniffi for iOS, generate Swift bindings, and assemble
# an XCFramework.
#
# Usage: ./swift/build_swift.sh [OUTPUT_DIR]
#   OUTPUT_DIR: where the .xcframework lands (default: swift/)
set -euo pipefail

PROJECT_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BASE_PATH="$PROJECT_ROOT/swift"
PACKAGE_NAME="siegel-uniffi"   # cargo package
LIB_NAME="siegel_uniffi"       # cdylib / staticlib basename
SWIFT_MODULE="Siegel"          # consumer-facing module name
FRAMEWORK="${SWIFT_MODULE}.xcframework"

OUTPUT_DIR="${1:-$BASE_PATH}"
if [[ "$OUTPUT_DIR" != /* ]]; then
    OUTPUT_DIR="$BASE_PATH/$OUTPUT_DIR"
fi

# Build with the test_utils feature so the foreign test suite can reach
# `sha256_consume`. Production consumers should rebuild without it.
FEATURES="test_utils"

SWIFT_SOURCES_DIR="$OUTPUT_DIR/Sources/$SWIFT_MODULE"
HEADERS_DIR="$BASE_PATH/ios_build/Headers/$SWIFT_MODULE"
FRAMEWORK_OUTPUT="$OUTPUT_DIR/$FRAMEWORK"

echo "Building $FRAMEWORK to $FRAMEWORK_OUTPUT"

rm -rf "$BASE_PATH/ios_build" "$FRAMEWORK_OUTPUT"
mkdir -p "$BASE_PATH/ios_build/bindings" \
         "$BASE_PATH/ios_build/target/universal-ios-sim/release" \
         "$SWIFT_SOURCES_DIR" \
         "$HEADERS_DIR"

export IPHONEOS_DEPLOYMENT_TARGET="13.0"
export RUSTFLAGS="-C link-arg=-Wl,-application_extension"

echo "Compiling Rust for iOS targets..."
cd "$PROJECT_ROOT"
cargo build --package "$PACKAGE_NAME" --features "$FEATURES" --target aarch64-apple-ios-sim --release
cargo build --package "$PACKAGE_NAME" --features "$FEATURES" --target aarch64-apple-ios     --release
cargo build --package "$PACKAGE_NAME" --features "$FEATURES" --target x86_64-apple-ios      --release

echo "Building universal simulator binary..."
lipo -create \
    "target/aarch64-apple-ios-sim/release/lib${LIB_NAME}.a" \
    "target/x86_64-apple-ios/release/lib${LIB_NAME}.a" \
    -output "$BASE_PATH/ios_build/target/universal-ios-sim/release/lib${LIB_NAME}.a"
lipo -info "$BASE_PATH/ios_build/target/universal-ios-sim/release/lib${LIB_NAME}.a"

echo "Generating Swift bindings..."
cargo run -p uniffi-bindgen -- generate \
    "target/aarch64-apple-ios-sim/release/lib${LIB_NAME}.dylib" \
    --library \
    --language swift \
    --no-format \
    --out-dir "$BASE_PATH/ios_build/bindings"

mv "$BASE_PATH/ios_build/bindings/${LIB_NAME}.swift"    "$SWIFT_SOURCES_DIR/"
mv "$BASE_PATH/ios_build/bindings/${LIB_NAME}FFI.h"     "$HEADERS_DIR/"
# The generated modulemap names a module after the FFI header. Rename it to
# `${SWIFT_MODULE}FFI` so the consumer-facing Swift code can `import ${SWIFT_MODULE}FFI`.
cat "$BASE_PATH/ios_build/bindings/${LIB_NAME}FFI.modulemap" > "$HEADERS_DIR/module.modulemap"

echo "Creating XCFramework..."
xcodebuild -create-xcframework \
    -library "target/aarch64-apple-ios/release/lib${LIB_NAME}.a" \
      -headers "$BASE_PATH/ios_build/Headers" \
    -library "$BASE_PATH/ios_build/target/universal-ios-sim/release/lib${LIB_NAME}.a" \
      -headers "$BASE_PATH/ios_build/Headers" \
    -output "$FRAMEWORK_OUTPUT"

rm -rf "$BASE_PATH/ios_build"

echo "Swift framework built at: $FRAMEWORK_OUTPUT"
