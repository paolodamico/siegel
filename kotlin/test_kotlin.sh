#!/usr/bin/env bash
# Build the host cdylib + generate Kotlin bindings, then run a JUnit suite on
# the JVM. Prints a colored per-test summary. Set VERBOSE=1 to stream the full
# Gradle output.
#
# Usage: ./kotlin/test_kotlin.sh [uniffi|boltffi]   (default: uniffi)
set -euo pipefail

BASE_PATH="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

BINDING="${1:-uniffi}"
case "$BINDING" in
    uniffi)  MODULE="siegel-tests";         BUILD_SCRIPT="build_kotlin.sh";         BUILD_ARGS=() ;;
    boltffi) MODULE="siegel-boltffi-tests"; BUILD_SCRIPT="build_boltffi_kotlin.sh"; BUILD_ARGS=(--test-utils) ;;
    *) echo "Unknown binding '$BINDING' (expected: uniffi, boltffi)" >&2; exit 1 ;;
esac

if [ -t 1 ] && [ "${NO_COLOR:-}" = "" ]; then
    GREEN='\033[0;32m'; RED='\033[0;31m'; YELLOW='\033[0;33m'
    BOLD='\033[1m'; DIM='\033[2m'; NC='\033[0m'
else
    GREEN=''; RED=''; YELLOW=''; BOLD=''; DIM=''; NC=''
fi

# Best-effort JAVA_HOME discovery for local dev. CI sets this directly
# via setup-java, so this block is only exercised when running by hand.
# Each branch validates the resolved path before exporting, so we never
# hand Gradle something like `/usr` (which would happen if you ran two
# `dirname`s on macOS's `/usr/bin/java` shim).
if [ -z "${JAVA_HOME:-}" ]; then
    case "$OSTYPE" in
        darwin*)
            # /usr/libexec/java_home is the canonical macOS JDK locator.
            # `-v 17` asks for the matching major version; if it's missing
            # we let it fall through unset so Gradle's own error message
            # surfaces ("install a JDK 17") instead of a confusing one.
            if [ -x /usr/libexec/java_home ]; then
                detected="$(/usr/libexec/java_home -v 17 2>/dev/null || true)"
                if [ -n "$detected" ] && [ -x "$detected/bin/java" ]; then
                    export JAVA_HOME="$detected"
                fi
            fi
            ;;
        linux*)
            # GNU `readlink -f` resolves symlink chains like
            # /usr/bin/java -> /etc/alternatives/java -> /usr/lib/jvm/.../bin/java
            # Two `dirname`s then give the JDK home. macOS's BSD readlink
            # lacks `-f`, which is why this branch is Linux-only.
            if command -v java >/dev/null 2>&1; then
                resolved="$(readlink -f "$(command -v java)" 2>/dev/null || true)"
                if [ -n "$resolved" ]; then
                    candidate="$(dirname "$(dirname "$resolved")")"
                    if [ -x "$candidate/bin/java" ]; then
                        export JAVA_HOME="$candidate"
                    fi
                fi
            fi
            ;;
    esac
fi

echo "Step 1: building host cdylib + Kotlin bindings ($BINDING)"
bash "$BASE_PATH/$BUILD_SCRIPT" ${BUILD_ARGS[@]+"${BUILD_ARGS[@]}"}

cd "$BASE_PATH"

# Bootstrap a Gradle wrapper if the checkout doesn't have one. Pinned to a
# Gradle version that's compatible with our Kotlin/JVM toolchain (8.x).
# The SHA-256 is pinned alongside the version so the downloaded archive is
# verified before any code from it is executed; the published value lives
# at https://services.gradle.org/distributions/gradle-<ver>-bin.zip.sha256.
GRADLE_VERSION="8.10.2"
GRADLE_SHA256="31c55713e40233a8303827ceb42ca48a47267a0ad4bab9177123121e71524c26"

if [ ! -f "gradlew" ]; then
    echo "Step 2: bootstrapping Gradle wrapper..."
    DIST_URL="https://services.gradle.org/distributions/gradle-${GRADLE_VERSION}-bin.zip"
    TMP="$(mktemp -d)"
    trap 'rm -rf "$TMP"' EXIT
    curl --fail -sSL --retry 3 --retry-all-errors --max-time 300 "$DIST_URL" -o "$TMP/gradle.zip"

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

TEST_RESULTS_DIR="$BASE_PATH/$MODULE/build/test-results/test"
rm -rf "$TEST_RESULTS_DIR"

LOG_FILE="$(mktemp -t siegel-kotlin.XXXXXX)"
# shellcheck disable=SC2064
trap "rm -f \"$LOG_FILE\"; rm -rf \"${TMP:-}\"" EXIT

printf 'Step 3: running gradle test %b(output buffered; set VERBOSE=1 to stream)%b\n' "$DIM" "$NC"

set +e
if [ "${VERBOSE:-0}" = "1" ]; then
    ./gradlew --no-daemon "$MODULE:test" --info --continue 2>&1 | tee "$LOG_FILE"
    GRADLE_STATUS=${PIPESTATUS[0]}
else
    ./gradlew --no-daemon "$MODULE:test" --continue >"$LOG_FILE" 2>&1
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

printf '\n%b===== Kotlin Test Results (%s) =====%b\n' "$BOLD" "$BINDING" "$NC"
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
