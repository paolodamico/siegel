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

// SIGSEGV is 11 on both hosts; SIGBUS differs, and the other host's value is a
// live signal here (10 is SIGUSR1 on Linux, 7 is SIGEMT on Darwin), so accepting
// both would let an unrelated termination pass.
private const val SIGSEGV: Int = 11
private val SIGBUS: Int =
    if (System.getProperty("os.name").orEmpty().startsWith("Mac")) 10 else 7

class SiegelGuardTests {

    @Test
    fun `reading protected memory segfaults`() {
        val session = SiegelSession(64u)
        val signal = SiegelTestNative.frontGuardSegFault(session.handleId().toLong())
        assertTrue(
            signal == SIGSEGV || signal == SIGBUS,
            "expected SIGSEGV ($SIGSEGV) or SIGBUS ($SIGBUS), got $signal",
        )
    }
}
