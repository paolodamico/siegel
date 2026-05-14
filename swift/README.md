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
