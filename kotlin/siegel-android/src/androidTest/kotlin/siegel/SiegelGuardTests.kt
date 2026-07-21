package siegel

import com.sun.jna.Library
import com.sun.jna.Native
import kotlin.test.Test
import kotlin.test.assertTrue
import uniffi.siegel_uniffi.SiegelSession

/**
 * The fork happens inside Rust — managed runtimes (ART on Android) can't
 * fork safely, since they hold their own locks across the call. The Rust
 * helper forks, has the child read one byte from the front guard page
 * (`PROT_NONE`), waits, and returns the terminating signal observed by the
 * parent.
 *
 * One foreign segfault test is enough to verify the binding boundary is
 * wired up. The full matrix of front/back guards + sealed data + canary
 * is covered by Rust unit tests in the `siegel` crate.
 */
internal interface SiegelGuardFFI : Library {
    @Suppress("FunctionName")
    fun unsafe_test_only_siegel_front_guard_seg_fault(handle: Long): Int

    // Forks a child that zeroes RLIMIT_MEMLOCK (forcing a real mlock
    // failure), then reports whether allocation degraded gracefully.
    @Suppress("FunctionName")
    fun unsafe_test_only_siegel_degrades_without_mlock(): Int
}

internal val GUARD: SiegelGuardFFI by lazy {
    Native.load("siegel_uniffi", SiegelGuardFFI::class.java)
}

// POSIX signal numbers on the Android (Linux) kernel. A memory-protection
// fault surfaces as SIGSEGV (11) or, less commonly, SIGBUS (7).
private const val SIGSEGV: Int = 11
private const val SIGBUS: Int = 7

// Mirror the codes from `unsafe_test_only_siegel_degrades_without_mlock`.
// mlock is best-effort on every platform, so DEGRADE_OK (degraded but usable)
// and DEGRADE_STILL_LOCKED are both non-failures. STILL_LOCKED means the env
// didn't enforce RLIMIT_MEMLOCK (e.g. running privileged), so the failure
// couldn't be forced — inconclusive but not a bug. DEGRADE_HARD_ERROR means a
// failed lock wrongly aborted allocation.
private const val DEGRADE_OK: Int = 0
private const val DEGRADE_HARD_ERROR: Int = 1
private const val DEGRADE_STILL_LOCKED: Int = 2

class SiegelGuardTests {

    @Test
    fun `reading protected memory segfaults`() {
        val session = SiegelSession(64u)
        val signal = GUARD.unsafe_test_only_siegel_front_guard_seg_fault(
            session.handleId().toLong(),
        )
        assertTrue(
            signal == SIGSEGV || signal == SIGBUS,
            "expected SIGSEGV ($SIGSEGV) or SIGBUS ($SIGBUS), got $signal",
        )
    }

    @Test
    fun `mlock failure degrades gracefully`() {
        val result = GUARD.unsafe_test_only_siegel_degrades_without_mlock()
        assertTrue(
            result == DEGRADE_OK || result == DEGRADE_STILL_LOCKED,
            "expected best-effort degrade: DEGRADE_OK ($DEGRADE_OK) or " +
                "DEGRADE_STILL_LOCKED ($DEGRADE_STILL_LOCKED), got $result " +
                "(DEGRADE_HARD_ERROR=$DEGRADE_HARD_ERROR, negatives are fork/wait errors)",
        )
    }
}
