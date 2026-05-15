// JVM-only test module. Compiles the uniffi-generated bindings together
// with the handwritten test sources and links against the host cdylib via
// JNA (the same way uniffi's runtime does on Android).
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
    // uniffi's generated Kotlin code calls into the cdylib through JNA.
    implementation("net.java.dev.jna:jna:5.14.0")
    testImplementation("org.jetbrains.kotlin:kotlin-test")
    testImplementation("junit:junit:4.13.2")
}

tasks.test {
    useJUnit()
    // JNA discovers `libsiegel_uniffi.{so,dylib}` here; `build_kotlin.sh`
    // drops the host cdylib into this directory before tests run.
    systemProperty("jna.library.path", "${rootDir}/libs")
    // We render our own summary in `test_kotlin.sh`; the HTML report is
    // dead weight in CI logs.
    reports.html.required.set(false)
    testLogging {
        events("passed", "failed", "skipped")
    }
}
