use std::io;

use crate::ProtectedRegion;
use crate::Siegel;

impl ProtectedRegion {
    /// Test-only: read one byte from the front guard page. The guard
    /// page is always `PROT_NONE`, so this triggers a segfault before
    /// the read completes.
    ///
    /// Used by foreign integration tests as a single smoke check that
    /// memory protection survives the binding boundary. Exhaustive
    /// coverage of front/back guards, sealed data and canary lives in
    /// `protected.rs`'s native unit tests.
    ///
    /// # Safety
    /// Intentionally crashes the process. Call only from a forked
    /// child that the parent waits on.
    pub unsafe fn test_touch_front_guard(&self) {
        let guard = unsafe { self.data().sub(1) };
        let _ = unsafe { std::ptr::read_volatile(guard) };
    }
}

impl<State> Siegel<State> {
    /// See [`ProtectedRegion::test_touch_front_guard`].
    ///
    /// # Safety
    /// Intentionally crashes the process. Forked child only.
    pub unsafe fn test_touch_front_guard(&self) {
        unsafe { self.region.test_touch_front_guard() }
    }
}

/// Sentinel return values for the segfault helpers.
///
/// Shared by both binding crates so a fix to the wait-status decoding or the
/// signal handling lands once.
pub const ERR_INVALID_HANDLE: i32 = -1;
pub const ERR_FORK_FAILED: i32 = -2;
pub const ERR_WAITPID_FAILED: i32 = -3;
pub const ERR_NOT_SIGNALED: i32 = -4;

/// Wall-clock bound on the forked child. It is expected to fault within
/// microseconds; anything longer means it wedged.
const CHILD_TIMEOUT_SECS: libc::c_uint = 5;

/// Forks a child that runs `work`, waits for it, and returns the terminating
/// signal (e.g. `SIGSEGV` / `SIGBUS`). Used by foreign integration tests that
/// can't call `fork(2)` directly: Swift on iOS hides it, Kotlin/JVM can't
/// touch process state.
///
/// Returns one of [`ERR_FORK_FAILED`], [`ERR_WAITPID_FAILED`],
/// [`ERR_NOT_SIGNALED`], or the positive signal number on success.
pub fn fork_and_run(work: impl FnOnce()) -> i32 {
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

        // Bound the child. It is forked from a live JVM or XCTest process, so
        // another thread may have held the malloc lock or a session mutex at
        // fork time, leaving the child deadlocked. Without this the parent's
        // blocking `waitpid` would hang the whole test job until the CI
        // timeout; `SIGALRM` turns that into an observable signal instead.
        // SAFETY: alarm() is async-signal-safe.
        unsafe { libc::alarm(CHILD_TIMEOUT_SECS) };

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
