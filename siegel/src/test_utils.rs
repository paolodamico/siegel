use crate::ProtectedRegion;
use crate::Siegel;

impl ProtectedRegion {
    /// Test-only: read one byte from the front guard page. The guard
    /// page is always `PROT_NONE`, so this triggers a segfault before
    /// the read completes.
    ///
    /// Used by foreign integration tests as a single smoke check that
    /// memory protection survives the binding boundary. Exhaustive
    /// coverage of front/back guards, sealed data and canary lives in
    /// `protected.rs`'s native unit tests.
    ///
    /// # Safety
    /// Intentionally crashes the process. Call only from a forked
    /// child that the parent waits on.
    pub unsafe fn test_touch_front_guard(&self) {
        let guard = unsafe { self.data().sub(1) };
        let _ = unsafe { std::ptr::read_volatile(guard) };
    }
}

impl<State> Siegel<State> {
    /// See [`ProtectedRegion::test_touch_front_guard`].
    ///
    /// # Safety
    /// Intentionally crashes the process. Forked child only.
    pub unsafe fn test_touch_front_guard(&self) {
        unsafe { self.region.test_touch_front_guard() }
    }
}
