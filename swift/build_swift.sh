#!/usr/bin/env bash
# Cross-compile siegel-uniffi for iOS, generate Swift bindings, and assemble
# an XCFramework.
#
# Usage: ./swift/build_swift.sh [--sim-only] [OUTPUT_DIR]
#
#   --sim-only   Build only the iOS Simulator slice matching the host arch.
#                Skips the device (arm64) and Intel-sim (x86_64) slices and
#                produces a sim-only XCFramework. Intended for running the
#                XCTest suite — release/distribution builds must omit this.
#   OUTPUT_DIR   Where the .xcframework lands (default: swift/).
set -euo pipefail

SIM_ONLY=0
OUTPUT_DIR=""
for arg in "$@"; do
    case "$arg" in
        --sim-only) SIM_ONLY=1 ;;
        --help|-h)
            sed -n '2,11p' "$0"
            exit 0
            ;;
        *) OUTPUT_DIR="$arg" ;;
    esac
done

PROJECT_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BASE_PATH="$PROJECT_ROOT/swift"
PACKAGE_NAME="siegel-uniffi"   # cargo package
LIB_NAME="siegel_uniffi"       # cdylib / staticlib basename
SWIFT_MODULE="Siegel"          # consumer-facing module name
FRAMEWORK="${SWIFT_MODULE}.xcframework"

OUTPUT_DIR="${OUTPUT_DIR:-$BASE_PATH}"
if [[ "$OUTPUT_DIR" != /* ]]; then
    OUTPUT_DIR="$BASE_PATH/$OUTPUT_DIR"
fi

# Build with the test-utils feature so the foreign test suite can reach
# `sha256_consume` + the segfault helpers. Production consumers should
# rebuild without it.
FEATURES="test-utils"

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

cd "$PROJECT_ROOT"

if [ "$SIM_ONLY" = "1" ]; then
    # Pick the simulator slice that matches the host. Apple Silicon hosts
    # run the arm64 simulator; Intel hosts run the x86_64 simulator. We only
    # build the one that's actually loadable on this machine.
    case "$(uname -m)" in
        arm64)  SIM_TARGET="aarch64-apple-ios-sim" ;;
        x86_64) SIM_TARGET="x86_64-apple-ios" ;;
        *)
            echo "Unsupported host arch for --sim-only: $(uname -m)" >&2
            exit 1
            ;;
    esac

    echo "Compiling Rust for $SIM_TARGET (sim-only)..."
    cargo build --package "$PACKAGE_NAME" --features "$FEATURES" --target "$SIM_TARGET" --release

    SIM_LIB="target/$SIM_TARGET/release/lib${LIB_NAME}.a"
    BINDGEN_DYLIB="target/$SIM_TARGET/release/lib${LIB_NAME}.dylib"
else
    echo "Compiling Rust for iOS targets..."
    cargo build --package "$PACKAGE_NAME" --features "$FEATURES" --target aarch64-apple-ios-sim --release
    cargo build --package "$PACKAGE_NAME" --features "$FEATURES" --target aarch64-apple-ios     --release
    cargo build --package "$PACKAGE_NAME" --features "$FEATURES" --target x86_64-apple-ios      --release

    echo "Building universal simulator binary..."
    UNIVERSAL_SIM="$BASE_PATH/ios_build/target/universal-ios-sim/release/lib${LIB_NAME}.a"
    lipo -create \
        "target/aarch64-apple-ios-sim/release/lib${LIB_NAME}.a" \
        "target/x86_64-apple-ios/release/lib${LIB_NAME}.a" \
        -output "$UNIVERSAL_SIM"
    lipo -info "$UNIVERSAL_SIM"

    SIM_LIB="$UNIVERSAL_SIM"
    BINDGEN_DYLIB="target/aarch64-apple-ios-sim/release/lib${LIB_NAME}.dylib"
fi

echo "Generating Swift bindings..."
cargo run -p uniffi-bindgen -- generate \
    "$BINDGEN_DYLIB" \
    --library \
    --language swift \
    --no-format \
    --out-dir "$BASE_PATH/ios_build/bindings"

mv "$BASE_PATH/ios_build/bindings/${LIB_NAME}.swift" "$SWIFT_SOURCES_DIR/"
mv "$BASE_PATH/ios_build/bindings/${LIB_NAME}FFI.h"  "$HEADERS_DIR/"
# The generated modulemap names a module after the FFI header. Rename it to
# `${SWIFT_MODULE}FFI` so the consumer-facing Swift code can `import ${SWIFT_MODULE}FFI`.
cat "$BASE_PATH/ios_build/bindings/${LIB_NAME}FFI.modulemap" > "$HEADERS_DIR/module.modulemap"

echo "Creating XCFramework..."
if [ "$SIM_ONLY" = "1" ]; then
    xcodebuild -create-xcframework \
        -library "$SIM_LIB" \
          -headers "$BASE_PATH/ios_build/Headers" \
        -output "$FRAMEWORK_OUTPUT"
else
    xcodebuild -create-xcframework \
        -library "target/aarch64-apple-ios/release/lib${LIB_NAME}.a" \
          -headers "$BASE_PATH/ios_build/Headers" \
        -library "$SIM_LIB" \
          -headers "$BASE_PATH/ios_build/Headers" \
        -output "$FRAMEWORK_OUTPUT"
fi

rm -rf "$BASE_PATH/ios_build"

echo "Swift framework built at: $FRAMEWORK_OUTPUT"
