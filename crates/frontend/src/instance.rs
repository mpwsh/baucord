//! One desktop per config directory.
//!
//! Launching the binary while it already runs — a second double-click,
//! a "reopen" from the dock — must not start a second client: it would
//! fight the first for the identity key, the settings file and the
//! audio devices. Instead the newcomer knocks on the running instance's
//! local socket and exits, and the running one raises its main window,
//! exactly as the tray's "Show" does — the connect page when not
//! connected, the room when it is. The socket is keyed by the config
//! directory, so `--profile bob` still runs beside the default.
//!
//! The socket is a named pipe on Windows, an abstract-namespace socket
//! on Linux and a file under `/tmp` elsewhere. The newcomer always
//! knocks before binding: a live instance answers the same way
//! everywhere, while a refused bind means different things per OS
//! (a corpse file on macOS, a held pipe name on Windows).

use std::{
    io::{self, prelude::*, BufReader},
    path::Path,
    time::{Duration, Instant},
};

use interprocess::local_socket::{
    prelude::*, GenericNamespaced, Listener, ListenerOptions, Name, Stream,
};
use log::{debug, info, warn};

/// Set by the updater on the process it relaunches after an install:
/// the instance still answering the socket is the one that is exiting
/// to make way, so wait for it instead of knocking.
pub const RELAUNCH_ENV: &str = "BAUCORD_RELAUNCH_AFTER_UPDATE";

/// How long a relaunched process waits for the exiting one. Exit is
/// normally tens of milliseconds; audio drivers unloading can take a
/// few hundred.
const RELAUNCH_WAIT: Duration = Duration::from_secs(15);

/// Who got here first.
pub enum Claim {
    /// This process. The listener runs on its own thread for the
    /// program's life and calls `on_show` for every knock.
    First,
    /// Another instance answered; it has been asked to show itself.
    AlreadyRunning,
}

/// Claim the instance for `config_dir`. A failure to set the guard up
/// at all (an exotic platform, a permission problem) is logged and
/// treated as being first: running is better than refusing to.
pub fn claim(config_dir: &Path, on_show: impl Fn() + Send + 'static) -> Claim {
    let name = match socket_name(config_dir) {
        Ok(name) => name,
        Err(e) => {
            warn!("single-instance guard unavailable: {e}");
            return Claim::First;
        }
    };
    if std::env::var_os(RELAUNCH_ENV).is_some() {
        wait_for_previous(&name);
    }
    // Knock first, on every platform. A live instance answers; what a
    // second listener would get otherwise differs per OS — AddrInUse
    // on Unix, but on Windows a pipe name held with
    // FIRST_PIPE_INSTANCE refuses with ERROR_ACCESS_DENIED, i.e.
    // PermissionDenied — and reading the bind error alone let a second
    // copy through there.
    if knock(name.clone()).is_ok() {
        info!("baucord is already running; asked it to show its window");
        return Claim::AlreadyRunning;
    }
    match ListenerOptions::new().name(name.clone()).create_sync() {
        Ok(listener) => {
            serve(listener, on_show);
            Claim::First
        }
        Err(e) if e.kind() == io::ErrorKind::AddrInUse => {
            // Nobody answered the knock, yet the name is taken: a corpse
            // socket file from a crash (macOS). Reclaim it.
            debug!("reclaiming a stale instance socket ({e})");
            match ListenerOptions::new()
                .name(name)
                .try_overwrite(true)
                .create_sync()
            {
                Ok(listener) => serve(listener, on_show),
                Err(e) => warn!("single-instance guard unavailable: {e}"),
            }
            Claim::First
        }
        Err(e) => {
            // Lost a race with a copy that bound between our knock and
            // our bind (Windows says PermissionDenied): knock again.
            if knock(name).is_ok() {
                info!("baucord is already running; asked it to show its window");
                return Claim::AlreadyRunning;
            }
            warn!("single-instance guard unavailable: {e}");
            Claim::First
        }
    }
}

/// After an update: the previous process spawned us and is exiting.
/// Wait until its socket stops answering (a bare connect, no "show":
/// the listener ignores a connection that says nothing), then claim
/// as usual. A process that is still there after [`RELAUNCH_WAIT`] is
/// hung; the normal knock then decides.
fn wait_for_previous(name: &Name<'static>) {
    let started = Instant::now();
    while started.elapsed() < RELAUNCH_WAIT {
        if Stream::connect(name.clone()).is_err() {
            info!(
                "relaunched after an update; the previous instance left after {} ms",
                started.elapsed().as_millis()
            );
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    warn!("relaunched after an update, but the previous instance is still running");
}

/// `baucord-<hash of the config dir>.sock`: short (a Unix socket path
/// has ~100 bytes) and one per profile. The path is hashed as given,
/// not canonicalized: the directory may not exist yet on a first run,
/// and the default one is absolute and stable anyway. FNV-1a over the
/// path's bytes, because std's `DefaultHasher` may change between Rust
/// releases — two builds of baucord must agree on the name.
fn socket_name(config_dir: &Path) -> io::Result<Name<'static>> {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in config_dir.as_os_str().as_encoded_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("baucord-{hash:016x}.sock").to_ns_name::<GenericNamespaced>()
}

/// Ask the running instance to show itself.
fn knock(name: Name<'static>) -> io::Result<()> {
    let mut conn = Stream::connect(name)?;
    conn.write_all(b"show\n")?;
    conn.flush()
}

/// Accept knocks for the program's life.
fn serve(listener: Listener, on_show: impl Fn() + Send + 'static) {
    std::thread::Builder::new()
        .name("instance-guard".into())
        .spawn(move || {
            let mut line = String::new();
            for conn in listener.incoming() {
                let conn = match conn {
                    Ok(conn) => conn,
                    Err(e) => {
                        debug!("instance socket accept failed: {e}");
                        continue;
                    }
                };
                line.clear();
                if BufReader::new(conn).read_line(&mut line).is_ok() && line.trim() == "show" {
                    on_show();
                }
            }
        })
        .map_or_else(
            |e| warn!("could not start the instance guard thread: {e}"),
            drop,
        );
}
