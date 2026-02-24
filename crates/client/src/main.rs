// main.rs
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod config;
mod hotkeys;
mod network;
mod screen;
mod ui;
mod voice;

use app::App;

fn main() {
    #[cfg(debug_assertions)]
    {
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    }

    #[cfg(not(debug_assertions))]
    {
        use env_logger::Target;
        use std::fs::File;

        let log_file = File::create("baucord.log").unwrap();
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
            .target(Target::Pipe(Box::new(log_file)))
            .init();
    }

    let mut app = App::new();
    app.run();

    // Force exit. If any threads are stuck (cpal deadlock, etc.),
    // normal return from main() would hang waiting for them.
    // Settings are already saved by this point.
    std::process::exit(0);
}
