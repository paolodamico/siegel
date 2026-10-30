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
    // JNA discovers `libsiegel_uniffi.{so,dylib}` here; `cargo xtask kotlin
    // build` drops the host cdylib into this directory before tests run.
    systemProperty("jna.library.path", "${rootDir}/libs")
    reports.html.required.set(false)
    testLogging {
        events("passed", "failed", "skipped")
    }
}
