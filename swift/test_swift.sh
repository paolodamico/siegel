#!/usr/bin/env bash
# Build the iOS XCFramework, then run the XCTest suite under
# `swift/tests/` on an iOS Simulator via xcodebuild.
set -euo pipefail

BASE_PATH="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TESTS_PATH="$BASE_PATH/tests"
SWIFT_MODULE="Siegel"
LIB_NAME="siegel_uniffi"
SOURCES_REL="Sources/${SWIFT_MODULE}"

# Disable color codes when the output isn't a TTY (e.g. CI logs).
if [ -t 1 ] && [ "${NO_COLOR:-}" = "" ]; then
    GREEN='\033[0;32m'; RED='\033[0;31m'; YELLOW='\033[0;33m'
    BOLD='\033[1m'; DIM='\033[2m'; NC='\033[0m'
else
    GREEN=''; RED=''; YELLOW=''; BOLD=''; DIM=''; NC=''
fi

if ! xcodebuild -showsdks | grep -q 'iphonesimulator'; then
    echo "No iOS Simulator SDK installed. Available SDKs:" >&2
    xcodebuild -showsdks || true
    exit 1
fi

echo "Step 1: building Swift bindings"
bash "$BASE_PATH/build_swift.sh"
[ -d "$BASE_PATH/${SWIFT_MODULE}.xcframework" ] || { echo "Missing XCFramework" >&2; exit 1; }

echo "Step 2: copying generated Swift sources into the test package"
mkdir -p "$TESTS_PATH/$SOURCES_REL"
cp "$BASE_PATH/$SOURCES_REL/${LIB_NAME}.swift" "$TESTS_PATH/$SOURCES_REL/${LIB_NAME}.swift"

echo "Step 3: picking simulator"
SIMULATOR_ID="$(xcrun simctl list devices available \
                  | grep -E 'iPhone 1[5-7]' \
                  | head -1 \
                  | grep -oE '[0-9A-F-]{36}' || true)"
if [ -z "$SIMULATOR_ID" ]; then
    SIMULATOR_ID="$(xcrun simctl list devices available \
                      | grep 'iPhone' \
                      | head -1 \
                      | grep -oE '[0-9A-F-]{36}' || true)"
fi
[ -n "$SIMULATOR_ID" ] || { echo "No iPhone simulator available" >&2; exit 1; }
echo "Using simulator: $SIMULATOR_ID"

# CI runners benefit from a clean simulator state — previously-leaked sims
# have been observed to hang the test runner. Skip on local for speed.
if [ "${GITHUB_ACTIONS:-false}" = "true" ] || [ "${CI:-false}" = "true" ]; then
    echo "Cleaning simulator state (CI)..."
    xcrun simctl shutdown "$SIMULATOR_ID" >/dev/null 2>&1 || true
    xcrun simctl erase    "$SIMULATOR_ID"
    xcrun simctl boot     "$SIMULATOR_ID"
    xcrun simctl bootstatus "$SIMULATOR_ID" -b
fi

rm -rf "$TESTS_PATH/.build" 2>/dev/null || true

echo "Step 4: running xcodebuild test"
cd "$TESTS_PATH"

LOG_FILE="$(mktemp -t siegel-xctest.XXXXXX)"
trap 'rm -f "$LOG_FILE"' EXIT

set +e
xcodebuild test \
    -scheme SiegelIntegrationTests \
    -destination "platform=iOS Simulator,id=$SIMULATOR_ID" \
    -sdk iphonesimulator \
    CODE_SIGNING_ALLOWED=NO \
    2>&1 | tee "$LOG_FILE"
XCODE_STATUS=${PIPESTATUS[0]}
set -e

# Parse xcodebuild's log. Each test emits one "started" and exactly one
# "passed" or "failed" line, e.g.:
#   Test Case '-[SiegelTests.SiegelGuardTests testFoo]' passed (0.001 seconds).
PASSED=$(grep -cE "Test Case .* passed " "$LOG_FILE" || true)
FAILED=$(grep -cE "Test Case .* failed " "$LOG_FILE" || true)
TOTAL=$((PASSED + FAILED))

# Pull out per-suite results in source order. Print each test once.
suite_lines=$(grep -E "Test Case .*( passed | failed )" "$LOG_FILE" || true)

printf '\n%b===== Swift Test Results =====%b\n' "$BOLD" "$NC"
if [ -n "$suite_lines" ]; then
    while IFS= read -r line; do
        # `'-[Module.Suite testName]' passed (0.001 seconds).`
        name=$(printf '%s\n' "$line" | sed -E "s/.*'-\[([^]]+)\]'.*/\1/")
        verdict=$(printf '%s\n' "$line" | grep -oE ' (passed|failed) ' | tr -d ' ')
        duration=$(printf '%s\n' "$line" | grep -oE '\([0-9.]+ seconds\)' | tr -d '()')
        if [ "$verdict" = "passed" ]; then
            printf '  %b✓%b %s %b%s%b\n' "$GREEN" "$NC" "$name" "$DIM" "$duration" "$NC"
        else
            printf '  %b✗%b %s %b%s%b\n' "$RED" "$NC" "$name" "$DIM" "$duration" "$NC"
        fi
    done <<<"$suite_lines"
else
    printf '  %b(no test cases were executed)%b\n' "$YELLOW" "$NC"
fi

printf '\n%bTotal:%b  %d   %bPassed:%b %d   %bFailed:%b %d\n' \
    "$BOLD" "$NC" "$TOTAL" "$GREEN" "$NC" "$PASSED" "$RED" "$NC" "$FAILED"

if [ "$FAILED" -gt 0 ] || [ "$XCODE_STATUS" -ne 0 ] || [ "$TOTAL" -eq 0 ]; then
    printf '%bFAIL%b — xcodebuild exit %d\n' "$RED" "$NC" "$XCODE_STATUS"
    if [ "$FAILED" -gt 0 ]; then
        printf '\n%bFailure detail:%b\n' "$BOLD" "$NC"
        grep -E "error:|failed:" "$LOG_FILE" | head -20 || true
    fi
    exit 1
fi
printf '%bPASS%b — all %d tests succeeded\n' "$GREEN" "$NC" "$TOTAL"
