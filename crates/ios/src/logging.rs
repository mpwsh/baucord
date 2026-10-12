//! Two destinations for one `log` stream: stderr (what Xcode's console,
//! `xcrun simctl launch --console-pty` and `xcrun devicectl … --console`
//! show — `ios/run.sh` streams it) and `baucord.log` in the app's
//! Documents folder (what users send, from the Files app; the same
//! format as the desktop's `env_logger` file). The level knob is
//! `log::set_max_level`, which `frontend::app::apply_log_level` turns
//! from Settings → General → Log level, exactly as on desktop and
//! Android. The Android host's `logging.rs` with stderr for logcat.

use std::path::Path;

use app_core::logfile::{self, FileSink};
use log::{LevelFilter, Log, Metadata, Record};

struct Tee {
    /// `baucord.log`, rolled over to `baucord.log.1` past 5 MB — as it
    /// is written, not only at launch (review P3-45).
    file: Option<FileSink>,
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
        let ts = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%.3f%:z");
        let line = logfile::line(ts, record);
        eprintln!("{line}");
        if let Some(file) = &self.file {
            file.write_line(&line);
        }
    }

    fn flush(&self) {
        if let Some(file) = &self.file {
            file.flush();
        }
    }
}

/// Install the tee. Call once per process; a second call is refused by
/// `log` and ignored here.
pub fn init(log_dir: Option<&Path>) {
    let tee = Tee {
        file: log_dir.and_then(open_log_file),
    };
    if log::set_boxed_logger(Box::new(tee)).is_ok() {
        // Until the settings file says otherwise (`RUST_LOG` has no
        // meaning on a phone, so the setting rules).
        log::set_max_level(LevelFilter::Info);
    }
}

/// Settings → Debug shows, sends and clears the file this opens
/// (`FileSink::open` names it as the app's log).
fn open_log_file(dir: &Path) -> Option<FileSink> {
    match FileSink::open(dir) {
        Ok(sink) => Some(sink),
        Err(e) => {
            // Nothing to log to yet; stderr still works.
            eprintln!("baucord: could not open a log in {}: {e}", dir.display());
            None
        }
    }
}
