# baucord for Android — building it

The design is `../ANDROID-SPEC.md`; this is the checklist for the first
build on a Windows machine with Android Studio. Nothing in this
directory is Rust: `app/build.gradle.kts` runs `cargo ndk` on the
`android` crate in `../crates/android` and packages what it produces.

## 1. Install (once)

1. **Android Studio** — <https://developer.android.com/studio>. In its
   SDK Manager (*More Actions → SDK Manager*):
   - *SDK Platforms*: **Android 16 (API 36)**.
   - *SDK Tools* (tick *Show Package Details*): **Build-Tools 36.x**,
     **NDK (Side by side) 29.x**, **CMake** (newest), **Platform-Tools**,
     **Emulator**.
   - *Virtual Device Manager*: create a device with a **Google APIs,
     x86_64, API 36** image. For the desktop UI that M0–M1 show, pick a
     **tablet** profile (Pixel Tablet) or run a phone in landscape.
2. **Rust targets**:
   ```powershell
   rustup target add aarch64-linux-android x86_64-linux-android
   cargo install cargo-ndk
   ```
3. **Environment variables** — *user* variables (Settings → System →
   About → Advanced system settings → Environment Variables), because
   Studio's Gradle daemon inherits those, not a PowerShell session:
   | variable | value |
   |---|---|
   | `ANDROID_HOME` | `C:\Users\<you>\AppData\Local\Android\Sdk` |
   | `ANDROID_NDK_HOME` | `%ANDROID_HOME%\ndk\<the 29.x folder>` |
   | `JAVA_HOME` | `C:\Program Files\Android\Android Studio\jbr` |
   | `CMAKE_GENERATOR` | `Ninja` |
   | `PATH` (append) | `%ANDROID_HOME%\platform-tools`; `%ANDROID_HOME%\cmake\<ver>\bin`; `%USERPROFILE%\.cargo\bin` (rustup adds this one) |

   Open a **new** terminal and check:
   ```powershell
   cargo ndk --version      # 4.x
   adb version
   javac -version           # 21 (the JBR) — fine with build-tools 36
   cmake --version; ninja --version
   dir $env:ANDROID_HOME\platforms   # android-36
   ```

## 2. First build: the toolchain alone

From the repo root:

```powershell
cd android
.\gradlew assembleDebug -PcargoTargets=x86_64 -PcargoFeatures=hello
```

The first run downloads Gradle 8.14.3 and the Android Gradle Plugin,
then `cargo ndk` builds the `android` crate with only Slint in it. Two
things happen there for the first time and take minutes: Slint's
Android backend compiles its Java helper (needs `JAVA_HOME` and
`ANDROID_HOME`), and `skia-bindings` downloads a prebuilt Skia for
`x86_64-linux-android` from GitHub (if it says it is *building* Skia
instead, stop and read ANDROID-SPEC.md §13). Then:

```powershell
# start the AVD from Studio's Device Manager, then
adb install -r app\build\outputs\apk\debug\app-debug.apk
adb shell monkey -p dev.baucord 1          # or tap the icon
adb logcat -s baucord
```

A dark page with "baucord — Android M0" and a button that counts taps
means NDK, Skia, Java helper, Gradle and the emulator all work. Note
the build time in the session guide.

## 3. Second build: the client

```powershell
.\gradlew assembleDebug -PcargoTargets=x86_64
adb install -r app\build\outputs\apk\debug\app-debug.apk
adb logcat -s baucord
```

This builds the whole engine for Android: expect the first compile
errors of the project here (`crates/android/src/java.rs` is the
likeliest place — paste the first `error:` with its `-->`). Once it
runs: the usual connect screen; the emulator reaches a server on the
Windows host at `10.0.2.2:51000`, and the real server by its invite.
Accept the microphone dialog (or `adb shell pm grant dev.baucord
android.permission.RECORD_AUDIO`) and, in the emulator's *Extended
controls → Microphone*, enable *Virtual microphone uses host audio
input*. Settings, the admin panel and the stream viewer say "not
available on Android yet" — they are separate windows on desktop and
become pages in M2.

The log file is `files/baucord.log` inside the app's private
directory; on a debug build `adb shell run-as dev.baucord cat
files/baucord.log` prints it.

## 4. A phone

Developer options → USB debugging, then:

```powershell
.\gradlew assembleDebug -PcargoTargets=arm64-v8a
adb install -r app\build\outputs\apk\debug\app-debug.apk
```

Release builds (`assembleRelease`) are unsigned until the keystore
properties in `app/build.gradle.kts` are set in
`%USERPROFILE%\.gradle\gradle.properties`; a debug APK is enough to
sideload for testing.

## 5. Knobs

| property | default | meaning |
|---|---|---|
| `-PcargoTargets=` | `arm64-v8a,x86_64` | ABIs to build (both doubles the Rust build time; pick one while iterating) |
| `-PcargoFeatures=` | *(empty = the app)* | `hello` for the smoke test |

Both also live in `gradle.properties`. Slint picks the **material**
widget style for Android targets (the desktop gets fluent on Windows,
cupertino on macOS) — only `std-widgets` scrollbars and the like
differ; `SLINT_STYLE=fluent` in the environment forces the desktop look
if that ever matters. CI (`.github/workflows/android.yml`)
builds the arm64 debug APK on every push touching `crates/` or
`android/` and uploads it as an artifact.
