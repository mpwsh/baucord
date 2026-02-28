use clap::Parser;
use log::{error, info};
use std::thread;
use std::time::Duration;
mod config;
mod relay;
mod server;
use config::Config;
use env_logger::{Builder, Target};
use server::ChatServer;
use std::fs::{self, OpenOptions};
use std::io::Write;

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
    let mut config = Config::from_file(&args.config).unwrap_or_else(|e| {
        eprintln!("Failed to load config from {}: {e}", args.config);
        let config = Config::default();
        if let Err(save_err) = config.save_to_file(&args.config) {
            eprintln!(
                "Could not save default config to {}: {save_err}",
                args.config
            );
        } else {
            eprintln!("Default configuration saved to {}", args.config);
        }
        config
    });

    if let Some(host) = args.host {
        config.server.host = host;
    }
    if let Some(port) = args.port {
        config.server.port = port;
    }

    let log_path = "server.log";

    // Cap log file at 5 MB
    if let Ok(meta) = fs::metadata(log_path) {
        if meta.len() > 5 * 1024 * 1024 {
            let _ = fs::write(log_path, b"");
        }
    }

    let log_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)?;
    let log_file = std::sync::Mutex::new(log_file);

    Builder::new()
        .filter_level(
            config
                .logging
                .level
                .parse()
                .unwrap_or(log::LevelFilter::Info),
        )
        .format(move |buf, record| {
            let line = format!(
                "[{} {} {}] {}\n",
                chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
                record.level(),
                record.target(),
                record.args()
            );
            // Write to stderr (terminal)
            eprint!("{line}");
            // Write to file
            if let Ok(mut f) = log_file.lock() {
                let _ = f.write_all(line.as_bytes());
            }
            Ok(())
        })
        .init();

    info!("Starting {}", config.server.name);

    let mut server = ChatServer::new(config)?;

    info!("Server listening on:");
    for addr in server.addresses() {
        info!("  {addr}");
    }
    info!("Press Ctrl+C to stop");

    // ── Tight poll loop ─────────────────────────────────────────────
    //
    // Voice packets need sub-millisecond relay turnaround. At 10 ms
    // sleep the server adds 0–10 ms (avg 5 ms) of pure relay latency
    // on every voice packet — that's perceptible.
    //
    // At 1 ms sleep we add 0–1 ms (avg 0.5 ms) which is negligible.
    // CPU cost is ~0.5–1% of a single core on modern hardware, well
    // worth the latency savings for a voice server.
    //
    // The socket is set to non-blocking in ChatServer::new(), so
    // update() returns immediately when there's nothing to process.
    loop {
        if let Err(e) = server.update() {
            error!("Server error: {e}");
        }
        thread::sleep(Duration::from_millis(1));
    }
}
