//! Helpers for integration tests.

use std::sync::Arc;

use sha2::digest::generic_array::GenericArray;
use sha2::{Digest, Sha256};

use siegel::session::lookup_session;
use siegel::test_utils::{ERR_INVALID_HANDLE, fork_and_run};

use crate::session::{SessionError, SiegelSession};

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

/// Consume the session and return a new one holding SHA-256 of its bytes,
/// for `siegel_take`.
///
/// # Warning
/// Only for integration tests.
///
/// # Errors
/// Same as [`SiegelSession::read_once`] and [`SiegelSession::new_written`].
#[uniffi::export]
pub fn sha256_reseal(input: &SiegelSession) -> Result<Arc<SiegelSession>, SessionError> {
    SiegelSession::new_written(32, |buf| {
        input.read_once(|bytes| {
            // Finalize into protected memory, not a returned temporary.
            Sha256::new_with_prefix(bytes).finalize_into(GenericArray::from_mut_slice(buf));
        })
    })?
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
