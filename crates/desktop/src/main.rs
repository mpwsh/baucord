// main.rs
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod hotkeys;
mod ui;

use app::App;

/// How many timestamped log files to keep (including the current run's).
#[cfg(not(debug_assertions))]
const LOG_FILES_TO_KEEP: usize = 5;

fn main() {
    #[cfg(debug_assertions)]
    {
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    }

    #[cfg(not(debug_assertions))]
    {
        use env_logger::Target;
        use std::fs::{self, OpenOptions};

        // One log file per run: baucord-YYYYmmdd-HHMMSS.log
        let log_path = format!(
            "baucord-{}.log",
            chrono::Local::now().format("%Y%m%d-%H%M%S")
        );

        // Remove the legacy single log file from older versions.
        let _ = fs::remove_file("baucord.log");

        // Prune old run logs, keeping the most recent few. The timestamp
        // format sorts lexicographically, so a plain sort is chronological.
        if let Ok(entries) = fs::read_dir(".") {
            let mut logs: Vec<String> = entries
                .flatten()
                .filter_map(|e| e.file_name().into_string().ok())
                .filter(|n| n.starts_with("baucord-") && n.ends_with(".log"))
                .collect();
            logs.sort();
            if logs.len() + 1 > LOG_FILES_TO_KEEP {
                let excess = logs.len() + 1 - LOG_FILES_TO_KEEP;
                for old in logs.iter().take(excess) {
                    let _ = fs::remove_file(old);
                }
            }
        }

        let log_file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .unwrap();

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
