use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

use clap::{Parser, Subcommand};
use env_logger::Builder;
use log::{info, warn};
use server::{config::Config, persistence, server::ChatServer};
use transport::{DriverHost, ServerIdentity, UdpIo};

const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;
const LOG_ROTATE_CHECK_INTERVAL: Duration = Duration::from_mins(1);
/// The close reason clients key their automatic reconnect on. Wire
/// contract with `client::network`; do not reword.
const SHUTDOWN_REASON: &str = "server restarting";
/// How long to keep pumping after closing everything, so the close
/// frames actually leave.
const SHUTDOWN_GRACE: Duration = Duration::from_millis(500);

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
    /// Write a consistent snapshot of the database (via `SQLite`'s
    /// online backup API — a plain copy of a live database is not
    /// safe) together with the `blobs/` directory into one tar
    /// archive. Safe to run while the server is up.
    Backup {
        /// Output archive, e.g. `baucord-2026-09-04.tar`.
        file: PathBuf,
    },
    /// Database maintenance.
    Db {
        #[command(subcommand)]
        command: DbCommand,
    },
    /// Invite codes (needs persistence on; a running server picks up
    /// changes within 2 s).
    Invite {
        #[command(subcommand)]
        command: InviteCommand,
    },
}

#[derive(Subcommand, Debug)]
enum InviteCommand {
    /// Mint a code and print the join link.
    Create {
        /// Role the code grants: guest, member, organizer, admin (or a
        /// role id).
        #[arg(long, default_value = "guest")]
        grants: String,
        /// How long the code stays valid, e.g. `1h`, `24h`, `7d`, or
        /// `never`.
        #[arg(long, default_value = "24h")]
        expires: String,
        /// How many people may join with it (default: unlimited).
        #[arg(long)]
        uses: Option<u32>,
    },
    /// List every code with its state.
    List,
    /// Stop a code from admitting anyone else. People who already
    /// joined keep their access.
    Revoke { code: String },
}

#[derive(Subcommand, Debug)]
enum DbCommand {
    /// Run `SQLite`'s integrity check and print the schema version and
    /// row counts. Read-only; safe while the server is up.
    Check,
}

#[allow(clippy::too_many_lines)]
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

    match args.command {
        Some(Command::RotateKey) => return rotate_key(&config),
        Some(Command::Backup { file }) => return backup(&config, &file),
        Some(Command::Db {
            command: DbCommand::Check,
        }) => return db_check(&config),
        Some(Command::Invite { command }) => return invite_command(&config, command),
        None => {}
    }

    // The file is optional: a container captures stderr and may run
    // from a read-only directory. Failing to open it is a warning.
    let log_path = config.logging.file.trim().to_string();
    let log_file: Option<Arc<Mutex<File>>> = if log_path.is_empty() {
        None
    } else {
        match open_log_file(&log_path) {
            Ok(f) => Some(Arc::new(Mutex::new(f))),
            Err(e) => {
                eprintln!("warning: cannot open log file {log_path} ({e}); logging to stderr only");
                None
            }
        }
    };

    Builder::new()
        .filter_level(
            config
                .logging
                .level
                .parse()
                .unwrap_or(log::LevelFilter::Info),
        )
        .format({
            let log_file = log_file.clone();
            move |_, record| {
                let line = format!(
                    "[{} {} {}] {}\n",
                    chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
                    record.level(),
                    record.target(),
                    record.args()
                );
                eprint!("{line}");
                if let Some(f) = &log_file
                    && let Ok(mut f) = f.lock()
                {
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
        "Address: {}#{} (the 1.0 invite form: verify-only, no code)",
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

    // SIGTERM (docker stop, systemd) / SIGINT: leave through the loop,
    // not mid-packet, so every client gets a close with a reason.
    let stop = Arc::new(AtomicBool::new(false));
    {
        let stop = Arc::clone(&stop);
        ctrlc::set_handler(move || stop.store(true, Ordering::SeqCst))?;
    }

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
        if stop.load(Ordering::SeqCst) {
            server.shutdown(now, SHUTDOWN_REASON);
            // Let the close frames (and their acks) travel: a few ticks
            // is plenty, and a peer that is already gone costs nothing.
            let deadline = Instant::now() + SHUTDOWN_GRACE;
            while Instant::now() < deadline && server.driver().peer_count() > 0 {
                let now = Instant::now();
                let _ = udp.recv_batch(|d| server.driver().handle_datagram(now, d));
                server.driver().handle_timeouts(now);
                while let Some(t) = server.driver().poll_transmit() {
                    udp.send(&t);
                }
                thread::sleep(Duration::from_millis(5));
            }
            info!("Stopped");
            return Ok(());
        }
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
            if let Some(f) = &log_file {
                rotate_log_if_needed(&log_path, f);
            }
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

/// `server backup <file.tar>`.
fn backup(config: &Config, out: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let db = database_path(config)?;
    let report = persistence::backup_to_tar(&db, out)?;
    println!(
        "Wrote {} — database {:.1} MB, {} blob file(s)",
        out.display(),
        megabytes(report.db_bytes),
        report.blob_files
    );
    Ok(())
}

/// `server db check`.
fn db_check(config: &Config) -> Result<(), Box<dyn std::error::Error>> {
    let db = database_path(config)?;
    let report = persistence::check(&db)?;
    println!("{} — {:.1} MB", db.display(), megabytes(report.file_bytes));
    match report.schema_version {
        Some(v) if v <= report.supported_version => {
            println!("schema version {v} (this build: {})", report.supported_version);
        }
        Some(v) => println!(
            "schema version {v} — NEWER than this build ({}); this server will refuse to open it",
            report.supported_version
        ),
        None => println!("no schema_version table — not a baucord database, or empty"),
    }
    if report.integrity.as_slice() == ["ok"] {
        println!("integrity: ok");
    } else {
        println!("integrity: PROBLEMS");
        for line in &report.integrity {
            println!("  {line}");
        }
    }
    for (table, count) in &report.counts {
        if *count >= 0 {
            println!("  {table:<18} {count}");
        }
    }
    if report.is_ok() {
        Ok(())
    } else {
        Err("database check failed".into())
    }
}

/// `server invite …`.
fn invite_command(
    config: &Config,
    command: InviteCommand,
) -> Result<(), Box<dyn std::error::Error>> {
    let db = database_path(config)?;
    match command {
        InviteCommand::Create {
            grants,
            expires,
            uses,
        } => {
            let role = parse_role(&grants)?;
            let ttl_ms = parse_ttl(&expires)?;
            let (code, invite) = persistence::cli_create_invite(&db, role, ttl_ms, uses)?;
            let identity_dir = PathBuf::from(&config.identity.dir);
            let (identity, created) = ServerIdentity::load_or_create(&identity_dir)?;
            if created {
                eprintln!(
                    "warning: no server identity existed in {}; generated one — the server \
                     has never run from here?",
                    identity_dir.display()
                );
            }
            println!(
                "baucord://{}/{code}#{}",
                config.public_endpoint(),
                identity.fingerprint()
            );
            println!(
                "  grants {}, {}, {}",
                role_name(role),
                describe_expiry(invite.expires_at),
                match invite.max_uses {
                    Some(n) => format!("{n} use(s)"),
                    None => "unlimited uses".to_string(),
                }
            );
            Ok(())
        }
        InviteCommand::List => {
            let invites = persistence::cli_list_invites(&db)?;
            if invites.is_empty() {
                println!("no invites");
                return Ok(());
            }
            let now = persistence_wall_now();
            for (code, invite) in invites {
                let state = if invite.revoked_at.is_some() {
                    "revoked"
                } else if invite.expires_at.is_some_and(|at| at <= now) {
                    "expired"
                } else if invite.max_uses.is_some_and(|max| invite.uses >= max) {
                    "used up"
                } else {
                    "valid"
                };
                println!(
                    "{code}  {:<9} {:<10} uses {}{}  {}",
                    role_name(invite.grants_role),
                    state,
                    invite.uses,
                    invite.max_uses.map(|m| format!("/{m}")).unwrap_or_default(),
                    describe_expiry(invite.expires_at),
                );
            }
            Ok(())
        }
        InviteCommand::Revoke { code } => {
            let code = server::policy::normalize_invite_code(&code)
                .ok_or_else(|| format!("'{code}' is not an invite code"))?;
            if persistence::cli_revoke_invite(&db, &code)? {
                println!("revoked {code}");
                Ok(())
            } else {
                Err(format!("{code}: no such code, or already revoked").into())
            }
        }
    }
}

fn parse_role(text: &str) -> Result<u32, String> {
    let lower = text.trim().to_ascii_lowercase();
    if let Some(role) = protocol::permissions::BUILTIN_ROLES
        .iter()
        .find(|r| r.name == lower)
    {
        return Ok(role.id);
    }
    lower
        .parse()
        .map_err(|_| format!("'{text}' is not a role name or id"))
}

/// `30m`, `1h`, `24h`, `7d`, `never`.
fn parse_ttl(text: &str) -> Result<Option<u64>, String> {
    let text = text.trim().to_ascii_lowercase();
    if text == "never" || text == "0" {
        return Ok(None);
    }
    let (number, unit) = text.split_at(text.trim_end_matches(char::is_alphabetic).len());
    let n: u64 = number
        .parse()
        .map_err(|_| format!("'{text}': expected a number and a unit, like 24h"))?;
    let per_unit: u64 = match unit {
        "s" => 1_000,
        "m" | "min" => 60_000,
        "h" => 3_600_000,
        "d" => 86_400_000,
        _ => return Err(format!("'{text}': unit must be s, m, h or d")),
    };
    Ok(Some(n.saturating_mul(per_unit)))
}

fn role_name(role: u32) -> String {
    protocol::permissions::BUILTIN_ROLES
        .iter()
        .find(|r| r.id == role)
        .map_or_else(|| format!("role {role}"), |r| r.name.to_string())
}

fn describe_expiry(expires_at: Option<u64>) -> String {
    match expires_at {
        None => "never expires".to_string(),
        Some(at) => {
            let now = persistence_wall_now();
            if at <= now {
                "expired".to_string()
            } else {
                let left = (at - now) / 1000;
                if left >= 86_400 {
                    format!("expires in {} d", left / 86_400)
                } else if left >= 3_600 {
                    format!("expires in {} h", left / 3_600)
                } else {
                    format!("expires in {} min", (left / 60).max(1))
                }
            }
        }
    }
}

fn persistence_wall_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

#[allow(clippy::cast_precision_loss)] // display only
fn megabytes(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

/// The database file, or an error when persistence is off.
fn database_path(config: &Config) -> Result<PathBuf, persistence::PersistError> {
    if !config.persistence.enabled {
        return Err(persistence::PersistError::Disabled);
    }
    Ok(PathBuf::from(config.persistence.path.trim()))
}

fn open_log_file(path: &str) -> std::io::Result<File> {
    OpenOptions::new().create(true).append(true).open(path)
}

/// Rotate `server.log` → `server.log.1` once it exceeds the cap, so a
/// long-running server keeps recent history instead of either growing
/// unbounded or vaporizing all evidence at startup (the previous
/// behavior — the log was truncated exactly when you restart to debug
/// a crash, which is when you needed it).
fn rotate_log_if_needed(path: &str, log_file: &Arc<Mutex<File>>) {
    let Ok(meta) = fs::metadata(path) else {
        return;
    };
    if meta.len() <= MAX_LOG_BYTES {
        return;
    }
    let Ok(mut guard) = log_file.lock() else {
        return;
    };
    let _ = guard.flush();
    let rotated = format!("{path}.1");
    if fs::rename(path, &rotated).is_ok() {
        // Open a fresh file at the canonical path and swap it in; the
        // renamed handle keeps writing to .1 until the swap lands.
        match open_log_file(path) {
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
