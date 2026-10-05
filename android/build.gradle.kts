// Android Gradle Plugin. 8.13 is the last 8.x line and pairs with the
// Gradle 8.14 wrapper in gradle/wrapper; Android Studio's "AGP Upgrade
// Assistant" moves both together when a newer pair is wanted. No Kotlin
// plugin yet: the one Kotlin file (VoiceService, spec §10.1) comes with
// the foreground service in M3.
plugins {
    id("com.android.application") version "8.13.0" apply false
}
