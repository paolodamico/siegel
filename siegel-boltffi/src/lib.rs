//! # `BoltFFI` Bindings
//!
//! This crate exposes foreign bindings to allow [`siegel::Siegel`]s
//! to cross foreign boundaries on iOS and Android.
//!
//! Conceptually, this allows foreign code (e.g. an iOS app) to load a secret
//! from a secure store (e.g. Keychain), and pass it to Rust-code without the
//! plaintext being duplicated into buffers the caller cannot reach and cannot
//! zeroize.
#![doc = include_str!("../README.md")]

mod session;

pub use session::{
    FILL_ERR_INVALID_HANDLE, FILL_ERR_LEN_MISMATCH, FILL_ERR_NULL_SRC, FILL_ERR_PROTECTION,
    FILL_ERR_WRONG_STATE, FILL_OK, SessionError, SiegelSession, siegel_fill_bolt,
};

// Compile-time check: required for Android to fill a session
#[cfg(all(target_os = "android", not(feature = "jvm")))]
compile_error!("siegel-boltffi requires the `jvm` feature on Android");

#[cfg(feature = "jvm")]
pub mod jvm;

#[cfg(feature = "test-utils")]
mod test_utils;
#[cfg(feature = "test-utils")]
pub use test_utils::sha256_consume;
