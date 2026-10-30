package dev.siegel

import java.nio.ByteBuffer

/**
 * Raw JNI entry points for filling and taking a session. Prefer [fillSession]
 * and [takeSession], which check the codes and wipe the staging buffer.
 */
public object SiegelNative {

    init {
        System.loadLibrary("siegel_boltffi")
    }

    /**
     * Low-level fill. Prefer [fillSession].
     *
     * Copies [len] bytes from a **direct** [buffer] whose capacity is exactly
     * [len] into the session's protected memory. Reads [len] bytes at the
     * buffer's JNI base address: Java position and limit are ignored.
     *
     * @return [FILL_OK] or one of the `FILL_ERR_*` codes
     */
    @JvmStatic
    public external fun fillDirect(handle: Long, buffer: ByteBuffer, len: Int): Int

    /**
     * Low-level take. Prefer [takeSession].
     *
     * Copies a secret Rust wrote into a **writable direct** [buffer] whose capacity
     * is exactly [len] (the session's capacity), then wipes and consumes the
     * session. Writes at the buffer's JNI base address. Zeroizing [buffer]
     * afterwards is the caller's job.
     *
     * @return [TAKE_OK] or one of the `TAKE_ERR_*` codes
     */
    @JvmStatic
    public external fun takeDirect(handle: Long, buffer: ByteBuffer, len: Int): Int

    // Must match the constants in `siegel::session` and `siegel_boltffi::jvm`.
    public const val FILL_OK: Int = 0
    public const val FILL_ERR_INVALID_HANDLE: Int = -1
    public const val FILL_ERR_LEN_MISMATCH: Int = -2
    public const val FILL_ERR_NULL_SRC: Int = -3
    public const val FILL_ERR_WRONG_STATE: Int = -4
    public const val FILL_ERR_PROTECTION: Int = -5
    public const val FILL_ERR_NOT_DIRECT: Int = -6

    /** A Rust panic, in builds that unwind; release builds abort instead. */
    public const val FILL_ERR_JNI: Int = -7

    public const val TAKE_OK: Int = 0
    public const val TAKE_ERR_INVALID_HANDLE: Int = -1
    public const val TAKE_ERR_LEN_MISMATCH: Int = -2
    public const val TAKE_ERR_NULL_DST: Int = -3
    public const val TAKE_ERR_WRONG_STATE: Int = -4
    public const val TAKE_ERR_PROTECTION: Int = -5
    public const val TAKE_ERR_CANARY: Int = -6
    public const val TAKE_ERR_NOT_DIRECT: Int = -7

    /** A Rust panic, in builds that unwind; release builds abort instead. */
    public const val TAKE_ERR_JNI: Int = -8
}

/** A raw fill or take was refused. [code] is the `FILL_ERR_*` or `TAKE_ERR_*` value. */
public class SiegelNativeException(public val code: Int, message: String) :
    RuntimeException(message)

private fun fillFailure(code: Int): SiegelNativeException {
    val reason = when (code) {
        SiegelNative.FILL_ERR_INVALID_HANDLE -> "unknown or closed session"
        SiegelNative.FILL_ERR_LEN_MISMATCH -> "buffer size doesn't match the session's capacity"
        SiegelNative.FILL_ERR_NULL_SRC -> "null buffer"
        SiegelNative.FILL_ERR_WRONG_STATE -> "session was already filled or consumed"
        SiegelNative.FILL_ERR_PROTECTION -> "sealing failed; the session is unusable"
        SiegelNative.FILL_ERR_NOT_DIRECT -> "not a direct ByteBuffer"
        else -> "unexpected failure"
    }
    return SiegelNativeException(code, "fill failed ($code): $reason")
}

private fun takeFailure(code: Int): SiegelNativeException {
    val reason = when (code) {
        SiegelNative.TAKE_ERR_INVALID_HANDLE -> "unknown or closed session"
        SiegelNative.TAKE_ERR_LEN_MISMATCH -> "buffer size doesn't match the session's capacity"
        SiegelNative.TAKE_ERR_NULL_DST -> "null buffer"
        SiegelNative.TAKE_ERR_WRONG_STATE -> "no Rust-written secret, or it was already taken"
        SiegelNative.TAKE_ERR_PROTECTION -> "unsealing failed; the session is consumed"
        SiegelNative.TAKE_ERR_CANARY -> "canary check failed: possible memory corruption"
        SiegelNative.TAKE_ERR_NOT_DIRECT -> "not a writable direct ByteBuffer"
        else -> "unexpected failure"
    }
    return SiegelNativeException(code, "take failed ($code): $reason")
}

/**
 * Overwrite a buffer's contents with zeros. Never throws, so it is safe in a
 * `finally`.
 *
 * Only fully effective on a direct buffer: on a heap-backed buffer this wipes the
 * one copy you can reach and leaves any GC-relocated duplicates intact. A
 * read-only view is left as is; wipe the buffer it views (as [takeSession] does).
 */
public fun ByteBuffer.wipe() {
    if (isReadOnly) return
    // Overwrite in place: allocating here could fail under memory pressure and
    // leave the secret resident.
    val view = duplicate().apply { clear() }
    while (view.hasRemaining()) {
        view.put(0)
    }
}

/**
 * Runs [block] with a direct buffer of [size] bytes, wiping it afterwards.
 *
 * The wipe runs in a `finally`, so it happens even if [block] throws.
 *
 * The buffer is ordinary off-heap memory: not `mlock`ed, not sealed, and
 * included in core dumps. The secret is exposed there until the wipe, so keep
 * [block] to the one fill or take.
 */
public inline fun <T> withDirectSecretBuffer(size: Int, block: (ByteBuffer) -> T): T {
    val buffer = ByteBuffer.allocateDirect(size)
    try {
        return block(buffer)
    } finally {
        buffer.wipe()
    }
}

/**
 * Append-only writer over [fillSession]'s staging buffer. Bytes land in
 * write order from index 0, which is exactly what [SiegelNative.fillDirect]
 * reads; there is no way to reposition. Don't keep it past the callback.
 */
public class SecretSink internal constructor(private val buffer: ByteBuffer) {
    /** Bytes still to write. */
    public val remaining: Int get() = buffer.remaining()

    public fun put(b: Byte) {
        buffer.put(b)
    }

    public fun put(src: ByteArray, offset: Int = 0, length: Int = src.size - offset) {
        buffer.put(src, offset, length)
    }

    /** Copies [src]'s remaining bytes, advancing its position. */
    public fun put(src: ByteBuffer) {
        buffer.put(src)
    }
}

/**
 * Moves a secret into [session] through off-heap memory, wiping it afterwards.
 *
 * This is the intended way to fill a session from Kotlin:
 *
 * ```kotlin
 * fillSession(session) { sink -> keystore.readInto(sink) }
 * ```
 *
 * [write] must put exactly `session.len()` bytes into the sink.
 *
 * @throws IllegalArgumentException if [write] put fewer bytes.
 * @throws java.nio.BufferOverflowException if [write] put more.
 * @throws SiegelNativeException if Rust refused the fill.
 */
public fun fillSession(session: SiegelSession, write: (SecretSink) -> Unit) {
    val size = session.len().toInt()
    withDirectSecretBuffer(size) { buffer ->
        write(SecretSink(buffer))
        require(buffer.position() == size) {
            "expected $size bytes written, got ${buffer.position()}"
        }
        val rc = SiegelNative.fillDirect(session.handleId().toLong(), buffer, size)
        if (rc != SiegelNative.FILL_OK) throw fillFailure(rc)
    }
}

/**
 * Takes the secret Rust wrote into [session] (see `SiegelSession::new_written`),
 * runs [read] on a read-only view of it, and wipes it afterwards. Consumes the
 * session.
 *
 * Don't let the buffer escape [read]: it is wiped as soon as [read] returns.
 *
 * @throws SiegelNativeException if Rust refused the take.
 */
public fun <T> takeSession(session: SiegelSession, read: (ByteBuffer) -> T): T {
    val size = session.len().toInt()
    return withDirectSecretBuffer(size) { buffer ->
        val rc = SiegelNative.takeDirect(session.handleId().toLong(), buffer, size)
        if (rc != SiegelNative.TAKE_OK) throw takeFailure(rc)
        read(buffer.asReadOnlyBuffer())
    }
}

/**
 * Runs [block] with a fresh session of [size] bytes and closes it afterwards.
 *
 * **Not thread-safe.** `BoltFFI` hands the JVM a raw `Box` pointer with no
 * reference counting, so closing a session while another thread is inside
 * any of its methods is a use-after-free. Confine a session to one thread.
 *
 * **Use this, or `SiegelSession(...).use { }`, rather than constructing a
 * session and letting it go out of scope.** The generated [SiegelSession] is
 * `AutoCloseable` but registers no `Cleaner`, so the garbage collector will
 * never release the native session. `obliviate()` is not a substitute: it
 * frees the secret and its slot under the session cap, but not the session.
 *
 * Calling a session after [block] returns throws `IllegalStateException`.
 */
public inline fun <T> withSession(size: Int, block: (SiegelSession) -> T): T {
    require(size > 0) { "session size must be positive, got $size" }
    return SiegelSession(size.toUInt()).use { block(it) }
}
