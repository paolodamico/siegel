package siegel

import com.sun.jna.Library
import com.sun.jna.Memory
import com.sun.jna.Native
import com.sun.jna.Pointer
import java.security.MessageDigest
import kotlin.test.Test
import kotlin.test.assertContentEquals
import kotlin.test.assertEquals
import kotlin.test.assertFailsWith
import kotlin.test.assertFalse
import kotlin.test.assertNotEquals
import kotlin.test.assertTrue
import uniffi.siegel_uniffi.SessionException
import uniffi.siegel_uniffi.SiegelSession
import uniffi.siegel_uniffi.sha256Consume

/**
 * `siegel_fill` is a hand-written `extern "C"` symbol outside the UniFFI
 * scaffolding. JNA loads `libsiegel_uniffi` from `jna.library.path`
 * (configured in the test task) and calls the symbol directly.
 *
 * C ABI: `i32 siegel_fill(u64 handle, *const u8 src, usize len)`.
 */
internal interface SiegelRawFFI : Library {
    @Suppress("FunctionName")
    fun siegel_fill(handle: Long, src: Pointer?, len: Long): Int
}

internal val RAW: SiegelRawFFI by lazy {
    Native.load("siegel_uniffi", SiegelRawFFI::class.java)
}

// Match the constants exported from `siegel-uniffi::session`.
private const val FILL_OK: Int = 0
private const val FILL_ERR_INVALID_HANDLE: Int = -1
private const val FILL_ERR_LEN_MISMATCH: Int = -2
private const val FILL_ERR_NULL_SRC: Int = -3
private const val FILL_ERR_WRONG_STATE: Int = -4

/** Helper: copy `bytes` into native memory and call `siegel_fill`. */
private fun fill(session: SiegelSession, bytes: ByteArray): Int {
    val mem = Memory(bytes.size.toLong())
    mem.write(0, bytes, 0, bytes.size)
    return RAW.siegel_fill(session.handleId().toLong(), mem, bytes.size.toLong())
}

private fun bytesOf(value: Byte, len: Int): ByteArray = ByteArray(len) { value }

class SiegelSessionTests {

    @Test
    fun `new session is not consumed`() {
        val session = SiegelSession(32u)
        assertFalse(session.isConsumed())
        assertEquals(session.handleId(), session.handleId())
    }

    @Test
    fun `zero length is rejected`() {
        assertFailsWith<SessionException.InvalidLength> { SiegelSession(0u) }
    }

    @Test
    fun `fill and consume returns sha256`() {
        val secret = bytesOf(0x42, 16)
        val session = SiegelSession(16u)

        assertEquals(FILL_OK, fill(session, secret))

        val digest = sha256Consume(session)
        val expected = MessageDigest.getInstance("SHA-256").digest(secret)

        assertContentEquals(expected, digest)
        assertTrue(session.isConsumed())
    }

    @Test
    fun `fill rejects null src`() {
        val session = SiegelSession(8u)
        val rc = RAW.siegel_fill(session.handleId().toLong(), null, 8L)
        assertEquals(FILL_ERR_NULL_SRC, rc)
    }

    @Test
    fun `fill rejects unknown handle`() {
        val mem = Memory(4L)
        mem.write(0L, ByteArray(4), 0, 4)
        val rc = RAW.siegel_fill(0xDEADL, mem, 4L)
        assertEquals(FILL_ERR_INVALID_HANDLE, rc)
    }

    @Test
    fun `fill rejects length mismatch and recovers`() {
        val session = SiegelSession(16u)
        assertEquals(FILL_ERR_LEN_MISMATCH, fill(session, ByteArray(8)))
        // Session is still usable for a correct-length fill.
        assertEquals(FILL_OK, fill(session, ByteArray(16)))
    }

    @Test
    fun `double fill is rejected`() {
        val session = SiegelSession(8u)
        assertEquals(FILL_OK, fill(session, bytesOf(1, 8)))
        assertEquals(FILL_ERR_WRONG_STATE, fill(session, bytesOf(1, 8)))
    }

    @Test
    fun `consume before fill is InvalidState`() {
        val session = SiegelSession(8u)
        assertFailsWith<SessionException.InvalidState> { sha256Consume(session) }
        assertFalse(session.isConsumed())
        // After an InvalidState error the session is still fillable.
        assertEquals(FILL_OK, fill(session, bytesOf(2, 8)))
        sha256Consume(session)
    }

    @Test
    fun `second consume is rejected`() {
        val session = SiegelSession(8u)
        assertEquals(FILL_OK, fill(session, bytesOf(3, 8)))
        sha256Consume(session)
        assertFailsWith<SessionException.Consumed> { sha256Consume(session) }
    }

    @Test
    fun `fill after consume is rejected`() {
        val session = SiegelSession(8u)
        assertEquals(FILL_OK, fill(session, bytesOf(4, 8)))
        sha256Consume(session)
        assertEquals(FILL_ERR_WRONG_STATE, fill(session, bytesOf(4, 8)))
    }

    @Test
    fun `obliviate wipes and is idempotent`() {
        val session = SiegelSession(16u)
        assertEquals(FILL_OK, fill(session, bytesOf(5, 16)))

        session.obliviate()
        assertTrue(session.isConsumed())
        session.obliviate() // no-op the second time

        assertFailsWith<SessionException.Consumed> { sha256Consume(session) }
    }

    @Test
    fun `distinct sessions have distinct handles`() {
        val a = SiegelSession(8u)
        val b = SiegelSession(8u)
        assertNotEquals(a.handleId(), b.handleId())
    }

    @Test
    fun `handle is invalidated after session is destroyed`() {
        // Drop the session explicitly via `close()`. UniFFI registers a
        // cleanable so the underlying Arc is released here; the Weak entry
        // in the registry can no longer be upgraded, and the next fill
        // returns FILL_ERR_INVALID_HANDLE.
        var handle: Long
        SiegelSession(8u).use { session ->
            handle = session.handleId().toLong()
        }
        // Some VMs lag on AutoCloseable bookkeeping — nudge GC to give the
        // cleaner a chance, then retry briefly.
        var rc = 0
        repeat(10) {
            val mem = Memory(8L)
            mem.write(0L, ByteArray(8), 0, 8)
            rc = RAW.siegel_fill(handle, mem, 8L)
            if (rc == FILL_ERR_INVALID_HANDLE) return@repeat
            System.gc()
            Thread.sleep(50)
        }
        assertEquals(FILL_ERR_INVALID_HANDLE, rc)
    }
}
