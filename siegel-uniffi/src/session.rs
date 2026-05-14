use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, PoisonError, Weak};

use siegel::{Empty, Loaded, Siegel, SiegelError};

/// Type for the global registry of handles.
///
/// `Weak` is used instead of `Arc` so the session gets `Drop`ped after
/// the last foreign-held Arc is dropped (i.e. the registry does not hold an owning
/// reference).
type Registry = Mutex<HashMap<u64, Weak<SiegelSession>>>;

/// Global registry of handles. Each [`SiegelSession`] is initialized in this registry
/// so that Rust can enforce ownership of handles.
///
/// Siegels are filled with raw pointers on the foreign side (see [`siegel_fill`]),
/// and without the registry, foreign code could pass garbage or deallocated pointers. More
/// commonly it avoids accidental use after drop.
static REGISTRY: LazyLock<Registry> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// Monotonic counter that issues unique handle IDs.
///
/// Starts at 1 so 0 can represent the _nil_ state.
static NEXT_HANDLE: LazyLock<Mutex<u64>> = LazyLock::new(|| Mutex::new(1));

/// One-time secret-handling session, held by foreign code as
/// `Arc<SiegelSession>`.
///
/// The session wraps an internal `Siegel<Empty>` or `Siegel<Loaded>`
/// behind a `Mutex` so it can be driven through `&self` methods.
/// [`siegel_fill`] writes bytes into the empty siegel and transitions
/// the state to `Loaded`; [`read_once`](Self::read_once)
/// runs the application operation against the loaded siegel and wipes.
#[derive(uniffi::Object)]
pub struct SiegelSession {
    state: Mutex<SessionState>,
    handle_id: u64,
}

#[uniffi::export]
impl SiegelSession {
    /// Open a new session sized for `len` bytes.
    ///
    /// Foreign code retrieves [`SiegelSession::handle`], calls [`siegel_fill`]
    /// to write the bytes, then calls the application's `#[uniffi::export]`
    /// function (which internally invokes [`SiegelSession::read_once`]).
    ///
    /// # Errors
    ///
    /// `SessionError::InvalidLength` for `len == 0` or `len > 1 MiB`.
    /// Allocation / protection / lock errors propagate from `siegel`.
    #[uniffi::constructor]
    pub fn new(len: u32) -> Result<Arc<Self>, SessionError> {
        let len = usize::try_from(len).map_err(|_| SessionError::InvalidLength)?;
        let empty = Siegel::<Empty>::new(len)?;

        let handle_id = allocate_handle_id();
        let session = Arc::new(Self {
            state: Mutex::new(SessionState::Empty(empty)),
            handle_id,
        });

        registry_lock().insert(handle_id, Arc::downgrade(&session));

        Ok(session)
    }
}

#[uniffi::export]
impl SiegelSession {
    /// Opaque identifier handle for [`siegel_fill`].
    ///
    /// Stable for the lifetime of the session.
    pub fn handle_id(&self) -> u64 {
        self.handle_id
    }

    /// Wipe the session without using it. Idempotent.
    pub fn obliviate(&self) {
        let mut state = lock_state(&self.state);
        *state = SessionState::Consumed; // automatically drops any loaded `Siegel`
    }

    /// Whether the session has been consumed or otherwise reached
    /// a terminal error state.
    pub fn is_consumed(&self) -> bool {
        matches!(*lock_state(&self.state), SessionState::Consumed)
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
        let loaded = {
            let mut state = lock_state(&self.state);
            match std::mem::replace(&mut *state, SessionState::Consumed) {
                SessionState::Loaded(s) => s,
                SessionState::Empty(s) => {
                    *state = SessionState::Empty(s);
                    return Err(SessionError::InvalidState);
                }
                SessionState::Consumed => return Err(SessionError::Consumed),
            }
        };

        loaded.read_once(f).map_err(SessionError::from)
    }
}

impl Drop for SiegelSession {
    fn drop(&mut self) {
        // The Arc is probably dropped, but clean up the stale entry from the registry
        registry_lock().remove(&self.handle_id);
    }
}

pub const FILL_OK: i32 = 0;
pub const FILL_ERR_INVALID_HANDLE: i32 = -1;
pub const FILL_ERR_LEN_MISMATCH: i32 = -2;
pub const FILL_ERR_NULL_SRC: i32 = -3;
pub const FILL_ERR_WRONG_STATE: i32 = -4;
pub const FILL_ERR_PROTECTION: i32 = -5;

/// Fills the `Siegel` with the actual data.
///
/// Copies `len` bytes from `src` raw pointer into the session's siegel.
///
/// This is the only function that crosses the foreign boundary outside
/// of `UniFFI`. This exists to avoid the lowering behavior from `UniFFI` which
/// creates a new buffer of the bytes in transit.
///
/// # Arguments
/// - `handle`: the opaque handler received from [`SiegelSession::handle`].
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
    if src.is_null() {
        return FILL_ERR_NULL_SRC;
    }

    // Ensure the provided handle is valid
    let Some(session) = registry_lock().get(&handle).and_then(Weak::upgrade) else {
        return FILL_ERR_INVALID_HANDLE;
    };

    let mut state = lock_state(&session.state);

    // Set  a `Consumed` state to ensure it is persisted on errors.
    let empty = match std::mem::replace(&mut *state, SessionState::Consumed) {
        SessionState::Empty(s) => s,
        other => {
            *state = other;
            return FILL_ERR_WRONG_STATE;
        }
    };

    if empty.len() != len {
        *state = SessionState::Empty(empty);
        return FILL_ERR_LEN_MISMATCH;
    }

    // SAFETY: `src` is valid for `len` bytes if the caller implemented correctly
    let bytes = unsafe { std::slice::from_raw_parts(src, len) };

    match empty.write(bytes) {
        Ok(loaded) => {
            *state = SessionState::Loaded(loaded);
            FILL_OK
        }
        Err(_) => {
            // Siegel::write consumed `empty` on failure. State stays Consumed.
            FILL_ERR_PROTECTION
        }
    }
}

/// The state of the session.
enum SessionState {
    Empty(Siegel<Empty>),
    Loaded(Siegel<Loaded>),
    Consumed,
}

/// Acquire the registry lock
fn registry_lock() -> MutexGuard<'static, HashMap<u64, Weak<SiegelSession>>> {
    REGISTRY.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Return a fresh unique handle and advance the counter.
fn allocate_handle_id() -> u64 {
    let mut counter = NEXT_HANDLE.lock().unwrap_or_else(PoisonError::into_inner);
    let id = *counter;
    *counter = counter.wrapping_add(1);
    id
}

/// Acquire a session's state mutex
fn lock_state(s: &Mutex<SessionState>) -> MutexGuard<'_, SessionState> {
    s.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Errors surfaced to foreign bindings.
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
    #[error("memory allocation failed: {reason}")]
    AllocationFailed { reason: String },
    #[error("memory protection failed: {reason}")]
    ProtectionFailed { reason: String },
    #[error("memory lock failed: {reason}")]
    LockFailed { reason: String },
    #[error("canary check failed: possible memory corruption")]
    CanaryCorrupted,
}

impl From<SiegelError> for SessionError {
    fn from(e: SiegelError) -> Self {
        match e {
            SiegelError::InvalidLength => Self::InvalidLength,
            SiegelError::LengthMismatch { .. } => Self::LengthMismatch,
            SiegelError::AllocationFailed { reason } => Self::AllocationFailed { reason },
            SiegelError::ProtectionFailed { reason } => Self::ProtectionFailed { reason },
            SiegelError::LockFailed { reason } => Self::LockFailed { reason },
            SiegelError::CanaryCorrupted => Self::CanaryCorrupted,
        }
    }
}

/// Resolve a registry handle to a [`SiegelSession`] reference. Used by `test_utils` only.
#[cfg(feature = "test-utils")]
pub(crate) fn lookup_session(handle: u64) -> Option<Arc<SiegelSession>> {
    registry_lock().get(&handle).and_then(Weak::upgrade)
}

#[cfg(feature = "test-utils")]
impl SiegelSession {
    /// Touch the active siegel's front guard page. No-op if consumed.
    ///
    /// # Safety
    /// Intentionally crashes the process. Forked child only.
    pub(crate) unsafe fn test_touch_front_guard(&self) {
        let state = lock_state(&self.state);
        match &*state {
            SessionState::Empty(s) => unsafe { s.test_touch_front_guard() },
            SessionState::Loaded(s) => unsafe { s.test_touch_front_guard() },
            SessionState::Consumed => {}
        }
    }

    /// Touch the active siegel's back guard page. No-op if consumed.
    ///
    /// # Safety
    /// Intentionally crashes the process. Forked child only.
    pub(crate) unsafe fn test_touch_back_guard(&self) {
        let state = lock_state(&self.state);
        match &*state {
            SessionState::Empty(s) => unsafe { s.test_touch_back_guard() },
            SessionState::Loaded(s) => unsafe { s.test_touch_back_guard() },
            SessionState::Consumed => {}
        }
    }

    /// Touch the active siegel's sealed data region. No-op if consumed.
    ///
    /// # Safety
    /// Intentionally crashes the process when the region is sealed.
    pub(crate) unsafe fn test_touch_sealed_data(&self) {
        let state = lock_state(&self.state);
        match &*state {
            SessionState::Empty(s) => unsafe { s.test_touch_sealed_data() },
            SessionState::Loaded(s) => unsafe { s.test_touch_sealed_data() },
            SessionState::Consumed => {}
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

    #[test]
    fn begin_session_starts_empty() {
        let s = SiegelSession::new(32).unwrap();
        assert!(!s.is_consumed());
        assert_eq!(s.handle_id(), s.handle_id());
    }

    #[test]
    fn begin_session_rejects_zero_length() {
        assert!(SiegelSession::new(0).is_err());
    }

    #[test]
    fn fill_then_consume_with_arbitrary_closure() {
        let secret = vec![0x42; 16];
        let s = SiegelSession::new(16).unwrap();
        assert_eq!(fill(&s, &secret), FILL_OK);
        let digest = s
            .read_once(|bytes| {
                let mut h = Sha256::new();
                h.update(bytes);
                h.update(b"context");
                h.finalize().to_vec()
            })
            .unwrap();
        let mut expected = Sha256::new();
        expected.update(&secret);
        expected.update(b"context");
        assert_eq!(digest, expected.finalize().to_vec());
        assert!(s.is_consumed());
    }

    #[test]
    fn fill_rejects_null_src() {
        let s = SiegelSession::new(8).unwrap();
        let rc = unsafe { siegel_fill(s.handle_id(), std::ptr::null(), 8) };
        assert_eq!(rc, FILL_ERR_NULL_SRC);
    }

    #[test]
    fn fill_rejects_unknown_handle() {
        let bytes = [0u8; 8];
        let rc = unsafe { siegel_fill(127, bytes.as_ptr(), 8) };
        assert_eq!(rc, FILL_ERR_INVALID_HANDLE);
    }

    #[test]
    fn fill_rejects_wrong_length() {
        let s = SiegelSession::new(16).unwrap();
        assert_eq!(fill(&s, &[0u8; 8]), FILL_ERR_LEN_MISMATCH);
        assert_eq!(fill(&s, &[0u8; 16]), FILL_OK);
    }

    #[test]
    fn double_fill_rejected() {
        let s = SiegelSession::new(8).unwrap();
        assert_eq!(fill(&s, &[1u8; 8]), FILL_OK);
        assert_eq!(fill(&s, &[1u8; 8]), FILL_ERR_WRONG_STATE);
    }

    #[test]
    fn use_rejects_empty_session() {
        let s = SiegelSession::new(8).unwrap();
        let err = s.read_once(<[u8]>::to_vec).unwrap_err();
        assert!(matches!(err, SessionError::InvalidState));
        assert!(!s.is_consumed());
        assert_eq!(fill(&s, &[2u8; 8]), FILL_OK);
        assert!(s.read_once(<[u8]>::to_vec).is_ok());
    }

    #[test]
    fn use_rejects_consumed_session() {
        let s = SiegelSession::new(8).unwrap();
        assert_eq!(fill(&s, &[3u8; 8]), FILL_OK);
        s.read_once(<[u8]>::to_vec).unwrap();
        let err = s.read_once(<[u8]>::to_vec).unwrap_err();
        assert!(matches!(err, SessionError::Consumed));
    }

    #[test]
    fn fill_rejected_after_consume() {
        let s = SiegelSession::new(8).unwrap();
        assert_eq!(fill(&s, &[4u8; 8]), FILL_OK);
        s.read_once(<[u8]>::to_vec).unwrap();
        assert_eq!(fill(&s, &[4u8; 8]), FILL_ERR_WRONG_STATE);
    }

    #[test]
    fn obliviate_wipes() {
        let s = SiegelSession::new(16).unwrap();
        assert_eq!(fill(&s, &[5u8; 16]), FILL_OK);
        s.obliviate();
        assert!(s.is_consumed());
        s.obliviate();
        let err = s.read_once(<[u8]>::to_vec).unwrap_err();
        assert!(matches!(err, SessionError::Consumed));
    }

    #[test]
    fn handle_invalidated_after_session_drop() {
        let handle = {
            let s = SiegelSession::new(8).unwrap();
            s.handle_id()
        };
        let rc = unsafe { siegel_fill(handle, [0u8; 8].as_ptr(), 8) };
        assert_eq!(rc, FILL_ERR_INVALID_HANDLE);
    }

    #[test]
    fn distinct_sessions_have_distinct_handles() {
        let a = SiegelSession::new(8).unwrap();
        let b = SiegelSession::new(8).unwrap();
        assert_ne!(a.handle_id(), b.handle_id());
    }
}
