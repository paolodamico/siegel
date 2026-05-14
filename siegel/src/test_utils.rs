use crate::ProtectedRegion;
use crate::Siegel;

impl ProtectedRegion {
    /// Test-only: read one byte from the front guard page. The guard
    /// page is always `PROT_NONE`, so this triggers a segfault before
    /// the read completes.
    ///
    /// # Safety
    /// Intentionally crashes the process. Call only from a forked
    /// child that the parent waits on.
    pub unsafe fn test_touch_front_guard(&self) {
        let guard = unsafe { self.data().sub(1) };
        let _ = unsafe { std::ptr::read_volatile(guard) };
    }

    /// Test-only: read one byte from the back guard page. Same
    /// semantics as [`test_touch_front_guard`](Self::test_touch_front_guard).
    ///
    /// # Safety
    /// Intentionally crashes the process. Call only from a forked
    /// child that the parent waits on.
    pub unsafe fn test_touch_back_guard(&self) {
        let guard = unsafe { self.data().add(self.data_pages_len()) };
        let _ = unsafe { std::ptr::read_volatile(guard) };
    }

    /// Test-only: read the first byte of the (sealed) data region.
    /// Segfaults whenever the region is `PROT_NONE`, which is the
    /// at-rest state outside of `with_read` / `with_write`. The canary
    /// lives inside this same protected region.
    ///
    /// # Safety
    /// Intentionally crashes the process when the region is sealed.
    /// Call only from a forked child that the parent waits on.
    pub unsafe fn test_touch_sealed_data(&self) {
        let _ = unsafe { std::ptr::read_volatile(self.data()) };
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

    /// See [`ProtectedRegion::test_touch_back_guard`].
    ///
    /// # Safety
    /// Intentionally crashes the process. Forked child only.
    pub unsafe fn test_touch_back_guard(&self) {
        unsafe { self.region.test_touch_back_guard() }
    }

    /// See [`ProtectedRegion::test_touch_sealed_data`].
    ///
    /// # Safety
    /// Intentionally crashes the process when the region is sealed.
    /// Forked child only.
    pub unsafe fn test_touch_sealed_data(&self) {
        unsafe { self.region.test_touch_sealed_data() }
    }
}
