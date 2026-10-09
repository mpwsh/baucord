# baucord for Android — building it

The design is `../ANDROID-SPEC.md`; this is the checklist for building
it on a Windows machine with Android Studio. Nothing in this
directory is Rust: `app/build.gradle.kts` runs `cargo ndk` on the
`android` crate in `../crates/android` and packages what it produces.

## 1. Install (once)

1. **Android Studio** — <https://developer.android.com/studio>. In its
   SDK Manager (*More Actions → SDK Manager*):
   - *SDK Platforms*: **Android 16 (API 36)**.
   - *SDK Tools* (tick *Show Package Details*): **Build-Tools 36.x
     and 35.0.0** (AGP 8.13 insists on 35.0.0 and downloads it itself
     only if the SDK licences were accepted — `sdkmanager --licenses`,
     or just tick it), **NDK (Side by side) 29.x**, **CMake** (newest),
     **Platform-Tools**, **Emulator**.
   - Note the **Android SDK Location** shown at the top of that page:
     it is what `ANDROID_HOME` must be. It is never the Studio install
     directory — `Using Android SDK: D:\android\jbr` in a Gradle
     error means the variable points at Studio's Java runtime.
   - *Virtual Device Manager*: create a device with a **Google APIs,
     x86_64, API 36** image (any phone profile).
2. **Rust targets** — from inside the repo, so they land on the pinned
   1.95 toolchain (`rust-toolchain.toml`), not on `stable`:
   ```powershell
   cd <repo>
   rustup target add aarch64-linux-android x86_64-linux-android
   cargo install cargo-ndk
   ```
3. **Environment variables** — *user* variables (Settings → System →
   About → Advanced system settings → Environment Variables), because
   Studio's Gradle daemon inherits those, not a PowerShell session:
   | variable | value |
   |---|---|
   | `ANDROID_HOME` | the SDK Manager's *Android SDK Location* (usually `C:\Users\<you>\AppData\Local\Android\Sdk`) |
   | `ANDROID_SDK_ROOT` | the same (older tools read this name) |
   | `ANDROID_NDK_HOME` | `%ANDROID_HOME%\ndk\<the 29.x folder>` |
   | `ANDROID_NDK` | the same folder (only read if Skia ever builds from source) |
   | `JAVA_HOME` | a **JDK 21** (e.g. Temurin: `winget install EclipseAdoptium.Temurin.21.JDK` → `C:\Program Files\Eclipse Adoptium\jdk-21…`). Not Studio's `jbr`: that is JDK 25 in 2026 builds, and Gradle 8.14 runs on 17–24 only — the symptom is `What went wrong: 25.0.3` while compiling the settings script |
   | `CMAKE_GENERATOR` | *not needed* — the Gradle tasks set `Ninja` for the Android build only. A global value also switches the **desktop** build's libopus to Ninja and breaks its cached VS build (`Does not match the generator used previously`; fix: `cargo clean -p opusic-sys`). Only set it if you run `cargo ndk` by hand |
   | `ANDROID_JAR` | `<sdk>\platforms\android-36\android.jar` — only needed when running `cargo ndk` by hand; the Gradle tasks set it themselves. (cargo-ndk's `ANDROID_PLATFORM=29` otherwise sends Slint's build script after a `platforms\android-29` nobody has: "No Android platforms found") |
   | `PATH` (append) | `%ANDROID_HOME%\platform-tools`; `%ANDROID_HOME%\cmake\<ver>\bin`; `%USERPROFILE%\.cargo\bin` (rustup adds this one) |

   Android Studio ignores `JAVA_HOME` and runs Gradle on its own JBR
   (25) unless told otherwise — before the first sync of `android/` in
   the IDE: *Settings → Build, Execution, Deployment → Build Tools →
   Gradle → Gradle JDK* → the Temurin 21. Same `25.0.3` failure
   otherwise, just inside the IDE.

   Open a **new** terminal and check:
   ```powershell
   cargo ndk --version      # 4.x
   adb version
   javac -version           # 21.x — Gradle 8.14 refuses 25 (see the table)
   cmake --version; ninja --version
   dir $env:ANDROID_HOME\platforms   # android-36
   ```

## 1a. Unpacking a drop

Tarballs from the assistant are unpacked over the repo root with
`tar -xzf <file>.tar.gz -m` — the `-m` gives every file the extraction
time as its mtime. Without it, files keep older timestamps and cargo
can skip rebuilding `ui` (the Slint build script), leaving `frontend`
to fail on properties that "don't exist".

## 1b. The everyday loop: `run.ps1`

Once §1 is done, `run.ps1` is `cargo run`: it builds the APK, starts
the emulator if none is running (and waits for Android to boot),
installs, launches, and tails `logcat` until Ctrl+C.

```powershell
cd android
.\run.ps1                   # emulator, debug build
.\run.ps1 -Avd "Pixel 10"   # a specific AVD (default: the running one, else the first)
.\run.ps1 -Device           # the USB phone instead (arm64, debug)
.\run.ps1 -Device -Release  # the signed release on the phone
.\run.ps1 -Hello            # the smoke test (the toolchain alone)
.\run.ps1 -NoBuild          # just reinstall + relaunch
.\run.ps1 -ColdBoot         # full boot, skipping the AVD's quick-boot snapshot
```

If the emulator loads its snapshot (`Loading snapshot 'default_boot'`)
and then sits at `adb: device offline`, the snapshot is bad (usually an
emulator closed mid-save): close it and run with `-ColdBoot` once; the
next normal close saves a good snapshot again.

`run.cmd` is the same thing for a plain Command Prompt (it bypasses
the PowerShell script policy for this one file). The sections below
are the long form of what it does, for the first time through.

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

A dark page saying "Android M0 — toolchain hello" with a button that
counts taps means NDK, Skia, Java helper, Gradle and the emulator all
work.

## 3. Second build: the client

```powershell
.\gradlew assembleDebug -PcargoTargets=x86_64
adb install -r app\build\outputs\apk\debug\app-debug.apk
adb logcat -s baucord
```

This builds the whole engine for Android (paste the first `error:`
with its `-->` if it stops). Once it runs: the usual connect screen;
the emulator reaches a server on the Windows host at
`10.0.2.2:51000`, and the real server by its invite.
Accept the microphone dialog (or `adb shell pm grant dev.baucord
android.permission.RECORD_AUDIO`) and, in the emulator's *Extended
controls → Microphone*, enable *Virtual microphone uses host audio
input*. Settings opens as a page over the main screen; *Watch stream*
on a sharing user opens the viewer as a page (hardware H.264 decode
through MediaCodec — the first `MediaCodec …: decoding video/avc`,
`output layout` and `first picture` lines in logcat say which decoder
the device picked and how its frames are laid out; Back or *Stop
watching* leaves it). The admin panel is a desktop window ("Not
available on phones yet"); on a phone the slash commands stand in
(`/reports` for reports).

The log file is `files/baucord.log` inside the app's private
directory; on a debug build `adb shell run-as dev.baucord cat
files/baucord.log` prints it. A release build's users have no way
there: **Settings → Debug → Send log to server** sends the end of it
(up to 2 MB, once per 10 minutes) to the server they are connected to,
which keeps it in its identity folder's `logs/` (`<name>-<id>-<time>.log`;
the server's own log names the file). The same tab shows the last 100
lines and can clear the log.

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

Both also live in `gradle.properties`. Two more the release workflow
passes: `-PbaucordVersionCode=` (the run number) and
`-PbaucordVersionName=` (the tag); local builds default to 1 / the
workspace version. `run.ps1` builds with the versionCode already
installed on the target instead: a phone that has the CI or website
APK (run number N) refuses a lower code, and the same code installs
over it while the next CI build still installs over this one. The `std-widgets` style is **fluent on every target,
Android included**: `crates/ui/build.rs` names no style, and the Slint
compiler then uses fluent (only an explicit `native` would pick material
for Android). Fluent's scroll views do not pan on a drag, so the app
turns panning on itself where the host is a touch screen
(`State.touch-host`, ANDROID-SPEC.md D27). `SLINT_STYLE=material` in
the environment would switch the phone's widgets to material, which
would change how every picker, field and switch looks.

## 5a. The Java, and the icon

`app/src/main/java/dev/baucord/` holds the only Java: `BaucordActivity`
(the platform's `NativeActivity` plus what native code cannot receive
on its own — the file picker's result — and helpers the Rust side calls:
permissions, Downloads, opening files and links, the voice service),
`VoiceService` (the foreground service and notification that keep
a connected session alive off screen) and `Notices` (every
notification's look, the message and voice-activity notifications, and
the ringer / Do Not Disturb check before a chime — ANDROID-SPEC.md
D52). Gradle compiles them with the app; there is no Kotlin. Their methods are only called from native
code, so release builds keep `isMinifyEnabled = false` (R8 would strip
them).

The launcher icon is `assets/icons/icon.png` at the repo root, the
desktop's: the `baucordDebugLauncherIcon` / `baucordReleaseLauncherIcon` tasks render it into the adaptive
icon's foreground at every density on each build (ANDROID-SPEC.md D36).
Change the PNG and the next build picks it up. The notifications'
status-bar icon is different: Android draws it as a white silhouette,
so `notification-icon.py` (Pillow) cuts the dog out of `icon.png` into
`app/src/main/res/drawable-*/ic_stat_baucord.png`; it is tuned to the
current picture, run by hand and committed — re-run it (and check the
result) when the icon changes. The manifest also opts out of predictive back
(`enableOnBackInvokedCallback="false"`): on Android 16 an app targeting
API 36 is otherwise never sent the Back key. ANDROID-SPEC.md D29–D30.

## 6. Releases

There is no push/PR build — day-to-day testing is local. When a GitHub
Release is **published**, `.github/workflows/android-release.yml`
builds, in one Gradle run (`assembleRelease bundleRelease`, arm64):

- **The APK**, signed with the release key, attached to the release as
  `baucord-<tag>-android-arm64.apk` (`baucord-v1.2.0-android-arm64.apk`:
  the desktop zips' pattern) and put on the website's R2 bucket beside
  the zips, where the download section links it.
- **The app bundle for Google Play**, `baucord-<tag>-android.aab`,
  signed with the same key, with the native debug symbols inside
  (Play shows crash stacks with Rust function names). It is only a
  workflow artifact: open the run on the Actions tab, download it under
  *Artifacts* (it comes as the `.aab` itself; unzip it if your browser
  hands you a `.zip`), and upload it in Play Console (*Test and release
  → Internal testing → Create new release*). PLAY-STORE.md §4.

Both have versionCode = the run number and versionName = the tag
without its `v` (ANDROID-SPEC.md D55), so Play and GitHub builds of a
release are the same version. Gradle builds both unsigned; later steps
sign them, the APK first (and publish it) and then the bundle, so the
key is never in the environment while cargo and the build scripts
run, and a problem with the bundle never holds back the APK (D62).
The run prints the key's SHA-256 in the format Play Console uses. The
secrets (*Settings → Secrets and variables → Actions*), plus the R2
pair `release.yml` already uses (`AWS_ACCESS_KEY_ID`,
`AWS_SECRET_ACCESS_KEY`):

| secret | value |
|---|---|
| `BAUCORD_KEYSTORE_BASE64` | the release keystore as base64: `[Convert]::ToBase64String([IO.File]::ReadAllBytes("$env:USERPROFILE\baucord-release.jks")) \| Set-Clipboard`, then paste |
| `BAUCORD_KEYSTORE_PASSWORD` | its password |
| `BAUCORD_KEY_PASSWORD` | the key password (empty: the keystore's) |

The key's alias, `baucord`, is set in the workflow itself (`KEY_ALIAS`),
not as a secret: GitHub masks every occurrence of a secret's value in
the log, so a `baucord` secret turned every file name into `***`.

**One key for everything** (PLAY-STORE.md §4): it signs the APK, Play
holds a copy of it as the app signing key, and it is also the upload key
Play checks bundles against. There is no separate upload key. If Play
Console asks for an upload key certificate, give it this key's:

```powershell
keytool -export -rfc -keystore $env:USERPROFILE\baucord-release.jks `
  -alias baucord -file upload_certificate.pem
```

The key cannot be replaced: keep its offline backup. Play only takes it
if it is RSA of 2048 bits or more: `keytool -list -v -keystore
baucord-release.jks` shows *Subject Public Key Algorithm*.

Without the key both outputs stay unsigned, artifacts only: the APK is
never attached or published, and Play refuses the bundle. *Run
workflow* on the Actions tab with the default input is a dry run of the
chosen branch: same build, artifacts only (signed with the same key, so the APK
installs over a release for testing). With a release tag as the input (`v1.1.7`)
it rebuilds that release instead — the tag's code, the branch's
workflow — and attaches the APK to the existing release and the
website: the fix when a release's own run broke on CI (re-running that
run uses the workflow as it was at the tag, so it breaks again).
ANDROID-SPEC.md D54. A rebuild gets a new run number, so its bundle can
go to Play after the first one.
