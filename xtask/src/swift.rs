//! Swift bindings, packaging, and the foreign XCTest suite.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

use crate::Binding;
use crate::boltffi;
use crate::common::{
    UNIFFI_FEATURES, capture, capture_lossy, copy_file, create_dir, dir_listing, ensure_file,
    find_by_extension, in_ci, move_file, project_root, remove_dir, run, run_streamed,
    uniffi_generate,
};
use crate::report::{Outcome, Runner, TestCase, summarize};

/// Consumer-facing Swift module name.
const SWIFT_MODULE: &str = "Siegel";
/// `siegel-uniffi`'s `staticlib`/`cdylib` basename.
const UNIFFI_LIB: &str = "siegel_uniffi";
/// Kept in sync with `deployment_target` in `siegel-boltffi/boltffi.toml`.
const IOS_DEPLOYMENT_TARGET: &str = "13.0";
const IOS_RUSTFLAGS: &str = "-C link-arg=-Wl,-application_extension";

/// Absolute path to the `swift/` directory inside the workspace.
fn swift_dir() -> PathBuf {
    project_root().join("swift")
}

/// Cross-compile for iOS, generate Swift bindings, and assemble an XCFramework.
pub fn build(binding: Binding, sim_only: bool) -> Result<()> {
    match binding {
        Binding::Uniffi => build_uniffi(sim_only),
        Binding::Boltffi => build_boltffi(sim_only, false),
    }
}

/// Build `Siegel.xcframework` from `siegel-uniffi` into `swift/`.
fn build_uniffi(sim_only: bool) -> Result<()> {
    let root = project_root();
    let swift = swift_dir();

    let ios_build = swift.join("ios_build");
    let bindings = ios_build.join("bindings");
    let headers = ios_build.join(format!("Headers/{SWIFT_MODULE}"));
    let sources = swift.join(format!("Sources/{SWIFT_MODULE}"));
    let framework = swift.join(format!("{SWIFT_MODULE}.xcframework"));

    println!(
        "Building {SWIFT_MODULE}.xcframework to {}",
        framework.display()
    );
    remove_dir(&ios_build)?;
    remove_dir(&framework)?;
    for dir in [&bindings, &headers, &sources] {
        create_dir(dir)?;
    }

    let slices = if sim_only {
        build_sim_slice(&root)?
    } else {
        build_all_slices(&root, &ios_build.join("target/universal-ios-sim/release"))?
    };

    println!("Generating Swift bindings...");
    uniffi_generate(&root, &slices.bindgen_dylib, "swift", &bindings)?;
    move_file(
        &bindings.join(format!("{UNIFFI_LIB}.swift")),
        &sources.join(format!("{UNIFFI_LIB}.swift")),
    )?;
    move_file(
        &bindings.join(format!("{UNIFFI_LIB}FFI.h")),
        &headers.join(format!("{UNIFFI_LIB}FFI.h")),
    )?;
    // clang only picks up a module map named `module.modulemap` inside the
    // headers directory, so the generated one is copied under that name.
    copy_file(
        &bindings.join(format!("{UNIFFI_LIB}FFI.modulemap")),
        &headers.join("module.modulemap"),
    )?;

    println!("Creating XCFramework...");
    create_xcframework(&slices, &ios_build.join("Headers"), &framework)?;

    remove_dir(&ios_build)?;
    println!("Swift framework built at: {}", framework.display());
    Ok(())
}

/// The static library slices that go into the XCFramework, plus the dylib
/// `uniffi-bindgen` reads the interface metadata from.
struct Slices {
    /// `None` for a simulator-only build.
    device_lib: Option<PathBuf>,
    sim_lib: PathBuf,
    bindgen_dylib: PathBuf,
}

/// Build only the simulator slice this host can actually load: Apple Silicon
/// runs the arm64 simulator, Intel the `x86_64` one.
fn build_sim_slice(root: &Path) -> Result<Slices> {
    let target = if cfg!(target_arch = "aarch64") {
        "aarch64-apple-ios-sim"
    } else if cfg!(target_arch = "x86_64") {
        "x86_64-apple-ios"
    } else {
        bail!("unsupported host architecture for --sim-only");
    };
    ensure_targets_installed(&[target])?;
    println!("Compiling Rust for {target} (sim-only)...");
    cargo_ios_build(root, target)?;
    let release = root.join(format!("target/{target}/release"));
    Ok(Slices {
        device_lib: None,
        sim_lib: release.join(format!("lib{UNIFFI_LIB}.a")),
        bindgen_dylib: release.join(format!("lib{UNIFFI_LIB}.dylib")),
    })
}

/// Build the device slice plus both simulator slices, then `lipo` the
/// simulator ones into a universal binary.
fn build_all_slices(root: &Path, universal_dir: &Path) -> Result<Slices> {
    let targets = [
        "aarch64-apple-ios-sim",
        "aarch64-apple-ios",
        "x86_64-apple-ios",
    ];
    ensure_targets_installed(&targets)?;
    println!("Compiling Rust for iOS targets...");
    for target in targets {
        cargo_ios_build(root, target)?;
    }

    println!("Building universal simulator binary...");
    create_dir(universal_dir)?;
    let universal = universal_dir.join(format!("lib{UNIFFI_LIB}.a"));
    run(Command::new("lipo")
        .arg("-create")
        .arg(root.join(format!(
            "target/aarch64-apple-ios-sim/release/lib{UNIFFI_LIB}.a"
        )))
        .arg(root.join(format!("target/x86_64-apple-ios/release/lib{UNIFFI_LIB}.a")))
        .arg("-output")
        .arg(&universal))?;
    run(Command::new("lipo").arg("-info").arg(&universal))?;

    Ok(Slices {
        device_lib: Some(root.join(format!(
            "target/aarch64-apple-ios/release/lib{UNIFFI_LIB}.a"
        ))),
        sim_lib: universal,
        bindgen_dylib: root.join(format!(
            "target/aarch64-apple-ios-sim/release/lib{UNIFFI_LIB}.dylib"
        )),
    })
}

/// Fail with an actionable message when an iOS target is not installed, rather
/// than letting rustc report `can't find crate for 'core'`.
fn ensure_targets_installed(targets: &[&str]) -> Result<()> {
    let installed = capture_lossy(Command::new("rustup").args(["target", "list", "--installed"]))
        .unwrap_or_default();
    // An absent rustup (e.g. a distro toolchain) leaves nothing to check.
    if installed.is_empty() {
        return Ok(());
    }
    let missing: Vec<_> = targets
        .iter()
        .filter(|target| !installed.lines().any(|line| line.trim() == **target))
        .copied()
        .collect();
    if !missing.is_empty() {
        bail!(
            "missing Rust target(s): {}\nInstall them with: rustup target add {}",
            missing.join(", "),
            missing.join(" ")
        );
    }
    Ok(())
}

/// `cargo build` `siegel-uniffi` for one iOS target.
fn cargo_ios_build(root: &Path, target: &str) -> Result<()> {
    run(Command::new("cargo")
        .current_dir(root)
        .args([
            "build",
            "--package",
            "siegel-uniffi",
            "--features",
            UNIFFI_FEATURES,
            "--release",
            "--target",
            target,
        ])
        .env("IPHONEOS_DEPLOYMENT_TARGET", IOS_DEPLOYMENT_TARGET)
        .env("RUSTFLAGS", IOS_RUSTFLAGS))
}

fn create_xcframework(slices: &Slices, headers: &Path, out: &Path) -> Result<()> {
    let mut command = Command::new("xcodebuild");
    command.arg("-create-xcframework");
    if let Some(device_lib) = &slices.device_lib {
        command
            .arg("-library")
            .arg(device_lib)
            .arg("-headers")
            .arg(headers);
    }
    run(command
        .arg("-library")
        .arg(&slices.sim_lib)
        .arg("-headers")
        .arg(headers)
        .arg("-output")
        .arg(out))
}

/// Cross-compile `siegel-boltffi` for iOS and assemble an XCFramework.
///
/// Unlike the UniFFI path this does not drive cargo/lipo/xcodebuild by hand:
/// `boltffi pack apple` builds every configured slice, generates the Swift
/// sources, and assembles the XCFramework in one step. Layout is controlled by
/// `siegel-boltffi/boltffi.toml`.
fn build_boltffi(sim_only: bool, test_utils: bool) -> Result<()> {
    let crate_dir = boltffi::crate_dir();
    boltffi::ensure_cli()?;

    // Pack into a clean tree. A previous --sim-only run leaves a simulator-only
    // XCFramework here, and nothing downstream distinguishes it from a release
    // artifact.
    let dist = crate_dir.join("dist/apple");
    remove_dir(&dist)?;

    let mut command = Command::new("boltffi");
    command
        .current_dir(&crate_dir)
        .args(["pack", "apple", "--release"]);
    if test_utils {
        // Test-only helpers are off by default. They export `sha256_consume`
        // and `unsafe_test_only_siegel_front_guard_bolt`.
        command.args(["--cargo-arg", "--features", "--cargo-arg", "test-utils"]);
    }
    if sim_only {
        command.args(["--overlay", "boltffi.ci.toml"]);
    }
    run(&mut command)?;

    // `pack apple` exits zero when every configured slice is disabled, which
    // would otherwise report success with no artifact.
    let framework = dist.join(format!("{SWIFT_MODULE}.xcframework"));
    if !framework.is_dir() {
        bail!(
            "boltffi pack apple produced no {SWIFT_MODULE}.xcframework at {} — check the enabled \
             architectures in boltffi.toml\n{}",
            framework.display(),
            dir_listing(&dist, 2)
        );
    }

    println!();
    println!("Apple artifacts in {}:", dist.display());
    println!("{}", dir_listing(&dist, 1));
    Ok(())
}

/// Build the bindings sim-only and run the foreign XCTest suite on a simulator.
pub fn test(binding: Binding) -> Result<()> {
    let swift = swift_dir();
    let (tests, scheme, framework) = match binding {
        Binding::Uniffi => (
            swift.join("tests"),
            "SiegelIntegrationTests",
            swift.join(format!("{SWIFT_MODULE}.xcframework")),
        ),
        Binding::Boltffi => (
            swift.join("boltffi-tests"),
            "SiegelBoltffiIntegrationTests",
            boltffi::crate_dir().join(format!("dist/apple/{SWIFT_MODULE}.xcframework")),
        ),
    };

    // Tolerate a non-zero exit so xcodebuild's own message survives: a machine
    // with no Xcode selected, or an unaccepted licence, explains itself here.
    let sdks = capture_lossy(Command::new("xcodebuild").arg("-showsdks")).unwrap_or_default();
    if !sdks.contains("iphonesimulator") {
        bail!("No iOS Simulator SDK installed. `xcodebuild -showsdks` said:\n{sdks}");
    }

    println!("Step 1: building Swift bindings ({binding}, sim-only)");
    match binding {
        Binding::Uniffi => build_uniffi(true)?,
        // The suite drives `sha256_consume` and the guard helpers end to end.
        Binding::Boltffi => build_boltffi(true, true)?,
    }
    if !framework.is_dir() {
        bail!("Missing XCFramework at {}", framework.display());
    }

    println!("Step 2: copying generated sources into the test package");
    match binding {
        Binding::Uniffi => copy_uniffi_sources(&swift, &tests)?,
        Binding::Boltffi => copy_boltffi_sources(&framework, &tests)?,
    }

    println!("Step 3: picking simulator");
    let simulator = pick_simulator()?;
    println!("Using simulator: {simulator}");
    if in_ci() {
        clean_simulator(&simulator)?;
    }
    remove_dir(&tests.join(".build"))?;

    println!("Step 4: running xcodebuild test (set VERBOSE=1 to stream the full log)");
    let (log, status) = run_streamed(
        Command::new("xcodebuild")
            .current_dir(&tests)
            .arg("test")
            .args(["-scheme", scheme])
            .arg("-destination")
            .arg(format!("platform=iOS Simulator,id={simulator}"))
            .args(["-sdk", "iphonesimulator", "CODE_SIGNING_ALLOWED=NO"]),
        is_output_interesting,
    )?;

    summarize(
        &format!("Swift Test Results ({binding})"),
        &parse_results(&log),
        &Runner {
            name: "xcodebuild",
            status,
            log: &log,
            detail: is_failure_detail,
        },
    )
}

/// UniFFI's generated Swift lives next to the framework; copy it into the test
/// package's source set.
fn copy_uniffi_sources(swift: &Path, tests: &Path) -> Result<()> {
    let dest = tests.join(format!("Sources/{SWIFT_MODULE}"));
    create_dir(&dest)?;
    let source = swift.join(format!("Sources/{SWIFT_MODULE}/{UNIFFI_LIB}.swift"));
    ensure_file(&source, "generated Swift bindings")?;
    copy_file(&source, &dest.join(format!("{UNIFFI_LIB}.swift")))
}

/// `SwiftPM` requires target paths inside the package directory, so the
/// generated sources and the framework are copied in rather than referenced
/// across the repo.
fn copy_boltffi_sources(framework: &Path, tests: &Path) -> Result<()> {
    let packed = boltffi::crate_dir().join("dist/apple");
    let packed_sources = packed.join("Sources");
    let dest_sources = tests.join("Sources");
    let dest_framework = tests.join(format!("{SWIFT_MODULE}.xcframework"));

    remove_dir(&dest_sources)?;
    remove_dir(&dest_framework)?;
    create_dir(&dest_sources)?;

    // `pack apple` nests the generated Swift under `Sources/BoltFFI`, so copy
    // the tree rather than flattening it — the `Package.swift` in the test
    // package points its target at `Sources`. `cp -R` also handles the
    // xcframework below, where only `cp` preserves the symlinks and attributes
    // that make the bundle loadable.
    run(Command::new("cp")
        .arg("-R")
        .arg(packed_sources.join("."))
        .arg(&dest_sources))?;

    if find_by_extension(&dest_sources, "swift")?.is_empty() {
        bail!(
            "No generated Swift under {} — layout changed?\n{}",
            packed_sources.display(),
            dir_listing(&packed, 3)
        );
    }

    run(Command::new("cp")
        .arg("-R")
        .arg(framework)
        .arg(&dest_framework))
}

/// Pick an available iPhone simulator, preferring recent models.
///
/// Falls through to any iPhone when no preferred model yields a UDID, rather
/// than failing on a preferred line that happens not to carry one.
fn pick_simulator() -> Result<String> {
    let listing = capture(Command::new("xcrun").args(["simctl", "list", "devices", "available"]))?;
    let preferred = listing.lines().filter(|line| {
        ["iPhone 15", "iPhone 16", "iPhone 17"]
            .iter()
            .any(|model| line.contains(model))
    });
    let any_iphone = listing.lines().filter(|line| line.contains("iPhone"));
    preferred
        .chain(any_iphone)
        .find_map(simulator_udid)
        .map(str::to_owned)
        .context("No iPhone simulator with a parseable UDID is available")
}

/// Extract the UDID from a `simctl list devices` line, which ends in
/// `(<UDID>) (<state>)`.
fn simulator_udid(line: &str) -> Option<&str> {
    line.split(['(', ')'])
        .map(str::trim)
        .find(|token| is_udid(token))
}

fn is_udid(token: &str) -> bool {
    token.len() == 36
        && token
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() || byte == b'-')
        && token.bytes().filter(|byte| *byte == b'-').count() == 4
}

/// CI runners benefit from a clean simulator state — previously leaked
/// simulators have been observed to hang the test runner. Skipped locally for speed.
fn clean_simulator(id: &str) -> Result<()> {
    println!("Cleaning simulator state (CI)...");
    // `simctl shutdown` exits non-zero when the simulator is already shut down.
    let _ = Command::new("xcrun")
        .args(["simctl", "shutdown", id])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    run(Command::new("xcrun").args(["simctl", "erase", id]))?;
    run(Command::new("xcrun").args(["simctl", "boot", id]))?;
    run(Command::new("xcrun").args(["simctl", "bootstatus", id, "-b"]))
}

/// Whitelist filter: show only the xcodebuild lines that matter (test progress,
/// the final summary, failures, and compiler diagnostics). Hides build-phase
/// chatter such as `CompileSwiftSources` and `CodeSign`.
fn is_output_interesting(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("Test Suite")
        || trimmed.starts_with("Test Case")
        || trimmed.starts_with("Executed ")
        || trimmed.starts_with("** TEST ")
        || trimmed.starts_with("** BUILD FAILED")
        || line.contains(": error:")
        || line.contains(": warning:")
}

/// Selects the xcodebuild log lines worth showing when a test case fails.
fn is_failure_detail(line: &str) -> bool {
    line.contains("error:") || line.contains("failed:")
}

/// Parse xcodebuild's log into one entry per reported test case.
fn parse_results(log: &str) -> Vec<TestCase> {
    log.lines().filter_map(parse_result_line).collect()
}

/// Parse one test-result line. Both the legacy XCTest form
/// `Test Case '-[Module.Suite testName]' passed (0.001 seconds).` and the newer
/// `Test case 'Suite.testName()' passed on 'Clone 1 of iPhone 16' (0.001 seconds).`
/// are accepted, so selecting a different Xcode does not silently empty the report.
fn parse_result_line(line: &str) -> Option<TestCase> {
    let trimmed = line.trim_start();
    if !(trimmed.starts_with("Test Case ") || trimmed.starts_with("Test case ")) {
        return None;
    }
    let outcome = if line.contains(" passed ") {
        Outcome::Passed
    } else if line.contains(" failed ") {
        Outcome::Failed
    } else if line.contains(" skipped ") {
        Outcome::Skipped
    } else {
        // `started`, and anything else without a verdict.
        return None;
    };
    Some(TestCase {
        name: case_name(trimmed)?,
        duration: duration(line),
        outcome,
    })
}

/// The test name, from either `'-[Module.Suite testName]'` or `'Suite.testName()'`.
fn case_name(trimmed: &str) -> Option<String> {
    let (_, rest) = trimmed.split_once('\'')?;
    let (quoted, _) = rest.split_once('\'')?;
    let name = quoted
        .strip_prefix("-[")
        .and_then(|inner| inner.strip_suffix(']'))
        .unwrap_or(quoted);
    Some(name.to_owned())
}

/// The parenthesised duration, e.g. `0.001 seconds`. Empty when absent.
fn duration(line: &str) -> String {
    line.rsplit_once('(')
        .and_then(|(_, tail)| tail.split_once(')'))
        .map_or_else(String::new, |(value, _)| value.to_owned())
}

#[cfg(test)]
mod tests {
    use super::{Outcome, is_failure_detail, parse_results, simulator_udid};

    #[test]
    fn reads_the_udid_from_a_simctl_line() {
        let line = "    iPhone 16 (0A1B2C3D-4E5F-6789-ABCD-0123456789AB) (Shutdown)";
        assert_eq!(
            simulator_udid(line),
            Some("0A1B2C3D-4E5F-6789-ABCD-0123456789AB")
        );
    }

    #[test]
    fn returns_none_without_a_udid() {
        assert_eq!(simulator_udid("-- iOS 18.2 --"), None);
    }

    #[test]
    fn parses_passed_and_failed_cases() {
        let log = "Test Case '-[SiegelTests.SiegelGuardTests testFoo]' passed (0.001 seconds).\n\
                   Test Case '-[SiegelTests.SiegelGuardTests testBar]' started.\n\
                   Test Case '-[SiegelTests.SiegelGuardTests testBar]' failed (0.250 seconds).\n\
                   Executed 2 tests, with 1 failure";
        let cases = parse_results(log);
        assert_eq!(cases.len(), 2);
        assert_eq!(cases[0].name, "SiegelTests.SiegelGuardTests testFoo");
        assert_eq!(cases[0].duration, "0.001 seconds");
        assert_eq!(cases[0].outcome, Outcome::Passed);
        assert_eq!(cases[1].outcome, Outcome::Failed);
    }

    #[test]
    fn parses_the_newer_lowercase_form_without_bracket_syntax() {
        let log = concat!(
            "Test case 'SiegelGuardTests.testFoo()' passed ",
            "on 'Clone 1 of iPhone 16' (0.002 seconds)."
        );
        let cases = parse_results(log);
        assert_eq!(cases.len(), 1);
        assert_eq!(cases[0].name, "SiegelGuardTests.testFoo()");
        assert_eq!(cases[0].duration, "0.002 seconds");
        assert_eq!(cases[0].outcome, Outcome::Passed);
    }

    #[test]
    fn skipped_cases_are_not_reported_as_passes() {
        let log = "Test Case '-[SiegelTests.SiegelGuardTests testFoo]' skipped (0.001 seconds).";
        assert_eq!(parse_results(log)[0].outcome, Outcome::Skipped);
    }

    #[test]
    fn ignores_lines_without_a_verdict() {
        let log = "Test Case '-[SiegelTests.SiegelGuardTests testFoo]' started.";
        assert!(parse_results(log).is_empty());
    }

    #[test]
    fn failure_detail_selects_compiler_and_test_errors() {
        assert!(is_failure_detail(
            "SiegelSessionTests.swift:12: error: boom"
        ));
        assert!(is_failure_detail("XCTAssertEqual failed: (1) is not (2)"));
        assert!(!is_failure_detail("Test Suite 'All tests' started"));
    }
}
