// Android instrumented-test module

plugins {
    id("com.android.library")
    id("org.jetbrains.kotlin.android")
}

android {
    namespace = "siegel.android"
    compileSdk = 34

    defaultConfig {
        minSdk = 24
        testInstrumentationRunner = "androidx.test.runner.AndroidJUnitRunner"
        // x86_64 for the emulator, arm64-v8a for local physical-device runs.
        ndk {
            abiFilters += setOf("x86_64", "arm64-v8a")
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
}

kotlin {
    jvmToolchain(17)
}

dependencies {
    // uniffi's generated Kotlin loads the bundled .so through JNA. The AAR
    // ships libjnidispatch.so for every Android ABI
    implementation("net.java.dev.jna:jna:5.14.0@aar")
    androidTestImplementation("org.jetbrains.kotlin:kotlin-test")
    androidTestImplementation("androidx.test.ext:junit:1.2.1")
    androidTestImplementation("androidx.test:runner:1.6.2")
}
