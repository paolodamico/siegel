# Android bindings for `siegel-uniffi`

Integration tests for the Kotlin bindings, run as **instrumented tests on a real Android runtime** (emulator or device) for specific libc behavior.

## Fast compile gate (no emulator)

Cross-compiles the `.so`, regenerates bindings, and compiles the tests into an
APK to catch Rust/binding/Kotlin issues without booting an
emulator:

```bash
./kotlin/build_android.sh
(cd kotlin && ./gradlew :siegel-android:assembleDebugAndroidTest)
```

## Full test run on an emulator/device

Boot an x86_64 emulator (or connect a device), then:

```bash
./kotlin/test_android.sh
```
