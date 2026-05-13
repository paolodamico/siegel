//!
//! # `UniFFI` Bindings
//!
//! This crate exposes foreign bindings to allow [`siegel::Siegel`]s
//! to cross foreign boundaries. This is particularly tested for Kotlin and
//! Swift.
//!
//! Conceptually, this allows foreign code (e.g. an iOS app) to load a secret
//! from a secure store (e.g. Keychain), and pass it to Rust-code through raw
//! memory pointers. This addresses the issue that when passing values between
//! the foreign boundary through `UniFFI`, any value is byte copied, which leads
//! to dangling non-zeroized copies of secrets in memory.
#![doc = include_str!("../README.md")]

mod session;

pub use session::{
    FILL_ERR_INVALID_HANDLE, FILL_ERR_LEN_MISMATCH, FILL_ERR_NULL_SRC, FILL_ERR_PROTECTION,
    FILL_ERR_WRONG_STATE, FILL_OK, SessionError, SiegelSession, siegel_fill,
};

uniffi::setup_scaffolding!();
