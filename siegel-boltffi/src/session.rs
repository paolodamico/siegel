#![expect(
    clippy::used_underscore_items,
    reason = "boltffi's #[export] expansion references underscore-prefixed items"
)]

use std::sync::Arc;

use boltffi::export;
use siegel::session::{self, MAX_ACTIVE_SESSIONS, SessionCore};

pub use siegel::session::{
    FILL_ERR_INVALID_HANDLE, FILL_ERR_LEN_MISMATCH, FILL_ERR_NULL_SRC, FILL_ERR_PROTECTION,
    FILL_ERR_WRONG_STATE, FILL_OK,
};

/// One-time secret-handling session.
///
/// Foreign code can fill the session through the raw path ([`siegel_fill_bolt`] on Apple,
/// `SiegelNative.fillDirect` on the JVM).
///
/// # Thread safety
///
/// Confine a session to one thread. While the Rust-side is `Mutex`-guarded,
/// a session is one-time use, racing a fill yields an arbitrary winner.
pub struct SiegelSession(Arc<SessionCore>);

#[export]
impl SiegelSession {
    /// Open a new session sized for `len` bytes.
    ///
    /// # Errors
    ///
    /// `SessionError::InvalidLength` for `len == 0` or `len > 1 MiB`.
    /// `SessionError::TooManyActiveSessions` if the registry is at its cap.
    /// Allocation / protection / lock errors propagate from `siegel`.
    pub fn new(len: u32) -> Result<Self, SessionError> {
        Ok(Self(SessionCore::new(len)?))
    }

    /// Opaque identifier handle for the raw fill path.
    ///
    /// Drawn from the OS CSPRNG to reduce the likelihood of
    /// accidental access by non-expected callers. It is not an infallible
    /// security guarantee. Stable for the lifetime of the session.
    #[must_use]
    pub fn handle_id(&self) -> u64 {
        self.0.handle_id()
    }

    /// Capacity of the session in bytes.
    ///
    /// Stable for the lifetime of the session. Callers can use this to
    /// verify the session matches the size they expected before filling.
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
/// # Arguments
/// - `handle`: the opaque handler received from [`SiegelSession::handle_id()`].
/// - `src`: the raw pointer to the bytes to copy.
/// - `len`: the size of the data.
///
/// # Safety
/// - `src` **MUST** be valid for `len` bytes of read. This is the caller's responsibility.
/// - The caller must not race fills against `read_once` or
///   `obliviate` on the same session.
///
/// # Rationale
/// `BoltFFI` has special treatment for `&[u8]` lifted on Swift as `Data` (`ByteArray`) on Kotlin,
/// this happens through a `writeBytes` function which creates a dandling copy (one more in `finalize`),
/// i.e. dangling non-zeroized copies, what we want to avoid. We could go around this, e.g. using `i8`, but
/// there's no guarantee this inner behavior won't change and we can't rely on that for security.
///
/// Reference: <https://github.com/boltffi/boltffi/blob/b33f1ae1b1e6a9ec5508143f5846afad10756ef4/boltffi_backend/templates/target/swift/wire.swift#L250>
#[unsafe(no_mangle)]
pub unsafe extern "C" fn siegel_fill_bolt(handle: u64, src: *const u8, len: usize) -> i32 {
    // SAFETY: forwarded verbatim; the caller upholds this function's contract.
    unsafe { session::fill_into(handle, src, len) }
}

/// Errors surfaced to foreign bindings.
///
/// Mirrors [`siegel::session::SessionError`]. Declared here because each
/// binding generator needs the error type in its own crate, and because the
/// foreign-facing shape is binding-specific: `BoltFFI` carries struct-variant
/// fields through to the generated exception as properties, where `UniFFI`
/// flattens them to a message.
#[derive(Debug, thiserror::Error)]
#[boltffi::error]
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

    fn fill(session: &SiegelSession, bytes: &[u8]) -> i32 {
        unsafe { siegel_fill_bolt(session.handle_id(), bytes.as_ptr(), bytes.len()) }
    }

    #[test]
    fn fill_then_consume_returns_digest() {
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
        assert!(matches!(
            SiegelSession::new(0),
            Err(SessionError::InvalidLength)
        ));
    }

    #[test]
    fn fill_rejects_wrong_length() {
        let s = SiegelSession::new(16).unwrap();
        assert_eq!(fill(&s, &[0u8; 8]), FILL_ERR_LEN_MISMATCH);
        // The session stays usable after a rejected fill.
        assert_eq!(fill(&s, &[1u8; 16]), FILL_OK);
    }

    #[test]
    fn fill_rejects_double_fill() {
        let s = SiegelSession::new(8).unwrap();
        assert_eq!(fill(&s, &[1u8; 8]), FILL_OK);
        assert_eq!(fill(&s, &[1u8; 8]), FILL_ERR_WRONG_STATE);
    }

    #[test]
    fn read_once_rejects_unfilled_session() {
        let s = SiegelSession::new(8).unwrap();
        assert!(matches!(
            s.read_once(<[u8]>::to_vec),
            Err(SessionError::InvalidState)
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

    /// Dropping the wrapper must drop the inner `SessionCore` and evict its
    /// registry entry, or handles would leak across the boundary.
    #[test]
    fn dropping_wrapper_invalidates_handle() {
        let handle = {
            let s = SiegelSession::new(8).unwrap();
            s.handle_id()
        };
        let rc = unsafe { session::fill_into(handle, [0u8; 8].as_ptr(), 8) };
        assert_eq!(rc, FILL_ERR_INVALID_HANDLE);
    }
}
