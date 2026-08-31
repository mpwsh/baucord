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

    app::run();

    // Force exit. If any threads are stuck (cpal deadlock, etc.),
    // normal return from main() would hang waiting for them.
    // Settings are already saved by this point.
    std::process::exit(0);
}
