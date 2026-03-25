@file:Suppress("DEPRECATION", "DEPRECATION_ERROR")

import java.io.ByteArrayOutputStream

plugins {
    alias(libs.plugins.android.application)
    alias(libs.plugins.kotlin.android)
    alias(libs.plugins.kotlin.compose)
    alias(libs.plugins.ksp)
}

android {
    namespace = "net.murmur.app"
    compileSdk = 36

    defaultConfig {
        applicationId = "net.murmur.app"
        minSdk = 26
        targetSdk = 36
        versionCode = 1
        versionName = "0.1.0"

        testInstrumentationRunner = "androidx.test.runner.AndroidJUnitRunner"

        ndk {
            // Only package the ABI matching the current jniLibs content.
            abiFilters += setOf("x86_64", "arm64-v8a")
        }
    }

    buildTypes {
        release {
            isMinifyEnabled = false
            proguardFiles(
                getDefaultProguardFile("proguard-android-optimize.txt"),
                "proguard-rules.pro"
            )
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    kotlinOptions {
        jvmTarget = "17"
    }

    buildFeatures {
        compose = true
    }

    packaging {
        jniLibs {
            useLegacyPackaging = false  // enables compressed .so -> smaller APK
        }
    }

    // JNI libraries are placed under src/main/jniLibs/<abi>/libmurmur_ffi.so
    // They are built with cargo-ndk:
    //   cargo ndk -t arm64-v8a -t armeabi-v7a -t x86_64 \
    //     -o app/src/main/jniLibs build --release -p murmur-ffi
    sourceSets {
        getByName("main") {
            jniLibs.srcDirs("src/main/jniLibs")
        }
    }
}

// ---------------------------------------------------------------------------
// Cargo-NDK task — builds the Rust FFI library for all ABI targets.
// Run with: ./gradlew cargoBuildDebug  or  ./gradlew cargoBuildRelease
// ---------------------------------------------------------------------------

val cargoBuildDebug by tasks.registering(Exec::class) {
    group = "rust"
    description = "Build murmur-ffi for Android (debug) via cargo-ndk"
    workingDir = rootProject.rootDir.parentFile.parentFile // repo root
    commandLine(
        "cargo", "ndk",
        "-t", "arm64-v8a",
        "-t", "armeabi-v7a",
        "-t", "x86_64",
        "-o", "${projectDir}/src/main/jniLibs",
        "build",
        "-p", "murmur-ffi"
    )
}

val cargoBuildRelease by tasks.registering(Exec::class) {
    group = "rust"
    description = "Build murmur-ffi for Android (release) via cargo-ndk"
    workingDir = rootProject.rootDir.parentFile.parentFile // repo root
    commandLine(
        "cargo", "ndk",
        "-t", "arm64-v8a",
        "-t", "armeabi-v7a",
        "-t", "x86_64",
        "-o", "${projectDir}/src/main/jniLibs",
        "build", "--release",
        "-p", "murmur-ffi"
    )
}

// Generate UniFFI Kotlin bindings after the release build.
val uniffiBindgen by tasks.registering(Exec::class) {
    group = "rust"
    description = "Generate Kotlin bindings from murmur-ffi via uniffi-bindgen"
    dependsOn(cargoBuildRelease)
    workingDir = rootProject.rootDir.parentFile.parentFile // repo root
    commandLine(
        "uniffi-bindgen", "generate",
        "--library",
        "target/aarch64-linux-android/release/libmurmur_ffi.so",
        "--language", "kotlin",
        "--out-dir",
        "${projectDir}/src/main/java/net/murmur/generated"
    )
}

dependencies {
    // JNA is required by UniFFI-generated Kotlin bindings.
    implementation("net.java.dev.jna:jna:5.18.0@aar")

    implementation(libs.androidx.core.ktx)
    implementation(libs.androidx.lifecycle.runtime.ktx)
    implementation(libs.androidx.lifecycle.viewmodel.compose)
    implementation(libs.androidx.activity.compose)

    // Compose BOM — pins all Compose library versions.
    implementation(platform(libs.androidx.compose.bom))
    implementation(libs.androidx.ui)
    implementation(libs.androidx.ui.graphics)
    implementation(libs.androidx.ui.tooling.preview)
    implementation(libs.androidx.material3)
    implementation(libs.androidx.material.icons)

    // Room — DAG entry persistence.
    implementation(libs.androidx.room.runtime)
    implementation(libs.androidx.room.ktx)
    ksp(libs.androidx.room.compiler)

    // Coroutines
    implementation(libs.kotlinx.coroutines.android)

    // WorkManager — background sync tasks.
    implementation(libs.androidx.work.runtime.ktx)

    // Tests
    testImplementation(libs.junit)
    androidTestImplementation(libs.androidx.junit)
    androidTestImplementation(libs.androidx.espresso.core)
    androidTestImplementation(libs.androidx.test.runner)
    androidTestImplementation(libs.androidx.test.core)
    androidTestImplementation(platform(libs.androidx.compose.bom))
    androidTestImplementation(libs.androidx.ui.test.junit4)
    debugImplementation(libs.androidx.ui.tooling)
    debugImplementation(libs.androidx.ui.test.manifest)
}
