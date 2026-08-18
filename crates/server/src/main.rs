use clap::Parser;
use log::{error, info};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use env_logger::Builder;
use server::config::Config;
use server::server::ChatServer;
use std::fs::{self, File, OpenOptions};
use std::io::Write;

const LOG_PATH: &str = "server.log";
const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;
const LOG_ROTATE_CHECK_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Path to configuration file
    #[arg(short, long, default_value = "server.yaml")]
    config: String,
    /// Override host from config
    #[arg(long)]
    host: Option<String>,
    /// Override port from config
    #[arg(long)]
    port: Option<u16>,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    // Missing config: write defaults and continue. Present-but-invalid
    // config: refuse to start — never overwrite an operator's file with
    // defaults just because it failed to parse.
    let mut config = if Path::new(&args.config).exists() {
        match Config::from_file(&args.config) {
            Ok(config) => config,
            Err(e) => {
                eprintln!("Failed to parse config {}: {e}", args.config);
                eprintln!("Fix the file, or delete it to regenerate defaults.");
                std::process::exit(1);
            }
        }
    } else {
        let config = Config::default();
        match config.save_to_file(&args.config) {
            Ok(()) => eprintln!("Wrote default configuration to {}", args.config),
            Err(e) => eprintln!("Could not save default config to {}: {e}", args.config),
        }
        config
    };

    if let Some(host) = args.host {
        config.server.host = host;
    }
    if let Some(port) = args.port {
        config.server.port = port;
    }

    let log_file = Arc::new(Mutex::new(open_log_file()?));

    Builder::new()
        .filter_level(
            config
                .logging
                .level
                .parse()
                .unwrap_or(log::LevelFilter::Info),
        )
        .format({
            let log_file = Arc::clone(&log_file);
            move |_, record| {
                let line = format!(
                    "[{} {} {}] {}\n",
                    chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
                    record.level(),
                    record.target(),
                    record.args()
                );
                eprint!("{line}");
                if let Ok(mut f) = log_file.lock() {
                    let _ = f.write_all(line.as_bytes());
                }
                Ok(())
            }
        })
        .init();

    info!("Starting {}", config.server.name);

    let mut server = ChatServer::new(config)?;
    // Hot reload: edits to the [channels] section of the config file
    // apply live; everything else still needs a restart.
    server.watch_config_file(std::path::PathBuf::from(&args.config));

    info!("Server listening on:");
    for addr in server.addresses() {
        info!("  {addr}");
    }
    info!("Press Ctrl+C to stop");

    // ── Adaptive poll loop ──────────────────────────────────────────
    //
    // Voice packets need sub-millisecond relay turnaround, so the loop
    // runs at 1 ms while media is flowing. But a voice server is idle
    // most of its life; the server tells us how fast the next tick
    // needs to come (1 ms hot, 5 ms quiet, 20 ms empty), which cuts
    // idle CPU roughly 5–20× at a worst-case cost of +5 ms on the first
    // packet of a talk burst — inaudible under the jitter buffer.
    let mut last_rotate_check = Instant::now();
    loop {
        if let Err(e) = server.update() {
            error!("Server error: {e}");
        }

        if last_rotate_check.elapsed() >= LOG_ROTATE_CHECK_INTERVAL {
            rotate_log_if_needed(&log_file);
            last_rotate_check = Instant::now();
        }

        thread::sleep(server.suggested_sleep());
    }
}

fn open_log_file() -> std::io::Result<File> {
    OpenOptions::new().create(true).append(true).open(LOG_PATH)
}

/// Rotate `server.log` → `server.log.1` once it exceeds the cap, so a
/// long-running server keeps recent history instead of either growing
/// unbounded or vaporizing all evidence at startup (the previous
/// behavior — the log was truncated exactly when you restart to debug
/// a crash, which is when you needed it).
fn rotate_log_if_needed(log_file: &Arc<Mutex<File>>) {
    let Ok(meta) = fs::metadata(LOG_PATH) else {
        return;
    };
    if meta.len() <= MAX_LOG_BYTES {
        return;
    }
    let Ok(mut guard) = log_file.lock() else {
        return;
    };
    let _ = guard.flush();
    let rotated = format!("{LOG_PATH}.1");
    if fs::rename(LOG_PATH, &rotated).is_ok() {
        // Open a fresh file at the canonical path and swap it in; the
        // renamed handle keeps writing to .1 until the swap lands.
        match open_log_file() {
            Ok(new_file) => *guard = new_file,
            Err(_) => {
                let _ = guard.set_len(0);
            }
        }
    } else {
        // Rename can fail with an open handle on some platforms — fall
        // back to truncating in place.
        let _ = guard.set_len(0);
    }
}
