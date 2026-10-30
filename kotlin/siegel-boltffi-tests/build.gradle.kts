// JVM-only test module for the BoltFFI bindings.
//
// Unlike `:siegel-tests` (UniFFI + JNA), this module has no JNA dependency:
// BoltFFI generates JNI glue

plugins {
    kotlin("jvm")
}

repositories {
    mavenCentral()
}

kotlin {
    jvmToolchain(17)
}

dependencies {
    testImplementation("org.jetbrains.kotlin:kotlin-test")
    testImplementation("junit:junit:4.13.2")
}

tasks.test {
    useJUnit()
    // `System.loadLibrary` resolves `libsiegel_boltffi_jni.so` and
    // `libsiegel_boltffi.so` from here; `build_boltffi_kotlin.sh` drops both
    // into this directory before tests run.
    systemProperty("java.library.path", "${rootDir}/boltffi-libs")
    // We render our own summary in `test_kotlin.sh boltffi`; the HTML report
    // is dead weight in CI logs.
    reports.html.required.set(false)
    testLogging {
        events("passed", "failed", "skipped")
    }
}
