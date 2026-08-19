# Swift bindings for `siegel-uniffi`

Contains integration tests for foreign Swift bindings of `Siegel` and Swift package builds.

## Build the XCFramework

```bash
cargo xtask swift build
```

## Run the integration tests

```bash
cargo xtask swift test
```

Builds the framework, copies the generated Swift into `swift/tests/`, picks an
available iPhone simulator, and runs the XCTest suite via `xcodebuild test`.

## Two bindings

This directory tests both binding crates. The runner takes the binding as an argument:

```sh
cargo xtask swift test uniffi
cargo xtask swift test boltffi
```

Set `VERBOSE=1` to stream the full `xcodebuild` log instead of the filtered one.

| package | crate | built by |
|---------|-------|----------|
| `tests/` | `siegel-uniffi` | `cargo xtask swift build uniffi` |
| `boltffi-tests/` | `siegel-boltffi` | `cargo xtask swift build boltffi` (wraps `boltffi pack apple`) |
