#!/usr/bin/env bash
# Build the Android jniLibs + bindings, then run the instrumented test suite
# on a connected emulator/device via Gradle. Prints a colored summary.
#
# In CI the emulator is provided by reactivecircus/android-emulator-runner,
# which invokes this script with a booted emulator. Locally, connect a device
# or boot an emulator (an x86_64 AVD) first. Set VERBOSE=1 to stream Gradle.
# Set ASSEMBLE_ONLY=1 to only cross-compile + build the test APK (no emulator).
set -euo pipefail

BASE_PATH="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

if [ -t 1 ] && [ "${NO_COLOR:-}" = "" ]; then
    GREEN='\033[0;32m'; RED='\033[0;31m'; YELLOW='\033[0;33m'
    BOLD='\033[1m'; DIM='\033[2m'; NC='\033[0m'
else
    GREEN=''; RED=''; YELLOW=''; BOLD=''; DIM=''; NC=''
fi

# if [ -z "${ANDROID_HOME:-}" ] && [ -z "${ANDROID_SDK_ROOT:-}" ]; then
#     echo "ANDROID_HOME/ANDROID_SDK_ROOT not set; the Android SDK is required." >&2
#     exit 1
# fi

echo "Step 1: cross-compiling native libs + generating bindings"
bash "$BASE_PATH/build_android.sh"

cd "$BASE_PATH"

# Bootstrap a verified Gradle wrapper if the checkout doesn't have one. Pinned
# version + SHA-256 so the archive is verified before any of its code runs.
# (Will be split back into a shared helper in a follow-up PR.)
GRADLE_VERSION="8.10.2"
GRADLE_SHA256="31c55713e40233a8303827ceb42ca48a47267a0ad4bab9177123121e71524c26"

if [ ! -f "gradlew" ]; then
    echo "Step 2: bootstrapping Gradle wrapper ${GRADLE_VERSION}..."
    DIST_URL="https://services.gradle.org/distributions/gradle-${GRADLE_VERSION}-bin.zip"
    TMP="$(mktemp -d)"
    trap 'rm -rf "$TMP"' EXIT
    curl --fail -sSL "$DIST_URL" -o "$TMP/gradle.zip"

    if command -v sha256sum >/dev/null 2>&1; then
        actual_sha="$(sha256sum "$TMP/gradle.zip" | awk '{print $1}')"
    elif command -v shasum >/dev/null 2>&1; then
        actual_sha="$(shasum -a 256 "$TMP/gradle.zip" | awk '{print $1}')"
    else
        echo "ERROR: neither sha256sum nor shasum is available; cannot verify Gradle distribution" >&2
        exit 1
    fi
    if [ "$actual_sha" != "$GRADLE_SHA256" ]; then
        echo "ERROR: Gradle distribution SHA-256 mismatch for $DIST_URL" >&2
        echo "  expected: $GRADLE_SHA256" >&2
        echo "  actual:   $actual_sha" >&2
        exit 1
    fi

    mkdir -p "$TMP/unzip"
    if command -v unzip >/dev/null 2>&1; then
        unzip -q "$TMP/gradle.zip" -d "$TMP/unzip"
    else
        (cd "$TMP/unzip" && jar xvf "$TMP/gradle.zip" >/dev/null)
    fi
    "$TMP/unzip/gradle-${GRADLE_VERSION}/bin/gradle" wrapper \
        --gradle-version "$GRADLE_VERSION" \
        --gradle-distribution-sha256-sum "$GRADLE_SHA256" \
        --quiet
fi

# Fast compile gate: build the androidTest APK without an emulator, then stop.
# The CI android-build job runs this to get a quick red/green before the slow
# emulator run.
if [ "${ASSEMBLE_ONLY:-0}" = "1" ]; then
    echo "Assembling androidTest APK (compile-only, no emulator)..."
    ./gradlew --no-daemon :siegel-android:assembleDebugAndroidTest
    exit 0
fi

RESULTS_DIR="$BASE_PATH/siegel-android/build/outputs/androidTest-results/connected"
rm -rf "$RESULTS_DIR"

LOG_FILE="$(mktemp -t siegel-android.XXXXXX)"
trap 'rm -f "$LOG_FILE"; rm -rf "${TMP:-}"' EXIT

printf 'Step 3: running connectedDebugAndroidTest %b(output buffered; set VERBOSE=1 to stream)%b\n' "$DIM" "$NC"

set +e
if [ "${VERBOSE:-0}" = "1" ]; then
    ./gradlew --no-daemon :siegel-android:connectedDebugAndroidTest --info 2>&1 | tee "$LOG_FILE"
    GRADLE_STATUS=${PIPESTATUS[0]}
else
    ./gradlew --no-daemon :siegel-android:connectedDebugAndroidTest >"$LOG_FILE" 2>&1
    GRADLE_STATUS=$?
fi
set -e

# Parse JUnit-style XML the instrumentation runner writes. Each <testcase> is
# one test; a nested <failure>/<error> marks it failed.
PASSED=0
FAILED=0
suite_lines=""

if [ -d "$RESULTS_DIR" ]; then
    while IFS= read -r xml; do
        while IFS=$'\t' read -r name time verdict; do
            [ -z "$name" ] && continue
            duration="${time}s"
            if [ "$verdict" = "failed" ]; then
                FAILED=$((FAILED + 1))
                line=$(printf '  %b✗%b %s %b(%s)%b' "$RED" "$NC" "$name" "$DIM" "$duration" "$NC")
            else
                PASSED=$((PASSED + 1))
                line=$(printf '  %b✓%b %s %b(%s)%b' "$GREEN" "$NC" "$name" "$DIM" "$duration" "$NC")
            fi
            suite_lines="${suite_lines}${line}"$'\n'
        done < <(awk '
            /<testcase/ {
                nm = ""; t = ""; verdict = "passed"
                if (match($0, /name="[^"]*"/))
                    nm = substr($0, RSTART+6, RLENGTH-7)
                if (match($0, /time="[^"]*"/))
                    t = substr($0, RSTART+6, RLENGTH-7)
                if ($0 ~ /\/>/) { print nm "\t" t "\tpassed"; next }
                while ((getline next_line) > 0) {
                    if (next_line ~ /<failure|<error/) verdict = "failed"
                    if (next_line ~ /<\/testcase>/) break
                }
                print nm "\t" t "\t" verdict
            }
        ' "$xml")
    done < <(find "$RESULTS_DIR" -name "*.xml" | sort)
fi
TOTAL=$((PASSED + FAILED))

printf '\n%b===== Android Instrumented Test Results =====%b\n' "$BOLD" "$NC"
if [ "$TOTAL" -gt 0 ]; then
    printf '%s' "$suite_lines"
else
    printf '  %b(no test cases were executed)%b\n' "$YELLOW" "$NC"
fi

printf '\n%bTotal:%b  %d   %bPassed:%b %d   %bFailed:%b %d\n' \
    "$BOLD" "$NC" "$TOTAL" "$GREEN" "$NC" "$PASSED" "$RED" "$NC" "$FAILED"

if [ "$FAILED" -gt 0 ] || [ "$GRADLE_STATUS" -ne 0 ] || [ "$TOTAL" -eq 0 ]; then
    printf '%bFAIL%b — gradle exit %d\n' "$RED" "$NC" "$GRADLE_STATUS"
    grep -E "FAILED|error:|Exception|Caused by" "$LOG_FILE" | head -30 || true
    exit 1
fi
printf '%bPASS%b — all %d tests succeeded\n' "$GREEN" "$NC" "$TOTAL"
