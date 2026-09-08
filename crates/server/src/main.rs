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
    /// Users and their roles (needs persistence on).
    User {
        #[command(subcommand)]
        command: UserCommand,
    },
    /// Custom roles: named permission sets (needs persistence on; a
    /// running server tells every client the new table within 2 s).
    /// Where a role stands for moderation follows from what it can
    /// do: everything a member has sits above members, everything an
    /// organizer has above organizers, never above admins.
    Role {
        #[command(subcommand)]
        command: RoleCommand,
    },
    /// Kicks, bans and mutes (needs persistence on).
    Restriction {
        #[command(subcommand)]
        command: RestrictionCommand,
    },
    /// Device keys (needs persistence on).
    Key {
        #[command(subcommand)]
        command: KeyCommand,
    },
    /// A user's stored telemetry samples (needs persistence on).
    Telemetry {
        who: String,
        /// Only samples from the last `since`, e.g. `30m`, `2h`, `7d`.
        #[arg(long, default_value = "24h")]
        since: String,
        /// At most this many samples, newest.
        #[arg(long, default_value_t = 50)]
        last: u32,
    },
}

#[derive(Subcommand, Debug)]
enum UserCommand {
    /// Every user of record with roles, keys and last-seen.
    List,
    /// Add a role (`admin` makes a second administrator; the bootstrap
    /// code is the other way).
    Grant { who: String, role: String },
    /// Remove a role. The last admin keeps `admin`.
    Revoke { who: String, role: String },
    /// Close the session and forget their devices and roles: they need
    /// a new invite to come back.
    Kick {
        who: String,
        #[arg(long, default_value = "")]
        reason: String,
    },
    /// Kick, and block the key and the address too.
    Ban {
        who: String,
        /// `1h`, `7d`, or `never` (default).
        #[arg(long = "for", default_value = "never")]
        duration: String,
        #[arg(long, default_value = "")]
        reason: String,
    },
    /// Delete a person: their row, keys, roles, restrictions, progress
    /// and messages. One audit line stays.
    Purge { who: String },
    /// Drop their voice at the relay.
    Mute {
        who: String,
        #[arg(long = "for", default_value = "never")]
        duration: String,
        #[arg(long, default_value = "")]
        reason: String,
    },
}

#[derive(Subcommand, Debug)]
enum RoleCommand {
    /// Every role with its rank, permissions and how many hold it.
    List,
    /// Add a role. Start from a built-in's permission set (`--like`),
    /// then add and remove names from `role list`.
    Create {
        /// 1–32 lowercase letters, digits and hyphens.
        name: String,
        /// The set to start from: guest (default), member, organizer,
        /// or admin (everything a custom role may hold; `--deny gold`
        /// for an administrator without the gold name).
        #[arg(long, default_value = "guest")]
        like: String,
        /// Permissions to add, comma-separated, e.g. `share,upload`.
        #[arg(long, value_delimiter = ',')]
        grant: Vec<String>,
        /// Permissions to remove, comma-separated.
        #[arg(long, value_delimiter = ',')]
        deny: Vec<String>,
        /// A glyph for the badge: star, crown, shield, wrench, zap,
        /// flame, heart, gem, trophy, sword, music, gamepad, video,
        /// mic, eye, bot.
        #[arg(long, default_value = "")]
        icon: String,
        /// A color for the badge and the card, as `#rrggbb`.
        #[arg(long)]
        color: Option<String>,
    },
    /// Change a custom role. Built-ins cannot be changed.
    Edit {
        /// Its current name or id.
        role: String,
        #[arg(long)]
        name: Option<String>,
        /// Replace the set with a built-in's before `--grant`/`--deny`.
        #[arg(long)]
        like: Option<String>,
        #[arg(long, value_delimiter = ',')]
        grant: Vec<String>,
        #[arg(long, value_delimiter = ',')]
        deny: Vec<String>,
        #[arg(long)]
        icon: Option<String>,
        /// `#rrggbb`, or `none`.
        #[arg(long)]
        color: Option<String>,
    },
    /// Remove a custom role; whoever holds it loses it.
    Delete { role: String },
}

#[derive(Subcommand, Debug, Clone, Copy)]
enum RestrictionCommand {
    /// Every restriction with its id.
    List,
    /// Remove one by id — including one an admin put on themselves.
    Lift { id: u64 },
}

#[derive(Subcommand, Debug)]
enum KeyCommand {
    /// Attach a device key (64 hex digits, from `user list` or the
    /// server log) to a user.
    Link { who: String, key_hash: String },
    /// Retire a device key: refused until re-linked.
    Unlink { key_hash: String },
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
fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
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
        Some(Command::User { command }) => return user_command(&config, command),
        Some(Command::Role { command }) => return role_command(&config, command),
        Some(Command::Restriction { command }) => return restriction_command(&config, command),
        Some(Command::Key { command }) => return key_command(&config, command),
        Some(Command::Telemetry { who, since, last }) => {
            return telemetry_command(&config, &who, &since, last);
        }
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
        "Address for clients: {} — identity {} (pinned on first connect; paste it into \
         Settings → Servers to verify up front)",
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
            println!(
                "schema version {v} (this build: {})",
                report.supported_version
            );
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
            let policy = load_policy_for_listing(&db)?;
            let role = parse_role(&policy, &grants)?;
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
            println!("{}", protocol::invite::display_code(&code));
            println!(
                "  grants {}, {}, {}",
                role_name_in(&policy, role),
                describe_expiry(invite.expires_at),
                match invite.max_uses {
                    Some(n) => format!("{n} use(s)"),
                    None => "unlimited uses".to_string(),
                }
            );
            println!(
                "  for {} (identity {})",
                config.public_endpoint(),
                identity.fingerprint()
            );
            Ok(())
        }
        InviteCommand::List => {
            let invites = persistence::cli_list_invites(&db)?;
            if invites.is_empty() {
                println!("no invites");
                return Ok(());
            }
            let policy = load_policy_for_listing(&db)?;
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
                    role_name_in(&policy, invite.grants_role),
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

/// `server user …`.
fn user_command(config: &Config, command: UserCommand) -> Result<(), Box<dyn std::error::Error>> {
    let db = database_path(config)?;
    match command {
        UserCommand::List => {
            let policy = load_policy_for_listing(&db)?;
            if policy.users.is_empty() {
                println!("no users yet");
                return Ok(());
            }
            let mut users: Vec<_> = policy.users.iter().collect();
            users.sort_by_key(|(_, u)| u.name.to_ascii_lowercase());
            for (id, u) in users {
                let roles: Vec<String> =
                    u.roles.iter().map(|r| role_name_in(&policy, *r)).collect();
                println!(
                    "{}  {:<24} {:<20} last seen {}",
                    persistence::hex(id),
                    u.name,
                    roles.join(","),
                    describe_ago(u.last_seen)
                );
                for (hash, key) in policy.keys.iter().filter(|(_, k)| k.user_id == *id) {
                    println!(
                        "    key {}{}",
                        persistence::hex(hash),
                        if key.retired_at.is_some() {
                            "  (retired)"
                        } else {
                            ""
                        }
                    );
                }
            }
            Ok(())
        }
        UserCommand::Grant { who, role } => {
            let (user, policy) = persistence::cli_find_user(&db, &who)?;
            let role = parse_role(&policy, &role)?;
            let mut roles: Vec<u32> = policy.users[&user].roles.iter().copied().collect();
            if !roles.contains(&role) {
                roles.push(role);
            }
            persistence::cli_set_roles(&db, &user, &roles)?;
            println!(
                "{}: {}",
                policy.users[&user].name,
                describe_roles(&policy, &roles)
            );
            Ok(())
        }
        UserCommand::Revoke { who, role } => {
            let (user, policy) = persistence::cli_find_user(&db, &who)?;
            let role = parse_role(&policy, &role)?;
            let roles: Vec<u32> = policy.users[&user]
                .roles
                .iter()
                .copied()
                .filter(|r| *r != role)
                .collect();
            persistence::cli_set_roles(&db, &user, &roles)?;
            println!(
                "{}: {}",
                policy.users[&user].name,
                describe_roles(&policy, &roles)
            );
            Ok(())
        }
        UserCommand::Kick { who, reason } => {
            let (user, policy) = persistence::cli_find_user(&db, &who)?;
            persistence::cli_kick(&db, &user, &reason)?;
            println!(
                "kicked {}: their devices and roles are forgotten; they need a new invite",
                policy.users[&user].name
            );
            Ok(())
        }
        UserCommand::Ban {
            who,
            duration,
            reason,
        } => {
            let (user, policy) = persistence::cli_find_user(&db, &who)?;
            let until = parse_ttl(&duration)?.map(|ms| persistence_wall_now() + ms);
            // Block the key they used last as well; the address is only
            // known to a running server.
            let key = policy
                .keys
                .iter()
                .filter(|(_, k)| k.user_id == user)
                .max_by_key(|(_, k)| k.last_seen)
                .map(|(hash, _)| *hash);
            let mut r = restriction_for(user, until, None, reason);
            r.key_hash = key;
            let id = persistence::cli_restrict(&db, r)?;
            println!("banned {} (restriction {id})", policy.users[&user].name);
            Ok(())
        }
        UserCommand::Purge { who } => {
            let (user, policy) = persistence::cli_find_user(&db, &who)?;
            persistence::cli_purge(&db, &user)?;
            println!(
                "purged {}: row, keys, roles, restrictions, progress and messages",
                policy.users[&user].name
            );
            Ok(())
        }
        UserCommand::Mute {
            who,
            duration,
            reason,
        } => {
            let (user, policy) = persistence::cli_find_user(&db, &who)?;
            let until = parse_ttl(&duration)?.map(|ms| persistence_wall_now() + ms);
            let mut r = restriction_for(user, until, None, reason);
            r.kind = protocol::RestrictionKind::Voice;
            let id = persistence::cli_restrict(&db, r)?;
            println!("muted {} (restriction {id})", policy.users[&user].name);
            Ok(())
        }
    }
}

/// `server telemetry <who>`: the stored samples (spec §9).
fn telemetry_command(
    config: &Config,
    who: &str,
    since: &str,
    last: u32,
) -> Result<(), Box<dyn std::error::Error>> {
    let db = database_path(config)?;
    let (user, policy) = persistence::cli_find_user(&db, who)?;
    let window = parse_ttl(since)?.unwrap_or(u64::MAX);
    let from = persistence_wall_now().saturating_sub(window);
    let lines = persistence::cli_telemetry(&db, &user, from, last)?;
    if lines.is_empty() {
        println!(
            "no telemetry from {} in that window",
            policy.users[&user].name
        );
        return Ok(());
    }
    println!(
        "{} — {} sample(s), oldest first:",
        policy.users[&user].name,
        lines.len()
    );
    for l in lines {
        println!(
            "{}  rtt {:>4} ms  loss {:>5.1}%  voice {}  behind {}  rung {}  in '{}'  out '{}'",
            describe_ago(l.ts),
            l.rtt_ms,
            l.path_loss,
            l.voice_loss
                .map_or_else(|| "  -  ".to_string(), |v| format!("{v:>5.1}%")),
            l.groups_behind
                .map_or_else(|| "-".to_string(), |g| g.to_string()),
            l.rung.map_or_else(|| "-".to_string(), |r| format!("{r}%")),
            l.input_device.as_deref().unwrap_or("?"),
            l.output_device.as_deref().unwrap_or("?"),
        );
    }
    Ok(())
}

/// `server restriction …`.
fn restriction_command(
    config: &Config,
    command: RestrictionCommand,
) -> Result<(), Box<dyn std::error::Error>> {
    let db = database_path(config)?;
    match command {
        RestrictionCommand::List => {
            let policy = load_policy_for_listing(&db)?;
            if policy.restrictions.is_empty() {
                println!("no restrictions");
                return Ok(());
            }
            for r in &policy.restrictions {
                let who = r.user_id.and_then(|u| policy.users.get(&u)).map_or_else(
                    || {
                        r.key_hash.map_or_else(
                            || r.ip.map_or("?".to_string(), |ip| ip.to_string()),
                            |k| format!("key {}", persistence::hex(&k[..8])),
                        )
                    },
                    |u| u.name.clone(),
                );
                println!(
                    "{:<4} {:<8} {:<24} {:<18} {}",
                    r.id,
                    r.kind.name(),
                    who,
                    describe_expiry(r.until),
                    r.reason
                );
            }
            Ok(())
        }
        RestrictionCommand::Lift { id } => {
            if persistence::cli_lift_restriction(&db, id)? {
                println!("lifted {id}");
                Ok(())
            } else {
                Err(format!("no restriction {id}").into())
            }
        }
    }
}

/// `server key …`.
fn key_command(config: &Config, command: KeyCommand) -> Result<(), Box<dyn std::error::Error>> {
    let db = database_path(config)?;
    match command {
        KeyCommand::Link { who, key_hash } => {
            let (user, policy) = persistence::cli_find_user(&db, &who)?;
            let hash = persistence::parse_hex_id(&key_hash)
                .ok_or("the key hash is 64 hex digits (see `user list`)")?;
            persistence::cli_set_key(&db, &hash, Some(&user))?;
            println!("linked to {}", policy.users[&user].name);
            Ok(())
        }
        KeyCommand::Unlink { key_hash } => {
            let hash = persistence::parse_hex_id(&key_hash)
                .ok_or("the key hash is 64 hex digits (see `user list`)")?;
            if persistence::cli_set_key(&db, &hash, None)? {
                println!("unlinked");
                Ok(())
            } else {
                Err("no such key".into())
            }
        }
    }
}

fn load_policy_for_listing(db: &Path) -> Result<server::policy::Policy, persistence::PersistError> {
    persistence::cli_load_policy(db)
}

fn restriction_for(
    user: [u8; 32],
    until: Option<u64>,
    key_hash: Option<[u8; 32]>,
    reason: String,
) -> server::policy::Restriction {
    server::policy::Restriction {
        id: 0,
        user_id: Some(user),
        key_hash,
        ip: None,
        kind: protocol::RestrictionKind::Server,
        until,
        reason,
        by: None,
        created_at: 0,
    }
}

fn describe_roles(policy: &server::policy::Policy, roles: &[u32]) -> String {
    if roles.is_empty() {
        return "no roles".to_string();
    }
    roles
        .iter()
        .map(|r| role_name_in(policy, *r))
        .collect::<Vec<_>>()
        .join(", ")
}

/// "3 min ago", "2 d ago", or "never" for a zero stamp.
fn describe_ago(ts: u64) -> String {
    if ts == 0 {
        return "never".to_string();
    }
    let ago = persistence_wall_now().saturating_sub(ts) / 1000;
    if ago >= 86_400 {
        format!("{} d ago", ago / 86_400)
    } else if ago >= 3_600 {
        format!("{} h ago", ago / 3_600)
    } else {
        format!("{} min ago", ago / 60)
    }
}

/// A role as typed: any role's name (built-in or custom,
/// case-insensitive) or its id.
fn parse_role(policy: &server::policy::Policy, text: &str) -> Result<u32, String> {
    let lower = text.trim().to_ascii_lowercase();
    if let Some((id, _)) = policy.roles.iter().find(|(_, r)| r.name == lower) {
        return Ok(*id);
    }
    lower
        .parse::<u32>()
        .ok()
        .filter(|id| policy.roles.contains_key(id))
        .ok_or_else(|| format!("'{text}' is not a role name or id (see `role list`)"))
}

/// A comma-split permission list as typed, to bits.
fn parse_permissions(names: &[String]) -> Result<u64, String> {
    let mut bits = 0;
    for name in names.iter().flat_map(|n| n.split(',')) {
        let name = name.trim();
        if name.is_empty() {
            continue;
        }
        bits |= protocol::permissions::permission_by_name(name).ok_or_else(|| {
            persistence::PersistError::BadPermission(name.to_string()).to_string()
        })?;
    }
    Ok(bits)
}

/// `#rrggbb` (or `none`) to the RGB the wire carries.
fn parse_color(text: &str) -> Result<u32, String> {
    let t = text.trim().trim_start_matches('#');
    if t.eq_ignore_ascii_case("none") || t.is_empty() {
        return Ok(0);
    }
    if t.len() != 6 {
        return Err(format!("'{text}' is not a color; use #rrggbb"));
    }
    u32::from_str_radix(t, 16).map_err(|_| format!("'{text}' is not a color; use #rrggbb"))
}

/// Where a rank stands, in words: "above organizers".
fn describe_standing(rank: u16) -> String {
    use protocol::permissions as p;
    if rank >= p::RANK_ADMIN {
        "admin".to_string()
    } else if rank == p::RANK_ORGANIZER {
        "organizer".to_string()
    } else if rank > p::RANK_ORGANIZER {
        "above organizers".to_string()
    } else if rank == p::RANK_MEMBER {
        "member".to_string()
    } else if rank > p::RANK_MEMBER {
        "above members".to_string()
    } else if rank == p::RANK_GUEST {
        "guest".to_string()
    } else {
        "above guests".to_string()
    }
}

/// The permission set a `--like` names: a built-in's, less what a
/// custom role can never hold (`admin`): `--like admin --deny gold`
/// is the administrator without the gold band.
fn like_permissions(like: &str) -> Result<u64, String> {
    let lower = like.trim().to_ascii_lowercase();
    protocol::permissions::BUILTIN_ROLES
        .iter()
        .find(|r| r.name == lower)
        .map(|r| r.permissions & protocol::permissions::CUSTOM_ROLE_PERMISSIONS)
        .ok_or_else(|| format!("'{like}': --like takes guest, member, organizer or admin"))
}

fn role_command(config: &Config, command: RoleCommand) -> Result<(), Box<dyn std::error::Error>> {
    let db = database_path(config)?;
    match command {
        RoleCommand::List => {
            let policy = load_policy_for_listing(&db)?;
            let mut roles: Vec<_> = policy.roles.iter().collect();
            roles.sort_by_key(|(id, r)| (r.rank, **id));
            for (id, r) in roles {
                let holders = policy
                    .users
                    .values()
                    .filter(|u| u.roles.contains(id))
                    .count();
                println!(
                    "{id:>3}  {:<16} {:<8} {:<18} {} user(s)\n     {}",
                    r.name,
                    if r.builtin { "built-in" } else { "custom" },
                    describe_standing(r.rank),
                    holders,
                    protocol::permissions::permission_names(r.permissions).join(", ")
                );
            }
            Ok(())
        }
        RoleCommand::Create {
            name,
            like,
            grant,
            deny,
            icon,
            color,
        } => {
            let base = like_permissions(&like)?;
            let permissions = (base | parse_permissions(&grant)?) & !parse_permissions(&deny)?;
            let color = color.as_deref().map(parse_color).transpose()?.unwrap_or(0);
            let id = persistence::cli_role_create(&db, &name, permissions, &icon, color)?;
            println!(
                "created {name} (id {id}), {}: {}",
                describe_standing(protocol::permissions::derived_rank(permissions)),
                protocol::permissions::permission_names(permissions).join(", ")
            );
            Ok(())
        }
        RoleCommand::Edit {
            role,
            name,
            like,
            grant,
            deny,
            icon,
            color,
        } => {
            let policy = load_policy_for_listing(&db)?;
            let id = parse_role(&policy, &role)?;
            let current = policy.roles[&id].permissions;
            let base = match like {
                Some(like) => like_permissions(&like)?,
                None => current,
            };
            let permissions = (base | parse_permissions(&grant)?) & !parse_permissions(&deny)?;
            let edit = persistence::RoleEdit {
                name,
                permissions: (permissions != current || policy.roles[&id].builtin)
                    .then_some(permissions),
                icon,
                color: color.as_deref().map(parse_color).transpose()?,
            };
            let id = persistence::cli_role_edit(&db, &role, &edit)?;
            let policy = load_policy_for_listing(&db)?;
            let r = &policy.roles[&id];
            println!(
                "{} (id {id}), {}: {}",
                r.name,
                describe_standing(r.rank),
                protocol::permissions::permission_names(r.permissions).join(", ")
            );
            Ok(())
        }
        RoleCommand::Delete { role } => {
            let (id, name) = persistence::cli_role_delete(&db, &role)?;
            println!("deleted {name} (id {id}); whoever held it has lost it");
            Ok(())
        }
    }
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

/// A role's name from the table (custom ones included), or `role N`.
fn role_name_in(policy: &server::policy::Policy, role: u32) -> String {
    policy
        .roles
        .get(&role)
        .map_or_else(|| format!("role {role}"), |r| r.name.clone())
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
