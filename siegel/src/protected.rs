//! Memory protection utilities. Offers memory allocation with best-effort security to store
//! secrets (page alignment, `mlock`, over/underflow guards).
//!
//! This module generally follows libsodium and [dryoc](https://docs.rs/dryoc). While
//! the implementation is very similar, this doesn't use Rust's Allocator API which
//! requires nightly Rust (~10 years since introduction).

use std::io;

use zeroize::{Zeroize, ZeroizeOnDrop};

const CANARY: [u8; 18] = *b"~!GUARDED_CANARY!~";
const MAX_SECRET_SIZE: usize = 1024 * 1024; // currently 1Mb

/// A protected region of memory for holding a one-time use secret.
///
/// Mirrors the design of libsodium's `sodium_malloc` / dryoc's
/// `Protected<HeapBytes, …>`: with pages, guards for over/under flows,
/// data canary, `mlock` to prevent disk swap, `mprotect`-able, zeroized on drop.
/// The access surface differs from dryoc (exposes scoped closures `with_read`/`with_write`)
/// to prevent references escaping the unsealed window.
///
/// Memory layout (all page-aligned):
/// ```text
/// [GUARD_PAGE] [DATA_PAGES] [CANARY] [GUARD_PAGE]
/// ```
///
/// The last `CANARY.len()` bytes of the data region hold a canary
/// that is verified on every read to detect overflow corruption.
///
/// - Guard pages are always `PROT_NONE`, access segfaults immediately.
/// - Data pages are `mlock`ed so they're never swapped to disk.
/// - Data pages default to `PROT_NONE` (sealed). Temporarily unsealed
///   via `with_read` / `with_write` scoped accessors.
/// - A canary value at the end of the data region detects overflows.
/// - On drop: zeroize data, `munlock`, `munmap` the entire region.
pub struct ProtectedRegion {
    /// Raw pointer to the start of the full mmap region (including guard pages).
    base: *mut u8,
    /// Total mmap size (guard + data + canary + guard).
    total_len: usize,
    /// Raw pointer to the start of usable data (after first guard page).
    data: *mut u8,
    /// Size of data pages (page-aligned, includes canary space).
    data_pages_len: usize,
    /// Actual usable byte count (excluding canary).
    usable_len: usize,
    /// Current protection level of data pages.
    protection: Protection,
}

// SAFETY: ProtectedRegion owns its mmap region and never shares the raw
// pointer (only this struct has ownership). Safe to move to other threads. Explicitly
// not `Sync` as the scoped closures require mutable references.
unsafe impl Send for ProtectedRegion {}

impl ProtectedRegion {
    /// Allocate a new protected region for `size` bytes of secret data.
    ///
    /// The region is returned **sealed** (`PROT_NONE`) — the at-rest
    /// state. To write bytes, call [`with_write`](Self::with_write);
    /// to read, [`with_read`](Self::with_read). Both flip the
    /// protection transiently and re-seal on return.
    ///
    /// # Errors
    ///
    /// `ProtectionError::InvalidSize` for `size == 0` or `size > 1 MiB`.
    /// `Mmap` / `Mprotect` / `Mlock` if the underlying syscalls fail.
    pub fn new(size: usize) -> Result<Self, ProtectionError> {
        if size == 0 || size > MAX_SECRET_SIZE {
            return Err(ProtectionError::InvalidSize);
        }

        let page_size = page_size();
        let needed = size + CANARY.len();
        let data_pages_len = needed.next_multiple_of(page_size);
        let total_len = page_size + data_pages_len + page_size;

        // Initialize mmap
        let base = mmap_anon(total_len)?;
        let data = unsafe { base.add(page_size) };

        // Open the data pages briefly to mlock them and write the canary,
        // then seal before returning.
        mprotect(data, data_pages_len, Protection::ReadWrite)?;

        if unsafe { libc::mlock(data.cast(), data_pages_len) } != 0 {
            unsafe { libc::munmap(base.cast(), total_len) };
            return Err(ProtectionError::Mlock(io::Error::last_os_error()));
        }

        // Canary sits at `data + size`, inside the page-aligned region
        // but past the caller's usable bytes.
        unsafe {
            std::ptr::copy_nonoverlapping(CANARY.as_ptr(), data.add(size), CANARY.len());
        }

        mprotect(data, data_pages_len, Protection::NoAccess)?;

        Ok(Self {
            base,
            total_len,
            data,
            data_pages_len,
            usable_len: size,
            protection: Protection::NoAccess,
        })
    }

    /// Usable byte capacity (excluding canary)
    #[must_use]
    #[expect(clippy::len_without_is_empty, reason = "regions are always non-empty")]
    pub fn len(&self) -> usize {
        self.usable_len
    }

    /// Execute `f` with read-only access to the secret bytes.
    /// Re-seals (no-access) after `f` returns or panics.
    ///
    /// Scoped equivalent of libsodium's
    /// `sodium_mprotect_readonly` + `sodium_mprotect_noaccess`.
    ///
    /// # Errors
    ///
    /// `CanaryCorrupted` if the canary check fails. `Mprotect` if a
    /// protection-state transition syscall fails.
    pub fn with_read<T>(&mut self, f: impl FnOnce(&[u8]) -> T) -> Result<T, ProtectionError> {
        self.verify_canary()?;
        let ptr = self.data;
        let len = self.usable_len;
        let _reseal = ResealOnDrop(self);
        // SAFETY: region is currently `PROT_READ`; `_reseal` returns it to
        // `PROT_NONE` on every exit path, including a closure panic.
        let slice = unsafe { std::slice::from_raw_parts(ptr, len) };
        Ok(f(slice))
    }

    /// Execute `f` with read-write access to the secret bytes.
    /// Re-seals (no-access) after `f` returns or panics.
    ///
    /// Scoped equivalent of libsodium's
    /// `sodium_mprotect_readwrite` + `sodium_mprotect_noaccess`.
    ///
    /// # Errors
    ///
    /// `Mprotect` if a protection-state transition syscall fails.
    pub fn with_write<T>(&mut self, f: impl FnOnce(&mut [u8]) -> T) -> Result<T, ProtectionError> {
        mprotect(self.data, self.data_pages_len, Protection::ReadWrite)?;
        self.protection = Protection::ReadWrite;
        let ptr = self.data;
        let len = self.usable_len;
        let _reseal = ResealOnDrop(self);
        // SAFETY: region is currently `PROT_RW`; `_reseal` returns it to
        // `PROT_NONE` on every exit path, including a closure panic.
        let slice = unsafe { std::slice::from_raw_parts_mut(ptr, len) };
        Ok(f(slice))
    }

    /// Mark the data pages as `PROT_NONE` — no read or write.
    ///
    /// Equivalent to libsodium's `sodium_mprotect_noaccess`.
    ///
    /// # Errors
    ///
    /// `ProtectionError::Mprotect` if the underlying syscall fails.
    fn mprotect_noaccess(&mut self) -> Result<(), ProtectionError> {
        mprotect(self.data, self.data_pages_len, Protection::NoAccess)?;
        self.protection = Protection::NoAccess;
        Ok(())
    }

    /// Reads the data pages and verifies the canary. This will make the raw data accessible.
    fn verify_canary(&mut self) -> Result<(), ProtectionError> {
        mprotect(self.data, self.data_pages_len, Protection::ReadOnly)?;
        self.protection = Protection::ReadOnly;
        let canary_ptr = unsafe { self.data.add(self.usable_len) };
        let stored = unsafe { std::slice::from_raw_parts(canary_ptr, CANARY.len()) };
        if stored != CANARY {
            return Err(ProtectionError::CanaryCorrupted);
        }
        Ok(())
    }
}

impl Zeroize for ProtectedRegion {
    /// Zero out this object from memory.
    fn zeroize(&mut self) {
        // If re-opening for writes (to write zeros) fails, this is non-recoverable,
        // the bytes are already inaccessible.
        let _ = mprotect(self.data, self.data_pages_len, Protection::ReadWrite);
        self.protection = Protection::ReadWrite;
        // SAFETY: data_pages_len bytes were mmap'd at self.data; this struct holds
        // exclusive access via &mut self.
        unsafe {
            std::ptr::write_bytes(self.data, 0, self.data_pages_len);
        }
    }
}

impl Drop for ProtectedRegion {
    fn drop(&mut self) {
        self.zeroize();
        unsafe {
            libc::munlock(self.data.cast(), self.data_pages_len);
            libc::munmap(self.base.cast(), self.total_len);
        }
    }
}

impl ZeroizeOnDrop for ProtectedRegion {}

#[cfg(feature = "test-utils")]
impl ProtectedRegion {
    /// Feature-gated accessor used by `test_utils::test_touch_front_guard`
    /// to compute a guard-page address. Kept minimal — the data pointer is
    /// the only piece of geometry the front-guard test needs.
    pub(crate) fn data(&self) -> *mut u8 {
        self.data
    }
}

/// Permission state of the data pages. Mirrors libsodium's `sodium_mprotect_*`
/// modes and dryoc's `traits::{NoAccess, ReadOnly, ReadWrite}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Protection {
    NoAccess,
    ReadOnly,
    ReadWrite,
}

/// RAII guard that re-seals the region to `PROT_NONE` when dropped.
///
/// Used by `with_read` / `with_write` so the scoped accessor re-seals on
/// every exit path, including closure panics. A failing `mprotect` is
/// swallowed because the region's own `Drop` will zeroize + munmap
/// regardless.
struct ResealOnDrop<'a>(&'a mut ProtectedRegion);

impl Drop for ResealOnDrop<'_> {
    fn drop(&mut self) {
        let _ = self.0.mprotect_noaccess();
    }
}

/// Errors from protected memory operations.
#[derive(Debug, thiserror::Error)]
pub enum ProtectionError {
    #[error("requested size must be 1..=1Mb")]
    InvalidSize,
    #[error("mmap failed: {0}")]
    Mmap(io::Error),
    #[error("mprotect failed: {0}")]
    Mprotect(io::Error),
    #[error("mlock failed: {0}")]
    Mlock(io::Error),
    #[error("canary corrupted. potential buffer overflow")]
    CanaryCorrupted,
}

/// System page size in bytes. Usually 4 KiB on Linux `x86_64`; 16 KiB on macOS/iOS and Android.
fn page_size() -> usize {
    // SAFETY: sysconf(_SC_PAGESIZE) is always safe and returns > 0 on
    // every platform we target (macOS, Linux, iOS, Android).
    let ps = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    usize::try_from(ps).unwrap_or(4096)
}

/// Creates a new virtual address mapping.
///
/// Map `len` bytes of anonymous (no file-backing), process-private memory with
/// `PROT_NONE`. The kernel picks a page-aligned address and the pages
/// start zero-filled. Caller must `munmap` the result.
fn mmap_anon(len: usize) -> Result<*mut u8, ProtectionError> {
    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_NONE,
            libc::MAP_ANON | libc::MAP_PRIVATE,
            -1,
            0,
        )
    };
    if ptr == libc::MAP_FAILED {
        return Err(ProtectionError::Mmap(io::Error::last_os_error()));
    }
    // Linux/Android only: exclude from core dumps. No-op elsewhere.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    unsafe {
        let _ = libc::madvise(ptr, len, libc::MADV_DONTDUMP);
    }
    Ok(ptr.cast())
}

/// Apply specific protection to `len` bytes starting at `addr`. Both `addr` and
/// `len` must be page-aligned (the kernel enforces this).
fn mprotect(addr: *mut u8, len: usize, prot: Protection) -> Result<(), ProtectionError> {
    let flags = match prot {
        Protection::NoAccess => libc::PROT_NONE,
        Protection::ReadOnly => libc::PROT_READ,
        Protection::ReadWrite => libc::PROT_READ | libc::PROT_WRITE,
    };
    if unsafe { libc::mprotect(addr.cast(), len, flags) } != 0 {
        return Err(ProtectionError::Mprotect(io::Error::last_os_error()));
    }
    Ok(())
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "tests")]
mod tests {
    use super::*;

    #[test]
    fn basic_write_read_cycle() {
        let mut region = ProtectedRegion::new(32).unwrap();
        region
            .with_write(|buf| {
                buf.copy_from_slice(&[0xAB; 32]);
            })
            .unwrap();
        let data = region.with_read(<[u8]>::to_vec).unwrap();
        assert_eq!(data, vec![0xAB; 32]);
    }

    #[test]
    fn mprotect_noaccess_sets_state() {
        let mut region = ProtectedRegion::new(16).unwrap();
        region.mprotect_noaccess().unwrap();
        assert_eq!(region.protection, Protection::NoAccess);
    }

    #[test]
    fn canary_verified_on_read() {
        let mut region = ProtectedRegion::new(8).unwrap();
        region
            .with_write(|buf| buf.copy_from_slice(&[1; 8]))
            .unwrap();
        let result = region.with_read(<[u8]>::to_vec);
        assert!(result.is_ok());
    }

    #[test]
    fn zero_length_rejected() {
        assert!(ProtectedRegion::new(0).is_err());
    }

    #[test]
    fn oversized_rejected() {
        assert!(ProtectedRegion::new(MAX_SECRET_SIZE + 1).is_err());
    }

    #[test]
    fn test_dropping() {
        let region = ProtectedRegion::new(64).unwrap();
        drop(region);
    }

    #[test]
    fn zeroize_wipes_canary() {
        let mut region = ProtectedRegion::new(32).unwrap();
        region
            .with_write(|buf| buf.copy_from_slice(&[0xAB; 32]))
            .unwrap();
        region.zeroize();
        let err = region.with_read(<[u8]>::to_vec).unwrap_err();
        assert!(matches!(err, ProtectionError::CanaryCorrupted));
    }

    #[test]
    fn zeroize_is_idempotent() {
        let mut region = ProtectedRegion::new(16).unwrap();
        region
            .with_write(|buf| buf.copy_from_slice(&[0xFF; 16]))
            .unwrap();
        region.zeroize();
        region.zeroize();
        drop(region);
    }

    #[test]
    fn multiple_read_write_cycles() {
        let mut region = ProtectedRegion::new(16).unwrap();
        for i in 0u8..5 {
            region
                .with_write(|buf| {
                    for b in buf.iter_mut() {
                        *b = i;
                    }
                })
                .unwrap();
            let data = region.with_read(|buf| buf[0]).unwrap();
            assert_eq!(data, i);
        }
    }

    /// Inspect the raw data pages after `zeroize` to confirm every byte
    /// is actually `0x00`. We bypass `with_read` (which would fail the
    /// canary check, since zeroize wipes the canary too) and read the
    /// pointer directly — zeroize leaves the pages `PROT_RW`.
    #[test]
    fn zeroize_writes_zeros_across_entire_data_region() {
        let mut region = ProtectedRegion::new(64).unwrap();
        region
            .with_write(|buf| buf.copy_from_slice(&[0xAA; 64]))
            .unwrap();
        region.zeroize();
        // SAFETY: zeroize left the pages PROT_RW; we hold &mut self.
        let observed = unsafe { std::slice::from_raw_parts(region.data, region.data_pages_len) };
        let first_nonzero = observed.iter().position(|&b| b != 0);
        assert!(
            first_nonzero.is_none(),
            "byte at offset {first_nonzero:?} was not zeroed"
        );
    }

    /// Manually corrupt the canary and verify the next read errors with
    /// `CanaryCorrupted`. Ensures the canary check actually detects
    /// tampering.
    #[test]
    fn canary_corruption_is_detected_on_next_read() {
        let mut region = ProtectedRegion::new(64).unwrap();
        region
            .with_write(|buf| buf.copy_from_slice(&[0xCC; 64]))
            .unwrap();

        // Flip protection to RW, corrupt one canary byte, reseal.
        mprotect(region.data, region.data_pages_len, Protection::ReadWrite).unwrap();
        region.protection = Protection::ReadWrite;
        // SAFETY: canary lives at data + usable_len, inside the locked region.
        unsafe {
            *region.data.add(region.usable_len) ^= 0xFF;
        }
        region.mprotect_noaccess().unwrap();

        let err = region.with_read(<[u8]>::to_vec).unwrap_err();
        assert!(matches!(err, ProtectionError::CanaryCorrupted));
    }

    /// Fork a child process, have it deliberately touch a guard page, and
    /// verify the child died from `SIGSEGV` or `SIGBUS`.
    #[cfg(unix)]
    fn assert_child_segfaults(child_work: impl FnOnce()) {
        // SAFETY: fork() in a single-threaded test runner thread is well-
        // defined; we do not call any async-signal-unsafe operation in the
        // child between fork and _exit.
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "fork failed");

        if pid == 0 {
            child_work();
            // If we reach here, no segfault happened: test failed.
            // SAFETY: _exit is async-signal-safe and skips atexit handlers.
            unsafe { libc::_exit(0) };
        }

        let mut status: libc::c_int = 0;
        // SAFETY: pid is a valid child
        let waited = unsafe { libc::waitpid(pid, &raw mut status, 0) };
        assert_eq!(waited, pid, "waitpid did not return our child");

        assert!(
            libc::WIFSIGNALED(status),
            "child exited cleanly (status {status}); expected to be killed by a signal"
        );
        let sig = libc::WTERMSIG(status);
        assert!(
            sig == libc::SIGSEGV || sig == libc::SIGBUS,
            "expected SIGSEGV or SIGBUS, got signal {sig}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn underflow_into_front_guard_page_segfaults() {
        assert_child_segfaults(|| {
            let region = ProtectedRegion::new(64).unwrap();
            // One byte before the data region lives in the front guard page.
            // SAFETY: deliberately reading from a PROT_NONE page to confirm
            // the protection is in force. Hardware traps before the read
            // completes.
            let guard = unsafe { region.data.sub(1) };
            let _ = unsafe { std::ptr::read_volatile(guard) };
        });
    }

    #[cfg(unix)]
    #[test]
    fn overflow_past_back_guard_page_segfaults() {
        assert_child_segfaults(|| {
            let region = ProtectedRegion::new(64).unwrap();
            // One byte past the data pages, bookend guard tripped.
            // SAFETY: same as underflow case
            let overflow = unsafe { region.data.add(region.data_pages_len) };
            let _ = unsafe { std::ptr::read_volatile(overflow) };
        });
    }
}
