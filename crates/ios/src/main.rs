//! baucord's iOS host (IOS-SPEC.md).
//!
//! Xcode bundles this binary as `baucord.app/baucord` and `UIKit` starts
//! it like any app's `main`. That function is the whole host: the log,
//! the panic hook, the app's folders, the audio session, Slint's winit
//! backend with Skia on Metal, and then the same [`frontend::app::run`]
//! the desktop binary runs — the UI, the network thread and the voice
//! worker are the shared crates, untouched.
//!
//! `run` never returns on iOS: winit hands the main thread to
//! `UIApplicationMain`, and the OS ends the process when it wants to
//! (a swipe in the app switcher, memory pressure while suspended). So
//! nothing may wait for "after the loop" — the host saves on the way
//! into the background instead (`lifecycle.rs`).
//!
//! Off iOS the binary only says where it belongs: the crate is a
//! workspace member, so host-side `cargo check -p ios` validates the
//! manifest and the platform-free code.

#[cfg(all(target_os = "ios", feature = "hello"))]
mod hello;
#[cfg(all(target_os = "ios", feature = "app", not(feature = "hello")))]
mod host;
#[cfg(target_os = "ios")]
mod insets;
#[cfg(all(target_os = "ios", feature = "app", not(feature = "hello")))]
mod lifecycle;
#[cfg(target_os = "ios")]
mod logging;
#[cfg(all(target_os = "ios", feature = "app", not(feature = "hello")))]
mod notices;
#[cfg(target_os = "ios")]
mod paths;
#[cfg(target_os = "ios")]
mod render;
#[cfg(all(target_os = "ios", feature = "app", not(feature = "hello")))]
mod session;

#[cfg(not(target_os = "ios"))]
fn main() {
    eprintln!("this is baucord's iOS app; build and run it with ios/run.sh on a Mac");
    std::process::exit(2);
}

#[cfg(target_os = "ios")]
fn main() {
    // App-private storage (spec §8): settings and the device key in
    // Application Support, the log in Documents (the Files app's view of
    // the app). Resolved before the logger so the log file can open.
    let dirs = paths::AppDirs::resolve();

    logging::init(dirs.as_ref().map(|d| d.documents.as_path()));
    install_panic_hook();

    log::info!(
        "baucord ios {} starting ({})",
        env!("CARGO_PKG_VERSION"),
        dirs.as_ref().map_or_else(
            || "no app folders: settings will not persist".to_string(),
            |d| format!(
                "config {}, log {}",
                d.config.display(),
                d.documents.display()
            ),
        ),
    );
    if let Some(d) = &dirs {
        // The device key is a credential: not in iCloud or computer
        // backups, which would restore it onto a second iPhone (§8).
        paths::exclude_from_backup(&d.config);
    }

    // Slint's iOS support is the winit backend with the Skia renderer
    // (spec D7). Chosen explicitly, before any Slint type is touched: the
    // frontend's own choice (Skia on the CPU, for Windows' sake) is
    // compiled out on iOS. In Slint 1.18 "skia" draws through wgpu's
    // Metal backend (the only Skia GPU path it has on Apple platforms)
    // and falls back to the CPU only if that fails, saying so on stderr
    // alone. A diagnostic launch may name another renderer (render.rs).
    let renderer = render::renderer_name();
    if let Err(e) = slint::BackendSelector::new()
        .backend_name("winit".into())
        .renderer_name(renderer.clone())
        .select()
    {
        log::error!("slint backend (winit + {renderer}): {e}");
    }

    // Slint keeps the safe area it read at the last resize; after a
    // rotation that is the old orientation's. A probe view makes winit
    // resend the resize once UIKit has the new one (spec §14 D34).
    insets::install();

    #[cfg(feature = "hello")]
    hello::run();

    #[cfg(all(feature = "app", not(feature = "hello")))]
    {
        if let Some(d) = &dirs {
            client::config::set_config_dir(d.config.clone());
        }

        // The microphone needs a recording category; cpal's iOS host
        // never sets one (spec §6.1). The system asks for microphone
        // permission itself the first time a stream opens it — the
        // engine retries a pending input every 2 s meanwhile.
        session::configure();

        // Save on the way into the background, where iOS may end the
        // process without warning (spec §4.3).
        lifecycle::install();

        // Attachments, saved files, links: UIKit's pickers, Quick Look
        // and Safari behind the frontend's host seam (spec §10).
        host::install(dirs.as_ref().map(|d| d.documents.clone()));

        // Notifications: the delegate, before anything can be posted,
        // and the message chime where notification sounds are found
        // (spec D50).
        notices::install(dirs.as_ref().map(|d| d.config.as_path()));

        frontend::app::run();
    }
}

/// A release build aborts on panic and an app launched from the home
/// screen has no stderr anyone reads, so the panic — thread, message,
/// location, backtrace — goes to the log first. Same hook as
/// `desktop/main.rs` and the Android host.
#[cfg(target_os = "ios")]
fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let thread = std::thread::current()
            .name()
            .unwrap_or("<unnamed>")
            .to_owned();
        let message = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| (*s).to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "<non-string panic payload>".to_string());
        let location = info.location().map_or_else(
            || "<unknown location>".to_string(),
            |l| format!("{}:{}:{}", l.file(), l.line(), l.column()),
        );
        let backtrace = std::backtrace::Backtrace::force_capture();
        log::error!("PANIC in thread '{thread}' at {location}: {message}\n{backtrace}");
        log::logger().flush();
    }));
}
