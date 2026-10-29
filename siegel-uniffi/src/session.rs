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

/// Max concurrent live sessions. Bounds mlocked memory on platforms where
/// `RLIMIT_MEMLOCK` is enforced (typically Linux server / containers).
/// Pruned opportunistically on every `SiegelSession::new`.
const MAX_ACTIVE_SESSIONS: usize = 1024;

/// Maximum tries to get a non-collision handle. Extremely unlikely to ever occur.
const MAX_HANDLE_ALLOC_RETRIES: u8 = 8;

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
    /// Bytes the session was allocated for. Stable for its lifetime.
    capacity: u32,
}

#[uniffi::export]
impl SiegelSession {
    /// Open a new session sized for `len` bytes.
    ///
    /// Foreign code retrieves [`SiegelSession::handle_id()`], calls [`siegel_fill`]
    /// to write the bytes, then calls the application's `#[uniffi::export]`
    /// function (which internally invokes [`SiegelSession::read_once`]).
    ///
    /// # Errors
    ///
    /// `SessionError::InvalidLength` for `len == 0` or `len > 1 MiB`.
    /// `SessionError::TooManyActiveSessions` if the registry is at the
    /// `MAX_ACTIVE_SESSIONS` cap.
    /// Allocation / protection / lock errors propagate from `siegel`.
    #[uniffi::constructor]
    pub fn new(len: u32) -> Result<Arc<Self>, SessionError> {
        let len_usize = usize::try_from(len).map_err(|_| SessionError::InvalidLength)?;

        // Hold the lock to the registry while allocation occurs to prevent
        // race conditions that could exceed the maximum active sessions.
        let mut registry = registry_lock();
        registry.retain(|_, w| w.strong_count() > 0); // Prune dropped entries
        if registry.len() >= MAX_ACTIVE_SESSIONS {
            // Checked before allocation to avoid wasting resources
            return Err(SessionError::TooManyActiveSessions);
        }

        let empty = Siegel::<Empty>::new(len_usize)?;

        let handle_id = allocate_handle_id(&registry)?;
        let session = Arc::new(Self {
            state: Mutex::new(SessionState::Empty(empty)),
            handle_id,
            capacity: len,
        });

        registry.insert(handle_id, Arc::downgrade(&session));
        drop(registry);

        #[cfg(feature = "tracing")]
        if !session.is_locked() {
            tracing::warn!(
                target: "siegel",
                capacity = len,
                "siegel could not be `mlock`ed; continuing"
            );
        }

        Ok(session)
    }
}

#[uniffi::export]
impl SiegelSession {
    /// Opaque identifier handle for [`siegel_fill`].
    ///
    /// Drawn from the OS CSPRNG to reduce the likelihood of
    /// accidental access by non-expected callers. It is not an infallible
    /// security guarantee. Stable for the lifetime of the session.
    pub fn handle_id(&self) -> u64 {
        self.handle_id
    }

    /// Capacity of the session in bytes.
    ///
    /// Stable for the lifetime of the session. Callers can use this to
    /// verify the session matches the size they expected before invoking
    /// [`read_once`](Self::read_once).
    #[must_use]
    #[expect(clippy::len_without_is_empty, reason = "sessions are always non-empty")]
    pub fn len(&self) -> u32 {
        self.capacity
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

    /// Whether the secret's pages are locked in RAM (`mlock`ed).
    pub fn is_locked(&self) -> bool {
        match &*lock_state(&self.state) {
            SessionState::Empty(s) => s.is_locked(),
            SessionState::Loaded(s) => s.is_locked(),
            SessionState::Consumed => false,
        }
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

    /// Writes the secret once by running `f` against the session's
    /// protected memory, so a secret output can be handed back to foreign
    /// code via [`siegel_take`] without crossing `UniFFI`'s byte copies.
    ///
    /// `f` gets the full capacity, zeroed. If `f` returns `Err` the bytes are
    /// zeroized and the session is consumed, so a partial output can never be
    /// taken. The session's lock is held while `f` runs; `f` must not call
    /// back into this session.
    ///
    /// # Errors
    /// - `InvalidState` if the session is already loaded.
    /// - `Consumed` if already consumed.
    /// - OS-level memory errors.
    /// - Whatever `f` returns.
    pub fn write_once<T, E, F>(&self, f: F) -> Result<T, E>
    where
        F: FnOnce(&mut [u8]) -> Result<T, E>,
        E: From<SessionError>,
    {
        let mut state = lock_state(&self.state);
        let empty = match std::mem::replace(&mut *state, SessionState::Consumed) {
            SessionState::Empty(s) => s,
            SessionState::Loaded(s) => {
                *state = SessionState::Loaded(s);
                return Err(SessionError::InvalidState.into());
            }
            SessionState::Consumed => return Err(SessionError::Consumed.into()),
        };

        let (loaded, result) = empty.write_with(f).map_err(SessionError::from)?;
        let value = result?;
        *state = SessionState::Loaded(loaded);
        Ok(value)
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

pub const TAKE_OK: i32 = 0;
pub const TAKE_ERR_INVALID_HANDLE: i32 = -1;
pub const TAKE_ERR_LEN_MISMATCH: i32 = -2;
pub const TAKE_ERR_NULL_DST: i32 = -3;
pub const TAKE_ERR_WRONG_STATE: i32 = -4;
pub const TAKE_ERR_PROTECTION: i32 = -5;
pub const TAKE_ERR_CANARY: i32 = -6;

/// Takes the secret out of the `Siegel` into foreign memory.
///
/// Copies the session's `len` loaded bytes into `dst`, then zeroizes the
/// siegel and consumes the session. The mirror of [`siegel_fill`] for
/// secrets Rust produced with [`SiegelSession::write_once`].
///
/// This exists to avoid the lifting behavior from `UniFFI`, which copies
/// returned bytes through intermediate buffers that are never zeroized.
/// The caller owns `dst` and is responsible for zeroizing it. On any code
/// other than [`TAKE_OK`], `dst` holds none of the secret.
///
/// # Arguments
/// - `handle`: the opaque handler received from [`SiegelSession::handle_id()`].
/// - `dst`: the raw pointer to copy the bytes into.
/// - `len`: the size of `dst`. Must equal the session's capacity.
///
/// # Safety
///
/// - `dst` **MUST** be valid for `len` bytes of write. This is the caller's responsibility.
/// - `dst` must not overlap the session's protected memory.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn siegel_take(handle: u64, dst: *mut u8, len: usize) -> i32 {
    if dst.is_null() {
        return TAKE_ERR_NULL_DST;
    }

    let Some(session) = registry_lock().get(&handle).and_then(Weak::upgrade) else {
        return TAKE_ERR_INVALID_HANDLE;
    };

    let mut state = lock_state(&session.state);

    let loaded = match std::mem::replace(&mut *state, SessionState::Consumed) {
        SessionState::Loaded(s) => s,
        other => {
            *state = other;
            return TAKE_ERR_WRONG_STATE;
        }
    };

    if loaded.len() != len {
        *state = SessionState::Loaded(loaded);
        return TAKE_ERR_LEN_MISMATCH;
    }

    // SAFETY: `dst` is valid for `len` bytes and doesn't overlap the region
    // if the caller implemented correctly; `bytes.len() == len` was checked above.
    let copied = loaded.read_once(|bytes| unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), dst, len);
    });

    // Siegel::read_once consumed `loaded` either way. State stays Consumed.
    let Err(e) = copied else {
        return TAKE_OK;
    };

    // A reseal failure surfaces after the copy already ran; don't leave the
    // secret in `dst` behind an error code.
    // SAFETY: same as the copy above.
    unsafe { std::ptr::write_bytes(dst, 0, len) };

    match e {
        SiegelError::CanaryCorrupted => TAKE_ERR_CANARY,
        SiegelError::ProtectionFailed { .. }
        | SiegelError::AllocationFailed { .. }
        | SiegelError::InvalidLength
        | SiegelError::LengthMismatch { .. } => TAKE_ERR_PROTECTION,
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

/// Generate a random handle ID.
///
/// - The caller must already hold the lock to the registry.
/// - `0` is reserved.
fn allocate_handle_id(registry: &HashMap<u64, Weak<SiegelSession>>) -> Result<u64, SessionError> {
    for _ in 0..MAX_HANDLE_ALLOC_RETRIES {
        let id = getrandom::u64().map_err(|e| SessionError::HandleAllocationFailed {
            reason: e.to_string(),
        })?;
        if id != 0 && !registry.contains_key(&id) {
            return Ok(id);
        }
    }
    Err(SessionError::HandleAllocationFailed {
        reason: "exhausted handle allocation retries".into(),
    })
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

impl From<SiegelError> for SessionError {
    fn from(e: SiegelError) -> Self {
        match e {
            SiegelError::InvalidLength => Self::InvalidLength,
            SiegelError::LengthMismatch { .. } => Self::LengthMismatch,
            SiegelError::AllocationFailed { reason } => Self::AllocationFailed { reason },
            SiegelError::ProtectionFailed { reason } => Self::ProtectionFailed { reason },
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
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "tests")]
mod tests {
    use sha2::{Digest, Sha256};

    use super::*;

    fn fill(session: &Arc<SiegelSession>, bytes: &[u8]) -> i32 {
        unsafe { siegel_fill(session.handle_id(), bytes.as_ptr(), bytes.len()) }
    }

    fn take(session: &Arc<SiegelSession>, dst: &mut [u8]) -> i32 {
        unsafe { siegel_take(session.handle_id(), dst.as_mut_ptr(), dst.len()) }
    }

    fn write(session: &Arc<SiegelSession>, bytes: &[u8]) -> Result<(), SessionError> {
        session.write_once(|buf| {
            buf.copy_from_slice(bytes);
            Ok(())
        })
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

    #[test]
    fn handles_are_not_sequential() {
        let sessions: Vec<_> = (0..16).map(|_| SiegelSession::new(8).unwrap()).collect();
        let ids: Vec<u64> = sessions.iter().map(|s| s.handle_id()).collect();
        assert!(ids.iter().all(|&id| id != 0), "0 is reserved");
        assert!(
            ids.iter().any(|&id| id > u64::from(u32::MAX)),
            "expected at least one handle in the high u64 range, got {ids:?}",
        );
    }

    #[test]
    fn len_reports_allocated_capacity() {
        let s = SiegelSession::new(64).unwrap();
        assert_eq!(s.len(), 64);
        // Capacity stays stable across state transitions.
        assert_eq!(fill(&s, &[7u8; 64]), FILL_OK);
        assert_eq!(s.len(), 64);
        s.read_once(<[u8]>::to_vec).unwrap();
        assert_eq!(s.len(), 64);
    }

    #[test]
    fn write_then_take_round_trips() {
        let s = SiegelSession::new(32).unwrap();
        let public = s
            .write_once(|buf| {
                buf.copy_from_slice(&Sha256::digest(b"seed"));
                Ok::<_, SessionError>("public half")
            })
            .unwrap();
        assert_eq!(public, "public half");
        assert!(!s.is_consumed());

        let mut out = [0u8; 32];
        assert_eq!(take(&s, &mut out), TAKE_OK);
        assert_eq!(out.to_vec(), Sha256::digest(b"seed").to_vec());
        assert!(s.is_consumed());
    }

    #[test]
    fn read_once_can_chain_into_write_once() {
        let input = SiegelSession::new(16).unwrap();
        assert_eq!(fill(&input, &[0x42; 16]), FILL_OK);
        let output = SiegelSession::new(32).unwrap();
        input
            .read_once(|seed| write(&output, &Sha256::digest(seed)))
            .unwrap()
            .unwrap();
        assert!(input.is_consumed());

        let digest = output.read_once(<[u8]>::to_vec).unwrap();
        assert_eq!(digest, Sha256::digest([0x42; 16]).to_vec());
    }

    #[derive(Debug)]
    enum AppError {
        DerivationFailed,
        Session(SessionError),
    }

    impl From<SessionError> for AppError {
        fn from(e: SessionError) -> Self {
            Self::Session(e)
        }
    }

    #[test]
    fn write_once_closure_error_consumes_session() {
        let s = SiegelSession::new(8).unwrap();
        let err = s
            .write_once(|buf| {
                buf.fill(0xAA);
                Err::<(), _>(AppError::DerivationFailed)
            })
            .unwrap_err();
        assert!(matches!(err, AppError::DerivationFailed));
        assert!(s.is_consumed());
        assert_eq!(take(&s, &mut [0u8; 8]), TAKE_ERR_WRONG_STATE);
    }

    #[test]
    fn write_once_converts_session_errors_into_caller_error() {
        let s = SiegelSession::new(8).unwrap();
        s.obliviate();
        let err = s.write_once(|_| Ok::<_, AppError>(())).unwrap_err();
        assert!(matches!(err, AppError::Session(SessionError::Consumed)));
    }

    #[test]
    fn write_once_rejects_loaded_session() {
        let s = SiegelSession::new(8).unwrap();
        assert_eq!(fill(&s, &[1u8; 8]), FILL_OK);
        let err = write(&s, &[2u8; 8]).unwrap_err();
        assert!(matches!(err, SessionError::InvalidState));
        // The filled secret is left intact.
        assert_eq!(s.read_once(<[u8]>::to_vec).unwrap(), vec![1u8; 8]);
    }

    #[test]
    fn write_once_panic_consumes_session() {
        let s = SiegelSession::new(8).unwrap();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            s.write_once(|_| -> Result<(), SessionError> {
                std::panic::resume_unwind(Box::new("derivation panicked"))
            })
        }));
        assert!(result.is_err());
        assert!(s.is_consumed());
    }

    #[test]
    fn fill_rejects_written_session() {
        let s = SiegelSession::new(8).unwrap();
        write(&s, &[3u8; 8]).unwrap();
        assert_eq!(fill(&s, &[4u8; 8]), FILL_ERR_WRONG_STATE);
    }

    #[test]
    fn take_rejects_null_dst() {
        let s = SiegelSession::new(8).unwrap();
        write(&s, &[5u8; 8]).unwrap();
        let rc = unsafe { siegel_take(s.handle_id(), std::ptr::null_mut(), 8) };
        assert_eq!(rc, TAKE_ERR_NULL_DST);
        assert!(!s.is_consumed());
    }

    #[test]
    fn take_rejects_unknown_handle() {
        let mut out = [0u8; 8];
        let rc = unsafe { siegel_take(127, out.as_mut_ptr(), 8) };
        assert_eq!(rc, TAKE_ERR_INVALID_HANDLE);
    }

    #[test]
    fn take_rejects_wrong_length_and_recovers() {
        let s = SiegelSession::new(16).unwrap();
        write(&s, &[6u8; 16]).unwrap();
        let mut short = [0u8; 8];
        assert_eq!(take(&s, &mut short), TAKE_ERR_LEN_MISMATCH);
        assert_eq!(short, [0u8; 8]);
        let mut out = [0u8; 16];
        assert_eq!(take(&s, &mut out), TAKE_OK);
        assert_eq!(out, [6u8; 16]);
    }

    #[test]
    fn take_rejects_empty_session() {
        let s = SiegelSession::new(8).unwrap();
        assert_eq!(take(&s, &mut [0u8; 8]), TAKE_ERR_WRONG_STATE);
        assert!(!s.is_consumed());
    }

    #[test]
    fn second_take_rejected() {
        let s = SiegelSession::new(8).unwrap();
        write(&s, &[7u8; 8]).unwrap();
        assert_eq!(take(&s, &mut [0u8; 8]), TAKE_OK);
        let mut again = [0u8; 8];
        assert_eq!(take(&s, &mut again), TAKE_ERR_WRONG_STATE);
        assert_eq!(again, [0u8; 8]);
    }

    #[test]
    fn take_rejected_after_obliviate() {
        let s = SiegelSession::new(8).unwrap();
        write(&s, &[8u8; 8]).unwrap();
        s.obliviate();
        assert_eq!(take(&s, &mut [0u8; 8]), TAKE_ERR_WRONG_STATE);
    }

    #[test]
    fn take_reports_canary_corruption_and_consumes() {
        use zeroize::Zeroize;

        let s = SiegelSession::new(8).unwrap();
        write(&s, &[0xEE; 8]).unwrap();
        // Zeroizing wipes the canary along with the data.
        if let SessionState::Loaded(siegel) = &mut *lock_state(&s.state) {
            siegel.zeroize();
        }
        let mut out = [0xFFu8; 8];
        assert_eq!(take(&s, &mut out), TAKE_ERR_CANARY);
        assert_eq!(out, [0u8; 8], "dst must be wiped on a failed take");
        assert!(s.is_consumed());
    }

    #[test]
    fn take_rejected_after_session_drop() {
        let handle = {
            let s = SiegelSession::new(8).unwrap();
            write(&s, &[9u8; 8]).unwrap();
            s.handle_id()
        };
        let mut out = [0u8; 8];
        let rc = unsafe { siegel_take(handle, out.as_mut_ptr(), 8) };
        assert_eq!(rc, TAKE_ERR_INVALID_HANDLE);
        assert_eq!(out, [0u8; 8]);
    }

    /// Sessions dropped on the foreign side should not pin the registry
    /// forever. The opportunistic prune in `new` makes capacity available
    /// again for subsequent allocations.
    #[test]
    fn registry_prunes_dropped_sessions() {
        let handle = {
            let s = SiegelSession::new(8).unwrap();
            s.handle_id()
        };
        // After the Arc is dropped, the registry entry's Weak has 0 strong
        // refs; the next `new` should prune it.
        let _next = SiegelSession::new(8).unwrap();
        let rc = unsafe { siegel_fill(handle, [0u8; 8].as_ptr(), 8) };
        assert_eq!(rc, FILL_ERR_INVALID_HANDLE);
    }
}
