# Kotlin/JVM bindings for `siegel-uniffi`

Integration tests for the Kotlin bindings. Tests run on the JVM (not on an Android device or emulator) against the **host** `cdylib`.

## Run the integration tests

```bash
./kotlin/test_kotlin.sh
```

## Two bindings

This directory tests both binding crates. The runner takes the binding as an
argument:

```sh
./kotlin/test_kotlin.sh            # UniFFI (default)
./kotlin/test_kotlin.sh uniffi
./kotlin/test_kotlin.sh boltffi
```

| module | crate | mechanism |
|--------|-------|-----------|
| `siegel-tests/` | `siegel-uniffi` | JNA, raw `siegel_fill` |
| `siegel-boltffi-tests/` | `siegel-boltffi` | JNI, direct `ByteBuffer` via `SiegelNative.fillDirect` |

`build_boltffi_android.sh` produces the Android distribution; see
[`siegel-boltffi/README.md`](../siegel-boltffi/README.md).
