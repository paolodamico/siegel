package siegel

import com.sun.jna.Library
import com.sun.jna.Native
import kotlin.test.Test
import kotlin.test.assertTrue
import uniffi.siegel_uniffi.SiegelSession

/**
 * The fork happens inside Rust — the JVM can't fork safely (HotSpot
 * holds its own locks across the call). The Rust helper forks, has the
 * child read one byte from the front guard page (`PROT_NONE`), waits,
 * and returns the terminating signal observed by the parent.
 *
 * One foreign segfault test is enough to verify the binding boundary is
 * wired up. The full matrix of front/back guards + sealed data + canary
 * is covered by Rust unit tests in the `siegel` crate.
 */
internal interface SiegelGuardFFI : Library {
    @Suppress("FunctionName")
    fun unsafe_test_only_siegel_front_guard_seg_fault(handle: Long): Int
}

internal val GUARD: SiegelGuardFFI by lazy {
    Native.load("siegel_uniffi", SiegelGuardFFI::class.java)
}

// POSIX signal numbers. Linux and macOS both use these values for SIGSEGV.
// SIGBUS differs (10 on macOS, 7 on Linux) — we accept either kernel since
// both indicate a memory-protection fault we want to see.
private const val SIGSEGV: Int = 11
private const val SIGBUS_LINUX: Int = 7
private const val SIGBUS_DARWIN: Int = 10

class SiegelGuardTests {

    @Test
    fun `reading protected memory segfaults`() {
        val session = SiegelSession(64u)
        val signal = GUARD.unsafe_test_only_siegel_front_guard_seg_fault(
            session.handleId().toLong(),
        )
        assertTrue(
            signal == SIGSEGV || signal == SIGBUS_LINUX || signal == SIGBUS_DARWIN,
            "expected SIGSEGV ($SIGSEGV) or SIGBUS ($SIGBUS_LINUX/$SIGBUS_DARWIN), got $signal",
        )
    }
}
