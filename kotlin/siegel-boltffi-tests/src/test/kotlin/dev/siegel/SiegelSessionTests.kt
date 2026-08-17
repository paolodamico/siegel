package dev.siegel

import java.nio.ByteBuffer
import java.security.MessageDigest
import kotlin.test.Test
import kotlin.test.assertContentEquals
import kotlin.test.assertEquals
import kotlin.test.assertFailsWith
import kotlin.test.assertFalse
import kotlin.test.assertNotEquals
import kotlin.test.assertTrue

/**
 * Fills a session through the direct-ByteBuffer JNI path.
 *
 * Deliberately *not* a `ByteArray`: BoltFFI's generated `&[u8]` lowering would
 * put the secret on the managed heap, where a copying collector can leave
 * unreachable duplicates and `GetByteArrayElements` may take a further copy
 * that Kotlin cannot wipe.
 */
private fun fill(session: SiegelSession, bytes: ByteArray): Int =
    fillSession(session, bytes.size) { buffer -> buffer.put(bytes) }

private fun bytesOf(value: Byte, len: Int): ByteArray = ByteArray(len) { value }

class SiegelSessionTests {

    @Test
    fun `new session is not consumed`() {
        val session = SiegelSession(32u)
        assertFalse(session.isConsumed())
        assertEquals(session.handleId(), session.handleId())
    }

    @Test
    fun `consumed session reports unlocked`() {
        // `isLocked()` is best-effort: a live session may be locked or not
        // depending on the platform's mlock support. A consumed session holds
        // no memory and must always report unlocked — deterministic anywhere.
        val session = SiegelSession(8u)
        session.obliviate()
        assertFalse(session.isLocked())
    }

    @Test
    fun `zero length is rejected`() {
        assertFailsWith<SessionError.InvalidLength> { SiegelSession(0u) }
    }

    @Test
    fun `fill and consume returns sha256`() {
        val secret = bytesOf(0x42, 16)
        val session = SiegelSession(16u)

        assertEquals(SiegelNative.FILL_OK, fill(session, secret))

        val digest = sha256Consume(session)
        val expected = MessageDigest.getInstance("SHA-256").digest(secret)

        assertContentEquals(expected, digest)
        assertTrue(session.isConsumed())
    }

    @Test
    fun `fill rejects a heap-backed buffer`() {
        // The whole point of this path: a non-direct buffer must be refused
        // rather than silently reintroducing unzeroizable copies.
        val session = SiegelSession(8u)
        val heapBuffer = ByteBuffer.allocate(8)
        assertEquals(
            SiegelNative.FILL_ERR_NOT_DIRECT,
            SiegelNative.fillDirect(session.handleId().toLong(), heapBuffer, 8),
        )
        // The session is untouched and still fillable.
        assertEquals(SiegelNative.FILL_OK, fill(session, bytesOf(1, 8)))
    }

    @Test
    fun `fill rejects unknown handle`() {
        val result = withDirectSecretBuffer(4) { buffer ->
            SiegelNative.fillDirect(0xDEADL, buffer, 4)
        }
        assertEquals(SiegelNative.FILL_ERR_INVALID_HANDLE, result)
    }

    @Test
    fun `fill rejects a zero handle`() {
        val result = withDirectSecretBuffer(4) { buffer ->
            SiegelNative.fillDirect(0L, buffer, 4)
        }
        assertEquals(SiegelNative.FILL_ERR_INVALID_HANDLE, result)
    }

    @Test
    fun `fill rejects length exceeding buffer capacity`() {
        // Guards the read-past-the-allocation case: the JVM's reported
        // capacity wins over the caller's claimed length.
        val session = SiegelSession(16u)
        val result = withDirectSecretBuffer(8) { buffer ->
            SiegelNative.fillDirect(session.handleId().toLong(), buffer, 16)
        }
        assertEquals(SiegelNative.FILL_ERR_LEN_MISMATCH, result)
    }

    @Test
    fun `fill rejects length mismatch and recovers`() {
        val session = SiegelSession(16u)
        assertEquals(SiegelNative.FILL_ERR_LEN_MISMATCH, fill(session, ByteArray(8)))
        // Session is still usable for a correct-length fill.
        assertEquals(SiegelNative.FILL_OK, fill(session, ByteArray(16)))
    }

    @Test
    fun `double fill is rejected`() {
        val session = SiegelSession(8u)
        assertEquals(SiegelNative.FILL_OK, fill(session, bytesOf(1, 8)))
        assertEquals(SiegelNative.FILL_ERR_WRONG_STATE, fill(session, bytesOf(1, 8)))
    }

    @Test
    fun `consume before fill is InvalidState`() {
        val session = SiegelSession(8u)
        assertFailsWith<SessionError.InvalidState> { sha256Consume(session) }
        assertFalse(session.isConsumed())
        // After an InvalidState error the session is still fillable.
        assertEquals(SiegelNative.FILL_OK, fill(session, bytesOf(2, 8)))
        sha256Consume(session)
    }

    @Test
    fun `second consume is rejected`() {
        val session = SiegelSession(8u)
        assertEquals(SiegelNative.FILL_OK, fill(session, bytesOf(3, 8)))
        sha256Consume(session)
        assertFailsWith<SessionError.Consumed> { sha256Consume(session) }
    }

    @Test
    fun `fill after consume is rejected`() {
        val session = SiegelSession(8u)
        assertEquals(SiegelNative.FILL_OK, fill(session, bytesOf(4, 8)))
        sha256Consume(session)
        assertEquals(SiegelNative.FILL_ERR_WRONG_STATE, fill(session, bytesOf(4, 8)))
    }

    @Test
    fun `obliviate wipes and is idempotent`() {
        val session = SiegelSession(16u)
        assertEquals(SiegelNative.FILL_OK, fill(session, bytesOf(5, 16)))

        session.obliviate()
        assertTrue(session.isConsumed())
        session.obliviate() // no-op the second time

        assertFailsWith<SessionError.Consumed> { sha256Consume(session) }
    }

    @Test
    fun `distinct sessions have distinct handles`() {
        val a = SiegelSession(8u)
        val b = SiegelSession(8u)
        assertNotEquals(a.handleId(), b.handleId())
    }

    @Test
    fun `len reports allocated capacity`() {
        val session = SiegelSession(64u)
        assertEquals(64u, session.len())
        assertEquals(SiegelNative.FILL_OK, fill(session, bytesOf(7, 64)))
        assertEquals(64u, session.len())
    }

    @Test
    fun `handle is invalidated after session is closed`() {
        // `close()` is synchronous: it releases the native box, the registry's
        // Weak can no longer be upgraded, and the next fill is rejected.
        var handle: Long
        SiegelSession(8u).use { session ->
            handle = session.handleId().toLong()
        }
        val rc = withDirectSecretBuffer(8) { buffer ->
            SiegelNative.fillDirect(handle, buffer, 8)
        }
        assertEquals(SiegelNative.FILL_ERR_INVALID_HANDLE, rc)
    }

    @Test
    fun `withSession closes the session`() {
        var handle = 0L
        withSession(8) { session -> handle = session.handleId().toLong() }
        val rc = withDirectSecretBuffer(8) { buffer ->
            SiegelNative.fillDirect(handle, buffer, 8)
        }
        assertEquals(SiegelNative.FILL_ERR_INVALID_HANDLE, rc)
    }

    /**
     * Pins the lifetime hazard that makes [withSession] mandatory: the
     * generated `SiegelSession` registers no `Cleaner`, so an unclosed session
     * survives garbage collection and keeps its mlocked pages and its slot in
     * the 1024-session registry for the life of the process.
     *
     * If BoltFFI ever starts registering a cleaner this test will fail — at
     * which point the warnings on [withSession] and in the README can be
     * relaxed.
     */
    @Test
    fun `unclosed session is not reclaimed by the garbage collector`() {
        val handle = SiegelSession(8u).handleId().toLong()
        repeat(3) {
            System.gc()
            Thread.sleep(50)
        }
        val rc = withDirectSecretBuffer(8) { buffer ->
            SiegelNative.fillDirect(handle, buffer, 8)
        }
        assertEquals(
            SiegelNative.FILL_OK,
            rc,
            "expected the leaked session to still be live; if this now fails, " +
                "BoltFFI may have added cleaner support — see withSession's KDoc",
        )
    }

    @Test
    fun `fillSession rejects a partially written buffer`() {
        // `fillDirect` reads from the allocation's base address, so a short
        // write would otherwise load whatever sat at index 0.
        val session = SiegelSession(16u)
        assertFailsWith<IllegalArgumentException> {
            fillSession(session, 16) { buffer -> buffer.put(ByteArray(8)) }
        }
        assertFalse(session.isConsumed())
    }

    @Test
    fun `fillDirect reads from index zero regardless of position`() {
        // Locks the documented base-address semantics: the Java-side cursor is
        // ignored. `fillSession` is the guard-railed API; this asserts the
        // primitive behaves as documented rather than as a NIO cursor.
        val session = SiegelSession(8u)
        val expected = bytesOf(0x5A, 8)
        val rc = withDirectSecretBuffer(8) { buffer ->
            buffer.put(expected)
            buffer.position(4) // deliberately repositioned
            SiegelNative.fillDirect(session.handleId().toLong(), buffer, 8)
        }
        assertEquals(SiegelNative.FILL_OK, rc)
        assertContentEquals(
            MessageDigest.getInstance("SHA-256").digest(expected),
            sha256Consume(session),
        )
    }

    @Test
    fun `withDirectSecretBuffer wipes the buffer afterwards`() {
        // The wipe is what makes this path safe; if it regresses, the secret
        // outlives the call in off-heap memory.
        lateinit var escaped: ByteBuffer
        withDirectSecretBuffer(16) { buffer ->
            buffer.put(bytesOf(0x7F, 16))
            escaped = buffer
        }
        val readBack = ByteArray(16)
        escaped.duplicate().apply { clear() }.get(readBack)
        assertContentEquals(ByteArray(16), readBack)
    }

    @Test
    fun `withDirectSecretBuffer wipes even when the block throws`() {
        lateinit var escaped: ByteBuffer
        assertFailsWith<IllegalStateException> {
            withDirectSecretBuffer(16) { buffer ->
                buffer.put(bytesOf(0x7F, 16))
                escaped = buffer
                error("boom")
            }
        }
        val readBack = ByteArray(16)
        escaped.duplicate().apply { clear() }.get(readBack)
        assertContentEquals(ByteArray(16), readBack)
    }
}
