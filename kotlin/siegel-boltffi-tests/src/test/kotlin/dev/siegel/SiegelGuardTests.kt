package dev.siegel

import kotlin.test.Test
import kotlin.test.assertTrue

/**
 * Test-only JNI entry point for the guard-page check.
 *
 * The fork happens inside Rust — the JVM can't fork safely.
 */
internal object SiegelTestNative {
    init {
        // Touching a generated class first would also load the library, but
        // this suite must not depend on that ordering.
        System.loadLibrary("siegel_boltffi")
    }

    @JvmStatic
    external fun frontGuardSegFault(handle: Long): Int
}

// POSIX signal numbers. Linux and macOS both use these values for SIGSEGV.
// SIGBUS differs (10 on macOS, 7 on Linux).
private const val SIGSEGV: Int = 11
private const val SIGBUS_LINUX: Int = 7
private const val SIGBUS_DARWIN: Int = 10

class SiegelGuardTests {

    @Test
    fun `reading protected memory segfaults`() {
        val session = SiegelSession(64u)
        val signal = SiegelTestNative.frontGuardSegFault(session.handleId().toLong())
        assertTrue(
            signal == SIGSEGV || signal == SIGBUS_LINUX || signal == SIGBUS_DARWIN,
            "expected SIGSEGV ($SIGSEGV) or SIGBUS ($SIGBUS_LINUX/$SIGBUS_DARWIN), got $signal",
        )
    }
}
