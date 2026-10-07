//! Two destinations for one `log` stream: stderr (what Xcode's console,
//! `xcrun simctl launch --console-pty` and `xcrun devicectl … --console`
//! show — `ios/run.sh` streams it) and `baucord.log` in the app's
//! Documents folder (what users send, from the Files app; the same
//! format as the desktop's `env_logger` file). The level knob is
//! `log::set_max_level`, which `frontend::app::apply_log_level` turns
//! from Settings → General → Log level, exactly as on desktop and
//! Android. The Android host's `logging.rs` with stderr for logcat.

use std::{
    fs::{File, OpenOptions},
    io::Write,
    path::Path,
    sync::Mutex,
};

use log::{LevelFilter, Log, Metadata, Record};

/// `baucord.log` is rolled over to `baucord.log.1` when it passes this
/// at startup. The desktop appends forever; a phone's storage and the
/// user's patience when attaching a log justify one cut.
const ROLL_OVER_BYTES: u64 = 5 * 1024 * 1024;

struct Tee {
    file: Mutex<Option<File>>,
}

/// winit 0.30's iOS backend warns on every turn of the run loop: it
/// sends `AboutToWait` after switching itself to redraw processing, then
/// warns that a non-redraw event came after the main event loop (its own
/// event, every wake — up to 120 a second while anything animates), and
/// it says `… is ignored on iOS` for each desktop-only window call
/// Slint makes. None of it is actionable for the app (IOS-SPEC.md §14
/// D32), so below `Error` that module is dropped — before the line is
/// formatted, so it costs nothing either.
const NOISY: &str = "winit::platform_impl::ios";

impl Log for Tee {
    fn enabled(&self, metadata: &Metadata) -> bool {
        // `log::max_level()` already gated the level.
        metadata.level() == log::Level::Error || !metadata.target().starts_with(NOISY)
    }

    fn log(&self, record: &Record) {
        // `log`'s macros do not ask `enabled` first; ask here.
        if !self.enabled(record.metadata()) {
            return;
        }
        // env_logger's default line: `[ts LEVEL target] message`.
        let ts = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%.3f%:z");
        let line = format!(
            "[{ts} {:<5} {}] {}",
            record.level(),
            record.target(),
            record.args()
        );
        eprintln!("{line}");
        if let Ok(mut guard) = self.file.lock()
            && let Some(file) = guard.as_mut()
        {
            let _ = writeln!(file, "{line}");
        }
    }

    fn flush(&self) {
        if let Ok(mut guard) = self.file.lock()
            && let Some(file) = guard.as_mut()
        {
            let _ = file.flush();
        }
    }
}

/// Install the tee. Call once per process; a second call is refused by
/// `log` and ignored here.
pub fn init(log_dir: Option<&Path>) {
    let tee = Tee {
        file: Mutex::new(log_dir.and_then(open_log_file)),
    };
    if log::set_boxed_logger(Box::new(tee)).is_ok() {
        // Until the settings file says otherwise (`RUST_LOG` has no
        // meaning on a phone, so the setting rules).
        log::set_max_level(LevelFilter::Info);
    }
}

fn open_log_file(dir: &Path) -> Option<File> {
    let path = dir.join("baucord.log");
    if let Ok(meta) = std::fs::metadata(&path)
        && meta.len() > ROLL_OVER_BYTES
    {
        let _ = std::fs::rename(&path, dir.join("baucord.log.1"));
    }
    match OpenOptions::new().create(true).append(true).open(&path) {
        Ok(file) => {
            // Settings → Debug shows, sends and clears it.
            #[cfg(all(feature = "app", not(feature = "hello")))]
            frontend::logfile::set_path(path);
            Some(file)
        }
        Err(e) => {
            // Nothing to log to yet; stderr still works.
            eprintln!("baucord: could not open {}: {e}", path.display());
            None
        }
    }
}
