package dev.siegel

import java.nio.BufferOverflowException
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
 * Raw fill through the direct-ByteBuffer JNI path, returning the code so tests
 * can assert on it. [fillSession] is the checked API.
 *
 * Deliberately *not* a `ByteArray`: BoltFFI's generated `&[u8]` lowering would
 * put the secret on the managed heap, where a copying collector can leave
 * unreachable duplicates and `GetByteArrayElements` may take a further copy
 * that Kotlin cannot wipe.
 */
private fun fillRaw(handle: Long, bytes: ByteArray): Int =
    withDirectSecretBuffer(bytes.size) { buffer ->
        buffer.put(bytes)
        SiegelNative.fillDirect(handle, buffer, bytes.size)
    }

private fun fill(session: SiegelSession, bytes: ByteArray): Int =
    fillRaw(session.handleId().toLong(), bytes)

/** Arbitrary non-secret bytes; rejected takes must leave the destination holding these. */
private const val SENTINEL: Byte = 0xA5.toByte()

/** Raw take into a sentinel-filled buffer of [len] bytes, returning the code and its contents. */
private fun takeRaw(session: SiegelSession, len: Int): Pair<Int, ByteArray> =
    withDirectSecretBuffer(len) { buffer ->
        while (buffer.hasRemaining()) buffer.put(SENTINEL)
        val rc = SiegelNative.takeDirect(session.handleId().toLong(), buffer, len)
        rc to ByteArray(len).also { buffer.duplicate().apply { clear() }.get(it) }
    }

private fun bytesOf(value: Byte, len: Int): ByteArray = ByteArray(len) { value }

private fun sha256(bytes: ByteArray): ByteArray = MessageDigest.getInstance("SHA-256").digest(bytes)

/** A session holding SHA-256 of [secret], written by Rust and ready to take. */
private fun resealed(secret: ByteArray): SiegelSession =
    SiegelSession(secret.size.toUInt()).use { input ->
        assertEquals(SiegelNative.FILL_OK, fill(input, secret))
        sha256Reseal(input)
    }

class SiegelSessionTests {

    @Test
    fun `new session is not consumed`(): Unit = withSession(32) { session ->
        assertFalse(session.isConsumed())
        assertEquals(session.handleId(), session.handleId())
    }

    @Test
    fun `consumed session reports unlocked`(): Unit = withSession(8) { session ->
        // `isLocked()` is best-effort: a live session may be locked or not
        // depending on the platform's mlock support. A consumed session holds
        // no memory and must always report unlocked — deterministic anywhere.
        session.obliviate()
        assertFalse(session.isLocked())
    }

    @Test
    fun `zero length is rejected`() {
        assertFailsWith<SessionError.InvalidLength> { SiegelSession(0u) }
    }

    @Test
    fun `withSession rejects a non-positive size`() {
        // `-4` would otherwise become 4294967292 through `toUInt()`.
        assertFailsWith<IllegalArgumentException> { withSession(-4) {} }
        assertFailsWith<IllegalArgumentException> { withSession(0) {} }
    }

    @Test
    fun `fill and consume returns sha256`(): Unit = withSession(16) { session ->
        val secret = bytesOf(0x42, 16)
        fillSession(session) { it.put(secret) }

        assertContentEquals(sha256(secret), sha256Consume(session))
        assertTrue(session.isConsumed())
    }

    @Test
    fun `fill rejects a heap-backed buffer`(): Unit = withSession(8) { session ->
        // The whole point of this path: a non-direct buffer must be refused
        // rather than silently reintroducing unzeroizable copies.
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
        assertEquals(SiegelNative.FILL_ERR_INVALID_HANDLE, fillRaw(0xDEADL, ByteArray(4)))
    }

    @Test
    fun `fill rejects a zero handle`() {
        assertEquals(SiegelNative.FILL_ERR_INVALID_HANDLE, fillRaw(0L, ByteArray(4)))
    }

    @Test
    fun `fill rejects length exceeding buffer capacity`(): Unit = withSession(16) { session ->
        // Guards the read-past-the-allocation case: the JVM's reported
        // capacity wins over the caller's claimed length.
        val result = withDirectSecretBuffer(8) { buffer ->
            SiegelNative.fillDirect(session.handleId().toLong(), buffer, 16)
        }
        assertEquals(SiegelNative.FILL_ERR_LEN_MISMATCH, result)
    }

    @Test
    fun `fill rejects a buffer larger than the length`(): Unit = withSession(8) { session ->
        // An oversized buffer means the secret may not start where the read does.
        val result = withDirectSecretBuffer(16) { buffer ->
            SiegelNative.fillDirect(session.handleId().toLong(), buffer, 8)
        }
        assertEquals(SiegelNative.FILL_ERR_LEN_MISMATCH, result)
        assertFalse(session.isConsumed())
    }

    @Test
    fun `fill rejects a negative length`(): Unit = withSession(8) { session ->
        val result = withDirectSecretBuffer(8) { buffer ->
            SiegelNative.fillDirect(session.handleId().toLong(), buffer, -8)
        }
        assertEquals(SiegelNative.FILL_ERR_LEN_MISMATCH, result)
    }

    @Test
    fun `fill rejects length mismatch and recovers`(): Unit = withSession(16) { session ->
        assertEquals(SiegelNative.FILL_ERR_LEN_MISMATCH, fill(session, ByteArray(8)))
        // Session is still usable for a correct-length fill.
        assertEquals(SiegelNative.FILL_OK, fill(session, ByteArray(16)))
    }

    @Test
    fun `double fill is rejected`(): Unit = withSession(8) { session ->
        assertEquals(SiegelNative.FILL_OK, fill(session, bytesOf(1, 8)))
        assertEquals(SiegelNative.FILL_ERR_WRONG_STATE, fill(session, bytesOf(1, 8)))
    }

    @Test
    fun `consume before fill is InvalidState`(): Unit = withSession(8) { session ->
        assertFailsWith<SessionError.InvalidState> { sha256Consume(session) }
        assertFalse(session.isConsumed())
        // After an InvalidState error the session is still fillable.
        assertEquals(SiegelNative.FILL_OK, fill(session, bytesOf(2, 8)))
        sha256Consume(session)
    }

    @Test
    fun `second consume is rejected`(): Unit = withSession(8) { session ->
        assertEquals(SiegelNative.FILL_OK, fill(session, bytesOf(3, 8)))
        sha256Consume(session)
        assertFailsWith<SessionError.Consumed> { sha256Consume(session) }
    }

    @Test
    fun `fill after consume is rejected`(): Unit = withSession(8) { session ->
        assertEquals(SiegelNative.FILL_OK, fill(session, bytesOf(4, 8)))
        sha256Consume(session)
        assertEquals(SiegelNative.FILL_ERR_WRONG_STATE, fill(session, bytesOf(4, 8)))
    }

    @Test
    fun `obliviate wipes and is idempotent`(): Unit = withSession(16) { session ->
        assertEquals(SiegelNative.FILL_OK, fill(session, bytesOf(5, 16)))

        session.obliviate()
        assertTrue(session.isConsumed())
        session.obliviate() // no-op the second time

        assertFailsWith<SessionError.Consumed> { sha256Consume(session) }
    }

    @Test
    fun `distinct sessions have distinct handles`(): Unit = withSession(8) { a ->
        withSession(8) { b -> assertNotEquals(a.handleId(), b.handleId()) }
    }

    @Test
    fun `len reports allocated capacity`(): Unit = withSession(64) { session ->
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
        assertEquals(SiegelNative.FILL_ERR_INVALID_HANDLE, fillRaw(handle, ByteArray(8)))
    }

    @Test
    fun `withSession closes the session`() {
        var handle = 0L
        withSession(8) { session -> handle = session.handleId().toLong() }
        assertEquals(SiegelNative.FILL_ERR_INVALID_HANDLE, fillRaw(handle, ByteArray(8)))
    }

    /** A session that escapes `withSession` must refuse calls rather than touch freed memory. */
    @Test
    fun `calls after close throw`() {
        val escaped = withSession(8) { it }
        assertFailsWith<IllegalStateException> { escaped.handleId() }
        assertFailsWith<IllegalStateException> { escaped.isConsumed() }
        assertFailsWith<IllegalStateException> { sha256Consume(escaped) }
        escaped.close() // idempotent
    }

    /**
     * Pins the lifetime hazard that makes [withSession] mandatory: the
     * generated `SiegelSession` registers no `Cleaner`, so an unclosed session
     * survives garbage collection and keeps its pages and its slot under the
     * session cap until it is consumed.
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
        assertEquals(
            SiegelNative.FILL_OK,
            fillRaw(handle, ByteArray(8)),
            "expected the leaked session to still be live; if this now fails, " +
                "BoltFFI may have added cleaner support — see withSession's KDoc",
        )
    }

    @Test
    fun `fillSession rejects a partial write`(): Unit = withSession(16) { session ->
        assertFailsWith<IllegalArgumentException> {
            fillSession(session) { it.put(ByteArray(8)) }
        }
        assertFalse(session.isConsumed())
    }

    @Test
    fun `fillSession rejects an overlong write`(): Unit = withSession(8) { session ->
        assertFailsWith<BufferOverflowException> {
            fillSession(session) { it.put(ByteArray(9)) }
        }
        assertFalse(session.isConsumed())
    }

    @Test
    fun `fillSession writes in order from index zero`(): Unit = withSession(8) { session ->
        val secret = byteArrayOf(1, 2, 3, 4, 5, 6, 7, 8)
        fillSession(session) { sink ->
            sink.put(secret[0])
            sink.put(secret, 1, 3)
            sink.put(ByteBuffer.wrap(secret, 4, 4))
            assertEquals(0, sink.remaining)
        }
        assertContentEquals(sha256(secret), sha256Consume(session))
    }

    @Test
    fun `fillSession throws when Rust refuses the fill`(): Unit = withSession(8) { session ->
        fillSession(session) { it.put(bytesOf(1, 8)) }
        val error = assertFailsWith<SiegelNativeException> {
            fillSession(session) { it.put(bytesOf(1, 8)) }
        }
        assertEquals(SiegelNative.FILL_ERR_WRONG_STATE, error.code)
    }

    @Test
    fun `fillDirect ignores the position`(): Unit = withSession(8) { session ->
        // Locks the documented base-address semantics: the Java-side cursor is
        // ignored. `fillSession` is the guard-railed API; this asserts the
        // primitive behaves as documented rather than as a NIO cursor.
        val expected = bytesOf(0x5A, 8)
        val rc = withDirectSecretBuffer(8) { buffer ->
            buffer.put(expected)
            buffer.position(4) // deliberately repositioned
            SiegelNative.fillDirect(session.handleId().toLong(), buffer, 8)
        }
        assertEquals(SiegelNative.FILL_OK, rc)
        assertContentEquals(sha256(expected), sha256Consume(session))
    }

    @Test
    fun `fillDirect reads a slice from the slice's base`(): Unit = withSession(8) { session ->
        // `slice()` moves the JNI base address; its capacity is what's left.
        val tail = bytesOf(0x22, 8)
        val rc = withDirectSecretBuffer(16) { buffer ->
            buffer.put(bytesOf(0x11, 8)).put(tail)
            buffer.position(8)
            SiegelNative.fillDirect(session.handleId().toLong(), buffer.slice(), 8)
        }
        assertEquals(SiegelNative.FILL_OK, rc)
        assertContentEquals(sha256(tail), sha256Consume(session))
    }

    @Test
    fun `wipe clears a heap buffer without throwing`() {
        val heap = ByteBuffer.wrap(bytesOf(0x7F, 8))
        heap.wipe()
        assertContentEquals(ByteArray(8), heap.array())
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

    @Test
    fun `reseal then take returns sha256`() {
        val secret = bytesOf(0x42, 16)
        resealed(secret).use { output ->
            assertFalse(output.isConsumed())
            assertEquals(32u, output.len())

            val (rc, digest) = takeRaw(output, 32)
            assertEquals(SiegelNative.TAKE_OK, rc)
            assertContentEquals(sha256(secret), digest)
            assertTrue(output.isConsumed())
        }
    }

    @Test
    fun `takeSession hands over the secret and wipes it`() {
        val secret = bytesOf(0x42, 16)
        lateinit var escaped: ByteBuffer
        val digest = resealed(secret).use { output ->
            takeSession(output) { buffer ->
                escaped = buffer
                ByteArray(buffer.remaining()).also { buffer.get(it) }
            }
        }
        assertContentEquals(sha256(secret), digest)
        val readBack = ByteArray(32)
        escaped.duplicate().apply { clear() }.get(readBack)
        assertContentEquals(ByteArray(32), readBack)
    }

    @Test
    fun `takeSession throws when Rust refuses the take`(): Unit = withSession(8) { session ->
        assertEquals(SiegelNative.FILL_OK, fill(session, bytesOf(6, 8)))
        val error = assertFailsWith<SiegelNativeException> { takeSession(session) { it } }
        assertEquals(SiegelNative.TAKE_ERR_WRONG_STATE, error.code)
    }

    @Test
    fun `written session can be passed back into rust`() {
        resealed(bytesOf(0x42, 16)).use { output ->
            assertContentEquals(sha256(sha256(bytesOf(0x42, 16))), sha256Consume(output))
            assertEquals(SiegelNative.TAKE_ERR_WRONG_STATE, takeRaw(output, 32).first)
        }
    }

    @Test
    fun `take rejects unknown handle`() {
        val rc = withDirectSecretBuffer(4) { SiegelNative.takeDirect(0xDEADL, it, 4) }
        assertEquals(SiegelNative.TAKE_ERR_INVALID_HANDLE, rc)
    }

    @Test
    fun `take rejects a heap-backed buffer`() {
        resealed(bytesOf(1, 8)).use { output ->
            val heap = ByteBuffer.allocate(32)
            val rc = SiegelNative.takeDirect(output.handleId().toLong(), heap, 32)
            assertEquals(SiegelNative.TAKE_ERR_NOT_DIRECT, rc)
            assertFalse(output.isConsumed())
        }
    }

    @Test
    fun `take rejects a read-only buffer`() {
        resealed(bytesOf(1, 8)).use { output ->
            val readOnly = ByteBuffer.allocateDirect(32).asReadOnlyBuffer()
            val rc = SiegelNative.takeDirect(output.handleId().toLong(), readOnly, 32)
            assertEquals(SiegelNative.TAKE_ERR_NOT_DIRECT, rc)
            assertFalse(output.isConsumed())
        }
    }

    @Test
    fun `wipe refuses a read-only view rather than silently skipping it`() {
        val buffer = ByteBuffer.allocateDirect(8).put(bytesOf(0x7F, 8))
        assertFailsWith<IllegalArgumentException> { buffer.asReadOnlyBuffer().wipe() }
        buffer.wipe()
        assertContentEquals(ByteArray(8), ByteArray(8).also { buffer.duplicate().clear().get(it) })
    }

    @Test
    fun `take rejects length mismatch and recovers`() {
        resealed(bytesOf(2, 8)).use { output ->
            val (rc, bytes) = takeRaw(output, 16)
            assertEquals(SiegelNative.TAKE_ERR_LEN_MISMATCH, rc)
            assertContentEquals(bytesOf(SENTINEL, 16), bytes)
            // Session is still takeable with a correctly sized buffer.
            assertEquals(SiegelNative.TAKE_OK, takeRaw(output, 32).first)
        }
    }

    @Test
    fun `take rejects a session filled from native`(): Unit = withSession(8) { session ->
        assertEquals(SiegelNative.FILL_OK, fill(session, bytesOf(6, 8)))

        val (rc, bytes) = takeRaw(session, 8)
        assertEquals(SiegelNative.TAKE_ERR_WRONG_STATE, rc)
        assertContentEquals(bytesOf(SENTINEL, 8), bytes)
        // The filled secret is still there for Rust to consume.
        assertContentEquals(sha256(bytesOf(6, 8)), sha256Consume(session))
    }

    @Test
    fun `take before write is rejected`(): Unit = withSession(8) { session ->
        assertEquals(SiegelNative.TAKE_ERR_WRONG_STATE, takeRaw(session, 8).first)
        assertFalse(session.isConsumed())
    }

    @Test
    fun `second take is rejected`() {
        resealed(bytesOf(3, 8)).use { output ->
            assertEquals(SiegelNative.TAKE_OK, takeRaw(output, 32).first)

            val (rc, bytes) = takeRaw(output, 32)
            assertEquals(SiegelNative.TAKE_ERR_WRONG_STATE, rc)
            assertContentEquals(bytesOf(SENTINEL, 32), bytes)
        }
    }

    @Test
    fun `reseal rejects empty input`(): Unit = withSession(8) { input ->
        assertFailsWith<SessionError.InvalidState> { sha256Reseal(input) }
        assertFalse(input.isConsumed())
    }
}
