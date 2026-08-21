# Swift bindings for `siegel-uniffi`

Contains integration tests for foreign Swift bindings of `Siegel` and Swift package builds.

## Build the XCFramework

```bash
./swift/build_swift.sh
```

## Run the integration tests

```bash
./swift/test_swift.sh
```

Builds the framework, copies the generated Swift into `swift/tests/`, picks an
available iPhone simulator, and runs the XCTest suite via `xcodebuild test`.

## Two bindings

This directory tests both binding crates. The runner takes the binding as an argument:

```sh
./swift/test_swift.sh uniffi
./swift/test_swift.sh boltffi
```

| package | crate | built by |
|---------|-------|----------|
| `tests/` | `siegel-uniffi` | `build_swift.sh` |
| `boltffi-tests/` | `siegel-boltffi` | `build_boltffi_swift.sh` (wraps `boltffi pack apple`) |
