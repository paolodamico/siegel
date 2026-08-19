//! Swift bindings, packaging, and the foreign XCTest suite.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

use crate::Binding;
use crate::boltffi;
use crate::common::{
    capture, copy_file, dir_listing, find_files, in_ci, move_file, project_root, remove_dir, run,
    run_streamed, uniffi_generate,
};
use crate::report::{Runner, TestCase, summarize};

/// Consumer-facing Swift module name.
const SWIFT_MODULE: &str = "Siegel";
/// `siegel-uniffi`'s `staticlib`/`cdylib` basename.
const UNIFFI_LIB: &str = "siegel_uniffi";
/// Kept in sync with `deployment_target` in `siegel-boltffi/boltffi.toml`.
const IOS_DEPLOYMENT_TARGET: &str = "13.0";
const IOS_RUSTFLAGS: &str = "-C link-arg=-Wl,-application_extension";
/// The UniFFI bindings always build with `test-utils`: they exist to drive the
/// integration suite. Production consumers rebuild without it.
const UNIFFI_FEATURES: &str = "test-utils";

/// Absolute path to the `swift/` directory inside the workspace.
fn swift_dir() -> PathBuf {
    project_root().join("swift")
}

/// Cross-compile for iOS, generate Swift bindings, and assemble an XCFramework.
pub fn build(
    binding: Binding,
    sim_only: bool,
    test_utils: bool,
    out_dir: Option<&Path>,
) -> Result<()> {
    match binding {
        Binding::Uniffi => {
            if test_utils {
                bail!("--test-utils is not applicable: the UniFFI build always enables it");
            }
            build_uniffi(sim_only, out_dir)
        }
        Binding::Boltffi => {
            if out_dir.is_some() {
                bail!("--out-dir is not applicable: boltffi's layout is set by boltffi.toml");
            }
            build_boltffi(sim_only, test_utils)
        }
    }
}

/// Build `Siegel.xcframework` from `siegel-uniffi`.
fn build_uniffi(sim_only: bool, out_dir: Option<&Path>) -> Result<()> {
    let root = project_root();
    let swift = swift_dir();
    let out_dir = match out_dir {
        Some(path) if path.is_absolute() => path.to_path_buf(),
        Some(path) => swift.join(path),
        None => swift.clone(),
    };

    let ios_build = swift.join("ios_build");
    let bindings = ios_build.join("bindings");
    let headers = ios_build.join(format!("Headers/{SWIFT_MODULE}"));
    let sources = out_dir.join(format!("Sources/{SWIFT_MODULE}"));
    let framework = out_dir.join(format!("{SWIFT_MODULE}.xcframework"));

    println!(
        "Building {SWIFT_MODULE}.xcframework to {}",
        framework.display()
    );
    remove_dir(&ios_build)?;
    remove_dir(&framework)?;
    for dir in [&bindings, &headers, &sources] {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
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
    println!("Compiling Rust for iOS targets...");
    for target in [
        "aarch64-apple-ios-sim",
        "aarch64-apple-ios",
        "x86_64-apple-ios",
    ] {
        cargo_ios_build(root, target)?;
    }

    println!("Building universal simulator binary...");
    std::fs::create_dir_all(universal_dir)
        .with_context(|| format!("creating {}", universal_dir.display()))?;
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

    let mut args = vec![
        "pack".to_owned(),
        "apple".to_owned(),
        "--release".to_owned(),
    ];
    if test_utils {
        // Test-only helpers are off by default. They export `sha256_consume`
        // and `unsafe_test_only_siegel_front_guard_bolt`.
        args.extend(["--cargo-arg", "--features", "--cargo-arg", "test-utils"].map(str::to_owned));
    }
    if sim_only {
        args.extend(["--overlay", "boltffi.ci.toml"].map(str::to_owned));
    }

    println!("Running: boltffi {}", args.join(" "));
    run(Command::new("boltffi").current_dir(&crate_dir).args(&args))?;

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

    let sdks = capture(Command::new("xcodebuild").arg("-showsdks"))?;
    if !sdks.contains("iphonesimulator") {
        bail!("No iOS Simulator SDK installed. Available SDKs:\n{sdks}");
    }

    println!(
        "Step 1: building Swift bindings ({binding}, sim-only — tests don't need the device/Intel slices)"
    );
    match binding {
        Binding::Uniffi => build_uniffi(true, None)?,
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
    std::fs::create_dir_all(&dest).with_context(|| format!("creating {}", dest.display()))?;
    let source = swift.join(format!("Sources/{SWIFT_MODULE}/{UNIFFI_LIB}.swift"));
    if !source.is_file() {
        bail!("generated bindings missing at {}", source.display());
    }
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
    std::fs::create_dir_all(&dest_sources)
        .with_context(|| format!("creating {}", dest_sources.display()))?;

    // `pack apple` nests the generated Swift under `Sources/BoltFFI`, so copy
    // the tree rather than flattening it — the `Package.swift` in the test
    // package points its target at `Sources`. `cp -R` also handles the
    // xcframework below, where only `cp` preserves the symlinks and attributes
    // that make the bundle loadable.
    run(Command::new("cp")
        .arg("-R")
        .arg(packed_sources.join("."))
        .arg(&dest_sources))?;

    let copied = find_files(&dest_sources, &|path| {
        path.extension() == Some(OsStr::new("swift"))
    })?;
    if copied.is_empty() {
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
fn pick_simulator() -> Result<String> {
    let listing = capture(Command::new("xcrun").args(["simctl", "list", "devices", "available"]))?;
    let preferred = listing.lines().find(|line| {
        ["iPhone 15", "iPhone 16", "iPhone 17"]
            .iter()
            .any(|model| line.contains(model))
    });
    let line = preferred
        .or_else(|| listing.lines().find(|line| line.contains("iPhone")))
        .context("No iPhone simulator available")?;
    simulator_udid(line)
        .map(str::to_owned)
        .with_context(|| format!("could not parse a simulator UDID from: {line}"))
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

/// Parse xcodebuild's log into one entry per executed test case.
fn parse_results(log: &str) -> Vec<TestCase> {
    log.lines().filter_map(parse_result_line).collect()
}

/// Parse a line of the form
/// `Test Case '-[Module.Suite testName]' passed (0.001 seconds).`
fn parse_result_line(line: &str) -> Option<TestCase> {
    if !line.trim_start().starts_with("Test Case ") {
        return None;
    }
    let passed = if line.contains(" passed ") {
        true
    } else if line.contains(" failed ") {
        false
    } else {
        return None;
    };
    let (_, rest) = line.split_once("'-[")?;
    let (name, rest) = rest.split_once("]'")?;
    let duration = rest
        .split_once('(')
        .and_then(|(_, tail)| tail.split_once(')'))
        .map_or_else(String::new, |(value, _)| value.to_owned());
    Some(TestCase {
        name: name.to_owned(),
        duration,
        passed,
    })
}

#[cfg(test)]
mod tests {
    use super::{is_failure_detail, parse_results, simulator_udid};

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
        assert!(cases[0].passed);
        assert!(!cases[1].passed);
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
