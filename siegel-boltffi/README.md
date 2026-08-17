# siegel-boltffi

`BoltFFI` bindings for [`siegel`](https://docs.rs/siegel), letting memory-protected secrets cross the foreign boundary into Swift and Kotlin.

One behavioural difference matters on the JVM: **`SiegelSession` is not released by the garbage collector.** See [Session lifetime](#session-lifetime).

## Filling a session

**Swift** binds the `siegel_fill` C symbol directly:

```swift
@_silgen_name("siegel_fill_bolt")
func siegel_fill_bolt(_ handle: UInt64, _ src: UnsafePointer<UInt8>?, _ len: Int) -> Int32

let session = try SiegelSession(len: 32)
var secret = try loadFromKeychain()          // 32 bytes
secret.withUnsafeBufferPointer { buf in
    siegel_fill_bolt(session.handleId(), buf.baseAddress, buf.count)
}
memset_s(&secret, secret.count, 0, secret.count)   // not a plain loop: see below
```

Use `memset_s`, not a zeroing loop — a loop writing zeros to memory that is never read again is a dead store the optimiser may delete. Prefer allocating the buffer manually (`UnsafeMutableRawBufferPointer`) over `Data`/`Array`, whose copy-on-write storage may have left duplicates you cannot reach.

**Kotlin** uses a direct `ByteBuffer` through a JNI entry point:

```kotlin
withSession(32) { session ->
    val rc = fillSession(session, 32) { buffer -> keystore.readInto(buffer) }
    check(rc == SiegelNative.FILL_OK)
    // The buffer is wiped on the way out, including if the block throws.
    doSomethingWith(session)
}
```

Unless you know what you're doing, always use `withSession { }` or `SiegelSession(...).use { }`. This differs from `siegel-uniffi`, beacause UniFFI registers a cleaner. Also note a `ByteArray` would be unsafe here as it's managed heap.

## Building

Requires the `boltffi` CLI:

```sh
cargo install boltffi_cli --locked
```

Then, from the repository root:

```sh
./swift/build_boltffi_swift.sh
./kotlin/build_boltffi_kotlin.sh
```

For Android distribution use `boltffi pack android --release` (needs the NDK), which emits `jniLibs/` for all four ABIs.

> [!IMPORTANT]
> The cdylib must be compiled with the `BOLTFFI_BINDING_EXPANSION*` environment that `build_boltffi_kotlin.sh` sets. A plain `cargo build` selects the macro's legacy expansion and emits different symbol names than `boltffi generate` targets, producing an `undefined symbol` failure at runtime rather than at link time.
