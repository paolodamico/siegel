//! Helpers for integration tests.

use boltffi::export;
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
#[export]
pub fn sha256_consume(session: &SiegelSession) -> Result<Vec<u8>, SessionError> {
    session.read_once(|bytes| {
        let mut h = Sha256::new();
        h.update(bytes);
        h.finalize().to_vec()
    })
}

/// Fork a child that reads one byte from the front guard page, returning the
/// terminating signal observed by the parent. Always expected to return
/// `SIGSEGV` or `SIGBUS`.
///
/// Shared by the C entry point (Swift) and the JNI entry point (Kotlin) so the
/// two cannot drift.
fn front_guard_seg_fault(handle: u64) -> i32 {
    let Some(session) = lookup_session(handle) else {
        return ERR_INVALID_HANDLE;
    };
    // SAFETY: `test_touch_front_guard` only runs in the forked child, which is
    // expected to die from the resulting fault.
    fork_and_run(|| unsafe { session.test_touch_front_guard() })
}

/// Test-only C entry point. Bound from Swift with `@_silgen_name`.
///
/// # Safety
/// Spawns a process that intentionally crashes. Caller must accept the
/// fork/wait semantics described on [`fork_and_run`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn unsafe_test_only_siegel_front_guard_bolt(handle: u64) -> i32 {
    front_guard_seg_fault(handle)
}

/// Test-only JNI entry point, mirroring
/// [`unsafe_test_only_siegel_front_guard_bolt`] for the JVM suite. Kotlin
/// cannot reach a bare C symbol without JNA, which `siegel-boltffi`
/// deliberately does not depend on.
#[cfg(feature = "jvm")]
#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_siegel_SiegelTestNative_frontGuardSegFault(
    mut env: jni::EnvUnowned<'_>,
    _class: jni::objects::JClass<'_>,
    handle: jni::sys::jlong,
) -> jni::sys::jint {
    env.with_env(|_env| -> jni::errors::Result<crate::jvm::TestSignal> {
        #[expect(clippy::cast_sign_loss, reason = "round-tripping a u64 handle")]
        let handle = handle as u64;
        Ok(crate::jvm::TestSignal(front_guard_seg_fault(handle)))
    })
    .resolve::<jni::errors::ThrowRuntimeExAndDefault>()
    .0
}
