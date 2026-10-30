//! JVM raw paths: direct `ByteBuffer` in and out of protected memory.
//!
//! # Rationale
//!
//! Apple binds [`siegel_fill_bolt`](crate::siegel_fill_bolt) directly. The JVM
//! cannot: it has no way to hand Rust a raw pointer to a `ByteArray`, and going
//! through `&[u8]` is not safe for secrets: the `ByteArray` lives on managed heap,
//! it can be copied to a new region without zeroization.
//!
//! A direct `java.nio.ByteBuffer` through `allocateDirect` returns
//! off-heap memory that the collector never moves, `GetDirectBufferAddress`
//! hands back that exact address with no copy, and the caller can overwrite the
//! buffer's contents the moment this function returns.
//!
//! # Residual exposure
//!
//! The direct buffer is ordinary process memory: not `mlock`ed, not sealed,
//! no guard pages, and included in core dumps. The secret sits there from the
//! moment the caller writes it (or `takeDirect` copies it out) until the wipe
//! in `withDirectSecretBuffer` runs. Keep that window to the one call.
//!
//! # Panics
//!
//! A Rust panic inside these functions is caught at the boundary and surfaces
//! as a Java `RuntimeException` plus [`FILL_ERR_JNI`] / [`TAKE_ERR_JNI`], but
//! only in builds that unwind. The release profile sets `panic = "abort"`,
//! where a panic aborts the process instead.

use jni::errors::ThrowRuntimeExAndDefault;
use jni::objects::{JByteBuffer, JClass};
use jni::sys::{jint, jlong};
use jni::{Env, EnvUnowned, jni_sig, jni_str};

use siegel::session::{
    FILL_ERR_LEN_MISMATCH, FILL_ERR_NULL_SRC, TAKE_ERR_LEN_MISMATCH, TAKE_ERR_NULL_DST,
};

/// The argument was not a direct buffer, or its address / capacity could not be
/// read
pub const FILL_ERR_NOT_DIRECT: jint = -6;

/// A Rust panic crossing the boundary, in builds that unwind. Accompanied by a
/// thrown Java exception.
pub const FILL_ERR_JNI: jint = -7;

/// [`FILL_ERR_NOT_DIRECT`] for `takeDirect`, which also refuses a read-only
/// buffer (writing a `READ_ONLY` mapping would fault). Renumbered because `-6`
/// is [`TAKE_ERR_CANARY`](siegel::session::TAKE_ERR_CANARY).
pub const TAKE_ERR_NOT_DIRECT: jint = -7;

/// [`FILL_ERR_JNI`] for `takeDirect`, which also returns it when the
/// `isReadOnly()` call fails (with the Java exception left pending).
pub const TAKE_ERR_JNI: jint = -8;

/// Return code wrapper whose `Default` is a *failure*.
#[derive(Clone, Copy)]
struct FillCode(jint);

impl Default for FillCode {
    fn default() -> Self {
        Self(FILL_ERR_JNI)
    }
}

/// [`FillCode`] for `takeDirect`.
#[derive(Clone, Copy)]
struct TakeCode(jint);

impl Default for TakeCode {
    fn default() -> Self {
        Self(TAKE_ERR_JNI)
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

/// Why a buffer was refused.
enum BufferError {
    Null,
    NotDirect,
    LenMismatch,
}

/// Base address of `buffer`, which must be direct with a capacity of exactly
/// `len` bytes. Reads start at the base address: Java position and limit are
/// ignored.
fn direct_buffer(
    env: &Env,
    buffer: &JByteBuffer,
    len: jint,
    op: &'static str,
) -> Result<(*mut u8, usize), BufferError> {
    let len = usize::try_from(len).map_err(|_| BufferError::LenMismatch)?;
    if buffer.is_null() {
        return Err(BufferError::Null);
    }

    // A non-direct (heap-backed) buffer would introduce unzeroizable copies
    let addr = env
        .get_direct_buffer_address(buffer)
        .map_err(|e| not_direct(op, "GetDirectBufferAddress", &e))?;
    let capacity = env
        .get_direct_buffer_capacity(buffer)
        .map_err(|e| not_direct(op, "GetDirectBufferCapacity", &e))?;

    // Exact, not `>=`: a larger buffer means the caller's bytes may not be
    // where the read starts.
    if capacity != len {
        return Err(BufferError::LenMismatch);
    }
    Ok((addr, len))
}

#[cfg_attr(
    not(feature = "tracing"),
    expect(unused_variables, reason = "only logged")
)]
fn not_direct(op: &'static str, call: &'static str, error: &jni::errors::Error) -> BufferError {
    #[cfg(feature = "tracing")]
    tracing::warn!(target: "siegel", op, call, %error, "refusing a non-direct ByteBuffer");
    BufferError::NotDirect
}

#[expect(clippy::cast_sign_loss, reason = "round-tripping a u64 handle")]
fn handle_from(handle: jlong) -> u64 {
    handle as u64
}

/// Copy `len` bytes from a direct `ByteBuffer` into the session's siegel.
///
/// `buffer`'s capacity must be exactly `len`. Returns
/// [`FILL_OK`](siegel::session::FILL_OK) or one of the `FILL_ERR_*` codes.
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
        let (addr, len) = match direct_buffer(env, &buffer, len, "fillDirect") {
            Ok(region) => region,
            Err(BufferError::Null) => return Ok(FillCode(FILL_ERR_NULL_SRC)),
            Err(BufferError::NotDirect) => return Ok(FillCode(FILL_ERR_NOT_DIRECT)),
            Err(BufferError::LenMismatch) => return Ok(FillCode(FILL_ERR_LEN_MISMATCH)),
        };

        // SAFETY: `addr` is the JVM-reported base of a direct buffer of exactly
        // `len` bytes, valid for the duration of this call.
        Ok(FillCode(unsafe {
            siegel::session::fill_into(handle_from(handle), addr, len)
        }))
    })
    .resolve::<ThrowRuntimeExAndDefault>()
    .0
}

/// Copy a Rust-written secret into a direct `ByteBuffer`, then wipe and consume
/// the session.
///
/// `buffer`'s capacity must be exactly `len`, the session's capacity. Returns
/// [`TAKE_OK`](siegel::session::TAKE_OK) or one of the `TAKE_ERR_*` codes;
/// see [`take_into`](siegel::session::take_into) for when `buffer` is zeroed.
#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_siegel_SiegelNative_takeDirect<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    handle: jlong,
    buffer: JByteBuffer<'local>,
    len: jint,
) -> jint {
    env.with_env(|env| -> jni::errors::Result<TakeCode> {
        let (addr, len) = match direct_buffer(env, &buffer, len, "takeDirect") {
            Ok(region) => region,
            Err(BufferError::Null) => return Ok(TakeCode(TAKE_ERR_NULL_DST)),
            Err(BufferError::NotDirect) => return Ok(TakeCode(TAKE_ERR_NOT_DIRECT)),
            Err(BufferError::LenMismatch) => return Ok(TakeCode(TAKE_ERR_LEN_MISMATCH)),
        };

        // JNI hands back the address of a read-only buffer too.
        if env
            .call_method(&buffer, jni_str!("isReadOnly"), jni_sig!("()Z"), &[])?
            .z()?
        {
            #[cfg(feature = "tracing")]
            tracing::warn!(target: "siegel", op = "takeDirect", "refusing a read-only ByteBuffer");
            return Ok(TakeCode(TAKE_ERR_NOT_DIRECT));
        }

        // SAFETY: `addr` is the base of a writable direct buffer of exactly `len`
        // bytes, off-heap and so disjoint from the session's protected memory.
        Ok(TakeCode(unsafe {
            siegel::session::take_into(handle_from(handle), addr, len)
        }))
    })
    .resolve::<ThrowRuntimeExAndDefault>()
    .0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_codes_are_failures() {
        assert_ne!(FillCode::default().0, siegel::session::FILL_OK);
        assert_eq!(FillCode::default().0, FILL_ERR_JNI);
        assert_ne!(TakeCode::default().0, siegel::session::TAKE_OK);
        assert_eq!(TakeCode::default().0, TAKE_ERR_JNI);
    }
}
