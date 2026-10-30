#![expect(
    clippy::used_underscore_items,
    reason = "boltffi's #[export] expansion references underscore-prefixed items"
)]

use std::sync::Arc;

use boltffi::export;
use siegel::session::{self, MAX_ACTIVE_SESSIONS, MAX_OPEN_SESSIONS, SessionCore};

pub use siegel::session::{
    FILL_ERR_INVALID_HANDLE, FILL_ERR_LEN_MISMATCH, FILL_ERR_NULL_SRC, FILL_ERR_PROTECTION,
    FILL_ERR_WRONG_STATE, FILL_OK, TAKE_ERR_CANARY, TAKE_ERR_INVALID_HANDLE, TAKE_ERR_LEN_MISMATCH,
    TAKE_ERR_NULL_DST, TAKE_ERR_PROTECTION, TAKE_ERR_WRONG_STATE, TAKE_OK,
};

/// One-time secret-handling session.
///
/// Foreign code fills the session through the raw path ([`siegel_fill_bolt`] on Apple,
/// `SiegelNative.fillDirect` on the JVM), and takes a Rust-written secret back out
/// through [`siegel_take_bolt`] / `SiegelNative.takeDirect`.
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
    /// `SessionError::TooManyActiveSessions` if the session cap is reached.
    /// Allocation / protection errors propagate from `siegel`. A failed `mlock`
    /// does not fail the constructor; query [`is_locked`](Self::is_locked).
    pub fn new(len: u32) -> Result<Self, SessionError> {
        Ok(Self(SessionCore::new(len)?))
    }

    /// Opaque identifier handle for the raw fill and take paths.
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
    /// verify the session matches the size they expected before filling, or to
    /// size the buffer for a take.
    #[must_use]
    #[expect(clippy::len_without_is_empty, reason = "sessions are always non-empty")]
    pub fn len(&self) -> u32 {
        self.0.len()
    }

    /// Wipe the session without using it. Idempotent.
    ///
    /// Not a substitute for releasing the session (`close()` on the JVM): the
    /// secret and its slot are freed, but the session object is not.
    pub fn obliviate(&self) {
        self.0.obliviate();
    }

    /// Whether the session has been consumed or otherwise reached
    /// a terminal error state.
    #[must_use]
    pub fn is_consumed(&self) -> bool {
        self.0.is_consumed()
    }

    /// Whether the secret's pages are locked in RAM (`mlock`ed). Always
    /// `false` once consumed: the session holds no pages to lock.
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
    /// - `InvalidState` if the session holds no secret yet.
    /// - `Consumed` if already consumed.
    /// - OS-level memory errors.
    pub fn read_once<T, F>(&self, f: F) -> Result<T, SessionError>
    where
        F: FnOnce(&[u8]) -> T,
    {
        self.0.read_once(f).map_err(SessionError::from)
    }

    /// Opens a session whose secret `f` writes straight into protected memory
    /// (zeroed, full capacity), for foreign code to take.
    ///
    /// If `f` fails, the bytes are zeroized, no session is returned and `f`'s
    /// error is the inner `Result`. Read any input session inside `f`, so the
    /// input isn't used up if the output can't be opened.
    ///
    /// # Errors
    /// Same as [`SiegelSession::new`], plus protection errors.
    pub fn new_written<E, F>(len: u32, f: F) -> Result<Result<Self, E>, SessionError>
    where
        F: FnOnce(&mut [u8]) -> Result<(), E>,
    {
        Ok(SessionCore::new_written(len, f)?.map(Self))
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
/// Same as [`siegel::session::fill_into`].
///
/// # Rationale
/// `BoltFFI` lifts `&[u8]` as `Data` on Swift and `ByteArray` on Kotlin through a
/// `writeBytes` function, which leaves a non-zeroized copy (and `finalize` another).
/// Workarounds such as `i8` would depend on generator internals that may change,
/// which is not something to rely on for security.
///
/// Reference: <https://github.com/boltffi/boltffi/blob/b33f1ae1b1e6a9ec5508143f5846afad10756ef4/boltffi_backend/templates/target/swift/wire.swift#L250>
#[must_use]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn siegel_fill_bolt(handle: u64, src: *const u8, len: usize) -> i32 {
    // SAFETY: forwarded verbatim; the caller upholds this function's contract.
    unsafe { session::fill_into(handle, src, len) }
}

/// Copies a secret Rust wrote with [`SiegelSession::new_written`] into `dst`,
/// then wipes and consumes the session. The reverse of [`siegel_fill_bolt`], for
/// the same reason. Zeroizing `dst` is the caller's job.
///
/// A session filled by [`siegel_fill_bolt`] can't be taken (`TAKE_ERR_WRONG_STATE`).
/// On [`TAKE_ERR_CANARY`] and [`TAKE_ERR_PROTECTION`] `dst` is zeroed; other
/// errors leave it untouched.
///
/// # Arguments
/// - `handle`: from [`SiegelSession::handle_id()`].
/// - `dst`: where to copy the bytes.
/// - `len`: size of `dst`; must equal the session's capacity.
///
/// # Safety
/// Same as [`siegel::session::take_into`].
#[must_use]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn siegel_take_bolt(handle: u64, dst: *mut u8, len: usize) -> i32 {
    // SAFETY: forwarded verbatim; the caller upholds this function's contract.
    unsafe { session::take_into(handle, dst, len) }
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
    #[error("requested length must be 1..=1MiB")]
    InvalidLength,
    #[error("input length doesn't match the session's capacity")]
    LengthMismatch,
    #[error("session is not in the expected state for this operation")]
    InvalidState,
    #[error("session has been consumed")]
    Consumed,
    #[error(
        "too many sessions (max {MAX_ACTIVE_SESSIONS} holding secrets, {MAX_OPEN_SESSIONS} \
         open); are sessions being released?"
    )]
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
            Core::TooManyActiveSessions { .. } => Self::TooManyActiveSessions,
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

    #[test]
    fn written_session_round_trips_through_raw_take() {
        let Ok(s) = SiegelSession::new_written(8, |buf| {
            buf.fill(0x5A);
            Ok::<_, std::convert::Infallible>(())
        })
        .unwrap();
        let mut out = [0u8; 8];
        let rc = unsafe { siegel_take_bolt(s.handle_id(), out.as_mut_ptr(), out.len()) };
        assert_eq!(rc, TAKE_OK);
        assert_eq!(out, [0x5A; 8]);
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
