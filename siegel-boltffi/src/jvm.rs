//! JVM fill path: direct `ByteBuffer` in, protected memory out.
//!
//! # Rationale
//!
//! Apple binds [`siegel_fill_bolt`] directly. The JVM cannot: it has no way to hand
//! Rust a raw pointer to a `ByteArray`, and going through `&[u8]` is not safe
//! for secrets: the `ByteArray` lives on managed heap, it can be copied to a new region
//! without zeroization.
//!
//! A direct `java.nio.ByteBuffer` through `allocateDirect` returns
//! off-heap memory that the collector never moves, `GetDirectBufferAddress`
//! hands back that exact address with no copy, and the caller can overwrite the
//! buffer's contents the moment this function returns.

use jni::objects::{JByteBuffer, JClass};
use jni::sys::{jint, jlong};
use jni::{EnvUnowned, errors::ThrowRuntimeExAndDefault};

use siegel::session::{FILL_ERR_INVALID_HANDLE, FILL_ERR_LEN_MISMATCH, FILL_ERR_NULL_SRC};

/// The argument was not a direct buffer, or its address / capacity could not be
/// read
pub const FILL_ERR_NOT_DIRECT: jint = -6;

/// A JNI-level failure or a panic crossing the boundary. Accompanied by a
/// thrown Java exception.
pub const FILL_ERR_JNI: jint = -7;

/// Return code wrapper whose `Default` is a *failure*.
#[derive(Clone, Copy)]
struct FillCode(jint);

impl Default for FillCode {
    fn default() -> Self {
        Self(FILL_ERR_JNI)
    }
}

#[cfg(feature = "test-utils")]
#[derive(Clone, Copy)]
pub(crate) struct TestSignal(pub(crate) jint);

#[cfg(feature = "test-utils")]
impl Default for TestSignal {
    fn default() -> Self {
        Self(FILL_ERR_JNI)
    }
}

/// Copy `len` bytes from a direct `ByteBuffer` into the session's siegel.
///
/// Returns [`FILL_OK`](siegel::session::FILL_OK) or one of the `FILL_ERR_*`
/// codes. JNI-level failures additionally throw a Java exception and return
/// [`FILL_ERR_JNI`].
///
/// The symbol name encodes the Kotlin package configured in `boltffi.toml`;
/// the Kotlin test suite fails with `UnsatisfiedLinkError` if the two drift
/// apart.
#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_siegel_SiegelNative_fillDirect<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    handle: jlong,
    buffer: JByteBuffer<'local>,
    len: jint,
) -> jint {
    env.with_env(|env| -> jni::errors::Result<FillCode> {
        let Ok(len) = usize::try_from(len) else {
            return Ok(FillCode(FILL_ERR_LEN_MISMATCH));
        };

        // A non-direct (heap-backed) buffer would introduce unzeroizable copies
        let Ok(addr) = env.get_direct_buffer_address(&buffer) else {
            return Ok(FillCode(FILL_ERR_NOT_DIRECT));
        };

        if addr.is_null() {
            return Ok(FillCode(FILL_ERR_NULL_SRC));
        }

        let Ok(capacity) = env.get_direct_buffer_capacity(&buffer) else {
            return Ok(FillCode(FILL_ERR_NOT_DIRECT));
        };

        if capacity < len {
            return Ok(FillCode(FILL_ERR_LEN_MISMATCH));
        }

        #[expect(clippy::cast_sign_loss, reason = "round-tripping a u64 handle")]
        let handle = handle as u64;
        if handle == 0 {
            return Ok(FillCode(FILL_ERR_INVALID_HANDLE));
        }

        // SAFETY: `addr` is the JVM-reported base of a direct buffer with at
        // least `len` bytes of capacity, valid for the duration of this call.
        Ok(FillCode(unsafe {
            siegel::session::fill_into(handle, addr, len)
        }))
    })
    .resolve::<ThrowRuntimeExAndDefault>()
    .0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_fill_code_is_a_failure() {
        assert_ne!(FillCode::default().0, siegel::session::FILL_OK);
        assert_eq!(FillCode::default().0, FILL_ERR_JNI);
    }
}
