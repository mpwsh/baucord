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
    hash::{DefaultHasher, Hash, Hasher},
    io::{self, prelude::*, BufReader},
    path::Path,
};

use interprocess::local_socket::{
    prelude::*, GenericNamespaced, Listener, ListenerOptions, Name, Stream,
};
use log::{debug, info, warn};

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

/// `baucord-<hash of the config dir>.sock`: short (a Unix socket path
/// has ~100 bytes) and one per profile. The path is hashed as given,
/// not canonicalized: the directory may not exist yet on a first run,
/// and the default one is absolute and stable anyway.
fn socket_name(config_dir: &Path) -> io::Result<Name<'static>> {
    let mut hasher = DefaultHasher::new();
    config_dir.hash(&mut hasher);
    format!("baucord-{:016x}.sock", hasher.finish()).to_ns_name::<GenericNamespaced>()
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
