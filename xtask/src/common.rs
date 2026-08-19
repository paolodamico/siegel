//! Shared process and filesystem helpers for the build-automation tasks.
//!
//! All paths resolve relative to the workspace root so every command behaves
//! the same whether it is invoked from the root or a sub-directory.

use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};

use anyhow::{Context, Result, bail};

/// The UniFFI bindings are only ever built to drive the integration suites, so
/// they always enable `test-utils`. Production consumers rebuild without it.
pub const UNIFFI_FEATURES: &str = "test-utils";

/// Absolute path to the workspace root.
pub fn project_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask Cargo.toml lives one directory below the workspace root")
        .to_path_buf()
}

/// Fail unless the workspace root baked in at compile time still looks like this
/// checkout
pub fn verify_project_root() -> Result<()> {
    let root = project_root();
    if !root.join("Cargo.toml").is_file() {
        bail!(
            "workspace root {} has no Cargo.toml — this xtask binary was built for a \
             different checkout; run `cargo clean -p xtask` and retry",
            root.display()
        );
    }
    Ok(())
}

/// Shared-library extension for the host platform.
pub fn host_lib_extension() -> Result<&'static str> {
    if cfg!(target_os = "macos") {
        Ok("dylib")
    } else if cfg!(target_os = "linux") {
        Ok("so")
    } else {
        bail!("unsupported host OS: only macOS and Linux can build the host bindings")
    }
}

/// Whether `VERBOSE=1` asked for the full output of long-running subprocesses.
pub fn verbose() -> bool {
    std::env::var("VERBOSE").as_deref() == Ok("1")
}

/// Whether we are running inside GitHub Actions or a generic CI environment.
pub fn in_ci() -> bool {
    std::env::var("CI").as_deref() == Ok("true")
        || std::env::var("GITHUB_ACTIONS").as_deref() == Ok("true")
}

/// Whether `name` resolves to an executable on `PATH`.
pub fn has_command(name: &str) -> bool {
    Command::new("which")
        .arg(name)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Run `cmd` to completion; return an error if it exits non-zero.
pub fn run(cmd: &mut Command) -> Result<()> {
    let pretty = format!("{cmd:?}");
    let status = cmd
        .status()
        .with_context(|| format!("failed to spawn: {pretty}"))?;
    if !status.success() {
        bail!("command failed ({status}): {pretty}");
    }
    Ok(())
}

/// Run `cmd` to completion and return its captured stdout as a UTF-8 string.
pub fn capture(cmd: &mut Command) -> Result<String> {
    let pretty = format!("{cmd:?}");
    let out = cmd
        .output()
        .with_context(|| format!("failed to spawn: {pretty}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let trailer = if stderr.trim().is_empty() {
            String::new()
        } else {
            format!("\n{}", stderr.trim())
        };
        bail!("command failed ({}): {pretty}{trailer}", out.status);
    }
    String::from_utf8(out.stdout).context("non-utf8 command output")
}

/// Run `cmd` and return its merged stdout+stderr regardless of exit status.
///
/// For probes where a non-zero exit is itself information and the output *is*
/// the diagnostic (`xcodebuild -showsdks`, `rustup target list`). Returns `None`
/// only when the command could not be spawned at all.
pub fn capture_lossy(cmd: &mut Command) -> Option<String> {
    let out = cmd.output().ok()?;
    let mut merged = String::from_utf8_lossy(&out.stdout).into_owned();
    merged.push_str(&String::from_utf8_lossy(&out.stderr));
    Some(merged)
}

/// Run `cmd`, capturing the merged stdout+stderr stream into a `String` while
/// live-printing the lines for which `show` returns `true` (or every line under
/// `VERBOSE=1`). The full buffer is returned so callers can post-process the
/// noise that was filtered from the console.
pub fn run_streamed(cmd: &mut Command, show: fn(&str) -> bool) -> Result<(String, ExitStatus)> {
    let pretty = format!("{cmd:?}");
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to spawn: {pretty}"))?;
    let stdout = child.stdout.take().expect("stdout was piped");
    let stderr = child.stderr.take().expect("stderr was piped");

    let (sender, receiver) = std::sync::mpsc::channel::<String>();
    let stderr_sender = sender.clone();
    let stdout_thread = std::thread::spawn(move || forward_lines(stdout, &sender));
    let stderr_thread = std::thread::spawn(move || forward_lines(stderr, &stderr_sender));

    let stream_all = verbose();
    let mut buffer = String::new();
    for line in receiver {
        if stream_all || show(&line) {
            println!("{line}");
        }
        buffer.push_str(&line);
        buffer.push('\n');
    }
    // A panicked forwarder means a silently short log, and the caller derives its
    // verdict from that log — surface it instead of summarising partial output.
    if stdout_thread.join().is_err() || stderr_thread.join().is_err() {
        bail!("a log forwarding thread panicked while running: {pretty}");
    }
    let status = child.wait().context("waiting for the child process")?;
    Ok((buffer, status))
}

/// Forward `stream` to `sink`, one line at a time.
///
/// Splits on bytes and converts lossily rather than using `BufRead::lines`,
/// which yields an error for non-UTF-8 input: that would end the forwarder,
/// truncate the captured log, and close the pipe under a still-running child.
fn forward_lines<R: std::io::Read>(stream: R, sink: &std::sync::mpsc::Sender<String>) {
    for chunk in std::io::BufReader::new(stream).split(b'\n') {
        let Ok(bytes) = chunk else { break };
        let line = String::from_utf8_lossy(&bytes);
        if sink.send(line.trim_end_matches('\r').to_owned()).is_err() {
            break;
        }
    }
}

/// Remove `path` (recursively) if it exists.
pub fn remove_dir(path: &Path) -> Result<()> {
    if path.exists() {
        std::fs::remove_dir_all(path).with_context(|| format!("removing {}", path.display()))?;
    }
    Ok(())
}

/// Remove `path` (recursively) if it exists, then recreate it as an empty directory.
pub fn reset_dir(path: &Path) -> Result<()> {
    remove_dir(path)?;
    create_dir(path)
}

/// Recursively copy the contents of `src` into `dst`, creating `dst` as needed.
pub fn copy_dir_all(src: &Path, dst: &Path) -> Result<()> {
    create_dir(dst)?;
    for entry in std::fs::read_dir(src).with_context(|| format!("reading {}", src.display()))? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_all(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)
                .with_context(|| format!("copying {} to {}", from.display(), to.display()))?;
        }
    }
    Ok(())
}

/// Copy a single file, reporting both paths on failure.
pub fn copy_file(from: &Path, to: &Path) -> Result<()> {
    std::fs::copy(from, to)
        .with_context(|| format!("copying {} to {}", from.display(), to.display()))?;
    Ok(())
}

/// Move a single file, reporting both paths on failure.
pub fn move_file(from: &Path, to: &Path) -> Result<()> {
    std::fs::rename(from, to)
        .with_context(|| format!("moving {} to {}", from.display(), to.display()))?;
    Ok(())
}

/// Create `path` and any missing parents.
pub fn create_dir(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path).with_context(|| format!("creating {}", path.display()))?;
    Ok(())
}

/// Fail unless `path` is an existing file. `what` names it in the error, e.g.
/// `"cdylib"` produces `cdylib missing at /path/to/lib.so`.
pub fn ensure_file(path: &Path, what: &str) -> Result<()> {
    if !path.is_file() {
        bail!("{what} missing at {}", path.display());
    }
    Ok(())
}

/// Create a fresh, private temporary directory named after `prefix`.
///
/// Equivalent to `mktemp -d`: the directory is created exclusively (so a
/// pre-existing path or symlink is a hard error rather than a hijack) and, on
/// Unix, is only accessible to the current user. That matters because the
/// bootstrapped Gradle distribution is unpacked here and then executed.
pub fn create_temp_dir(prefix: &str) -> Result<PathBuf> {
    let base = std::env::temp_dir();
    for attempt in 0..64 {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.subsec_nanos());
        let candidate = base.join(format!("{prefix}-{}-{nanos}-{attempt}", std::process::id()));
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt as _;
            builder.mode(0o700);
        }
        match builder.create(&candidate) {
            Ok(()) => return Ok(candidate),
            // The name collided; fall through and try the next candidate.
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(error).with_context(|| format!("creating {}", candidate.display()));
            }
        }
    }
    bail!(
        "could not create a temporary directory under {}",
        base.display()
    )
}

/// Recursively collect every file under `dir` for which `keep` returns `true`.
///
/// Returns an empty vector when `dir` does not exist.
pub fn find_files(dir: &Path, keep: &dyn Fn(&Path) -> bool) -> Result<Vec<PathBuf>> {
    let mut found = Vec::new();
    collect_files(dir, keep, &mut found)?;
    found.sort();
    Ok(found)
}

/// Recursively collect every file under `dir` whose extension is `extension`.
pub fn find_by_extension(dir: &Path, extension: &str) -> Result<Vec<PathBuf>> {
    find_files(dir, &|path| {
        path.extension() == Some(std::ffi::OsStr::new(extension))
    })
}

fn collect_files(dir: &Path, keep: &dyn Fn(&Path) -> bool, out: &mut Vec<PathBuf>) -> Result<()> {
    if !dir.is_dir() {
        return Ok(());
    }
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            collect_files(&path, keep, out)?;
        } else if keep(&path) {
            out.push(path);
        }
    }
    Ok(())
}

/// Render the entries of `dir`, up to `max_depth` levels deep, as indented
/// relative paths — one per line.
///
/// Best-effort by design: every caller uses this for human-facing context
/// (either "here is what the task produced" or a diagnostic attached to an
/// error), so an unreadable directory reports itself rather than masking the
/// failure the caller is already describing.
pub fn dir_listing(dir: &Path, max_depth: usize) -> String {
    match list_entries(dir, max_depth) {
        Ok(entries) => entries
            .iter()
            .map(|path| format!("  {}", path.strip_prefix(dir).unwrap_or(path).display()))
            .collect::<Vec<_>>()
            .join("\n"),
        Err(error) => format!("  <could not read {}: {error}>", dir.display()),
    }
}

fn list_entries(dir: &Path, max_depth: usize) -> Result<Vec<PathBuf>> {
    if max_depth == 0 {
        return Ok(Vec::new());
    }
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        let nested = if entry.file_type()?.is_dir() {
            list_entries(&path, max_depth - 1)?
        } else {
            Vec::new()
        };
        entries.push(path);
        entries.extend(nested);
    }
    entries.sort();
    Ok(entries)
}

/// Run `uniffi-bindgen` against `library` to emit `language` sources into `out_dir`.
pub fn uniffi_generate(root: &Path, library: &Path, language: &str, out_dir: &Path) -> Result<()> {
    run(Command::new("cargo")
        .current_dir(root)
        .args(["run", "-p", "uniffi-bindgen", "--", "generate"])
        .arg(library)
        .args([
            "--library",
            "--language",
            language,
            "--no-format",
            "--out-dir",
        ])
        .arg(out_dir))
}

#[cfg(test)]
mod tests {
    use super::list_entries;

    #[test]
    fn lists_entries_depth_first_and_honours_max_depth() {
        let root = std::env::temp_dir().join("xtask_common_list_entries");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("jniLibs/arm64-v8a")).unwrap();
        std::fs::create_dir_all(root.join("kotlin")).unwrap();
        std::fs::write(root.join("jniLibs/arm64-v8a/libsiegel.so"), "").unwrap();

        let relative = |depth| {
            list_entries(&root, depth)
                .unwrap()
                .iter()
                .map(|path| path.strip_prefix(&root).unwrap().display().to_string())
                .collect::<Vec<_>>()
        };

        assert_eq!(relative(1), ["jniLibs", "kotlin"]);
        assert_eq!(relative(2), ["jniLibs", "jniLibs/arm64-v8a", "kotlin"]);
        assert_eq!(
            relative(3),
            [
                "jniLibs",
                "jniLibs/arm64-v8a",
                "jniLibs/arm64-v8a/libsiegel.so",
                "kotlin"
            ]
        );
        std::fs::remove_dir_all(&root).unwrap();
    }
}
