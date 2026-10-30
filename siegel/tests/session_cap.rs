//! Session-cap behaviour, in its own test binary.
//!
//! Saturating the registry is only meaningful with exclusive access to it: the
//! registry is process-global, so running this alongside the unit tests would
//! starve them of sessions and fail them nondeterministically. A separate
//! integration binary gets its own process, which also lets the assertions be
//! exact rather than bounded. Every test saturates the cap, so they serialize.
#![cfg(feature = "session")]
#![expect(clippy::unwrap_used, clippy::panic, reason = "tests")]

use std::convert::Infallible;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use siegel::session::{MAX_ACTIVE_SESSIONS, MAX_OPEN_SESSIONS, SessionCore, SessionError, TAKE_OK};

static CAP: Mutex<()> = Mutex::new(());

/// Exclusive use of the cap. Poison is ignored so one failure doesn't mask the rest.
fn exclusive() -> MutexGuard<'static, ()> {
    CAP.lock().unwrap_or_else(PoisonError::into_inner)
}

fn refused() -> bool {
    matches!(
        SessionCore::new(1),
        Err(SessionError::TooManyActiveSessions { .. })
    )
}

/// Open sessions until refused. Bounded, so a broken cap fails instead of
/// allocating until the OS gives up.
fn fill_up() -> Vec<Arc<SessionCore>> {
    let mut sessions = Vec::new();
    for _ in 0..=MAX_ACTIVE_SESSIONS {
        match SessionCore::new(1) {
            Ok(s) => sessions.push(s),
            Err(SessionError::TooManyActiveSessions { .. }) => return sessions,
            Err(e) => panic!("unexpected error: {e}"),
        }
    }
    panic!("no refusal after {} sessions", MAX_ACTIVE_SESSIONS + 1)
}

/// Exactly `n` slots are free: `n` opens succeed and the next is refused.
fn assert_free_slots(n: usize) {
    let opened: Vec<_> = (0..n).map(|_| SessionCore::new(1).unwrap()).collect();
    assert!(refused(), "expected exactly {n} free slot(s)");
    drop(opened);
}

fn written(byte: u8) -> Arc<SessionCore> {
    let Ok(session) = SessionCore::new_written(1, |buf| {
        buf[0] = byte;
        Ok::<_, Infallible>(())
    })
    .unwrap();
    session
}

#[test]
fn cap_is_enforced_at_exactly_max_active_sessions() {
    let _cap = exclusive();
    let mut sessions = fill_up();
    assert_eq!(sessions.len(), MAX_ACTIVE_SESSIONS);

    // Dropping releases the Arcs, and each session removes its own entry.
    sessions.clear();
    assert!(
        SessionCore::new(1).is_ok(),
        "capacity should be reclaimed after drop"
    );
}

/// A consumed session holds no protected memory, so it must not hold a slot
/// either, even while foreign code keeps it alive. Covers every consuming path.
#[test]
fn consuming_paths_free_their_slot() {
    let _cap = exclusive();
    let taken = written(1);
    let sessions = fill_up();

    sessions[0].obliviate();
    sessions[1].fill(&[1]).unwrap();
    sessions[1].read_once(|_| ()).unwrap();
    sessions[2].fill(&[1]).unwrap();
    let panicked = std::panic::catch_unwind(|| sessions[2].read_once(|_| panic!("boom")));
    assert!(panicked.is_err());
    let mut out = [0u8; 1];
    assert_eq!(
        unsafe { siegel::session::take_into(taken.handle_id(), out.as_mut_ptr(), 1) },
        TAKE_OK
    );
    assert_free_slots(4);

    // A failed `new_written` gives back the slot it took.
    assert!(matches!(
        SessionCore::new_written(1, |_| Err("derivation failed")),
        Ok(Err("derivation failed"))
    ));
    let panicked = std::panic::catch_unwind(|| {
        SessionCore::new_written(1, |_| -> Result<(), Infallible> { panic!("boom") })
    });
    assert!(panicked.is_err());
    assert_free_slots(4);
    drop((sessions, taken));
}

#[cfg(feature = "test-utils")]
#[test]
fn failed_seals_free_their_slot() {
    let _cap = exclusive();
    let taken = written(1);
    let sessions = fill_up();

    siegel::test_utils::fail_next_reseal();
    assert!(sessions[0].fill(&[1]).is_err());
    let mut out = [0u8; 1];
    siegel::test_utils::fail_next_reseal();
    assert_ne!(
        unsafe { siegel::session::take_into(taken.handle_id(), out.as_mut_ptr(), 1) },
        TAKE_OK
    );
    assert_free_slots(2);

    // `new_written` takes a free slot, then fails sealing and gives it back.
    siegel::test_utils::fail_next_reseal();
    assert!(SessionCore::new_written(1, |_| Ok::<_, Infallible>(())).is_err());
    assert_free_slots(2);
}

/// A read holds the region outside the state lock, so obliviating mid-read must
/// not free the slot before the region is gone.
#[test]
fn in_flight_read_keeps_its_slot() {
    let _cap = exclusive();
    let sessions = fill_up();
    let s = &sessions[0];
    s.fill(&[1]).unwrap();
    s.read_once(|_| {
        s.obliviate();
        assert_free_slots(0);
    })
    .unwrap();
    assert_free_slots(1);
}

/// Consumed sessions that are never released are bounded too, or a leak
/// would grow the registry every `new` scans without limit.
#[test]
fn open_sessions_are_bounded() {
    let _cap = exclusive();
    let mut sessions = Vec::new();
    while sessions.len() <= MAX_OPEN_SESSIONS {
        match SessionCore::new(1) {
            Ok(s) => {
                s.obliviate();
                sessions.push(s);
            }
            Err(SessionError::TooManyActiveSessions { open, .. }) => {
                assert_eq!(open, MAX_OPEN_SESSIONS);
                break;
            }
            Err(e) => panic!("unexpected error: {e}"),
        }
    }
    assert_eq!(sessions.len(), MAX_OPEN_SESSIONS);
    sessions.clear();
    assert!(SessionCore::new(1).is_ok());
}
