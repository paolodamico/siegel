#!/usr/bin/env bash
# Build the host cdylib + generate Kotlin bindings, then run the JUnit
# suite under `kotlin/siegel-tests/` on the JVM. Prints a colored
# per-test summary. Set VERBOSE=1 to stream the full Gradle output.
set -euo pipefail

BASE_PATH="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

if [ -t 1 ] && [ "${NO_COLOR:-}" = "" ]; then
    GREEN='\033[0;32m'; RED='\033[0;31m'; YELLOW='\033[0;33m'
    BOLD='\033[1m'; DIM='\033[2m'; NC='\033[0m'
else
    GREEN=''; RED=''; YELLOW=''; BOLD=''; DIM=''; NC=''
fi

# Ensure a working JAVA_HOME. CI runners typically set this; on dev macs
# we look for a Homebrew-installed JDK 17 if nothing is configured.
if [ -z "${JAVA_HOME:-}" ]; then
    if [ -d "/opt/homebrew/opt/openjdk@17" ]; then
        JAVA_HOME="/opt/homebrew/opt/openjdk@17/libexec/openjdk.jdk/Contents/Home"
        export JAVA_HOME
    elif command -v java >/dev/null 2>&1; then
        JAVA_HOME="$(dirname "$(dirname "$(readlink -f "$(command -v java)" 2>/dev/null || command -v java)")")"
        export JAVA_HOME
    fi
fi

echo "Step 1: building host cdylib + Kotlin bindings"
bash "$BASE_PATH/build_kotlin.sh"

cd "$BASE_PATH"

# Bootstrap a Gradle wrapper if the checkout doesn't have one. Pinned to a
# Gradle version that's compatible with our Kotlin/JVM toolchain (8.x).
if [ ! -f "gradlew" ]; then
    echo "Step 2: bootstrapping Gradle wrapper..."
    GRADLE_VERSION="${GRADLE_VERSION:-8.10.2}"
    DIST_URL="https://services.gradle.org/distributions/gradle-${GRADLE_VERSION}-bin.zip"
    TMP="$(mktemp -d)"
    trap 'rm -rf "$TMP"' EXIT
    curl -sSL "$DIST_URL" -o "$TMP/gradle.zip"
    mkdir -p "$TMP/unzip"
    if command -v unzip >/dev/null 2>&1; then
        unzip -q "$TMP/gradle.zip" -d "$TMP/unzip"
    else
        (cd "$TMP/unzip" && jar xvf "$TMP/gradle.zip" >/dev/null)
    fi
    "$TMP/unzip/gradle-${GRADLE_VERSION}/bin/gradle" wrapper --gradle-version "$GRADLE_VERSION" --quiet
fi

TEST_RESULTS_DIR="$BASE_PATH/siegel-tests/build/test-results/test"
rm -rf "$TEST_RESULTS_DIR"

LOG_FILE="$(mktemp -t siegel-kotlin.XXXXXX)"
# shellcheck disable=SC2064
trap "rm -f \"$LOG_FILE\"; rm -rf \"${TMP:-}\"" EXIT

printf 'Step 3: running gradle test %b(output buffered; set VERBOSE=1 to stream)%b\n' "$DIM" "$NC"

set +e
if [ "${VERBOSE:-0}" = "1" ]; then
    ./gradlew --no-daemon siegel-tests:test --info --continue 2>&1 | tee "$LOG_FILE"
    GRADLE_STATUS=${PIPESTATUS[0]}
else
    ./gradlew --no-daemon siegel-tests:test --continue >"$LOG_FILE" 2>&1
    GRADLE_STATUS=$?
fi
set -e

# Parse JUnit XML reports. Each `<testcase>` element is one test; nested
# `<failure>`/`<error>` children mark it as failed.
PASSED=0
FAILED=0
suite_lines=""

if [ -d "$TEST_RESULTS_DIR" ]; then
    while IFS= read -r xml; do
        # Walk testcase entries. Mac sed lacks `-E` for capture groups so
        # use awk for cross-platform parsing.
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
                cls = ""; nm = ""; t = ""; verdict = "passed"
                if (match($0, /classname="[^"]*"/))
                    cls = substr($0, RSTART+11, RLENGTH-12)
                if (match($0, /name="[^"]*"/))
                    nm = substr($0, RSTART+6, RLENGTH-7)
                if (match($0, /time="[^"]*"/))
                    t = substr($0, RSTART+6, RLENGTH-7)
                # Self-closing testcase always passes; otherwise look ahead.
                if ($0 ~ /\/>/) {
                    print cls "." nm "\t" t "\tpassed"
                    next
                }
                # Read until </testcase>, watching for failure/error.
                while ((getline next_line) > 0) {
                    if (next_line ~ /<failure|<error/) verdict = "failed"
                    if (next_line ~ /<\/testcase>/) break
                }
                print cls "." nm "\t" t "\t" verdict
            }
        ' "$xml")
    done < <(find "$TEST_RESULTS_DIR" -name "*.xml" | sort)
fi
TOTAL=$((PASSED + FAILED))

printf '\n%b===== Kotlin Test Results =====%b\n' "$BOLD" "$NC"
if [ "$TOTAL" -gt 0 ]; then
    printf '%s' "$suite_lines"
else
    printf '  %b(no test cases were executed)%b\n' "$YELLOW" "$NC"
fi

printf '\n%bTotal:%b  %d   %bPassed:%b %d   %bFailed:%b %d\n' \
    "$BOLD" "$NC" "$TOTAL" "$GREEN" "$NC" "$PASSED" "$RED" "$NC" "$FAILED"

if [ "$FAILED" -gt 0 ] || [ "$GRADLE_STATUS" -ne 0 ] || [ "$TOTAL" -eq 0 ]; then
    printf '%bFAIL%b — gradle exit %d\n' "$RED" "$NC" "$GRADLE_STATUS"
    if [ "$FAILED" -gt 0 ]; then
        printf '\n%bFailure detail:%b\n' "$BOLD" "$NC"
        grep -E "FAILED|>.*FAILED|exception:" "$LOG_FILE" | head -20 || true
    fi
    exit 1
fi
printf '%bPASS%b — all %d tests succeeded\n' "$GREEN" "$NC" "$TOTAL"
