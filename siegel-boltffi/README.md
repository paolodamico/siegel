# siegel-boltffi

`BoltFFI` bindings for [`siegel`](https://docs.rs/siegel), letting memory-protected secrets cross the foreign boundary into Swift and Kotlin.

One behavioural difference matters on the JVM: **`SiegelSession` is not released by the garbage collector.** See [Session lifetime](#session-lifetime).

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

**Kotlin** uses a direct `ByteBuffer` through a JNI entry point:

```kotlin
withSession(32) { session ->
    val rc = fillSession(session, 32) { buffer -> keystore.readInto(buffer) }
    check(rc == SiegelNative.FILL_OK)
    // The buffer is wiped on the way out, including if the block throws.
    doSomethingWith(session)
}
```

Unless you know what you're doing, always use `withSession { }` or
`SiegelSession(...).use { }`. This differs from `siegel-uniffi`, because
`UniFFI` registers a cleaner. Also note a `ByteArray` would be unsafe here as
it's managed heap.

## Building

Requires the `boltffi` CLI:

```sh
cargo install boltffi_cli --version "$(cargo pkgid boltffi | sed 's/.*[@#]//')" --locked
```

The CLI version must equal the resolved `boltffi` version: the macro emits the
FFI symbols and the CLI generates the glue that calls them, with no
compatibility check between the two, so a mismatch fails at runtime with
`undefined symbol` rather than at link time. The build scripts verify it and
refuse to run on a mismatch, naming the version to install.

Then, from the repository root:

```sh
./swift/build_boltffi_swift.sh
./kotlin/build_boltffi_kotlin.sh
```

For Android distribution:

```sh
./kotlin/build_boltffi_android.sh   # needs the Android NDK
```

This runs `boltffi pack android --release` (emitting `jniLibs/` for all four ABIs) and then adds `SiegelNative.kt`. Running `boltffi pack android` on its own produces an incomplete package: the generated `Siegel.kt` exposes the session class but no way to fill it.

> [!IMPORTANT]
> The cdylib must be compiled with the `BOLTFFI_BINDING_EXPANSION*` environment that `build_boltffi_kotlin.sh` sets. A plain `cargo build` selects the macro's legacy expansion and emits different symbol names than `boltffi generate` targets, producing an `undefined symbol` failure at runtime rather than at link time.
