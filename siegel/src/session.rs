//! Handle-based session layer for foreign bindings. Enables foreign code
//! to use [`crate::Siegel`]. This module is binding-framework agnostic.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, PoisonError, Weak};

use zeroize::Zeroize;

use crate::{Empty, Loaded, Siegel, SiegelError};

/// Type for the global registry of handles.
///
/// `Weak` is used instead of `Arc` so the session gets `Drop`ped after
/// the last foreign-held Arc is dropped (i.e. the registry does not hold an owning
/// reference).
type Registry = Mutex<HashMap<u64, Weak<SessionCore>>>;

/// Global registry of handles. Each [`SessionCore`] is initialized in this registry
/// so that Rust can enforce ownership of handles.
///
/// Siegels are filled and taken with raw pointers on the foreign side (see [`fill_into`]
/// and [`take_into`]), and without the registry, foreign code could pass garbage or
/// deallocated pointers. More commonly it avoids accidental use after drop.
static REGISTRY: LazyLock<Registry> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// Live [`Slot`]s: protected regions owned by sessions.
static HELD: AtomicUsize = AtomicUsize::new(0);

/// Max sessions holding protected memory at once. Consumed sessions don't
/// count, even while foreign code still holds them.
///
/// This bounds the number of regions, not bytes: at up to 1 MiB each, the cap
/// exceeds a typical `RLIMIT_MEMLOCK`, past which sessions keep working but
/// unlocked (see [`SessionCore::is_locked`]).
///
/// The cap is per-process and shared by every consumer of this module: a
/// process embedding a binding crate *and* calling [`SessionCore`] directly
/// draws from one budget.
pub const MAX_ACTIVE_SESSIONS: usize = 1024;

/// Max sessions alive at once, consumed or not. A consumed session holds no
/// protected memory, but one foreign code never releases still holds its
/// registry entry and binding object; past this bound a leak becomes a loud
/// refusal instead of unbounded growth.
pub const MAX_OPEN_SESSIONS: usize = 16 * MAX_ACTIVE_SESSIONS;

/// Maximum tries to get a non-collision handle. Extremely unlikely to ever occur.
const MAX_HANDLE_ALLOC_RETRIES: u8 = 8;

/// One-time secret-handling session. Binding crates wrap it in their own
/// exported type; foreign code never sees it directly.
///
/// The session wraps an internal `Siegel<Empty>` or `Siegel<Loaded>`
/// behind a `Mutex` so it can be driven through `&self` methods.
/// [`fill_into`] writes bytes into the empty siegel and transitions
/// the state to `Filled`; [`read_once`](Self::read_once)
/// runs the application operation against the loaded siegel and wipes.
/// In reverse, [`new_written`](Self::new_written) opens a `Written` session and
/// [`take_into`] copies it out and wipes.
pub struct SessionCore {
    state: Mutex<SessionState>,
    handle_id: u64,
    /// Bytes the session was allocated for. Stable for its lifetime.
    capacity: u32,
}

/// One unit of [`MAX_ACTIVE_SESSIONS`], travelling with the protected region it
/// accounts for and released when that region is dropped, including on unwind
/// and while a read holds it outside the state lock.
struct Slot(());

impl Slot {
    /// Callers must hold the registry lock and have checked the cap.
    fn acquire() -> Self {
        HELD.fetch_add(1, Ordering::Relaxed);
        Self(())
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        HELD.fetch_sub(1, Ordering::Relaxed);
    }
}

impl SessionCore {
    /// Open a new session sized for `len` bytes.
    ///
    /// Foreign code retrieves [`SessionCore::handle_id()`], fills the bytes,
    /// then calls the application's exported function (which internally
    /// invokes [`SessionCore::read_once`]).
    ///
    /// # Errors
    ///
    /// `SessionError::InvalidLength` for `len == 0` or `len > 1 MiB`.
    /// `SessionError::TooManyActiveSessions` if [`MAX_ACTIVE_SESSIONS`] sessions
    /// already hold protected memory, or [`MAX_OPEN_SESSIONS`] are alive.
    /// Allocation / protection errors propagate from `siegel`. A failed `mlock`
    /// does not fail the constructor; query [`is_locked`](Self::is_locked).
    pub fn new(len: u32) -> Result<Arc<Self>, SessionError> {
        // Logged here, after `register` released the registry lock: a
        // subscriber runs synchronously, and one that opens or drops a session
        // would re-enter that non-reentrant lock.
        let session = match Self::register(len) {
            Ok(session) => session,
            Err(e) => {
                #[cfg(feature = "tracing")]
                tracing::warn!(
                    target: "siegel",
                    capacity = len,
                    error = %e,
                    "opening a session failed"
                );
                return Err(e);
            }
        };

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

    fn register(len: u32) -> Result<Arc<Self>, SessionError> {
        let len_usize = usize::try_from(len).map_err(|_| SessionError::InvalidLength)?;

        // Hold the lock to the registry while allocation occurs to prevent
        // race conditions that could exceed the maximum active sessions.
        // Dropped sessions remove their own entry (see `Drop`), so the registry
        // holds only live or mid-drop ones.
        let mut registry = registry_lock();
        let held = HELD.load(Ordering::Relaxed);
        if held >= MAX_ACTIVE_SESSIONS || registry.len() >= MAX_OPEN_SESSIONS {
            // Checked before allocation to avoid wasting resources
            return Err(SessionError::TooManyActiveSessions {
                held,
                open: registry.len(),
            });
        }

        let empty = Siegel::<Empty>::new(len_usize)?;
        let handle_id = allocate_handle_id(&registry)?;

        // Acquired under the registry lock, so concurrent `new`s can't both pass
        // the check above. Releases may land anywhere; they only lower it.
        let session = Arc::new(Self {
            state: Mutex::new(SessionState::Empty(empty, Slot::acquire())),
            handle_id,
            capacity: len,
        });
        registry.insert(handle_id, Arc::downgrade(&session));
        Ok(session)
    }

    /// Opens a session whose secret `f` writes straight into protected memory
    /// (zeroed, full capacity), for foreign code to [`take_into`].
    ///
    /// If `f` fails, the bytes are zeroized, no session is returned and `f`'s
    /// error is the inner `Result`. Read any input session inside `f`, so the
    /// input isn't used up if the output can't be opened.
    ///
    /// # Errors
    /// Same as [`SessionCore::new`], plus protection errors.
    pub fn new_written<E, F>(len: u32, f: F) -> Result<Result<Arc<Self>, E>, SessionError>
    where
        F: FnOnce(&mut [u8]) -> Result<(), E>,
    {
        let session = Self::new(len)?;
        let mut state = lock_state(&session.state);
        let SessionState::Empty(empty, slot) =
            std::mem::replace(&mut *state, SessionState::Consumed)
        else {
            // Unreachable: `session` is fresh and unshared.
            return Err(SessionError::InvalidState);
        };

        let written = match empty.write_with(f)? {
            Ok(written) => written,
            // `write_with` already zeroized the partial output.
            Err(e) => return Ok(Err(e)),
        };
        *state = SessionState::Written(written, slot);
        drop(state);
        Ok(Ok(session))
    }

    /// Opaque identifier handle for [`fill_into`] and [`take_into`].
    ///
    /// Drawn from the OS CSPRNG to reduce the likelihood of
    /// accidental access by non-expected callers. It is not an infallible
    /// security guarantee. Stable for the lifetime of the session.
    #[must_use]
    pub fn handle_id(&self) -> u64 {
        self.handle_id
    }

    /// Capacity of the session in bytes.
    ///
    /// Stable for the lifetime of the session. Callers can use this to
    /// verify the session matches the size they expected before invoking
    /// [`read_once`](Self::read_once), or to size the buffer for [`take_into`].
    #[must_use]
    #[expect(clippy::len_without_is_empty, reason = "sessions are always non-empty")]
    pub fn len(&self) -> u32 {
        self.capacity
    }

    /// Wipe the session without using it. Idempotent.
    ///
    /// Frees the protected memory and the session's slot under
    /// [`MAX_ACTIVE_SESSIONS`]; the session object itself lives until the
    /// binding releases it.
    pub fn obliviate(&self) {
        let mut state = lock_state(&self.state);
        *state = SessionState::Consumed; // automatically drops any loaded `Siegel`
    }

    /// Whether the session has been consumed or otherwise reached
    /// a terminal error state.
    #[must_use]
    pub fn is_consumed(&self) -> bool {
        matches!(*lock_state(&self.state), SessionState::Consumed)
    }

    /// Whether the secret's pages are locked in RAM (`mlock`ed).
    ///
    /// Always `false` once consumed: the session holds no pages to lock.
    #[must_use]
    pub fn is_locked(&self) -> bool {
        match &*lock_state(&self.state) {
            SessionState::Empty(s, _) => s.is_locked(),
            SessionState::Filled(s, _) | SessionState::Written(s, _) => s.is_locked(),
            SessionState::Consumed => false,
        }
    }

    /// Copy `bytes` into the session's siegel and seal it.
    ///
    /// The bytes are read straight from the caller's buffer into protected
    /// memory: no intermediate allocation, so the caller keeps sole ownership
    /// of the plaintext copy and can wipe it as soon as this returns.
    ///
    /// # Errors
    /// - `InvalidState` if the session was already filled or consumed.
    /// - `LengthMismatch` if `bytes.len()` doesn't equal the session's capacity.
    /// - Protection errors propagate from `siegel`.
    pub fn fill(&self, bytes: &[u8]) -> Result<(), SessionError> {
        self.fill_with(bytes.len(), || bytes)
    }

    /// Fill from `bytes()`, called only once the state and `len` check out, so
    /// a raw caller's wrong length never becomes a slice. The checks and the
    /// write share one critical section.
    fn fill_with<'a>(
        &self,
        len: usize,
        bytes: impl FnOnce() -> &'a [u8],
    ) -> Result<(), SessionError> {
        let mut state = lock_state(&self.state);

        // Set a `Consumed` state to ensure it is persisted on errors.
        let (empty, slot) = match std::mem::replace(&mut *state, SessionState::Consumed) {
            SessionState::Empty(s, slot) => (s, slot),
            other => {
                *state = other;
                return Err(SessionError::InvalidState);
            }
        };

        if empty.len() != len {
            *state = SessionState::Empty(empty, slot);
            return Err(SessionError::LengthMismatch);
        }

        match empty.write(bytes()) {
            Ok(loaded) => {
                *state = SessionState::Filled(loaded, slot);
                Ok(())
            }
            Err(e) => {
                // Siegel::write consumed `empty` on failure. State stays Consumed.
                // Release before emitting, for the same reason as in `new`.
                drop(state);
                drop(slot);
                #[cfg(feature = "tracing")]
                tracing::warn!(
                    target: "siegel",
                    error = %e,
                    "sealing the secret failed; session is unusable"
                );
                Err(SessionError::from(e))
            }
        }
    }

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
        let (loaded, slot) = {
            let mut state = lock_state(&self.state);
            match std::mem::replace(&mut *state, SessionState::Consumed) {
                SessionState::Filled(s, slot) | SessionState::Written(s, slot) => (s, slot),
                SessionState::Empty(s, slot) => {
                    *state = SessionState::Empty(s, slot);
                    return Err(SessionError::InvalidState);
                }
                SessionState::Consumed => return Err(SessionError::Consumed),
            }
        };

        let result = loaded.read_once(f);
        drop(slot);
        #[cfg(feature = "tracing")]
        if let Err(e) = &result {
            tracing::warn!(target: "siegel", error = %e, "reading the secret failed");
        }
        result.map_err(SessionError::from)
    }

    /// Copy a `Written` secret into `dst`, then wipe and consume the session.
    ///
    /// # Safety
    /// Same as [`take_into`].
    unsafe fn take(&self, dst: *mut u8, len: usize) -> i32 {
        let mut state = lock_state(&self.state);

        let (written, slot) = match std::mem::replace(&mut *state, SessionState::Consumed) {
            SessionState::Written(s, slot) => (s, slot),
            other => {
                *state = other;
                return TAKE_ERR_WRONG_STATE;
            }
        };

        if written.len() != len {
            *state = SessionState::Written(written, slot);
            return TAKE_ERR_LEN_MISMATCH;
        }

        // SAFETY: the caller guarantees `dst`; `len == bytes.len()` was checked above.
        let copied = written.read_once(|bytes| unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), dst, len);
        });
        // Siegel::read_once consumed `written` either way. State stays Consumed.
        drop(state);
        drop(slot);

        let Err(e) = copied else {
            return TAKE_OK;
        };

        // A failed reseal surfaces after the copy ran, so wipe `dst` with writes the
        // compiler can't drop. `MaybeUninit` because on other errors `dst` was never written.
        // SAFETY: the caller guarantees `dst` is valid and ours alone for `len` bytes.
        unsafe { std::slice::from_raw_parts_mut(dst.cast::<std::mem::MaybeUninit<u8>>(), len) }
            .zeroize();

        #[cfg(feature = "tracing")]
        tracing::error!(target: "siegel", error = %e, "take failed; destination wiped");

        match e {
            SiegelError::CanaryCorrupted => TAKE_ERR_CANARY,
            SiegelError::ProtectionFailed { .. }
            | SiegelError::AllocationFailed { .. }
            | SiegelError::InvalidLength
            | SiegelError::LengthMismatch { .. } => TAKE_ERR_PROTECTION,
        }
    }
}

impl Drop for SessionCore {
    fn drop(&mut self) {
        // The entry's id can't have been reissued: ids are only freed here.
        registry_lock().remove(&self.handle_id);
    }
}

pub const FILL_OK: i32 = 0;
pub const FILL_ERR_INVALID_HANDLE: i32 = -1;
pub const FILL_ERR_LEN_MISMATCH: i32 = -2;
pub const FILL_ERR_NULL_SRC: i32 = -3;
pub const FILL_ERR_WRONG_STATE: i32 = -4;
pub const FILL_ERR_PROTECTION: i32 = -5;

/// Fills the session's `Siegel` with the actual data.
///
/// Copies `len` bytes from `src` raw pointer into the session's siegel.
///
/// Binding crates re-export this behind their own `#[no_mangle] extern "C"`
/// symbol. It is deliberately *not* `#[no_mangle]` here so that linking
/// `siegel` never places an FFI symbol in a consumer's binary.
///
/// # Arguments
/// - `handle`: the opaque handler received from [`SessionCore::handle_id()`].
/// - `src`: the raw pointer to the bytes to copy.
/// - `len`: the size of the data.
///
/// # Safety
///
/// - `src` **MUST** be valid for `len` bytes of read. This is the caller's responsibility.
/// - Nothing may write to `src` until the call returns.
///
/// # Concurrency
///
/// Fills racing `read_once` or `obliviate` on the same session are memory-safe,
/// but which one wins is arbitrary.
#[must_use]
pub unsafe fn fill_into(handle: u64, src: *const u8, len: usize) -> i32 {
    if src.is_null() {
        return rejected("fill", FILL_ERR_NULL_SRC);
    }

    // Ensure the provided handle is valid
    let Some(session) = lookup(handle) else {
        return rejected("fill", FILL_ERR_INVALID_HANDLE);
    };

    // SAFETY: only reached once `len` equals the capacity, and the caller
    // guarantees `src` is valid for `len` bytes.
    match session.fill_with(len, || unsafe { std::slice::from_raw_parts(src, len) }) {
        Ok(()) => FILL_OK,
        Err(SessionError::LengthMismatch) => rejected("fill", FILL_ERR_LEN_MISMATCH),
        Err(SessionError::InvalidState) => rejected("fill", FILL_ERR_WRONG_STATE),
        // `fill_with` logged the cause.
        Err(_) => FILL_ERR_PROTECTION,
    }
}

pub const TAKE_OK: i32 = 0;
pub const TAKE_ERR_INVALID_HANDLE: i32 = -1;
pub const TAKE_ERR_LEN_MISMATCH: i32 = -2;
pub const TAKE_ERR_NULL_DST: i32 = -3;
pub const TAKE_ERR_WRONG_STATE: i32 = -4;
pub const TAKE_ERR_PROTECTION: i32 = -5;
pub const TAKE_ERR_CANARY: i32 = -6;

/// Copies a secret Rust wrote with [`SessionCore::new_written`] into `dst`,
/// then wipes and consumes the session. The reverse of [`fill_into`], skipping
/// the binding generator's non-zeroized copies. Zeroizing `dst` is the caller's job.
///
/// A session filled by [`fill_into`] can't be taken (`TAKE_ERR_WRONG_STATE`).
/// On [`TAKE_ERR_CANARY`] and [`TAKE_ERR_PROTECTION`] `dst` is zeroed; other
/// errors leave it untouched.
///
/// Like [`fill_into`], binding crates re-export this behind their own symbol.
///
/// # Arguments
/// - `handle`: from [`SessionCore::handle_id()`].
/// - `dst`: where to copy the bytes.
/// - `len`: size of `dst`; must equal the session's capacity.
///
/// # Safety
///
/// - `dst` **MUST** be valid for reads and writes of `len` bytes, and not accessed
///   elsewhere until the call returns.
/// - `dst` must not overlap the session's protected memory.
#[must_use]
pub unsafe fn take_into(handle: u64, dst: *mut u8, len: usize) -> i32 {
    if dst.is_null() {
        return rejected("take", TAKE_ERR_NULL_DST);
    }

    let Some(session) = lookup(handle) else {
        return rejected("take", TAKE_ERR_INVALID_HANDLE);
    };

    // SAFETY: forwarded verbatim; the caller upholds this function's contract.
    match unsafe { session.take(dst, len) } {
        code @ (TAKE_ERR_LEN_MISMATCH | TAKE_ERR_WRONG_STATE) => rejected("take", code),
        // Success, or a failure `take` already logged.
        code => code,
    }
}

/// Log a rejected raw call: foreign code only sees the bare code.
#[cfg_attr(
    not(feature = "tracing"),
    expect(unused_variables, reason = "`op` is only logged")
)]
fn rejected(op: &'static str, code: i32) -> i32 {
    #[cfg(feature = "tracing")]
    tracing::warn!(target: "siegel", op, code, "raw session call rejected");
    code
}

/// Resolve a live session from its handle.
fn lookup(handle: u64) -> Option<Arc<SessionCore>> {
    registry_lock().get(&handle).and_then(Weak::upgrade)
}

/// The state of the session. Every state holding a region holds its [`Slot`].
enum SessionState {
    Empty(Siegel<Empty>, Slot),
    /// Filled by foreign code ([`fill_into`]); only Rust can read it.
    Filled(Siegel<Loaded>, Slot),
    /// Written by Rust ([`SessionCore::new_written`]); Rust or [`take_into`] can read it.
    Written(Siegel<Loaded>, Slot),
    Consumed,
}

/// Acquire the registry lock
fn registry_lock() -> MutexGuard<'static, HashMap<u64, Weak<SessionCore>>> {
    REGISTRY.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Generate a random handle ID.
///
/// - The caller must already hold the lock to the registry.
/// - `0` is reserved.
fn allocate_handle_id(registry: &HashMap<u64, Weak<SessionCore>>) -> Result<u64, SessionError> {
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
#[derive(Debug, thiserror::Error)]
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
        "too many sessions: {held} holding secrets (max {MAX_ACTIVE_SESSIONS}), {open} open \
         (max {MAX_OPEN_SESSIONS}); are sessions being released?"
    )]
    TooManyActiveSessions { held: usize, open: usize },
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

/// Resolve a registry handle to a [`SessionCore`] reference. Used by `test_utils` only.
#[cfg(feature = "test-utils")]
#[must_use]
pub fn lookup_session(handle: u64) -> Option<Arc<SessionCore>> {
    lookup(handle)
}

#[cfg(feature = "test-utils")]
impl SessionCore {
    /// Touch the active siegel's front guard page. No-op if consumed.
    ///
    /// # Safety
    /// Intentionally crashes the process. Forked child only.
    pub unsafe fn test_touch_front_guard(&self) {
        let state = lock_state(&self.state);
        match &*state {
            SessionState::Empty(s, _) => unsafe { s.test_touch_front_guard() },
            SessionState::Filled(s, _) | SessionState::Written(s, _) => unsafe {
                s.test_touch_front_guard();
            },
            SessionState::Consumed => {}
        }
    }
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "tests")]
mod tests {
    use std::convert::Infallible;

    use sha2::{Digest, Sha256};

    use super::*;

    fn fill(session: &Arc<SessionCore>, bytes: &[u8]) -> i32 {
        unsafe { fill_into(session.handle_id(), bytes.as_ptr(), bytes.len()) }
    }

    fn take(session: &Arc<SessionCore>, dst: &mut [u8]) -> i32 {
        unsafe { take_into(session.handle_id(), dst.as_mut_ptr(), dst.len()) }
    }

    /// Non-secret filler that rejected takes must leave in `dst`.
    const SENTINEL: u8 = 0xA5;

    fn written(bytes: &[u8]) -> Arc<SessionCore> {
        let len = u32::try_from(bytes.len()).unwrap();
        let Ok(session) = SessionCore::new_written(len, |buf| {
            buf.copy_from_slice(bytes);
            Ok::<_, Infallible>(())
        })
        .unwrap();
        session
    }

    #[test]
    fn begin_session_starts_empty() {
        let s = SessionCore::new(32).unwrap();
        assert!(!s.is_consumed());
        assert_eq!(s.handle_id(), s.handle_id());
    }

    #[test]
    fn begin_session_rejects_zero_length() {
        assert!(SessionCore::new(0).is_err());
    }

    #[test]
    fn fill_then_consume_with_arbitrary_closure() {
        let secret = vec![0x42; 16];
        let s = SessionCore::new(16).unwrap();
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
        let s = SessionCore::new(8).unwrap();
        let rc = unsafe { fill_into(s.handle_id(), std::ptr::null(), 8) };
        assert_eq!(rc, FILL_ERR_NULL_SRC);
    }

    #[test]
    fn fill_rejects_unknown_handle() {
        let bytes = [0u8; 8];
        let rc = unsafe { fill_into(127, bytes.as_ptr(), 8) };
        assert_eq!(rc, FILL_ERR_INVALID_HANDLE);
    }

    #[test]
    fn fill_rejects_wrong_length() {
        let s = SessionCore::new(16).unwrap();
        assert_eq!(fill(&s, &[0u8; 8]), FILL_ERR_LEN_MISMATCH);
        assert_eq!(fill(&s, &[0u8; 16]), FILL_OK);
    }

    #[test]
    fn double_fill_rejected() {
        let s = SessionCore::new(8).unwrap();
        assert_eq!(fill(&s, &[1u8; 8]), FILL_OK);
        assert_eq!(fill(&s, &[1u8; 8]), FILL_ERR_WRONG_STATE);
    }

    #[test]
    fn use_rejects_empty_session() {
        let s = SessionCore::new(8).unwrap();
        let err = s.read_once(<[u8]>::to_vec).unwrap_err();
        assert!(matches!(err, SessionError::InvalidState));
        assert!(!s.is_consumed());
        assert_eq!(fill(&s, &[2u8; 8]), FILL_OK);
        assert!(s.read_once(<[u8]>::to_vec).is_ok());
    }

    #[test]
    fn use_rejects_consumed_session() {
        let s = SessionCore::new(8).unwrap();
        assert_eq!(fill(&s, &[3u8; 8]), FILL_OK);
        s.read_once(<[u8]>::to_vec).unwrap();
        let err = s.read_once(<[u8]>::to_vec).unwrap_err();
        assert!(matches!(err, SessionError::Consumed));
    }

    #[test]
    fn fill_rejected_after_consume() {
        let s = SessionCore::new(8).unwrap();
        assert_eq!(fill(&s, &[4u8; 8]), FILL_OK);
        s.read_once(<[u8]>::to_vec).unwrap();
        assert_eq!(fill(&s, &[4u8; 8]), FILL_ERR_WRONG_STATE);
    }

    #[test]
    fn obliviate_wipes() {
        let s = SessionCore::new(16).unwrap();
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
            let s = SessionCore::new(8).unwrap();
            s.handle_id()
        };
        let rc = unsafe { fill_into(handle, [0u8; 8].as_ptr(), 8) };
        assert_eq!(rc, FILL_ERR_INVALID_HANDLE);
    }

    #[test]
    fn distinct_sessions_have_distinct_handles() {
        let a = SessionCore::new(8).unwrap();
        let b = SessionCore::new(8).unwrap();
        assert_ne!(a.handle_id(), b.handle_id());
    }

    #[test]
    fn handles_are_not_sequential() {
        let sessions: Vec<_> = (0..16).map(|_| SessionCore::new(8).unwrap()).collect();
        let ids: Vec<u64> = sessions.iter().map(|s| s.handle_id()).collect();
        assert!(ids.iter().all(|&id| id != 0), "0 is reserved");
        assert!(
            ids.iter().any(|&id| id > u64::from(u32::MAX)),
            "expected at least one handle in the high u64 range, got {ids:?}",
        );
    }

    #[test]
    fn len_reports_allocated_capacity() {
        let s = SessionCore::new(64).unwrap();
        assert_eq!(s.len(), 64);
        // Capacity stays stable across state transitions.
        assert_eq!(fill(&s, &[7u8; 64]), FILL_OK);
        assert_eq!(s.len(), 64);
        s.read_once(<[u8]>::to_vec).unwrap();
        assert_eq!(s.len(), 64);
    }

    #[test]
    fn is_locked_reports_live_and_consumed_sessions() {
        let s = SessionCore::new(8).unwrap();
        let empty_locked = s.is_locked();
        assert_eq!(fill(&s, &[1u8; 8]), FILL_OK);
        // The region is allocated once, so filling can't change its lock.
        assert_eq!(s.is_locked(), empty_locked);
        s.obliviate();
        assert!(!s.is_locked());
    }

    #[test]
    fn wrong_state_outranks_length_mismatch() {
        let s = SessionCore::new(8).unwrap();
        assert_eq!(fill(&s, &[1u8; 8]), FILL_OK);
        s.read_once(<[u8]>::to_vec).unwrap();
        // Consumed *and* the wrong length: the state must win.
        let rc = unsafe { fill_into(s.handle_id(), [0u8; 4].as_ptr(), 4) };
        assert_eq!(rc, FILL_ERR_WRONG_STATE);
    }

    #[test]
    fn concurrent_fills_succeed_exactly_once() {
        const THREADS: usize = 8;
        let s = SessionCore::new(8).unwrap();
        let handle = s.handle_id();
        let barrier = Arc::new(std::sync::Barrier::new(THREADS));

        let threads: Vec<_> = (0..THREADS)
            .map(|i| {
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    let byte = u8::try_from(i).unwrap();
                    let bytes = [byte; 8];
                    barrier.wait();
                    (byte, unsafe { fill_into(handle, bytes.as_ptr(), 8) })
                })
            })
            .collect();

        let mut winners = Vec::new();
        for t in threads {
            let (byte, rc) = t.join().unwrap();
            if rc == FILL_OK {
                winners.push(byte);
            } else {
                assert_eq!(rc, FILL_ERR_WRONG_STATE);
            }
        }
        assert_eq!(winners.len(), 1);
        assert_eq!(s.read_once(<[u8]>::to_vec).unwrap(), vec![winners[0]; 8]);
    }

    #[test]
    fn safe_fill_matches_raw_fill() {
        let s = SessionCore::new(16).unwrap();
        s.fill(&[9u8; 16]).unwrap();
        let copy = s.read_once(<[u8]>::to_vec).unwrap();
        assert_eq!(copy, vec![9u8; 16]);
    }

    #[test]
    fn safe_fill_rejects_wrong_length() {
        let s = SessionCore::new(16).unwrap();
        let err = s.fill(&[0u8; 8]).unwrap_err();
        assert!(matches!(err, SessionError::LengthMismatch));
        // The session stays usable after a rejected fill.
        s.fill(&[1u8; 16]).unwrap();
    }

    #[test]
    fn safe_fill_rejects_double_fill() {
        let s = SessionCore::new(8).unwrap();
        s.fill(&[1u8; 8]).unwrap();
        let err = s.fill(&[1u8; 8]).unwrap_err();
        assert!(matches!(err, SessionError::InvalidState));
    }

    #[test]
    fn safe_fill_rejects_consumed_session() {
        let s = SessionCore::new(8).unwrap();
        s.fill(&[1u8; 8]).unwrap();
        s.read_once(<[u8]>::to_vec).unwrap();
        let err = s.fill(&[1u8; 8]).unwrap_err();
        assert!(matches!(err, SessionError::InvalidState));
    }

    #[cfg(feature = "test-utils")]
    #[test]
    fn fill_wipes_and_consumes_when_seal_fails() {
        let s = SessionCore::new(8).unwrap();
        crate::test_utils::fail_next_reseal();
        assert_eq!(fill(&s, &[1u8; 8]), FILL_ERR_PROTECTION);
        assert!(s.is_consumed());
    }

    #[test]
    fn written_then_take_round_trips() {
        let s = written(&Sha256::digest(b"seed"));
        assert_eq!(s.len(), 32);
        assert!(!s.is_consumed());

        let mut out = [SENTINEL; 32];
        assert_eq!(take(&s, &mut out), TAKE_OK);
        assert_eq!(out.to_vec(), Sha256::digest(b"seed").to_vec());
        assert!(s.is_consumed());
    }

    #[test]
    fn new_written_can_derive_from_an_input_session() {
        let input = SessionCore::new(16).unwrap();
        assert_eq!(fill(&input, &[0x42; 16]), FILL_OK);
        let output = SessionCore::new_written(32, |buf| {
            input.read_once(|seed| buf.copy_from_slice(&Sha256::digest(seed)))
        })
        .unwrap()
        .unwrap();
        assert!(input.is_consumed());

        let digest = output.read_once(<[u8]>::to_vec).unwrap();
        assert_eq!(digest, Sha256::digest([0x42; 16]).to_vec());
    }

    #[test]
    fn new_written_returns_closure_error_and_no_session() {
        let input = SessionCore::new(8).unwrap();
        let inner = SessionCore::new_written(8, |buf| {
            buf.fill(0xAA);
            input.read_once(|seed| buf.copy_from_slice(seed))
        })
        .unwrap();
        assert!(matches!(inner, Err(SessionError::InvalidState)));
        assert!(!input.is_consumed());
    }

    #[test]
    #[expect(clippy::panic, reason = "deliberately panicking inside the closure")]
    fn new_written_panic_leaves_sessions_usable() {
        let result = std::panic::catch_unwind(|| {
            SessionCore::new_written(8, |_| -> Result<(), Infallible> {
                panic!("derivation failed")
            })
        });
        assert!(result.is_err());
        let s = SessionCore::new(8).unwrap();
        assert_eq!(fill(&s, &[1u8; 8]), FILL_OK);
        assert_eq!(s.read_once(<[u8]>::to_vec).unwrap(), vec![1u8; 8]);
    }

    #[test]
    fn new_written_rejects_zero_length() {
        let result = SessionCore::new_written(0, |_| Ok::<_, Infallible>(()));
        assert!(matches!(result, Err(SessionError::InvalidLength)));
    }

    #[test]
    fn fill_rejects_written_session() {
        let s = written(&[3u8; 8]);
        assert_eq!(fill(&s, &[4u8; 8]), FILL_ERR_WRONG_STATE);
        let mut out = [SENTINEL; 8];
        assert_eq!(take(&s, &mut out), TAKE_OK);
        assert_eq!(out, [3u8; 8]);
    }

    #[test]
    fn take_rejects_null_dst() {
        let s = written(&[5u8; 8]);
        let rc = unsafe { take_into(s.handle_id(), std::ptr::null_mut(), 8) };
        assert_eq!(rc, TAKE_ERR_NULL_DST);
        assert!(!s.is_consumed());
    }

    #[test]
    fn take_rejects_unknown_handle() {
        let mut out = [SENTINEL; 8];
        let rc = unsafe { take_into(127, out.as_mut_ptr(), 8) };
        assert_eq!(rc, TAKE_ERR_INVALID_HANDLE);
        assert_eq!(out, [SENTINEL; 8]);
    }

    #[test]
    fn take_rejects_wrong_length_and_recovers() {
        let s = written(&[6u8; 16]);
        let mut short = [SENTINEL; 8];
        assert_eq!(take(&s, &mut short), TAKE_ERR_LEN_MISMATCH);
        assert_eq!(short, [SENTINEL; 8]);
        let mut out = [SENTINEL; 16];
        assert_eq!(take(&s, &mut out), TAKE_OK);
        assert_eq!(out, [6u8; 16]);
    }

    /// A leaked or mixed-up handle must not read back a secret foreign code filled.
    #[test]
    fn take_rejects_filled_session() {
        let s = SessionCore::new(8).unwrap();
        assert_eq!(fill(&s, &[0x5E; 8]), FILL_OK);
        let mut out = [SENTINEL; 8];
        assert_eq!(take(&s, &mut out), TAKE_ERR_WRONG_STATE);
        assert_eq!(out, [SENTINEL; 8]);
        assert_eq!(s.read_once(<[u8]>::to_vec).unwrap(), vec![0x5E; 8]);
    }

    #[test]
    fn concurrent_takes_succeed_exactly_once() {
        const THREADS: usize = 8;
        let s = written(&[0x5A; 8]);
        let handle = s.handle_id();
        let barrier = Arc::new(std::sync::Barrier::new(THREADS));

        let mut threads = Vec::new();
        for i in 0..THREADS {
            let barrier = Arc::clone(&barrier);
            // Odd threads race a wrong-length take against the real ones.
            let len = if i % 2 == 0 { 8 } else { 4 };
            threads.push(std::thread::spawn(move || {
                let mut out = vec![SENTINEL; len];
                barrier.wait();
                let rc = unsafe { take_into(handle, out.as_mut_ptr(), len) };
                (len, rc, out)
            }));
        }

        let mut taken = 0;
        for t in threads {
            let (len, rc, out) = t.join().unwrap();
            if rc == TAKE_OK {
                taken += 1;
                assert_eq!(out, vec![0x5A; 8]);
                continue;
            }
            assert!(out.iter().all(|&b| b == SENTINEL));
            if len == 8 {
                assert_eq!(rc, TAKE_ERR_WRONG_STATE);
            } else {
                assert!(rc == TAKE_ERR_LEN_MISMATCH || rc == TAKE_ERR_WRONG_STATE);
            }
        }
        assert_eq!(taken, 1);
        assert!(s.is_consumed());
    }

    #[test]
    fn take_rejects_empty_session() {
        let s = SessionCore::new(8).unwrap();
        let mut out = [SENTINEL; 8];
        assert_eq!(take(&s, &mut out), TAKE_ERR_WRONG_STATE);
        assert_eq!(out, [SENTINEL; 8]);
        assert!(!s.is_consumed());
    }

    #[test]
    fn second_take_rejected() {
        let s = written(&[7u8; 8]);
        assert_eq!(take(&s, &mut [0u8; 8]), TAKE_OK);
        let mut again = [SENTINEL; 8];
        assert_eq!(take(&s, &mut again), TAKE_ERR_WRONG_STATE);
        assert_eq!(again, [SENTINEL; 8]);
    }

    #[test]
    fn take_rejected_after_obliviate() {
        let s = written(&[8u8; 8]);
        s.obliviate();
        let mut out = [SENTINEL; 8];
        assert_eq!(take(&s, &mut out), TAKE_ERR_WRONG_STATE);
        assert_eq!(out, [SENTINEL; 8]);
    }

    #[test]
    fn take_reports_canary_corruption_and_consumes() {
        let s = written(&[0xEE; 8]);
        // Zeroizing wipes the canary along with the data.
        if let SessionState::Written(siegel, _) = &mut *lock_state(&s.state) {
            siegel.zeroize();
        }
        let mut out = [SENTINEL; 8];
        assert_eq!(take(&s, &mut out), TAKE_ERR_CANARY);
        assert_eq!(out, [0u8; 8], "dst must be wiped on a failed take");
        assert!(s.is_consumed());
    }

    #[cfg(feature = "test-utils")]
    #[test]
    fn take_wipes_dst_when_reseal_fails_after_copy() {
        let s = written(&[0xC3; 8]);
        let mut out = [SENTINEL; 8];
        crate::test_utils::fail_next_reseal();
        assert_eq!(take(&s, &mut out), TAKE_ERR_PROTECTION);
        assert_eq!(out, [0u8; 8], "the copied secret must be wiped");
        assert!(s.is_consumed());
    }

    #[test]
    fn take_rejected_after_session_drop() {
        let handle = {
            let s = written(&[9u8; 8]);
            s.handle_id()
        };
        let mut out = [SENTINEL; 8];
        let rc = unsafe { take_into(handle, out.as_mut_ptr(), 8) };
        assert_eq!(rc, TAKE_ERR_INVALID_HANDLE);
        assert_eq!(out, [SENTINEL; 8]);
    }
}
