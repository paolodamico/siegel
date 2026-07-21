//! Helpers for integration tests.

use std::io;

use sha2::{Digest, Sha256};
use siegel::{Empty, Siegel};

use crate::session::{SessionError, SiegelSession, lookup_session};

/// Consume the session and return SHA-256 of the loaded bytes.
///
/// Mirrors the shape of a real application call.
///
/// # Warning
/// Only for integration tests.
///
/// # Errors
/// - `SessionError::InvalidState` if the session hasn't been filled.
/// - `SessionError::Consumed` if already consumed.
#[uniffi::export]
pub fn sha256_consume(session: &SiegelSession) -> Result<Vec<u8>, SessionError> {
    session.read_once(|bytes| {
        let mut h = Sha256::new();
        h.update(bytes);
        h.finalize().to_vec()
    })
}

/// Sentinel return values for the segfault helpers.
const ERR_INVALID_HANDLE: i32 = -1;
const ERR_FORK_FAILED: i32 = -2;
const ERR_WAITPID_FAILED: i32 = -3;
const ERR_NOT_SIGNALED: i32 = -4;

/// Forks a child that runs `work`, waits for it, and returns the terminating
/// signal (e.g. `SIGSEGV` / `SIGBUS`). Used by foreign integration tests that
/// can't call `fork(2)` directly: Swift on iOS hides it, Kotlin/JVM can't
/// touch process state.
///
/// Returns one of [`ERR_FORK_FAILED`], [`ERR_WAITPID_FAILED`],
/// [`ERR_NOT_SIGNALED`], or the positive signal number on success.
fn fork_and_run(work: impl FnOnce()) -> i32 {
    // SAFETY: fork is safe; the child only calls async-signal-safe APIs
    // (setrlimit, _exit) plus the caller's `work`. The caller is responsible
    // for keeping `work` async-signal-safe enough not to deadlock on a mutex
    // held by another thread in the parent at fork time.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return ERR_FORK_FAILED;
    }
    if pid == 0 {
        // Child: reset SIGSEGV/SIGBUS to the kernel default so test runners
        // that install their own handlers (HotSpot on the JVM, XCTest on
        // Darwin, etc.) don't intercept the fault and abort the child with
        // a different signal (e.g. `SIGABRT` on JVM).
        // SAFETY: signal() is async-signal-safe.
        unsafe {
            libc::signal(libc::SIGSEGV, libc::SIG_DFL);
            libc::signal(libc::SIGBUS, libc::SIG_DFL);
        }

        // Suppress core dumps to keep CrashReporter quiet, then run the
        // unsafe work. If `work` returns instead of crashing, exit cleanly
        // so the parent observes ERR_NOT_SIGNALED.
        let lim = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: `&raw const lim` is a valid pointer to a stack rlimit.
        unsafe { libc::setrlimit(libc::RLIMIT_CORE, &raw const lim) };
        work();
        // SAFETY: _exit is async-signal-safe and skips parent's atexit handlers.
        unsafe { libc::_exit(0) };
    }

    // Parent: wait, retry on EINTR.
    let mut status: libc::c_int = 0;
    loop {
        // SAFETY: `pid` is a valid child pid we just forked.
        let r = unsafe { libc::waitpid(pid, &raw mut status, 0) };
        if r >= 0 {
            break;
        }
        if io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
            return ERR_WAITPID_FAILED;
        }
    }

    // Darwin / Linux wait-status: low 7 bits hold the terminating signal
    // when the process was killed; 0x7F is the stopped sentinel.
    let low = status & 0x7F;
    if low == 0 || low == 0x7F {
        return ERR_NOT_SIGNALED;
    }
    low
}

/// Test-only: fork a child that reads one byte from the front guard page,
/// returning the terminating signal observed by the parent. Always expected
/// to return `SIGSEGV` or `SIGBUS`.
///
/// # Safety
/// Spawns a process that intentionally crashes. Caller must accept the
/// fork/wait semantics described on [`fork_and_run`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn unsafe_test_only_siegel_front_guard_seg_fault(handle: u64) -> i32 {
    let Some(session) = lookup_session(handle) else {
        return ERR_INVALID_HANDLE;
    };
    fork_and_run(|| unsafe { session.test_touch_front_guard() })
}

const DEGRADE_OK: i32 = 0;
const DEGRADE_HARD_ERROR: i32 = 1;
const DEGRADE_STILL_LOCKED: i32 = 2;

/// Test-only: in a forked child, zero `RLIMIT_MEMLOCK` so the real `mlock`
/// syscall fails, then allocate a siegel and confirm it degrades gracefully.
/// Exercises the best-effort lock path against the *actual* platform libc.
///
/// # Returns
/// - [`DEGRADE_OK`]: degraded to unlocked yet stayed usable (the expected path);
/// - [`DEGRADE_STILL_LOCKED`]: `mlock` unexpectedly succeeded (e.g. privileged environment);
/// - [`DEGRADE_HARD_ERROR`]: allocation failed, lock failed aborted or degraded region was unusable;
/// - [`ERR_FORK_FAILED`] / [`ERR_WAITPID_FAILED`] on fork/wait failure.
///
/// Runs entirely in the child so the `RLIMIT_MEMLOCK` change never touches
/// the host test process.
///
/// # Safety
/// Runs on a forked process
#[unsafe(no_mangle)]
pub unsafe extern "C" fn unsafe_test_only_siegel_degrades_without_mlock() -> i32 {
    // SAFETY: single fork; no async-signal-unsafe parent state is touched.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return ERR_FORK_FAILED;
    }
    if pid == 0 {
        let lim = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: valid rlimit pointer; lowering the soft limit is permitted
        unsafe { libc::setrlimit(libc::RLIMIT_MEMLOCK, &raw const lim) };
        let code = degrade_probe();
        // SAFETY: _exit is async-signal-safe and skips atexit handlers.
        unsafe { libc::_exit(code) };
    }

    let mut status: libc::c_int = 0;
    loop {
        // SAFETY: `pid` is a valid child we just forked.
        let r = unsafe { libc::waitpid(pid, &raw mut status, 0) };
        if r >= 0 {
            break;
        }
        if io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
            return ERR_WAITPID_FAILED;
        }
    }
    if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status)
    } else {
        ERR_WAITPID_FAILED
    }
}

/// Allocate a siegel under a zeroed `RLIMIT_MEMLOCK` and classify the outcome,
/// for the child of [`unsafe_test_only_siegel_degrades_without_mlock`].
fn degrade_probe() -> i32 {
    match Siegel::<Empty>::new(64) {
        // Degraded to unlocked (best-effort) — must stay usable.
        Ok(empty) if !empty.is_locked() => round_trip_ok(empty),
        // mlock succeeded despite RLIMIT_MEMLOCK=0 — privileged env where the
        // limit isn't enforced. Can't force the failure here; inconclusive.
        Ok(_) => DEGRADE_STILL_LOCKED,
        // A failed lock must never abort allocation.
        Err(_) => DEGRADE_HARD_ERROR,
    }
}

/// Confirm a (best-effort, unlocked) siegel still round-trips its secret.
fn round_trip_ok(empty: Siegel<Empty>) -> i32 {
    let Ok(loaded) = empty.write(&[0x5A; 64]) else {
        return DEGRADE_HARD_ERROR;
    };
    match loaded.read_once(|bytes| bytes.iter().all(|&b| b == 0x5A)) {
        Ok(true) => DEGRADE_OK,
        _ => DEGRADE_HARD_ERROR,
    }
}
