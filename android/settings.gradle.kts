// The Gradle project for the Android APK (ANDROID-SPEC.md §2). It holds
// no Rust: `app/build.gradle.kts` runs cargo-ndk, which drops
// libbaucord_android.so into app/src/main/jniLibs, and Gradle packages
// it. Open this directory (not the repo root) in Android Studio.
pluginManagement {
    repositories {
        google()
        mavenCentral()
        gradlePluginPortal()
    }
}

dependencyResolutionManagement {
    repositoriesMode.set(RepositoriesMode.FAIL_ON_PROJECT_REPOS)
    repositories {
        google()
        mavenCentral()
    }
}

rootProject.name = "baucord"
include(":app")
