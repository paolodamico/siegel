#![doc = include_str!("../../README.md")]

use std::marker::PhantomData;
use zeroize::{Zeroize, ZeroizeOnDrop};

mod protected;
pub use protected::{ProtectedRegion, ProtectionError};

/// Container for a single secret in protected memory. Type safe.
///
/// A Siegel is structured so a secret can only be "used" (i.e. read)
/// once. This is to direct the loading of sensitive secrets into memory
/// only for the time they're required. A common path for this is:
/// 1. Load a secret from the keychain
/// 2. Store it in a [`Siegel`].
/// 3. Use it to sign an operation.
///
/// [`Siegel::new`] initializes a new empty `Siegel`. A secret can be
/// filled via [`Siegel::write`], and subsequently used with [`Siegel::use_and_consume`].
pub struct Siegel<State> {
    region: ProtectedRegion,
    _state: PhantomData<State>,
}

/// Marker for a freshly-allocated siegel that has not yet been filled.
pub struct Empty;

/// Marker for a siegel that holds bytes and is ready for one-shot use.
pub struct Loaded;

impl Siegel<Empty> {
    /// Allocate a new siegel for a specific `len`.
    ///
    /// Call [`write`](Self::write) to fill it.
    ///
    /// # Errors
    /// - `SiegelError::InvalidLength` (on well, invalid length)
    /// - Allocation / protection / lock errors if the OS calls fail.
    pub fn new(len: usize) -> Result<Self, SiegelError> {
        let region = ProtectedRegion::new(len)?;
        Ok(Self {
            region,
            _state: PhantomData,
        })
    }

    /// Copy `bytes` into the protected region and seal. This method
    /// consumes the `Siegel` and returns a "Loaded" Siegel.
    ///
    /// # Errors
    ///
    /// `SiegelError::LengthMismatch` if `bytes.len()` doesn't equal the
    /// siegel's capacity. Protection errors propagate.
    pub fn write(mut self, bytes: &[u8]) -> Result<Siegel<Loaded>, SiegelError> {
        let expected = self.region.len();
        if bytes.len() != expected {
            return Err(SiegelError::LengthMismatch {
                expected,
                got: bytes.len(),
            });
        }
        self.region.with_write(|buf| buf.copy_from_slice(bytes))?;
        Ok(Siegel {
            region: self.region,
            _state: PhantomData,
        })
    }
}

impl Siegel<Loaded> {
    /// Unseal, run `f` with `&[u8]` of the secret bytes and then drop.
    ///
    /// The closure runs while the region is briefly `PROT_READ`. The
    /// returned `T` is whatever the closure produces.
    ///
    /// The caller is responsible for the secrecy of any derived value.
    ///
    /// Consumes `self`: the siegel cannot be used again after this call.
    ///
    /// # Errors
    /// - Protection / canary errors.
    pub fn read_once<T, F>(mut self, f: F) -> Result<T, SiegelError>
    where
        F: FnOnce(&[u8]) -> T,
    {
        let result = self.region.with_read(f)?;
        Ok(result)
        // `ProtectedRegion` drops and gets zeroized.
    }
}

impl<State> Siegel<State> {
    /// Drop the siegel without using it. Zeroizes via `Drop`. 🪄
    pub fn obliviate(self) {
        drop(self);
    }

    /// Capacity of the siegel in bytes.
    #[must_use]
    #[expect(clippy::len_without_is_empty, reason = "always non-empty")]
    pub fn len(&self) -> usize {
        self.region.len()
    }
}

impl<State> Zeroize for Siegel<State> {
    fn zeroize(&mut self) {
        self.region.zeroize();
    }
}

impl<State> ZeroizeOnDrop for Siegel<State> {}

/// Errors with `Siegel` operations.
#[derive(Debug, thiserror::Error)]
pub enum SiegelError {
    #[error("requested size must be 1..=1Mb")]
    InvalidLength,
    #[error("input length {got} doesn't match siegel capacity {expected}")]
    LengthMismatch { expected: usize, got: usize },
    #[error("memory allocation failed: {reason}")]
    AllocationFailed { reason: String },
    #[error("memory protection failed: {reason}")]
    ProtectionFailed { reason: String },
    #[error("memory lock failed: {reason}")]
    LockFailed { reason: String },
    #[error("canary check failed — possible memory corruption")]
    CanaryCorrupted,
}

impl From<ProtectionError> for SiegelError {
    fn from(e: ProtectionError) -> Self {
        match e {
            ProtectionError::InvalidSize => Self::InvalidLength,
            ProtectionError::Mmap(e) => Self::AllocationFailed {
                reason: e.to_string(),
            },
            ProtectionError::Mprotect(e) => Self::ProtectionFailed {
                reason: e.to_string(),
            },
            ProtectionError::Mlock(e) => Self::LockFailed {
                reason: e.to_string(),
            },
            ProtectionError::CanaryCorrupted => Self::CanaryCorrupted,
        }
    }
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "tests")]
mod tests {
    use sha2::{Digest, Sha256};

    use super::*;

    #[test]
    fn new_creates_empty_of_given_size() {
        let s: Siegel<Empty> = Siegel::new(32).unwrap();
        assert_eq!(s.len(), 32);
    }

    #[test]
    fn new_rejects_zero_length() {
        assert!(Siegel::<Empty>::new(0).is_err());
    }

    #[test]
    fn test_completew_flow() {
        let secret = vec![0x42; 32];
        let empty: Siegel<Empty> = Siegel::new(32).unwrap();
        let loaded: Siegel<Loaded> = empty.write(&secret).unwrap();
        let copy = loaded.read_once(<[u8]>::to_vec).unwrap();
        assert_eq!(copy, secret);
    }

    #[test]
    #[expect(clippy::panic, reason = "looking for specific failure")]
    fn write_rejects_length_mismatch() {
        let empty: Siegel<Empty> = Siegel::new(16).unwrap();
        match empty.write(&[0u8; 8]) {
            Ok(_) => panic!("expected LengthMismatch"),
            Err(SiegelError::LengthMismatch {
                expected: 16,
                got: 8,
            }) => {}
            Err(other) => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn closure_can_perform_arbitrary_operation() {
        let secret = vec![0x42; 16];
        let loaded = Siegel::<Empty>::new(16).unwrap().write(&secret).unwrap();
        let digest = loaded
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
    }

    #[test]
    fn closure_return_type_is_generic() {
        let loaded = Siegel::<Empty>::new(8).unwrap().write(&[1u8; 8]).unwrap();
        let len: usize = loaded.read_once(<[u8]>::len).unwrap();
        assert_eq!(len, 8);
    }

    #[test]
    fn obliviate_on_empty() {
        let empty: Siegel<Empty> = Siegel::new(16).unwrap();
        empty.obliviate();
    }

    #[test]
    fn obliviate_on_loaded() {
        let loaded = Siegel::<Empty>::new(16).unwrap().write(&[7u8; 16]).unwrap();
        loaded.obliviate();
    }

    #[test]
    #[expect(clippy::panic, reason = "deliberately panicking inside the closure")]
    fn closure_panic_still_drops_loaded() {
        let loaded = Siegel::<Empty>::new(8).unwrap().write(&[6u8; 8]).unwrap();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            loaded.read_once(|_| panic!("operation failed"))
        }));
        assert!(result.is_err());
        // No usable handle to the siegel after a panicking consume; the
        // unwind dropped it, which zeroized the region.
    }
}
