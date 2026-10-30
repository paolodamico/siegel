//! Session-cap behaviour, in its own test binary.
//!
//! Saturating the registry is only meaningful with exclusive access to it: the
//! registry is process-global, so running this alongside the unit tests would
//! starve them of sessions and fail them nondeterministically. A separate
//! integration binary gets its own process, which also lets the assertions be
//! exact rather than bounded. Both tests saturate the cap, so they serialize.
#![cfg(feature = "session")]
#![expect(clippy::unwrap_used, reason = "tests")]

use std::sync::{Arc, Mutex};

use siegel::session::{MAX_ACTIVE_SESSIONS, SessionCore, SessionError};

static CAP: Mutex<()> = Mutex::new(());

/// Open sessions until the cap, asserting the refusal lands exactly on it.
fn saturate() -> Vec<Arc<SessionCore>> {
    let sessions: Vec<_> = (0..MAX_ACTIVE_SESSIONS)
        .map(|_| SessionCore::new(1).unwrap())
        .collect();
    assert!(
        matches!(
            SessionCore::new(1),
            Err(SessionError::TooManyActiveSessions)
        ),
        "rejection must happen exactly at the cap"
    );
    sessions
}

#[test]
fn cap_is_enforced_at_exactly_max_active_sessions() {
    let _cap = CAP.lock().unwrap();
    let mut sessions = saturate();

    // Dropping releases the Arcs; the next `new` prunes the dead entries.
    sessions.clear();
    assert!(
        SessionCore::new(1).is_ok(),
        "capacity should be reclaimed after drop"
    );
}

/// A consumed session holds no protected memory, so it must not hold a slot
/// either, even while foreign code keeps it alive.
#[test]
fn consumed_sessions_free_their_slot() {
    let _cap = CAP.lock().unwrap();
    let sessions = saturate();

    sessions[0].obliviate();
    let filled = &sessions[1];
    filled.fill(&[1]).unwrap();
    filled.read_once(|_| ()).unwrap();

    let reopened: Vec<_> = (0..2).map(|_| SessionCore::new(1).unwrap()).collect();
    assert!(
        matches!(
            SessionCore::new(1),
            Err(SessionError::TooManyActiveSessions)
        ),
        "only the two consumed slots should have been freed"
    );
    drop(reopened);
}
