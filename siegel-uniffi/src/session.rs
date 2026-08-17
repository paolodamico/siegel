use std::sync::Arc;

use siegel::session::{self, MAX_ACTIVE_SESSIONS, SessionCore};

pub use siegel::session::{
    FILL_ERR_INVALID_HANDLE, FILL_ERR_LEN_MISMATCH, FILL_ERR_NULL_SRC, FILL_ERR_PROTECTION,
    FILL_ERR_WRONG_STATE, FILL_OK,
};

/// One-time secret-handling session, held by foreign code as
/// `Arc<SiegelSession>`.
///
/// Thin `UniFFI` wrapper over [`SessionCore`]
#[derive(uniffi::Object)]
pub struct SiegelSession(Arc<SessionCore>);

#[uniffi::export]
impl SiegelSession {
    /// Open a new session sized for `len` bytes.
    ///
    /// Foreign code retrieves [`SiegelSession::handle_id()`] and then
    /// calls [`siegel_fill`] to write the bytes.
    ///
    /// # Errors
    ///
    /// `SessionError::InvalidLength` for `len == 0` or `len > 1 MiB`.
    /// `SessionError::TooManyActiveSessions` if the registry is at its cap.
    /// Allocation / protection / lock errors propagate from `siegel`.
    #[uniffi::constructor]
    pub fn new(len: u32) -> Result<Arc<Self>, SessionError> {
        Ok(Arc::new(Self(SessionCore::new(len)?)))
    }
}

#[uniffi::export]
impl SiegelSession {
    /// Opaque identifier handle for [`siegel_fill`].
    #[must_use]
    pub fn handle_id(&self) -> u64 {
        self.0.handle_id()
    }

    /// Capacity of the session in bytes.
    ///
    /// Stable for the lifetime of the session.
    #[must_use]
    #[expect(clippy::len_without_is_empty, reason = "sessions are always non-empty")]
    pub fn len(&self) -> u32 {
        self.0.len()
    }

    /// Wipe the session without using it. Idempotent.
    pub fn obliviate(&self) {
        self.0.obliviate();
    }

    /// Whether the session has been consumed or otherwise reached
    /// a terminal error state.
    #[must_use]
    pub fn is_consumed(&self) -> bool {
        self.0.is_consumed()
    }

    /// Whether the secret's pages are locked in RAM (`mlock`ed).
    #[must_use]
    pub fn is_locked(&self) -> bool {
        self.0.is_locked()
    }
}

/// Methods only available to Rust code.
impl SiegelSession {
    /// Reads the secret once, runs `f` function and then drops and
    /// zeroizes. This will consume the session to ensure it's only
    /// used once.
    ///
    /// # Errors
    /// - `InvalidState` if the session hasn't been filled.
    /// - `Consumed` if already consumed.
    /// - OS-level memory errors.
    pub fn read_once<T, F>(&self, f: F) -> Result<T, SessionError>
    where
        F: FnOnce(&[u8]) -> T,
    {
        self.0.read_once(f).map_err(SessionError::from)
    }
}

/// Fills the `Siegel` with the actual data.
///
/// Copies `len` bytes from `src` raw pointer into the session's siegel.
///
/// This is the only function that crosses the foreign boundary outside
/// of `UniFFI`. This exists to avoid the lowering behavior from `UniFFI` which
/// creates a new buffer of the bytes in transit.
///
/// # Arguments
/// - `handle`: the opaque handler received from [`SiegelSession::handle_id()`].
/// - `src`: the raw pointer to the bytes to copy.
/// - `len`: the size of the data.
///
/// # Safety
///
/// - `src` **MUST** be valid for `len` bytes of read. This is the caller's responsibility.
/// - The caller must not race fills against `read_once` or
///   `obliviate` on the same session.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn siegel_fill(handle: u64, src: *const u8, len: usize) -> i32 {
    // SAFETY: forwarded verbatim; the caller upholds this function's contract.
    unsafe { session::fill_into(handle, src, len) }
}

/// Errors surfaced to foreign bindings.
///
/// Mirrors [`siegel::session::SessionError`]
#[derive(Debug, thiserror::Error, uniffi::Error)]
#[uniffi(flat_error)]
pub enum SessionError {
    #[error("requested length must be 1..=1Mb")]
    InvalidLength,
    #[error("input length doesn't match the session's capacity")]
    LengthMismatch,
    #[error("session is not in the expected state for this operation")]
    InvalidState,
    #[error("session has been consumed")]
    Consumed,
    #[error("too many active sessions (max {MAX_ACTIVE_SESSIONS})")]
    TooManyActiveSessions,
    #[error("memory allocation failed: {reason}")]
    AllocationFailed { reason: String },
    #[error("memory protection failed: {reason}")]
    ProtectionFailed { reason: String },
    #[error("canary check failed: possible memory corruption")]
    CanaryCorrupted,
    #[error("could not allocate a unique handle id: {reason}")]
    HandleAllocationFailed { reason: String },
}

impl From<session::SessionError> for SessionError {
    fn from(e: session::SessionError) -> Self {
        use session::SessionError as Core;
        match e {
            Core::InvalidLength => Self::InvalidLength,
            Core::LengthMismatch => Self::LengthMismatch,
            Core::InvalidState => Self::InvalidState,
            Core::Consumed => Self::Consumed,
            Core::TooManyActiveSessions => Self::TooManyActiveSessions,
            Core::AllocationFailed { reason } => Self::AllocationFailed { reason },
            Core::ProtectionFailed { reason } => Self::ProtectionFailed { reason },
            Core::CanaryCorrupted => Self::CanaryCorrupted,
            Core::HandleAllocationFailed { reason } => Self::HandleAllocationFailed { reason },
        }
    }
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "tests")]
mod tests {
    use sha2::{Digest, Sha256};

    use super::*;

    fn fill(session: &Arc<SiegelSession>, bytes: &[u8]) -> i32 {
        unsafe { siegel_fill(session.handle_id(), bytes.as_ptr(), bytes.len()) }
    }

    /// The wrapper must forward the handle registered by the core, otherwise
    /// `siegel_fill` would never resolve the session.
    #[test]
    fn wrapper_handle_resolves_through_raw_fill() {
        let secret = vec![0x42; 16];
        let s = SiegelSession::new(16).unwrap();
        assert_eq!(fill(&s, &secret), FILL_OK);
        let digest = s
            .read_once(|bytes| {
                let mut h = Sha256::new();
                h.update(bytes);
                h.finalize().to_vec()
            })
            .unwrap();
        let mut expected = Sha256::new();
        expected.update(&secret);
        assert_eq!(digest, expected.finalize().to_vec());
        assert!(s.is_consumed());
    }

    #[test]
    fn constructor_errors_map_to_binding_error() {
        // `matches!` rather than `unwrap_err`: the Ok variant is
        // `Arc<SiegelSession>`, which is deliberately not `Debug`.
        assert!(matches!(
            SiegelSession::new(0),
            Err(SessionError::InvalidLength)
        ));
    }

    #[test]
    fn accessors_delegate_to_core() {
        let s = SiegelSession::new(64).unwrap();
        assert_eq!(s.len(), 64);
        assert!(!s.is_consumed());
        assert_ne!(s.handle_id(), 0);
        s.obliviate();
        assert!(s.is_consumed());
    }

    /// Dropping the foreign-held `Arc` must drop the inner `SessionCore` and
    /// evict its registry entry, or handles would leak across the boundary.
    #[test]
    fn dropping_wrapper_invalidates_handle() {
        let handle = {
            let s = SiegelSession::new(8).unwrap();
            s.handle_id()
        };
        let rc = unsafe { siegel_fill(handle, [0u8; 8].as_ptr(), 8) };
        assert_eq!(rc, FILL_ERR_INVALID_HANDLE);
    }
}
