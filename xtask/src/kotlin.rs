//! Kotlin/JVM and Android binding tasks.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::Binding;
use crate::boltffi;
use crate::common::{
    copy_dir_all, copy_file, dir_listing, find_files, host_lib_extension, project_root, remove_dir,
    reset_dir, run, run_streamed, uniffi_generate,
};
use crate::gradle;
use crate::report::{Runner, summarize};

/// The UniFFI bindings always build with `test-utils`: they exist to drive the
/// integration suite. Production consumers rebuild without it.
const UNIFFI_FEATURES: &str = "test-utils";

/// Absolute path to the `kotlin/` directory inside the workspace.
fn kotlin_dir() -> PathBuf {
    project_root().join("kotlin")
}

/// Build the host cdylib and Kotlin bindings for `binding`.
pub fn build(binding: Binding, test_utils: bool) -> Result<()> {
    match binding {
        Binding::Uniffi => {
            if test_utils {
                bail!("--test-utils is not applicable: the UniFFI build always enables it");
            }
            build_uniffi()
        }
        Binding::Boltffi => build_boltffi(test_utils),
    }
}

/// Build `siegel-uniffi` as a host cdylib and generate Kotlin/JNA bindings.
///
/// JNA loads the cdylib at runtime from `kotlin/libs/`.
fn build_uniffi() -> Result<()> {
    let root = project_root();
    let kotlin = kotlin_dir();
    let libs = kotlin.join("libs");
    // Generated bindings live alongside the test sources so they compile in the
    // same source set without extra Gradle wiring.
    let bindings = kotlin.join("siegel-tests/src/test/kotlin");

    remove_dir(&bindings.join("uniffi"))?;
    reset_dir(&libs)?;

    println!("Building host cdylib (siegel-uniffi, features: {UNIFFI_FEATURES})...");
    run(Command::new("cargo").current_dir(&root).args([
        "build",
        "--package",
        "siegel-uniffi",
        "--features",
        UNIFFI_FEATURES,
        "--release",
    ]))?;

    let lib = root.join(format!(
        "target/release/libsiegel_uniffi.{}",
        host_lib_extension()?
    ));
    if !lib.is_file() {
        bail!("cdylib missing at {}", lib.display());
    }
    let name = lib.file_name().expect("the library path names a file");
    copy_file(&lib, &libs.join(name))?;
    println!("Copied {} to {}", name.to_string_lossy(), libs.display());

    println!("Generating Kotlin bindings...");
    uniffi_generate(&root, &lib, "kotlin", &bindings)?;
    println!(
        "Kotlin bindings written to {}/uniffi/siegel_uniffi/",
        bindings.display()
    );
    Ok(())
}

/// Build `siegel-boltffi` as a host cdylib, generate Kotlin bindings, and
/// compile the JNI glue for the host JVM.
fn build_boltffi(test_utils: bool) -> Result<()> {
    let root = project_root();
    let kotlin = kotlin_dir();
    let crate_dir = boltffi::crate_dir();
    let libs = kotlin.join("boltffi-libs");
    let generated = kotlin.join("siegel-boltffi-tests/src/main/kotlin/generated");
    // Test-only helpers are off by default. They export `sha256_consume` and
    // `unsafe_test_only_siegel_front_guard_bolt`.
    let features = if test_utils { "jvm,test-utils" } else { "jvm" };

    boltffi::ensure_cli()?;
    let java_home = gradle::java_home_with_jni()?;
    let host = HostJni::detect()?;

    reset_dir(&libs)?;
    reset_dir(&generated)?;

    println!("Step 1: building host cdylib (siegel-boltffi, features: {features})");
    run(Command::new("cargo")
        .current_dir(&root)
        .args([
            "build",
            "--package",
            "siegel-boltffi",
            "--features",
            features,
            "--release",
        ])
        .envs(binding_expansion_env(&crate_dir, features)))?;

    let rust_lib = root.join(format!("target/release/libsiegel_boltffi.{}", host.lib_ext));
    if !rust_lib.is_file() {
        bail!("cdylib missing at {}", rust_lib.display());
    }
    copy_file(
        &rust_lib,
        &libs.join(rust_lib.file_name().expect("the library path names a file")),
    )?;
    println!("  -> {}", rust_lib.display());

    println!("Step 2: generating Kotlin bindings + JNI glue");
    // Codegen expands the crate, so the feature set must match the cdylib built
    // in step 1
    run(Command::new("boltffi")
        .current_dir(&crate_dir)
        .args([
            "generate",
            "kotlin",
            "--cargo-arg",
            "--features",
            "--cargo-arg",
            features,
        ])
        .envs(binding_expansion_env(&crate_dir, features)))?;

    let gen_root = crate_dir.join("dist/android/kotlin");
    let glue = gen_root.join("jni/jni_glue.c");
    if !glue.is_file() {
        bail!("expected JNI glue at {}", glue.display());
    }
    copy_dir_all(&gen_root.join("dev"), &generated.join("dev"))?;

    // Ship the hand-written fill path alongside the generated sources so
    // `dist/android` is a complete, copyable package, and mirror it into the
    // test module's source set.
    let handwritten = boltffi::handwritten_kotlin();
    if !handwritten.is_file() {
        bail!("missing {}", handwritten.display());
    }
    copy_file(&handwritten, &gen_root.join("dev/siegel/SiegelNative.kt"))?;
    copy_file(&handwritten, &generated.join("dev/siegel/SiegelNative.kt"))?;
    println!(
        "  -> {} Kotlin file(s)",
        find_files(&generated, &|path| path.extension()
            == Some(OsStr::new("kt")))?
        .len()
    );

    println!("Step 3: compiling JNI glue for the host JVM");
    compile_jni_glue(&host, &java_home, &gen_root, &libs)?;

    println!();
    println!("Artifacts in {}:", libs.display());
    println!("{}", dir_listing(&libs, 1));
    Ok(())
}

/// The `BOLTFFI_BINDING_EXPANSION*` environment is load-bearing, not incidental.
///
/// `boltffi`'s `#[export]` macro has two expansions. Without this environment it
/// takes the legacy path and emits short symbols (`boltffi_siegel_session_new`).
/// With it, the macro scans the whole package once and emits the current ABI.
///
/// `features` must match the value passed to `boltffi generate`: the macro scan
/// is cfg-sensitive, so a mismatch silently drops exports.
fn binding_expansion_env(crate_dir: &Path, features: &str) -> Vec<(String, String)> {
    vec![
        ("BOLTFFI_BINDING_EXPANSION".to_owned(), "1".to_owned()),
        (
            "BOLTFFI_BINDING_EXPANSION_ROOT".to_owned(),
            crate_dir.display().to_string(),
        ),
        (
            "BOLTFFI_BINDING_EXPANSION_SOURCE".to_owned(),
            crate_dir.join("src/lib.rs").display().to_string(),
        ),
        (
            "BOLTFFI_BINDING_EXPANSION_SURFACE".to_owned(),
            "native".to_owned(),
        ),
        (
            "BOLTFFI_BINDING_METADATA_FEATURES".to_owned(),
            features.to_owned(),
        ),
    ]
}

/// Host-specific pieces of the JNI build: library suffix, the platform
/// sub-directory holding `jni_md.h`, and the runtime search-path token.
struct HostJni {
    lib_ext: &'static str,
    jni_md: &'static str,
    rpath: &'static str,
}

impl HostJni {
    fn detect() -> Result<Self> {
        let lib_ext = host_lib_extension()?;
        let (jni_md, rpath) = if cfg!(target_os = "macos") {
            ("darwin", "@loader_path")
        } else {
            ("linux", "$ORIGIN")
        };
        Ok(Self {
            lib_ext,
            jni_md,
            rpath,
        })
    }
}

/// Link the generated JNI glue against the Rust cdylib.
///
/// The generated Kotlin loads `siegel_boltffi_jni` first, then falls back to
/// `siegel_boltffi`; the glue supplies the exported `boltffi_*` symbols.
fn compile_jni_glue(host: &HostJni, java_home: &Path, gen_root: &Path, libs: &Path) -> Result<()> {
    let cc = std::env::var("CC").unwrap_or_else(|_| "cc".to_owned());
    run(Command::new(cc)
        .args(["-shared", "-fPIC", "-O2"])
        .arg("-I")
        .arg(java_home.join("include"))
        .arg("-I")
        .arg(java_home.join("include").join(host.jni_md))
        .arg("-I")
        .arg(gen_root.join("jni"))
        .arg(gen_root.join("jni/jni_glue.c"))
        .arg("-L")
        .arg(libs)
        .arg("-lsiegel_boltffi")
        .arg(format!("-Wl,-rpath,{}", host.rpath))
        .arg("-o")
        .arg(libs.join(format!("libsiegel_boltffi_jni.{}", host.lib_ext))))
}

/// Build the Android distribution: `jniLibs` for every ABI, the generated Kotlin
/// bindings, and the hand-written JNI fill path.
///
/// `boltffi pack android` alone is not sufficient: it emits the generated
/// `Siegel.kt`, which exposes the session class but no way to fill it.
pub fn android(extra_args: &[String]) -> Result<()> {
    let crate_dir = boltffi::crate_dir();
    let handwritten = boltffi::handwritten_kotlin();

    boltffi::ensure_cli()?;
    if !handwritten.is_file() {
        bail!("missing {}", handwritten.display());
    }
    ensure_android_ndk()?;

    let dist = crate_dir.join("dist");
    let android_dist = dist.join("android");
    remove_dir(&android_dist)?;

    println!("Step 1: boltffi pack android --release");
    run(Command::new("boltffi")
        .current_dir(&crate_dir)
        .args([
            "pack",
            "android",
            "--release",
            "--cargo-arg",
            "--features",
            "--cargo-arg",
            "jvm",
        ])
        .args(extra_args))?;

    println!("Step 2: adding the hand-written JNI fill path");
    // Locate the generated bindings rather than assuming the configured output
    // path, so this keeps working if `boltffi.toml`'s layout changes.
    let generated = find_files(&dist, &|path| {
        path.file_name() == Some(OsStr::new("Siegel.kt"))
            && path.to_string_lossy().contains("dev/siegel/")
    })?
    .into_iter()
    .next()
    .with_context(|| {
        format!(
            "could not find a generated Siegel.kt under {}",
            dist.display()
        )
    })?;
    let package_dir = generated
        .parent()
        .expect("the generated file lives in a package directory");
    copy_file(&handwritten, &package_dir.join("SiegelNative.kt"))?;
    println!("  -> {}/SiegelNative.kt", package_dir.display());

    println!("Step 3: checking the packed library name matches the generated loader");
    check_loader_library(&generated, &android_dist)?;

    println!();
    println!("Android artifacts under {}:", android_dist.display());
    println!("{}", dir_listing(&android_dist, 2));
    Ok(())
}

/// Fail with an actionable message rather than whatever a missing linker
/// produces three layers down.
fn ensure_android_ndk() -> Result<()> {
    let direct = ["ANDROID_NDK_HOME", "ANDROID_NDK_ROOT"]
        .iter()
        .any(|key| std::env::var_os(key).is_some_and(|value| !value.is_empty()));
    let nested = ["ANDROID_HOME", "ANDROID_SDK_ROOT"].iter().any(|key| {
        std::env::var_os(key).is_some_and(|value| Path::new(&value).join("ndk").is_dir())
    });
    if direct || nested {
        return Ok(());
    }
    bail!("Android NDK not found. Set ANDROID_NDK_HOME (or install it under $ANDROID_HOME/ndk).")
}

/// The generated loader calls `System.loadLibrary` with the configured package
/// name; if the packed ABI libraries are named differently every consumer hits
/// an `UnsatisfiedLinkError` at class-init rather than a build failure.
fn check_loader_library(generated: &Path, android_dist: &Path) -> Result<()> {
    let source = std::fs::read_to_string(generated)
        .with_context(|| format!("reading {}", generated.display()))?;
    let Some(expected) = loaded_library_name(&source) else {
        return Ok(());
    };
    let wanted = format!("lib{expected}.so");
    let packed = find_files(android_dist, &|path| {
        path.extension() == Some(OsStr::new("so"))
    })?;
    if packed
        .iter()
        .any(|path| path.file_name() == Some(OsStr::new(&wanted)))
    {
        println!("  -> {wanted} present");
        return Ok(());
    }
    let mut names: Vec<_> = packed
        .iter()
        .filter_map(|path| path.file_name())
        .map(|name| format!("  {}", name.to_string_lossy()))
        .collect();
    names.sort();
    names.dedup();
    bail!(
        "No {wanted} in the packed jniLibs, but the generated loader requests exactly that \
         name — consumers would hit UnsatisfiedLinkError at class-init.\nPacked libraries:\n{}",
        names.join("\n")
    );
}

/// Extract the library name from the first `System.loadLibrary("…")` call.
fn loaded_library_name(source: &str) -> Option<&str> {
    let (_, rest) = source.split_once("System.loadLibrary(\"")?;
    rest.split_once('"').map(|(name, _)| name)
}

/// Build the host bindings and run the foreign Kotlin (JUnit) suite via Gradle.
pub fn test(binding: Binding) -> Result<()> {
    let kotlin = kotlin_dir();
    let module = match binding {
        Binding::Uniffi => "siegel-tests",
        Binding::Boltffi => "siegel-boltffi-tests",
    };

    println!("Step 1: building host cdylib + Kotlin bindings ({binding})");
    match binding {
        Binding::Uniffi => build_uniffi()?,
        // The suite drives `sha256_consume` and the guard helpers end to end.
        Binding::Boltffi => build_boltffi(true)?,
    }

    println!("Step 2: preparing Gradle");
    gradle::ensure_wrapper(&kotlin)?;
    let results = kotlin.join(module).join("build/test-results/test");
    remove_dir(&results)?;

    println!("Step 3: running gradle test (set VERBOSE=1 to stream the full log)");
    let mut command = Command::new(kotlin.join("gradlew"));
    command.current_dir(&kotlin).args([
        "--no-daemon",
        &format!("{module}:test"),
        "--info",
        "--continue",
    ]);
    if let Some(home) = gradle::java_home() {
        command.env("JAVA_HOME", home);
    }
    let (log, status) = run_streamed(&mut command, gradle::is_output_interesting)?;

    summarize(
        &format!("Kotlin Test Results ({binding})"),
        &gradle::parse_results(&results)?,
        &Runner {
            name: "gradle",
            status,
            log: &log,
            detail: gradle::is_failure_detail,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::loaded_library_name;

    #[test]
    fn reads_the_loader_library_name() {
        let source = "object Native {\n    init { System.loadLibrary(\"siegel_boltffi\") }\n}";
        assert_eq!(loaded_library_name(source), Some("siegel_boltffi"));
    }

    #[test]
    fn returns_none_without_a_loader_call() {
        assert_eq!(loaded_library_name("object Native {}"), None);
    }
}
