// The APK module. Everything here is packaging: the app is
// libbaucord_android.so, built by `cargo ndk` (ANDROID-SPEC.md §2, §11)
// through the two Exec tasks at the bottom, which run before AGP's
// own preDebugBuild / preReleaseBuild. The only Java is
// src/main/java/dev/baucord/ (the Activity and the voice service, D30,
// D34); the launcher icon is rendered from the repo's
// assets/icons/icon.png by the task at the bottom (D36).

import java.awt.RenderingHints
import java.awt.image.BufferedImage
import java.io.File
import java.util.Properties
import javax.imageio.ImageIO

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
// One number for AGP's compileSdk and for the android.jar the Rust
// build compiles Slint's Java helper against (see ANDROID_JAR below).
// Not named like AGP's own deprecated String property on the android
// extension, which would shadow a script val inside `android { }`.
val baucordCompileSdk = 36

// The SDK root the way AGP resolves it: local.properties' sdk.dir
// (written by Android Studio), else ANDROID_HOME, else ANDROID_SDK_ROOT.
fun androidSdkDir(): File? {
    val local = rootProject.file("local.properties")
    if (local.exists()) {
        val props = Properties()
        local.inputStream().use { stream -> props.load(stream) }
        val fromLocal = props.getProperty("sdk.dir")
        if (fromLocal != null) {
            return File(fromLocal)
        }
    }
    val fromEnv = System.getenv("ANDROID_HOME") ?: System.getenv("ANDROID_SDK_ROOT")
    return if (fromEnv != null) File(fromEnv) else null
}

android {
    namespace = "dev.baucord"
    compileSdk = baucordCompileSdk
    // AGP strips debug symbols out of jniLibs with the NDK's llvm-strip
    // and needs to be told which NDK; without this it packages the .so
    // as built ("Unable to strip the following libraries…" — ~10× the
    // size in a release APK). Newest installed NDK under the SDK.
    androidSdkDir()?.let { sdk ->
        File(sdk, "ndk").listFiles()?.filter { it.isDirectory }?.maxByOrNull { it.name }?.let {
            ndkVersion = it.name
        }
    }

    defaultConfig {
        applicationId = "dev.baucord"
        // AAudio's floor is 26; 29 is where permissions and audio behave
        // consistently (spec D6). cargo-ndk's --platform matches below.
        minSdk = 29
        targetSdk = 36
        // versionCode counts up every build that leaves the machine;
        // versionName follows the workspace version (spec §11). The
        // release workflow passes both (-PbaucordVersionCode=<run
        // number> -PbaucordVersionName=<tag>); local builds take the
        // defaults, which install over anything (same or lower code).
        versionCode = (project.findProperty("baucordVersionCode") as String?)
            ?.trim()?.toIntOrNull() ?: 1
        versionName = (project.findProperty("baucordVersionName") as String?)
            ?.trim()?.takeIf { it.isNotEmpty() } ?: "1.1.6"

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
            // Off: BaucordActivity's methods are only called from
            // native code (JNI), which R8 cannot see — shrinking would
            // strip them. One small class; nothing worth shrinking.
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

    // The default (Java 8) draws a deprecation warning from JDK 21;
    // D8 desugars 17 for minSdk 29 just the same.
    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
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
//
// The environment the Rust build needs beyond what Gradle inherits:
//
// ANDROID_JAR: cargo-ndk exports ANDROID_PLATFORM=<--platform> (the NDK
// API level) to cargo's children, and the `android-build` crate behind
// Slint's build.rs reads that same name as the *SDK platform* to compile
// against — so it looked for platforms/android-29/android.jar and found
// nothing (first build, 2026-10-05). It checks ANDROID_JAR first and
// cargo-ndk leaves that one alone, so the jar is named here from
// compileSdk.
//
// CMAKE_GENERATOR=Ninja: libopus (opusic-sys) builds with cmake through
// the NDK toolchain file, which the Visual Studio generator cannot
// drive. Set here, for this build only, so the desktop build on the
// same machine keeps whatever generator its target/ cache was made
// with (a global Ninja broke a cached VS build: "Does not match the
// generator used previously"). Ninja ships in the SDK's cmake/<ver>/bin.
fun Exec.cargoNdkEnvironment() {
    environment("CMAKE_GENERATOR", "Ninja")
    val sdk = androidSdkDir() ?: return
    val jar = File(sdk, "platforms/android-$baucordCompileSdk/android.jar")
    if (jar.exists()) {
        environment("ANDROID_JAR", jar.absolutePath)
    } else {
        logger.warn("ANDROID_JAR: $jar is missing — install SDK Platform $baucordCompileSdk")
    }
    // The newest SDK cmake (cmake + ninja) first on PATH, in case the
    // shell that started Gradle has neither.
    File(sdk, "cmake").listFiles()?.filter { it.isDirectory }?.maxByOrNull { it.name }?.let {
        val current = System.getenv("PATH") ?: ""
        environment("PATH", File(it, "bin").absolutePath + File.pathSeparator + current)
    }
}

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
    cargoNdkEnvironment()
    // Cargo is the incremental build system here; let it decide.
    outputs.upToDateWhen { false }
}

val cargoNdkRelease by tasks.registering(Exec::class) {
    group = "build"
    description = "cargo ndk build -p android --release → src/main/jniLibs"
    workingDir = workspaceDir.asFile
    commandLine(listOf("cargo") + cargoNdkArgs(release = true))
    cargoNdkEnvironment()
    outputs.upToDateWhen { false }
}

// AGP creates pre<Variant>Build lazily; `matching` + `configureEach`
// attaches to them whenever they appear.
tasks.matching { it.name == "preDebugBuild" }.configureEach { dependsOn(cargoNdkDebug) }
tasks.matching { it.name == "preReleaseBuild" }.configureEach { dependsOn(cargoNdkRelease) }

// ── Launcher icon: the repo's assets/icons/icon.png (ANDROID-SPEC.md D36) ─
// Rendered at build time into generated mipmaps, so the phone shows the
// desktop's icon and follows it when it changes. Every supported phone
// (API 26+) uses the adaptive icon (res/mipmap-anydpi-v26), whose
// foreground layer is a 108 dp square the launcher masks to a circle,
// squircle or rounded square that always shows at least the inner 66 dp
// circle. A picture with opaque corners is a full-bleed square: it
// fills the layer and the mask cuts it. One on transparency (a logo) is
// scaled so its farthest opaque pixel sits on that 66 dp circle — round
// or square, nothing of it is ever cut — over @color/baucord_bg.
// (Plain java.awt / javax.imageio, imported at the top: written out in
// full, `java.awt.…` would resolve `java` to the project's `java`
// extension and not compile.)
abstract class LauncherIconTask : DefaultTask() {
    @get:InputFile
    @get:PathSensitive(PathSensitivity.NONE)
    abstract val source: RegularFileProperty

    @get:OutputDirectory
    abstract val outputDir: DirectoryProperty

    @TaskAction
    fun render() {
        val file = source.get().asFile
        val src = ImageIO.read(file)
            ?: throw GradleException("$file: not an image ImageIO reads (a PNG is expected)")
        val out = outputDir.get().asFile
        out.deleteRecursively()
        val fullBleed = listOf(
            src.getRGB(0, 0), src.getRGB(src.width - 1, 0),
            src.getRGB(0, src.height - 1), src.getRGB(src.width - 1, src.height - 1),
        ).all { (it ushr 24) > 200 }
        val radius = if (fullBleed) 0.0 else opaqueRadius(src)
        val densities = linkedMapOf(
            "mdpi" to 1.0, "hdpi" to 1.5, "xhdpi" to 2.0, "xxhdpi" to 3.0, "xxxhdpi" to 4.0,
        )
        for ((name, scale) in densities) {
            val canvas = Math.round(108 * scale).toInt()
            val layer = BufferedImage(
                canvas, canvas, BufferedImage.TYPE_INT_ARGB,
            )
            val factor = if (fullBleed || radius <= 0.0) {
                // Cover the layer.
                maxOf(canvas.toDouble() / src.width, canvas.toDouble() / src.height)
            } else {
                // The farthest opaque pixel on the 66 dp circle.
                (33 * scale) / radius
            }
            val w = maxOf(1, Math.round(src.width * factor).toInt())
            val h = maxOf(1, Math.round(src.height * factor).toInt())
            val g = layer.createGraphics()
            g.drawImage(scaled(src, w, h), (canvas - w) / 2, (canvas - h) / 2, null)
            g.dispose()
            val dir = File(out, "mipmap-$name").apply { mkdirs() }
            ImageIO.write(layer, "png", File(dir, "ic_launcher_foreground.png"))
        }
    }

    /** Distance from the picture's centre to its farthest visible pixel. */
    private fun opaqueRadius(img: BufferedImage): Double {
        val cx = (img.width - 1) / 2.0
        val cy = (img.height - 1) / 2.0
        var best = 0.0
        for (y in 0 until img.height) {
            for (x in 0 until img.width) {
                if ((img.getRGB(x, y) ushr 24) > 16) {
                    best = maxOf(best, Math.hypot(x - cx, y - cy))
                }
            }
        }
        return best
    }

    /** High-quality resize: halve step by step, then one bicubic pass. */
    private fun scaled(img: BufferedImage, w: Int, h: Int): BufferedImage {
        var current = img
        var cw = img.width
        var ch = img.height
        while (cw / 2 >= w && ch / 2 >= h) {
            cw /= 2
            ch /= 2
            current = draw(current, cw, ch)
        }
        return draw(current, w, h)
    }

    private fun draw(img: BufferedImage, w: Int, h: Int): BufferedImage {
        val out = BufferedImage(w, h, BufferedImage.TYPE_INT_ARGB)
        val g = out.createGraphics()
        g.setRenderingHint(RenderingHints.KEY_INTERPOLATION,
            RenderingHints.VALUE_INTERPOLATION_BICUBIC)
        g.setRenderingHint(RenderingHints.KEY_RENDERING,
            RenderingHints.VALUE_RENDER_QUALITY)
        g.setRenderingHint(RenderingHints.KEY_ANTIALIASING,
            RenderingHints.VALUE_ANTIALIAS_ON)
        g.drawImage(img, 0, 0, w, h, null)
        g.dispose()
        return out
    }
}

androidComponents {
    onVariants { variant ->
        // One task per variant, as AGP expects for generated sources: it
        // sets the task's outputDir itself (build/generated/…).
        val icon = tasks.register<LauncherIconTask>(
            "baucord${variant.name.replaceFirstChar { it.uppercase() }}LauncherIcon",
        ) {
            group = "build"
            description = "assets/icons/icon.png → generated mipmap-*/ic_launcher_foreground.png"
            source.set(workspaceDir.file("assets/icons/icon.png"))
        }
        variant.sources.res?.addGeneratedSourceDirectory(icon, LauncherIconTask::outputDir)
    }
}
