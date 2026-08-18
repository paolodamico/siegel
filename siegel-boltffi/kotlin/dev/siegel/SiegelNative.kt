package dev.siegel

import java.nio.ByteBuffer

/**
 * Raw JNI entry point for filling a session, plus the safe wrapper callers
 * should use.
 */
public object SiegelNative {

    init {
        System.loadLibrary("siegel_boltffi")
    }

    /**
     * Low-level fill. Prefer [fillSession].
     *
     * Copies [len] bytes from a **direct** [buffer] into the session's
     * protected memory, reading the **first [len] bytes of the allocation**.
     *
     * @return [FILL_OK] or one of the `FILL_ERR_*` codes
     */
    @JvmStatic
    public external fun fillDirect(handle: Long, buffer: ByteBuffer, len: Int): Int

    // Must match the constants in `siegel::session` and `siegel_boltffi::jvm`.
    public const val FILL_OK: Int = 0
    public const val FILL_ERR_INVALID_HANDLE: Int = -1
    public const val FILL_ERR_LEN_MISMATCH: Int = -2
    public const val FILL_ERR_NULL_SRC: Int = -3
    public const val FILL_ERR_WRONG_STATE: Int = -4
    public const val FILL_ERR_PROTECTION: Int = -5
    public const val FILL_ERR_NOT_DIRECT: Int = -6
    public const val FILL_ERR_JNI: Int = -7
}

/**
 * Overwrite a direct buffer's contents with zeros.
 *
 * Only meaningful for direct buffers: on a heap-backed buffer this wipes the
 * one copy you can reach and leaves any GC-relocated duplicates intact.
 */
public fun ByteBuffer.wipe() {
    require(isDirect) { "wipe() is only meaningful on a direct ByteBuffer" }
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
 * Prefer [fillSession] when the buffer is destined for a session as it adds the
 * "written from index 0" check that [SiegelNative.fillDirect] cannot make for
 * itself.
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
 * Moves a secret into [session] through off-heap memory, wiping it afterwards.
 *
 * This is the intended way to fill a session from Kotlin:
 *
 * ```kotlin
 * val session = SiegelSession(32u)
 * val rc = fillSession(session, 32) { buf -> keystore.readInto(buf) }
 * check(rc == SiegelNative.FILL_OK)
 * ```
 *
 * [write] must fill the buffer sequentially from index 0, and must not call
 * `position()`, `limit()`, or `slice()`. [SiegelNative.fillDirect] always reads
 * the first [size] bytes of the allocation, so that is where the secret has to
 * be.
 *
 * The check verifies the byte *count* (the position advanced to [size]). It
 * cannot verify *placement*. Nothing observable distinguishes "wrote a zero byte" from "wrote
 * nothing", so this guards the plausible mistakes rather than correctness.
 *
 * @throws IllegalArgumentException if [write] did not advance the position to
 *   exactly [size].
 */
public fun fillSession(session: SiegelSession, size: Int, write: (ByteBuffer) -> Unit): Int =
    withDirectSecretBuffer(size) { buffer ->
        write(buffer)
        require(buffer.position() == size) {
            "expected $size bytes written, but the buffer's position is ${buffer.position()}"
        }
        SiegelNative.fillDirect(session.handleId().toLong(), buffer, size)
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
 * never release the native session. **IMPORTANT** otherwise, the session won't be
 * cleaned after use.
 */
public inline fun <T> withSession(size: Int, block: (SiegelSession) -> T): T =
    SiegelSession(size.toUInt()).use { block(it) }
