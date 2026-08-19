//! Shared helpers for driving the `boltffi` CLI.

use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::common::{capture, project_root};

/// Absolute path to the `siegel-boltffi` crate directory.
pub fn crate_dir() -> PathBuf {
    project_root().join("siegel-boltffi")
}

/// Absolute path to the hand-written JNI fill path.
///
/// `boltffi` generates `Siegel.kt`, which exposes the session class but no way
/// to fill it; this file is the fill path and ships alongside the generated
/// sources so consumers get a complete package.
pub fn handwritten_kotlin() -> PathBuf {
    crate_dir().join("kotlin/dev/siegel/SiegelNative.kt")
}

/// Verify that the installed `boltffi` CLI matches the `boltffi` version cargo resolved.
///
/// The macro emits the FFI symbols and the CLI generates the glue that calls
/// them, with no compatibility check between the two: a mismatch fails at
/// runtime with `undefined symbol` rather than at link time.
pub fn ensure_cli() -> Result<()> {
    let output = capture(Command::new("boltffi").arg("--version")).context(
        "could not run the boltffi CLI — install it with `cargo install boltffi_cli --locked`",
    )?;
    let installed = output
        .split_whitespace()
        .next_back()
        .context("could not parse the output of `boltffi --version`")?;

    let resolved = resolved_version()?;
    if installed != resolved {
        bail!(
            "boltffi CLI is {installed} but cargo resolved boltffi {resolved}.\n\
             Install the matching CLI: cargo install boltffi_cli --version {resolved} --locked"
        );
    }
    Ok(())
}

/// The `boltffi` version cargo resolved for this workspace.
fn resolved_version() -> Result<String> {
    let root = project_root();
    // `cargo pkgid` needs a resolved dependency graph.
    capture(
        Command::new("cargo")
            .current_dir(&root)
            .args(["metadata", "--format-version", "1"]),
    )
    .context("`cargo metadata` failed, so the boltffi version could not be resolved")?;

    let pkgid = capture(
        Command::new("cargo")
            .current_dir(&root)
            .args(["pkgid", "boltffi"]),
    )
    .context(
        "cargo could not resolve a unique boltffi version — check `cargo tree -p siegel-boltffi -i boltffi`",
    )?;
    Ok(package_version(pkgid.trim()).to_owned())
}

/// Extract the version from a `cargo pkgid` spec, which ends in `#<version>` or
/// `@<version>` depending on the source.
fn package_version(pkgid: &str) -> &str {
    pkgid
        .rsplit(['@', '#'])
        .next()
        .expect("rsplit always yields at least one element")
}

#[cfg(test)]
mod tests {
    use super::package_version;

    #[test]
    fn parses_registry_and_path_pkgids() {
        assert_eq!(
            package_version("registry+https://github.com/rust-lang/crates.io-index#boltffi@0.29.1"),
            "0.29.1"
        );
        assert_eq!(package_version("path+file:///tmp/boltffi#0.29.1"), "0.29.1");
        assert_eq!(package_version("boltffi@0.29.1"), "0.29.1");
    }
}
