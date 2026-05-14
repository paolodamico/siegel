//! Helpers for integration tests.

use std::io;

use sha2::{Digest, Sha256};

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
        // Child: suppress core dumps to keep CrashReporter quiet, then run
        // the unsafe work. If `work` returns instead of crashing, exit
        // cleanly so the parent observes ERR_NOT_SIGNALED.
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
