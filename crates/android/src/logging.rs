//! Two destinations for one `log` stream: logcat (`adb logcat -s
//! baucord` is the live view) and `baucord.log` in the app's private
//! directory (what users send; the same format as the desktop's
//! env_logger file). The level knob is `log::set_max_level`, which
//! `frontend::app::apply_log_level` turns from Settings → General →
//! Log level, exactly as on desktop.

use std::path::Path;

use app_core::logfile::{self, FileSink};
use log::{LevelFilter, Log, Metadata, Record};

struct Tee {
    logcat: android_logger::AndroidLogger,
    /// `baucord.log`, rolled over to `baucord.log.1` past 5 MB — as it
    /// is written, not only at launch (review P3-45).
    file: Option<FileSink>,
}

impl Log for Tee {
    fn enabled(&self, metadata: &Metadata) -> bool {
        // `log::max_level()` already gated this record; the logcat
        // config has no filter of its own.
        self.logcat.enabled(metadata)
    }

    fn log(&self, record: &Record) {
        self.logcat.log(record);
        if let Some(file) = &self.file {
            let ts = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%.3f%:z");
            file.write_line(&logfile::line(ts, record));
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
pub fn init(data_dir: Option<&Path>) {
    let file = data_dir.and_then(open_log_file);
    let logcat = android_logger::AndroidLogger::new(
        android_logger::Config::default()
            .with_max_level(LevelFilter::Trace)
            .with_tag("baucord"),
    );
    let tee = Tee { logcat, file };
    if log::set_boxed_logger(Box::new(tee)).is_ok() {
        // Everything through until the settings file says otherwise
        // (`RUST_LOG` has no meaning on a phone, so the setting rules).
        log::set_max_level(LevelFilter::Info);
    }
}

/// Settings → Debug shows, sends and clears the file this opens
/// (`FileSink::open` names it as the app's log).
fn open_log_file(dir: &Path) -> Option<FileSink> {
    match FileSink::open(dir) {
        Ok(sink) => Some(sink),
        Err(e) => {
            // Nothing to log to yet; logcat gets it once the tee is up.
            eprintln!("baucord: could not open a log in {}: {e}", dir.display());
            None
        }
    }
}
