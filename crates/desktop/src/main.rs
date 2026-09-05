// main.rs
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod clipboard;
mod hotkeys;
mod media;
mod present;
mod resources;
mod settingsui;
mod updater;
mod viewers;

fn main() {
    // `--config-dir <dir>` or `--profile <name>`: a second settings
    // file and identity key on the same machine — two instances for
    // testing, or two people sharing a PC. Parsed by hand: this binary
    // has no argument parser and two flags do not earn one. Must run
    // before anything reads settings or the key.
    if let Some(dir) = config_dir_from_args() {
        client::config::set_config_dir(dir);
    }

    #[cfg(debug_assertions)]
    {
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    }

    #[cfg(not(debug_assertions))]
    {
        use std::fs::{self, OpenOptions};

        use env_logger::Target;

        // Single append-mode log: every run keeps writing baucord.log.
        // One-time migration: sweep the per-run baucord-<timestamp>.log
        // files an older scheme left behind (exact prefix + suffix, so
        // nothing else can match).
        if let Ok(entries) = fs::read_dir(".") {
            for name in entries
                .flatten()
                .filter_map(|e| e.file_name().into_string().ok())
                .filter(|n| n.starts_with("baucord-") && n.ends_with(".log"))
            {
                let _ = fs::remove_file(name);
            }
        }

        match OpenOptions::new()
            .create(true)
            .append(true)
            .open("baucord.log")
        {
            Ok(log_file) => {
                env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
                    .target(Target::Pipe(Box::new(log_file)))
                    .init();
            }
            Err(e) => {
                // Read-only install dir etc. — run without file logging
                // rather than dying before the window exists.
                env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
                    .init();
                log::warn!("could not open baucord.log ({e}); logging to stderr");
            }
        }
    }

    log::info!(
        "Config directory: {}",
        client::config::settings_file_path()
            .parent()
            .map_or_else(|| "?".to_string(), |d| d.display().to_string())
    );

    // A release build aborts on panic and a GUI process has no stderr,
    // so without this a crash on any thread leaves nothing behind but a
    // log that stops mid-sentence. Write the panic — thread, message,
    // location, backtrace — to the log before the abort.
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
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "<unknown location>".to_string());
        let backtrace = std::backtrace::Backtrace::force_capture();
        log::error!("PANIC in thread '{thread}' at {location}: {message}\n{backtrace}");
        log::logger().flush();
    }));

    app::run();

    // Force exit. If any threads are stuck (cpal deadlock, etc.),
    // normal return from main() would hang waiting for them.
    // Settings are already saved by this point.
    std::process::exit(0);
}

/// `--config-dir <dir>`, `--config-dir=<dir>`, `--profile <name>`,
/// `--profile=<name>`. The environment variable `BAUCORD_CONFIG_DIR`
/// is read by the client crate itself and needs nothing here.
fn config_dir_from_args() -> Option<std::path::PathBuf> {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let (flag, inline) = match arg.split_once('=') {
            Some((flag, value)) => (flag.to_string(), Some(value.to_string())),
            None => (arg.clone(), None),
        };
        match flag.as_str() {
            "--config-dir" => {
                let dir = inline.or_else(|| args.next())?;
                return Some(std::path::PathBuf::from(dir));
            }
            "--profile" => {
                let name = inline.or_else(|| args.next())?;
                return Some(client::config::profile_dir(&name));
            }
            _ => {}
        }
    }
    None
}
