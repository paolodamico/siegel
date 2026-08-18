# Kotlin/JVM bindings for `siegel-uniffi`

Integration tests for the Kotlin bindings. Tests run on the JVM (not on an Android device or emulator) against the **host** `cdylib`.

## Run the integration tests

```bash
cargo xtask kotlin test
```

## Two bindings

This directory tests both binding crates. The runner takes the binding as an
argument:

```sh
cargo xtask kotlin test            # UniFFI (default)
cargo xtask kotlin test uniffi
cargo xtask kotlin test boltffi
```

Set `VERBOSE=1` to stream the full Gradle log instead of the filtered one.

| module | crate | mechanism |
|--------|-------|-----------|
| `siegel-tests/` | `siegel-uniffi` | JNA, raw `siegel_fill` |
| `siegel-boltffi-tests/` | `siegel-boltffi` | JNI, direct `ByteBuffer` via `SiegelNative.fillDirect` |

`cargo xtask kotlin android` produces the Android distribution; see
[`siegel-boltffi/README.md`](../siegel-boltffi/README.md).
