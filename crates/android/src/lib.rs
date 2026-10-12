//! baucord's Android host (ANDROID-SPEC.md).
//!
//! Android's `NativeActivity` loads `libbaucord_android.so` and calls
//! [`android_main`] on the Activity's native thread every time it
//! creates the Activity. That function is the whole host: logcat + the
//! file log, the panic hook, the config directory, Slint's Android
//! backend, and then the same [`frontend::app::run`] the desktop binary
//! runs — the UI, the network thread and the voice worker are the
//! shared crates, untouched.
//!
//! Everything in here is `cfg(target_os = "android")`; on any other
//! target this crate is empty, so `cargo check -p android` on the host
//! only validates the manifest.

#![cfg(target_os = "android")]

#[cfg(feature = "app")]
mod host;
mod java;
mod logging;

#[cfg(feature = "hello")]
mod hello;

use std::sync::OnceLock;

use slint::android::{
    android_activity::{MainEvent, PollEvent},
    AndroidApp,
};

/// Runtime permissions asked for at startup: the microphone, and from
/// Android 13 the notification that comes with staying connected in the
/// background (the voice service, D34). One prompt each, together.
fn runtime_permissions() -> Vec<&'static str> {
    let mut names = vec!["android.permission.RECORD_AUDIO"];
    if AndroidApp::sdk_version() >= 33 {
        names.push("android.permission.POST_NOTIFICATIONS");
    }
    names
}

/// Process-wide one-shots: Android may call `android_main` more than
/// once per process (see the note at the end of it), but a logger and
/// a panic hook are installed once.
static PROCESS_SETUP: OnceLock<()> = OnceLock::new();

/// The entry point `android-activity` (feature `native-activity`)
/// looks up in this library. Rust ABI by design — the declaration on
/// the other side is `extern "Rust"`.
#[unsafe(no_mangle)]
fn android_main(app: AndroidApp) {
    // App-private storage: `/data/user/0/<package>/files`. Settings,
    // the device identity key and the log live here (spec §8).
    let data_dir = app.internal_data_path();

    PROCESS_SETUP.get_or_init(|| {
        logging::init(data_dir.as_deref());
        // A release build aborts on panic and an Android process has
        // no stderr anyone reads: the panic goes to the log first.
        app_core::logfile::install_panic_hook();
    });
    // Every Java call goes to the Activity itself (java.rs: ndk-context
    // only holds the Application).
    java::init(app.vm_as_ptr(), app.activity_as_ptr());

    log::info!(
        "baucord android {} starting (sdk {}, data dir {:?})",
        env!("CARGO_PKG_VERSION"),
        AndroidApp::sdk_version(),
        data_dir
    );

    // The backend must be set before any Slint type is touched. The
    // listener sees the Activity's lifecycle (spec §4.3); for now it
    // only narrates it into the log, which is what diagnosing the
    // first builds needs.
    if let Err(e) = slint::android::init_with_event_listener(app.clone(), on_poll_event) {
        // A second Activity in a process whose first run did not exit
        // (should not happen: see the end of this function). There is
        // no way to re-point Slint at the new Activity, so leave.
        log::error!("slint android backend: {e}");
        std::process::exit(1);
    }

    #[cfg(feature = "hello")]
    hello::run();

    #[cfg(not(feature = "hello"))]
    {
        if let Some(dir) = data_dir.clone() {
            client::config::set_config_dir(dir);
        } else {
            log::warn!("no internal data path; settings will not persist");
        }

        // Voice needs the microphone and Android only grants it on
        // request. Asking at startup is the M0 shortcut; M3 moves the
        // request to the first voice join (spec §4.4). NativeActivity
        // never delivers the answer to native code — the next
        // `Resume` re-checks and the voice worker's device-open error
        // path covers a refusal in the meantime.
        request_missing_permissions();

        // Files and links (the picker, Downloads, ACTION_VIEW).
        host::install();

        frontend::app::run();
    }

    // `android_main` returns when the Activity is destroyed (Back on
    // the root page, a swipe from Recents). The process may survive
    // and Android would then call `android_main` again for the next
    // Activity — but Slint's platform can only be set once per
    // process. Until the foreground service makes the engine outlive
    // the Activity (spec D8, M3), ending the process here is the
    // correct behaviour: the next launch starts clean, exactly as the
    // desktop's `std::process::exit(0)` after `run()`. Settings are
    // saved by `shutdown()` before `run()` returns.
    log::info!("activity destroyed; exiting the process");
    log::logger().flush();
    std::process::exit(0);
}

/// Narrate the Activity lifecycle; the hooks of spec §4.3 (permission
/// re-check, socket rebind, timer pause) attach here as they land.
fn on_poll_event(event: &PollEvent<'_>) {
    let PollEvent::Main(main) = event else {
        return;
    };
    match main {
        MainEvent::Start => {
            log::info!("lifecycle: start");
            // On screen again: the stats lines, GIFs and the device
            // poll resume (ANDROID-SPEC.md D43).
            #[cfg(feature = "app")]
            frontend::app::set_host_visible(true);
        }
        MainEvent::Resume { .. } => {
            log::info!("lifecycle: resume");
            for permission in runtime_permissions() {
                match java::has_permission(permission) {
                    Ok(true) => {}
                    Ok(false) => log::warn!("permission {permission} still not granted"),
                    Err(e) => log::warn!("permission check {permission}: {e}"),
                }
            }
        }
        MainEvent::Pause => log::info!("lifecycle: pause"),
        MainEvent::Stop => {
            log::info!("lifecycle: stop");
            // Off screen (home, another app, the screen locked); voice
            // goes on under the foreground service, the UI-only work
            // pauses.
            #[cfg(feature = "app")]
            frontend::app::set_host_visible(false);
        }
        MainEvent::Destroy => log::info!("lifecycle: destroy"),
        MainEvent::LowMemory => log::warn!("lifecycle: low memory"),
        MainEvent::GainedFocus => log::debug!("lifecycle: focus gained"),
        MainEvent::LostFocus => log::debug!("lifecycle: focus lost"),
        MainEvent::ConfigChanged { .. } => log::info!("lifecycle: configuration changed"),
        _ => {}
    }
}

/// Ask for every runtime permission we do not hold yet, in one system
/// dialog. Failures are logged, never fatal: the app runs without the
/// microphone, it just cannot join voice.
#[cfg(not(feature = "hello"))]
fn request_missing_permissions() {
    let missing: Vec<&str> = runtime_permissions()
        .into_iter()
        .filter(|p| match java::has_permission(p) {
            Ok(granted) => !granted,
            Err(e) => {
                log::warn!("permission check {p}: {e}");
                false
            }
        })
        .collect();
    if missing.is_empty() {
        return;
    }
    log::info!("requesting permissions: {missing:?}");
    if let Err(e) = java::request_permissions(&missing) {
        log::warn!("requestPermissions failed: {e}");
    }
}
