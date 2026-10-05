// The APK module. Everything here is packaging: the app is
// libbaucord_android.so, built by `cargo ndk` (ANDROID-SPEC.md §2, §11)
// through the two Exec tasks at the bottom, which run before AGP's
// own preDebugBuild / preReleaseBuild. There is no Kotlin or Java
// source yet — `android.app.NativeActivity` is the Activity.

plugins {
    id("com.android.application")
}

// ── Knobs (gradle.properties, or -P on the command line) ───────────────
// Android ABI names, comma-separated; the same list feeds cargo-ndk and
// AGP's abiFilters so the APK only declares what it carries.
val cargoTargets: String = (project.findProperty("cargoTargets") as String?)
    ?.takeIf { it.isNotBlank() } ?: "arm64-v8a,x86_64"
// Cargo features for the `android` crate. Empty = the real client;
// "hello" = M0's toolchain smoke test (spec §12).
val cargoFeatures: String = (project.findProperty("cargoFeatures") as String?) ?: ""
// Where cargo-ndk drops the libraries (ABI subdirectories inside).
val jniLibsDir = layout.projectDirectory.dir("src/main/jniLibs")
// The Cargo workspace: the repo root, one level above android/.
val workspaceDir = rootProject.layout.projectDirectory.dir("..")

android {
    namespace = "dev.baucord"
    compileSdk = 36

    defaultConfig {
        applicationId = "dev.baucord"
        // AAudio's floor is 26; 29 is where permissions and audio behave
        // consistently (spec D6). cargo-ndk's --platform matches below.
        minSdk = 29
        targetSdk = 36
        // versionCode counts up every build that leaves the machine;
        // versionName follows the workspace version (spec §11).
        versionCode = 1
        versionName = "1.1.6"

        ndk {
            abiFilters += cargoTargets.split(',').map { it.trim() }
        }
    }

    // Release signing (spec §11): one permanent keystore, referenced
    // from ~/.gradle/gradle.properties, never from the repo:
    //   BAUCORD_KEYSTORE=C:/Users/you/baucord-release.jks
    //   BAUCORD_KEYSTORE_PASSWORD=…
    //   BAUCORD_KEY_ALIAS=baucord
    //   BAUCORD_KEY_PASSWORD=…
    // Without them the release APK is built unsigned (adb refuses it;
    // debug builds use the SDK's debug key automatically).
    val keystorePath = providers.gradleProperty("BAUCORD_KEYSTORE").orNull
    signingConfigs {
        if (keystorePath != null) {
            create("release") {
                storeFile = file(keystorePath)
                storePassword = providers.gradleProperty("BAUCORD_KEYSTORE_PASSWORD").orNull
                keyAlias = providers.gradleProperty("BAUCORD_KEY_ALIAS").orNull
                keyPassword = providers.gradleProperty("BAUCORD_KEY_PASSWORD").orNull
            }
        }
    }

    buildTypes {
        getByName("debug") {
            // The debug APK is what the emulator runs; a debuggable
            // process also lets `adb logcat` and the profiler attach.
            isDebuggable = true
        }
        getByName("release") {
            // Nothing to shrink: there is no bytecode of ours.
            isMinifyEnabled = false
            isShrinkResources = false
            signingConfig = signingConfigs.findByName("release")
        }
    }

    packaging {
        jniLibs {
            // Stored uncompressed and page-aligned (16 KB with AGP ≥
            // 8.5.1), as Play's 16 KB rule wants (spec D4).
            useLegacyPackaging = false
        }
    }

    lint {
        // Lint has nothing to say about a native-only app that should
        // stop a build.
        abortOnError = false
    }
}

// ── cargo-ndk ───────────────────────────────────────────────────────────
// `cargo ndk` finds the NDK through ANDROID_NDK_HOME (or the newest one
// under ANDROID_HOME/ndk), sets CC/AR/linker per target, and copies the
// resulting .so into <output-dir>/<abi>/. Slint's Android backend
// compiles its Java helper in its build.rs with javac + d8, so
// JAVA_HOME and ANDROID_HOME must be set in the environment Gradle
// inherits (spec §3). Always `-p android`: a workspace-wide build
// would unify the desktop-only features in.
fun cargoNdkArgs(release: Boolean): List<String> {
    val args = mutableListOf(
        "ndk",
        "--target", cargoTargets,
        "--platform", "29",
        "--output-dir", jniLibsDir.asFile.absolutePath,
        "build",
        "-p", "android",
    )
    if (release) args += "--release"
    if (cargoFeatures.isNotBlank()) {
        args += listOf("--no-default-features", "--features", cargoFeatures)
    }
    return args
}

val cargoNdkDebug by tasks.registering(Exec::class) {
    group = "build"
    description = "cargo ndk build -p android (debug) → src/main/jniLibs"
    workingDir = workspaceDir.asFile
    commandLine(listOf("cargo") + cargoNdkArgs(release = false))
    // Cargo is the incremental build system here; let it decide.
    outputs.upToDateWhen { false }
}

val cargoNdkRelease by tasks.registering(Exec::class) {
    group = "build"
    description = "cargo ndk build -p android --release → src/main/jniLibs"
    workingDir = workspaceDir.asFile
    commandLine(listOf("cargo") + cargoNdkArgs(release = true))
    outputs.upToDateWhen { false }
}

// AGP creates pre<Variant>Build lazily; `matching` + `configureEach`
// attaches to them whenever they appear.
tasks.matching { it.name == "preDebugBuild" }.configureEach { dependsOn(cargoNdkDebug) }
tasks.matching { it.name == "preReleaseBuild" }.configureEach { dependsOn(cargoNdkRelease) }
