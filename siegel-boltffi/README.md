# siegel-boltffi

`BoltFFI` bindings for [`siegel`](https://docs.rs/siegel), letting memory-protected secrets cross the foreign boundary into Swift and Kotlin.

> [!WARNING]
> Experimental. `boltffi` is young, pre-1.0 and has shipped breaking releases, so this crate pins it exactly. Prefer [`siegel-uniffi`](https://docs.rs/siegel-uniffi) for production secrets. The two crates are mutually exclusive on Apple: both emit a Swift module named `Siegel`.

One behavioural difference matters on the JVM: **`SiegelSession` is not released by the garbage collector.** See [Session lifetime](#session-lifetime).

## Features

- `jvm`: the JNI fill / take path (`SiegelNative.fillDirect` / `takeDirect`). Required on Android, where building without it is a compile error.
- `tracing`: logs rejected raw calls and memory failures under the `siegel` target.
- `test-utils`: exports SHA-256 helpers and a guard-page segfault probe for the integration suites. Never ship it.

## Filling a session

**Swift** binds the `siegel_fill_bolt` C symbol directly:

```swift
@_silgen_name("siegel_fill_bolt")
func siegel_fill_bolt(_ handle: UInt64, _ src: UnsafePointer<UInt8>?, _ len: Int) -> Int32

let session = try SiegelSession(len: 32)

// Own the allocation, so there is exactly one copy and you control its
// lifetime. `Data`/`[UInt8]` are copy-on-write, i.e. cannot be zeroized
let secret = UnsafeMutableRawBufferPointer.allocate(byteCount: 32, alignment: 1)
defer {
    memset_s(secret.baseAddress, secret.count, 0, secret.count)
    secret.deallocate()
}

try loadFromKeychain(into: secret)            // writes exactly 32 bytes
let rc = siegel_fill_bolt(
    session.handleId(),
    secret.baseAddress?.assumingMemoryBound(to: UInt8.self),
    secret.count
)
guard rc == 0 else { throw MyError.fillFailed(rc) }   // 0 == FILL_OK
```

**Kotlin** writes into a direct `ByteBuffer` through an append-only sink, so the bytes always land where the JNI read starts:

```kotlin
withSession(32) { session ->
    fillSession(session) { sink -> keystore.readInto(sink) } // throws on any refusal
    doSomethingWith(session)
}
```

The staging buffer is wiped on the way out, including if the block throws. Until then it is ordinary off-heap memory: not `mlock`ed, not sealed, and included in core dumps. A `ByteArray` would be worse, as it lives on the managed heap.

## Taking a secret back out

Rust code opens an output session with `SiegelSession::new_written`, and foreign code copies it out with `siegel_take_bolt` (Swift) or `takeSession` (Kotlin), which wipes and consumes the session. Zeroizing the destination is the caller's job; `takeSession` does it for you:

```kotlin
val key = takeSession(output) { buffer -> deriveKey(buffer) }
```

## Session lifetime

Unless you know what you're doing, always use `withSession { }` or `SiegelSession(...).use { }`. The generated `SiegelSession` is `AutoCloseable` but registers no `Cleaner`, unlike `siegel-uniffi`'s, so an unclosed session is never freed.

`obliviate()` and consuming a session free its protected memory and its slot under the process-wide cap of 1024 sessions holding secrets, but not the session object: only `close()` does that, and once 16384 sessions are open, new ones are refused. Calling a closed session throws `IllegalStateException`.

Confine a session to one thread: closing it while another thread is inside one of its methods is a use-after-free.

A Rust panic in a JNI entry point aborts the process in release builds (`panic = "abort"`); `FILL_ERR_JNI` / `TAKE_ERR_JNI` only surface in builds that unwind.

## Building

Requires the `boltffi` CLI, at exactly the version `Cargo.lock` pins:

```sh
cargo install boltffi_cli --version "$(cargo pkgid boltffi | sed 's/.*[@#]//')" --locked
```

The CLI version must equal the resolved `boltffi` version, otherwise this could lead to an `undefined symbol` error.

Then, from the repository root:

```sh
cargo xtask swift build boltffi
cargo xtask kotlin build boltffi
```

For Android distribution:

```sh
cargo xtask kotlin android   # needs the Android NDK
```

This runs `boltffi pack android --release` (emitting `jniLibs/` for all four ABIs) and then adds `SiegelNative.kt`. Running `boltffi pack android` on its own produces an incomplete package: the generated `Siegel.kt` exposes the session class but no way to fill it.

> [!IMPORTANT]
> `boltffi pack` sets the `BOLTFFI_BINDING_*` environment itself. A direct `cargo build` of the cdylib, as `cargo xtask kotlin build boltffi` does, must set it too: without it the macro selects its legacy expansion and emits different symbol names than the generated glue targets, failing with `undefined symbol` at runtime.
