//! Two destinations for one `log` stream: logcat (`adb logcat -s
//! baucord` is the live view) and `baucord.log` in the app's private
//! directory (what users send; the same format as the desktop's
//! env_logger file). The level knob is `log::set_max_level`, which
//! `frontend::app::apply_log_level` turns from Settings → General →
//! Log level, exactly as on desktop.

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
    logcat: android_logger::AndroidLogger,
    file: Mutex<Option<File>>,
}

impl Log for Tee {
    fn enabled(&self, metadata: &Metadata) -> bool {
        // `log::max_level()` already gated this record; the logcat
        // config has no filter of its own.
        self.logcat.enabled(metadata)
    }

    fn log(&self, record: &Record) {
        self.logcat.log(record);
        if let Ok(mut guard) = self.file.lock()
            && let Some(file) = guard.as_mut()
        {
            // env_logger's default line: `[ts LEVEL target] message`.
            let ts = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%.3f%:z");
            let _ = writeln!(
                file,
                "[{ts} {:<5} {}] {}",
                record.level(),
                record.target(),
                record.args()
            );
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
pub fn init(data_dir: Option<&Path>) {
    let file = data_dir.and_then(open_log_file);
    let logcat = android_logger::AndroidLogger::new(
        android_logger::Config::default()
            .with_max_level(LevelFilter::Trace)
            .with_tag("baucord"),
    );
    let tee = Tee {
        logcat,
        file: Mutex::new(file),
    };
    if log::set_boxed_logger(Box::new(tee)).is_ok() {
        // Everything through until the settings file says otherwise
        // (`RUST_LOG` has no meaning on a phone, so the setting rules).
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
            #[cfg(feature = "app")]
            frontend::logfile::set_path(path);
            Some(file)
        }
        Err(e) => {
            // Nothing to log to yet; logcat gets it once the tee is up.
            eprintln!("baucord: could not open {}: {e}", path.display());
            None
        }
    }
}
