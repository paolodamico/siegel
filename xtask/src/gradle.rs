//! Gradle bootstrapping, JDK discovery, and JUnit report parsing.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

use crate::common::{capture, create_temp_dir, find_by_extension, has_command, remove_dir, run};
use crate::report::{Outcome, TestCase};

/// Gradle version bootstrapped when the checkout has no wrapper. Pinned to an
/// 8.x release compatible with the Kotlin/JVM toolchain in `build.gradle.kts`.
const GRADLE_VERSION: &str = "8.10.2";

/// SHA-256 of the pinned distribution, so the archive is verified before any
/// code from it runs. The published value lives at
/// `https://services.gradle.org/distributions/gradle-<version>-bin.zip.sha256`.
const GRADLE_SHA256: &str = "31c55713e40233a8303827ceb42ca48a47267a0ad4bab9177123121e71524c26";

/// JDK major version the Gradle toolchain targets.
const JDK_VERSION: &str = "17";

/// Resolve a JDK home: `JAVA_HOME` when set, otherwise platform discovery.
///
/// CI sets `JAVA_HOME` directly via `setup-java`, so discovery only matters when
/// running by hand. The result is handed to child processes explicitly rather
/// than written back into this process's environment.
pub fn java_home() -> Option<PathBuf> {
    std::env::var_os("JAVA_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(discover_java_home)
}

/// Resolve a JDK home that actually contains `jni.h`, which the JNI glue needs.
pub fn java_home_with_jni() -> Result<PathBuf> {
    let home = java_home().context("JAVA_HOME must be set to a JDK (needed for jni.h)")?;
    if !home.join("include/jni.h").is_file() {
        bail!(
            "jni.h not found under {}/include — is JAVA_HOME a JDK rather than a JRE?",
            home.display()
        );
    }
    Ok(home)
}

/// Best-effort JDK discovery, using the canonical locator for each platform.
fn discover_java_home() -> Option<PathBuf> {
    if cfg!(target_os = "macos") {
        macos_java_home()
    } else {
        java_home_from_path()
    }
}

/// `/usr/libexec/java_home` is the canonical macOS JDK locator. When the
/// requested major version is missing we return `None` rather than guessing, so
/// Gradle's own "install a JDK 17" error surfaces instead of a confusing one.
fn macos_java_home() -> Option<PathBuf> {
    let locator = Path::new("/usr/libexec/java_home");
    if !locator.is_file() {
        return None;
    }
    let detected = capture(Command::new(locator).args(["-v", JDK_VERSION])).ok()?;
    let home = PathBuf::from(detected.trim());
    home.join("bin/java").is_file().then_some(home)
}

/// Derive `JAVA_HOME` from the resolved `java` binary on `PATH` (`<home>/bin/java`).
///
/// Canonicalization resolves symlink chains such as
/// `/usr/bin/java -> /etc/alternatives/java -> /usr/lib/jvm/.../bin/java`. This
/// is deliberately not used on macOS, where `/usr/bin/java` is a shim that would
/// resolve to a useless `/usr`.
fn java_home_from_path() -> Option<PathBuf> {
    let java = capture(Command::new("which").arg("java")).ok()?;
    let java = std::fs::canonicalize(java.trim()).ok()?;
    let home = java.parent()?.parent()?.to_path_buf();
    home.join("bin/java").is_file().then_some(home)
}

/// Generate a Gradle wrapper for `project` if one is not already present.
pub fn ensure_wrapper(project: &Path) -> Result<()> {
    if project.join("gradlew").is_file() {
        return Ok(());
    }
    println!("Bootstrapping Gradle {GRADLE_VERSION}...");

    let tmp = create_temp_dir("siegel-gradle")?;
    let result = bootstrap_wrapper(project, &tmp);
    // Report a cleanup failure without discarding `result`: the bootstrap error
    // is the one the user needs.
    if let Err(error) = remove_dir(&tmp) {
        println!("warning: could not clean up {}: {error}", tmp.display());
    }
    result
}

fn bootstrap_wrapper(project: &Path, tmp: &Path) -> Result<()> {
    // Wrapper generation must run on the JDK the tests will use, or it fails
    // outright against a JDK older than Gradle 8's minimum.
    let java = java_home();
    let zip = tmp.join("gradle.zip");
    let url = format!("https://services.gradle.org/distributions/gradle-{GRADLE_VERSION}-bin.zip");
    run(Command::new("curl")
        .args([
            "--fail",
            "-sSL",
            "--retry",
            "3",
            "--retry-all-errors",
            "--max-time",
            "300",
            &url,
        ])
        .arg("-o")
        .arg(&zip))
    .with_context(|| format!("downloading {url}"))?;
    verify_sha256(&zip, GRADLE_SHA256)?;

    let unpacked = tmp.join("unpacked");
    extract_zip(&zip, &unpacked)?;

    // `--no-daemon` is load-bearing: a daemon forked from `unpacked` would
    // outlive the temporary directory we delete right after, and the next
    // bootstrap would reuse it and die on the missing jars.
    let mut command = Command::new(unpacked.join(format!("gradle-{GRADLE_VERSION}/bin/gradle")));
    command.arg("--no-daemon").arg("-p").arg(project).args([
        "wrapper",
        "--gradle-version",
        GRADLE_VERSION,
        "--gradle-distribution-sha256-sum",
        GRADLE_SHA256,
        "--quiet",
    ]);
    if let Some(home) = java {
        command.env("JAVA_HOME", home);
    }
    run(&mut command)
}

/// Fail unless `path` hashes to `expected`.
fn verify_sha256(path: &Path, expected: &str) -> Result<()> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let actual = format!("{:x}", Sha256::digest(&bytes));
    if actual != expected {
        bail!(
            "SHA-256 mismatch for {}\n  expected: {expected}\n  actual:   {actual}",
            path.display()
        );
    }
    Ok(())
}

/// Unpack `zip` into `dest`, falling back to the JDK's `jar` when `unzip` is absent.
fn extract_zip(zip: &Path, dest: &Path) -> Result<()> {
    std::fs::create_dir_all(dest).with_context(|| format!("creating {}", dest.display()))?;
    if has_command("unzip") {
        return run(Command::new("unzip").arg("-q").arg(zip).arg("-d").arg(dest));
    }
    run(Command::new("jar").current_dir(dest).arg("xf").arg(zip))
}

/// Whitelist filter for Gradle's `--info` stream: task boundaries, per-test
/// outcomes, build results, and compiler diagnostics.
pub fn is_output_interesting(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("> Task")
        || trimmed.starts_with("BUILD ")
        || trimmed.contains("PASSED")
        || trimmed.contains("FAILED")
        || trimmed.contains("FAILURE")
        // Kotlin compiler diagnostics: `e: file:line:col: message`. Anchored,
        // because an unanchored "e: " also matches ordinary Gradle prose.
        || trimmed.starts_with("e: ")
        || trimmed.starts_with("w: ")
}

/// Selects the Gradle log lines worth showing when a test case fails.
pub fn is_failure_detail(line: &str) -> bool {
    line.contains("FAILED") || line.contains("exception:")
}

/// Parse every JUnit XML report under `dir` into one entry per `<testcase>`.
pub fn parse_results(dir: &Path) -> Result<Vec<TestCase>> {
    let mut cases = Vec::new();
    for file in find_by_extension(dir, "xml")? {
        let xml = std::fs::read_to_string(&file)
            .with_context(|| format!("reading {}", file.display()))?;
        cases.extend(parse_report(&xml));
    }
    Ok(cases)
}

/// Split a JUnit report into one entry per `<testcase>` element.
fn parse_report(xml: &str) -> Vec<TestCase> {
    xml.split("<testcase")
        .skip(1)
        .filter_map(parse_case)
        .collect()
}

/// Parse one `<testcase>` element from `chunk`, which begins immediately after
/// the opening `<testcase` and runs to the end of the document.
///
/// Element-based rather than line-based: Gradle currently newline-indents the
/// children, but `<testcase …><failure/></testcase>` on one line is equally
/// valid XML and a line-oriented scan reads it as a self-closing pass.
fn parse_case(chunk: &str) -> Option<TestCase> {
    let tag_end = chunk.find('>')?;
    let attributes = &chunk[..tag_end];
    // `<testcase … />` has no children: it ran and passed.
    let body = if attributes.trim_end().ends_with('/') {
        ""
    } else {
        let rest = &chunk[tag_end + 1..];
        &rest[..rest.find("</testcase>").unwrap_or(rest.len())]
    };

    // A failure outranks a skip: Gradle emits both when a test throws while
    // being disabled.
    let outcome = if body.contains("<failure") || body.contains("<error") {
        Outcome::Failed
    } else if body.contains("<skipped") {
        Outcome::Skipped
    } else {
        Outcome::Passed
    };
    Some(TestCase {
        name: format!(
            "{}.{}",
            attr(attributes, "classname").unwrap_or_default(),
            attr(attributes, "name").unwrap_or_default()
        ),
        duration: format!("{}s", attr(attributes, "time").unwrap_or_default()),
        outcome,
    })
}

/// Read the value of the `name="…"` attribute from an XML element line.
///
/// The match is anchored on preceding whitespace so `name` does not also match
/// the tail of `classname`.
fn attr<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    let needle = format!("{name}=\"");
    line.match_indices(&needle)
        .find(|(index, _)| *index == 0 || line.as_bytes()[index - 1].is_ascii_whitespace())
        .and_then(|(index, _)| line[index + needle.len()..].split_once('"'))
        .map(|(value, _)| value)
}

#[cfg(test)]
mod tests {
    use super::{Outcome, attr, is_failure_detail, is_output_interesting, parse_report};

    #[test]
    fn reads_attributes_without_matching_suffixes() {
        let line = r#"<testcase name="testFoo" classname="siegel.GuardTests" time="0.01"/>"#;
        assert_eq!(attr(line, "name"), Some("testFoo"));
        assert_eq!(attr(line, "classname"), Some("siegel.GuardTests"));
        assert_eq!(attr(line, "time"), Some("0.01"));
        assert_eq!(attr(line, "missing"), None);
    }

    #[test]
    fn reads_attributes_when_classname_comes_first() {
        let line = r#"<testcase classname="siegel.GuardTests" name="testFoo" time="0.01"/>"#;
        assert_eq!(attr(line, "name"), Some("testFoo"));
    }

    #[test]
    fn self_closing_testcase_passes() {
        let cases = parse_report(r#"<testcase name="a" classname="C" time="0.5"/>"#);
        assert_eq!(cases.len(), 1);
        assert_eq!(cases[0].name, "C.a");
        assert_eq!(cases[0].duration, "0.5s");
        assert_eq!(cases[0].outcome, Outcome::Passed);
    }

    #[test]
    fn nested_failure_marks_the_case_failed() {
        let xml = "<testsuite>\n\
                   <testcase name=\"a\" classname=\"C\" time=\"0.1\">\n\
                   <failure message=\"boom\"/>\n\
                   </testcase>\n\
                   <testcase name=\"b\" classname=\"C\" time=\"0.2\"/>\n\
                   </testsuite>";
        let cases = parse_report(xml);
        assert_eq!(cases.len(), 2);
        assert_eq!(cases[0].outcome, Outcome::Failed);
        assert_eq!(cases[1].outcome, Outcome::Passed);
    }

    #[test]
    fn nested_error_marks_the_case_failed() {
        let xml = "<testcase name=\"a\" classname=\"C\" time=\"0.1\">\n\
                   <error message=\"boom\"/>\n\
                   </testcase>";
        assert_eq!(parse_report(xml)[0].outcome, Outcome::Failed);
    }

    #[test]
    fn nested_skipped_is_not_reported_as_a_pass() {
        let xml = "<testcase name=\"a\" classname=\"C\" time=\"0.0\">\n\
                   <skipped/>\n\
                   </testcase>";
        assert_eq!(parse_report(xml)[0].outcome, Outcome::Skipped);
    }

    #[test]
    fn a_single_line_element_with_a_failure_child_is_not_read_as_a_pass() {
        let xml =
            r#"<testcase name="a" classname="C" time="0.1"><failure message="boom"/></testcase>"#;
        assert_eq!(parse_report(xml)[0].outcome, Outcome::Failed);
    }

    #[test]
    fn a_failure_outranks_a_skip_in_the_same_case() {
        let xml = "<testcase name=\"a\" classname=\"C\" time=\"0.0\">\n\
                   <skipped/>\n\
                   <failure message=\"boom\"/>\n\
                   </testcase>";
        assert_eq!(parse_report(xml)[0].outcome, Outcome::Failed);
    }

    #[test]
    fn a_later_failure_does_not_leak_into_the_previous_case() {
        let xml = "<testcase name=\"a\" classname=\"C\" time=\"0.1\"/>\n\
                   <testcase name=\"b\" classname=\"C\" time=\"0.2\">\n\
                   <failure/>\n\
                   </testcase>";
        let cases = parse_report(xml);
        assert_eq!(cases[0].outcome, Outcome::Passed);
        assert_eq!(cases[1].outcome, Outcome::Failed);
    }

    #[test]
    fn output_filter_ignores_info_prose_that_merely_contains_e_colon() {
        assert!(is_output_interesting(
            "e: Session.kt:12:5: unresolved reference"
        ));
        assert!(!is_output_interesting("Cache value: 12 entries"));
        assert!(!is_output_interesting(
            "Resolving dependencies for scope: test"
        ));
    }

    #[test]
    fn failure_detail_selects_gradle_failure_lines() {
        assert!(is_failure_detail("> Task :siegel-tests:test FAILED"));
        assert!(is_failure_detail("Caused by: java.lang.exception: boom"));
        assert!(!is_failure_detail("> Task :siegel-tests:compileKotlin"));
    }
}
