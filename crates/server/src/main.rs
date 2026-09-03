use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use clap::{Parser, Subcommand};
use env_logger::Builder;
use log::{info, warn};
use server::{config::Config, server::ChatServer};
use transport::{DriverHost, ServerIdentity, UdpIo};

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
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Generate a new identity key, signed by the current one so
    /// pinned clients accept it. The old key is kept under `retired/`
    /// (for two years) and continues to vouch for the new one. Run
    /// while the server is stopped, then restart it.
    RotateKey,
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

    if let Some(Command::RotateKey) = args.command {
        return rotate_key(&config);
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

    info!(
        "Starting {} (baucord {})",
        config.server.name,
        env!("CARGO_PKG_VERSION")
    );

    // ── Identity ────────────────────────────────────────────────────
    let identity_dir = PathBuf::from(&config.identity.dir);
    let (identity, created) = ServerIdentity::load_or_create(&identity_dir)?;
    if created {
        warn!(
            "Generated a new server identity in {} — back this directory up; every client will pin it",
            identity_dir.display()
        );
    }
    info!("Server identity: {}", identity.fingerprint());
    for old in identity.retired_fingerprints() {
        info!("  vouching for retired key {old}");
    }
    info!(
        "Invite: {}#{}",
        config.public_endpoint(),
        identity.fingerprint()
    );

    // ── Socket + server ─────────────────────────────────────────────
    let addr = config.socket_addr()?;
    let mut server = ChatServer::new(config, &identity)?;
    let mut udp = UdpIo::bind(addr, server.budget().socket_buffer_bytes)?;
    server.driver().set_max_gso_segments(udp.max_gso_segments());
    // Hot reload: edits to the [channels] section of the config file
    // apply live; everything else still needs a restart.
    server.watch_config_file(PathBuf::from(&args.config));

    info!("Server listening on {}", udp.local_addr()?);
    info!("Press Ctrl+C to stop");

    // ── Adaptive poll loop ──────────────────────────────────────────
    //
    // Voice packets need sub-millisecond relay turnaround, so the loop
    // runs at 1 ms while media is flowing. But a voice server is idle
    // most of its life; the server tells us how fast the next tick
    // needs to come (1 ms hot, 5 ms quiet, 20 ms empty), and the
    // transport's next timer caps the sleep so acks, keep-alives and
    // idle timeouts fire on time.
    let mut last_rotate_check = Instant::now();
    loop {
        let now = Instant::now();
        let drained = match udp.recv_batch(|d| server.driver().handle_datagram(now, d)) {
            Ok(outcome) => outcome.drained,
            Err(e) => {
                warn!("UDP receive error: {e}");
                true
            }
        };
        server.update(now);
        while let Some(t) = server.driver().poll_transmit() {
            udp.send(&t);
        }

        if last_rotate_check.elapsed() >= LOG_ROTATE_CHECK_INTERVAL {
            rotate_log_if_needed(&log_file);
            last_rotate_check = Instant::now();
        }

        // Never sleep on a backlog: whatever is still in the socket
        // queue is latency for everyone on this relay.
        if !drained {
            continue;
        }
        let mut sleep = server.suggested_sleep();
        if let Some(deadline) = server.driver().next_timeout() {
            sleep = sleep.min(deadline.saturating_duration_since(Instant::now()));
        }
        if !sleep.is_zero() {
            thread::sleep(sleep);
        }
    }
}

fn rotate_key(config: &Config) -> Result<(), Box<dyn std::error::Error>> {
    let dir = PathBuf::from(&config.identity.dir);
    let (current, created) = ServerIdentity::load_or_create(&dir)?;
    if created {
        println!(
            "No identity existed in {}; generated one ({}). Nothing to rotate.",
            dir.display(),
            current.fingerprint()
        );
        return Ok(());
    }
    let old = current.fingerprint();
    drop(current);
    let rotated = ServerIdentity::rotate(&dir)?;
    println!("Rotated server identity in {}", dir.display());
    println!("  old: {old} (retired, still vouching for the new key)");
    println!("  new: {}", rotated.fingerprint());
    println!(
        "Restart the server to serve the new certificate; pinned clients migrate automatically."
    );
    Ok(())
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
