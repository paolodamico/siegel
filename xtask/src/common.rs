//! Shared process and filesystem helpers for the build-automation tasks.
//!
//! All paths resolve relative to the workspace root so every command behaves
//! the same whether it is invoked from the root or a sub-directory.

use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};

use anyhow::{Context, Result, bail};

/// Absolute path to the workspace root.
pub fn project_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask Cargo.toml lives one directory below the workspace root")
        .to_path_buf()
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
        bail!("command failed ({}): {pretty}", out.status);
    }
    String::from_utf8(out.stdout).context("non-utf8 command output")
}

/// Run `cmd`, capturing the merged stdout+stderr stream into a `String` while
/// live-printing the lines for which `show` returns `true` (or every line under
/// `VERBOSE=1`). The full buffer is returned so callers can post-process the
/// noise that was filtered from the console.
pub fn run_streamed(
    cmd: &mut Command,
    show: impl Fn(&str) -> bool,
) -> Result<(String, ExitStatus)> {
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
    let _ = stdout_thread.join();
    let _ = stderr_thread.join();
    let status = child.wait().context("waiting for the child process")?;
    Ok((buffer, status))
}

fn forward_lines<R: std::io::Read>(stream: R, sink: &std::sync::mpsc::Sender<String>) {
    for line in std::io::BufReader::new(stream)
        .lines()
        .map_while(Result::ok)
    {
        if sink.send(line).is_err() {
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
    std::fs::create_dir_all(path).with_context(|| format!("creating {}", path.display()))?;
    Ok(())
}

/// Recursively copy the contents of `src` into `dst`, creating `dst` as needed.
pub fn copy_dir_all(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst).with_context(|| format!("creating {}", dst.display()))?;
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

/// Move a single file. Falls back to copy+delete so the move works across
/// filesystems (`std::fs::rename` fails with `EXDEV`).
pub fn move_file(from: &Path, to: &Path) -> Result<()> {
    if std::fs::rename(from, to).is_ok() {
        return Ok(());
    }
    copy_file(from, to)?;
    std::fs::remove_file(from).with_context(|| format!("removing {}", from.display()))?;
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

/// Print the entries of `dir` up to `max_depth` levels deep, one indented
/// relative path per line, so a task can show what it produced.
pub fn print_dir_entries(dir: &Path, max_depth: usize) -> Result<()> {
    for path in list_entries(dir, max_depth)? {
        println!("  {}", path.strip_prefix(dir).unwrap_or(&path).display());
    }
    Ok(())
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
