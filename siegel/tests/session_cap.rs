//! Session-cap behaviour, in its own test binary.
//!
//! Saturating the registry is only meaningful with exclusive access to it: the
//! registry is process-global, so running this alongside the unit tests would
//! starve them of sessions and fail them nondeterministically. A separate
//! integration binary gets its own process, which also lets the assertions be
//! exact rather than bounded.
#![cfg(feature = "session")]

use siegel::session::{MAX_ACTIVE_SESSIONS, SessionCore, SessionError};

#[test]
fn cap_is_enforced_at_exactly_max_active_sessions() {
    let mut sessions = Vec::new();
    let err = loop {
        match SessionCore::new(1) {
            Ok(s) => sessions.push(s),
            Err(e) => break e,
        }
    };

    assert!(matches!(err, SessionError::TooManyActiveSessions));
    assert_eq!(
        sessions.len(),
        MAX_ACTIVE_SESSIONS,
        "rejection must happen exactly at the cap"
    );

    // Dropping releases the Arcs; the next `new` prunes the dead entries.
    sessions.clear();
    assert!(
        SessionCore::new(1).is_ok(),
        "capacity should be reclaimed after drop"
    );
}
