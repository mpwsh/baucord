//! The app's own log file, as Settings → Debug sees it: the last lines
//! to show, the tail to send to the server, and clearing it.
//!
//! Each host opens `baucord.log` itself (the desktop's release build in
//! its config folder, Android in the app's private folder, iOS in
//! Documents) and says where with [`set_path`]; a build that logs only
//! to a terminal never does, and [`path`] stays `None`. Every host
//! writes through a [`FileSink`] (the desktop's logger through a
//! [`SinkWriter`]), which rolls a log past 5 MB over to `baucord.log.1`
//! — when it is opened and as it is written — so "the end of the log"
//! reads across both files, the rolled-over one first. The desktop's
//! grew without limit in its working folder (review 2026-10, P2-47).
//!
//! Clearing truncates the file in place: every host keeps it open in
//! append mode, so its next line lands at the new end — the start.
//!
//! Also here, because every host needs the same: the line format
//! ([`line`]) and the panic hook ([`install_panic_hook`]).

use std::{
    fmt::Display,
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock, PoisonError},
};

static PATH: OnceLock<PathBuf> = OnceLock::new();

/// Where the host writes the log. The first call wins.
pub fn set_path(path: PathBuf) {
    let _ = PATH.set(path);
}

/// The log file, if this build writes one.
#[must_use]
pub fn path() -> Option<&'static Path> {
    PATH.get().map(PathBuf::as_path)
}

/// The file a phone host rolls the log over to (`baucord.log.1`).
#[must_use]
pub(crate) fn rolled_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".1");
    path.with_file_name(name)
}

/// `baucord.log` is rolled over to `baucord.log.1` when it passes this:
/// a phone's storage, and the user's patience when sending a log,
/// justify the cut — and a desktop left running for weeks.
pub(crate) const ROLL_OVER_BYTES: u64 = 5 * 1024 * 1024;

/// One log line as every host writes it — `env_logger`'s default,
/// `[ts LEVEL target] message` — under the host's own clock.
#[must_use]
pub fn line(ts: impl Display, record: &log::Record) -> String {
    format!(
        "[{ts} {:<5} {}] {}",
        record.level(),
        record.target(),
        record.args()
    )
}

/// The log file a phone host appends to, cut to size as it grows.
///
/// A log past the limit is rolled over to [`rolled_path`], replacing
/// the one there, and a new file begun. The phones did that at launch
/// only, so a process alive for days grew its log without limit
/// (review P3-45).
pub struct FileSink {
    path: PathBuf,
    limit: u64,
    state: Mutex<SinkState>,
}

struct SinkState {
    /// `None` after a roll-over that could not open a new file; tried
    /// again at the next line.
    file: Option<File>,
    /// Bytes in the file, as far as this handle knows: its length when
    /// opened plus what was written since. [`clear`] empties the file
    /// behind our back, so the file itself is asked before a roll.
    len: u64,
}

fn append_to(path: &Path) -> io::Result<File> {
    OpenOptions::new().create(true).append(true).open(path)
}

impl FileSink {
    /// Open `dir/baucord.log` for appending, rolled over first if it is
    /// already past [`ROLL_OVER_BYTES`], and name it as the app's log
    /// ([`set_path`]).
    ///
    /// # Errors
    /// The file could not be opened.
    pub fn open(dir: &Path) -> io::Result<Self> {
        let sink = Self::with_limit(dir.join("baucord.log"), ROLL_OVER_BYTES)?;
        set_path(sink.path.clone());
        Ok(sink)
    }

    /// As [`Self::open`], for a file and a limit of the caller's, and
    /// without naming it as the app's log.
    ///
    /// # Errors
    /// The file could not be opened.
    pub(crate) fn with_limit(path: PathBuf, limit: u64) -> io::Result<Self> {
        if fs::metadata(&path).is_ok_and(|meta| meta.len() > limit) {
            let _ = fs::rename(&path, rolled_path(&path));
        }
        let file = append_to(&path)?;
        let len = file.metadata().map_or(0, |meta| meta.len());
        Ok(Self {
            path,
            limit,
            state: Mutex::new(SinkState {
                file: Some(file),
                len,
            }),
        })
    }

    /// Append `line` and a newline. A failure is dropped: there is
    /// nowhere to report a log that cannot be written.
    pub fn write_line(&self, line: &str) {
        let mut bytes = Vec::with_capacity(line.len() + 1);
        bytes.extend_from_slice(line.as_bytes());
        bytes.push(b'\n');
        self.write_bytes(&bytes);
    }

    /// Append `bytes` as they are (whole lines, from a logger that
    /// formats its own). A failure is dropped, as for
    /// [`Self::write_line`].
    pub(crate) fn write_bytes(&self, bytes: &[u8]) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.len > self.limit {
            self.roll(&mut state);
        }
        if state.file.is_none() {
            state.file = append_to(&self.path).ok();
            state.len = 0;
        }
        if let Some(file) = state.file.as_mut()
            && file.write_all(bytes).is_ok()
        {
            state.len += bytes.len() as u64;
        }
    }

    /// The file is past the limit, by our count: roll it over — unless
    /// it was cleared meanwhile, in which case the count is put right.
    fn roll(&self, state: &mut SinkState) {
        let on_disk = fs::metadata(&self.path).map_or(0, |meta| meta.len());
        if on_disk <= self.limit {
            state.len = on_disk;
            return;
        }
        // Closed before it is renamed: the next line opens a new file
        // under the old name.
        state.file = None;
        let _ = fs::rename(&self.path, rolled_path(&self.path));
        state.file = append_to(&self.path).ok();
        state.len = 0;
    }

    pub fn flush(&self) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(file) = state.file.as_mut() {
            let _ = file.flush();
        }
    }
}

/// A [`FileSink`] as an [`io::Write`], for a logger that takes one
/// (the desktop's `env_logger`, whose every record is one write).
pub struct SinkWriter(pub std::sync::Arc<FileSink>);

impl Write for SinkWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write_bytes(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush();
        Ok(())
    }
}

/// A release build aborts on panic, and a windowed process (or one on
/// a phone) has no stderr anyone reads: without this a crash on any
/// thread leaves nothing behind but a log that stops mid-sentence.
/// Write the panic — thread, message, location, backtrace — to the log
/// before the abort. The one hook of the desktop and both phone hosts;
/// each had its own copy (review P3-44).
pub fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let thread = std::thread::current()
            .name()
            .unwrap_or("<unnamed>")
            .to_owned();
        let message = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| (*s).to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "<non-string panic payload>".to_string());
        let location = info.location().map_or_else(
            || "<unknown location>".to_string(),
            |l| format!("{}:{}:{}", l.file(), l.line(), l.column()),
        );
        let backtrace = std::backtrace::Backtrace::force_capture();
        log::error!("PANIC in thread '{thread}' at {location}: {message}\n{backtrace}");
        log::logger().flush();
    }));
}

/// The last `max` bytes of `path` (fewer if the file is shorter);
/// empty if it does not exist.
fn file_tail(path: &Path, max: u64) -> io::Result<Vec<u8>> {
    let mut file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let len = file.metadata()?.len();
    let start = len.saturating_sub(max);
    file.seek(SeekFrom::Start(start))?;
    let mut out = Vec::with_capacity(usize::try_from(len - start).unwrap_or(0));
    file.take(max).read_to_end(&mut out)?;
    Ok(out)
}

/// The last `max` bytes of the log: the end of the rolled-over file,
/// then the current one. If that cut a line in two, the partial first
/// line is dropped — what is returned starts at a line.
///
/// # Errors
/// A file that exists but cannot be read.
pub(crate) fn tail_bytes(path: &Path, max: usize) -> io::Result<Vec<u8>> {
    let max = max as u64;
    let current = file_tail(path, max)?;
    let room = max.saturating_sub(current.len() as u64);
    let mut out = if room > 0 {
        file_tail(&rolled_path(path), room)?
    } else {
        Vec::new()
    };
    out.extend_from_slice(&current);
    // Something left out in front: start at the next line instead of
    // in the middle of one.
    if total_len(path) > out.len() as u64
        && let Some(nl) = out.iter().position(|&b| b == b'\n')
    {
        out.drain(..=nl);
    }
    Ok(out)
}

/// The last `lines` lines of the log as text (invalid UTF-8 replaced),
/// read from at most the last `max_bytes`.
///
/// # Errors
/// As [`tail_bytes`].
pub fn tail_lines(path: &Path, lines: usize, max_bytes: usize) -> io::Result<String> {
    let bytes = tail_bytes(path, max_bytes)?;
    let text = String::from_utf8_lossy(&bytes);
    let text = text.trim_end_matches(['\n', '\r']);
    let all: Vec<&str> = text.lines().collect();
    let from = all.len().saturating_sub(lines);
    Ok(all[from..].join("\n"))
}

/// What goes to the server: a line naming the app and the platform, then
/// the end of the log, `max` bytes in all.
///
/// # Errors
/// As [`tail_bytes`].
pub fn upload_bytes(path: &Path, header: &str, max: usize) -> io::Result<Vec<u8>> {
    let header = format!("{header}\n");
    let body = tail_bytes(path, max.saturating_sub(header.len()))?;
    let mut out = Vec::with_capacity(header.len() + body.len());
    out.extend_from_slice(header.as_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

/// Empty the log and remove the rolled-over file. The host's open handle
/// carries on appending, from the start.
///
/// # Errors
/// The file could not be truncated, or the rolled-over one removed.
pub fn clear(path: &Path) -> io::Result<()> {
    match OpenOptions::new().write(true).open(path) {
        Ok(file) => file.set_len(0)?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    match fs::remove_file(rolled_path(path)) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// The current log's size plus the rolled-over one's, in bytes.
#[must_use]
pub fn total_len(path: &Path) -> u64 {
    [path.to_path_buf(), rolled_path(path)]
        .iter()
        .filter_map(|p| fs::metadata(p).ok())
        .map(|m| m.len())
        .sum()
}

#[cfg(test)]
mod tests {
    use std::{fmt::Write as _, io::Write};

    use super::*;

    fn numbered(prefix: &str, n: usize, width: usize) -> String {
        let mut text = String::new();
        for i in 1..=n {
            writeln!(text, "{prefix} {i:0width$}").unwrap();
        }
        text
    }

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("baucord-logfile-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir.join("baucord.log")
    }

    #[test]
    fn the_rolled_file_sits_beside_the_log() {
        assert_eq!(
            rolled_path(Path::new("/a/b/baucord.log")),
            PathBuf::from("/a/b/baucord.log.1")
        );
    }

    #[test]
    fn a_missing_log_reads_empty() {
        let p = scratch("missing");
        assert_eq!(tail_lines(&p, 100, 4096).unwrap(), "");
        assert_eq!(total_len(&p), 0);
        clear(&p).unwrap();
    }

    #[test]
    fn the_last_lines_and_no_more() {
        let p = scratch("lines");
        fs::write(&p, numbered("line", 250, 1)).unwrap();
        let tail = tail_lines(&p, 100, 1 << 20).unwrap();
        let got: Vec<&str> = tail.lines().collect();
        assert_eq!(got.len(), 100);
        assert_eq!(got[0], "line 151");
        assert_eq!(got[99], "line 250");
    }

    #[test]
    fn a_cut_never_starts_mid_line() {
        let p = scratch("cut");
        fs::write(&p, "first line here\nsecond\nthird\n").unwrap();
        // The last 12 bytes are "econd\nthird\n": "second" is cut.
        assert_eq!(tail_bytes(&p, 12).unwrap(), b"third\n");
        // Exactly the whole file: nothing was left out, nothing dropped.
        assert_eq!(
            tail_bytes(&p, 29).unwrap(),
            b"first line here\nsecond\nthird\n"
        );
        assert_eq!(
            tail_bytes(&p, 1000).unwrap(),
            b"first line here\nsecond\nthird\n"
        );
    }

    #[test]
    fn the_rolled_over_file_comes_first() {
        let p = scratch("rolled");
        fs::write(rolled_path(&p), "old 1\nold 2\n").unwrap();
        fs::write(&p, "new 1\n").unwrap();
        assert_eq!(tail_lines(&p, 10, 1 << 20).unwrap(), "old 1\nold 2\nnew 1");
        assert_eq!(tail_lines(&p, 2, 1 << 20).unwrap(), "old 2\nnew 1");
        assert_eq!(total_len(&p), 18);
    }

    #[test]
    fn the_upload_fits_and_starts_with_the_header() {
        let p = scratch("upload");
        fs::write(&p, numbered("entry", 1000, 4)).unwrap();
        let up = upload_bytes(&p, "# baucord 1.2.3 (ios)", 200).unwrap();
        assert!(up.len() <= 200);
        let s = String::from_utf8(up).unwrap();
        assert!(s.starts_with("# baucord 1.2.3 (ios)\nentry "));
        assert!(s.ends_with("entry 1000\n"));
    }

    #[test]
    fn a_line_reads_like_env_loggers() {
        let args = format_args!("hello {}", 7);
        let record = log::Record::builder()
            .args(args)
            .level(log::Level::Warn)
            .target("client::network")
            .build();
        assert_eq!(
            line("2026-10-12T08:00:00.000+00:00", &record),
            "[2026-10-12T08:00:00.000+00:00 WARN  client::network] hello 7"
        );
    }

    /// Review P3-45: the log is cut as it grows, not only at launch.
    #[test]
    fn the_sink_rolls_the_log_over_as_it_is_written() {
        let p = scratch("sink-roll");
        let sink = FileSink::with_limit(p.clone(), 100).unwrap();
        // 11 bytes a line: the tenth takes the file past 100.
        for i in 0..10 {
            sink.write_line(&format!("line {i:05}"));
        }
        assert!(!rolled_path(&p).exists(), "not rolled until past the limit");
        assert_eq!(fs::metadata(&p).unwrap().len(), 110);
        sink.write_line("line 00010");
        assert_eq!(fs::read_to_string(&p).unwrap(), "line 00010\n");
        let rolled = fs::read_to_string(rolled_path(&p)).unwrap();
        assert!(rolled.starts_with("line 00000\n") && rolled.ends_with("line 00009\n"));
        // The reader sees one log across both.
        assert_eq!(
            tail_lines(&p, 2, 1 << 20).unwrap(),
            "line 00009\nline 00010"
        );
        // And again: the rolled-over file is replaced, never a third.
        for i in 11..21 {
            sink.write_line(&format!("line {i:05}"));
        }
        assert_eq!(fs::read_to_string(&p).unwrap(), "line 00020\n");
        let rolled = fs::read_to_string(rolled_path(&p)).unwrap();
        assert!(rolled.starts_with("line 00010\n") && rolled.ends_with("line 00019\n"));
    }

    #[test]
    fn the_sink_rolls_a_log_that_is_already_too_long_when_opened() {
        let p = scratch("sink-open");
        fs::write(&p, "x".repeat(150)).unwrap();
        let sink = FileSink::with_limit(p.clone(), 100).unwrap();
        sink.write_line("fresh");
        assert_eq!(fs::read_to_string(&p).unwrap(), "fresh\n");
        assert_eq!(fs::metadata(rolled_path(&p)).unwrap().len(), 150);

        // One within the limit is appended to.
        let q = scratch("sink-append");
        fs::write(&q, "earlier\n").unwrap();
        let sink = FileSink::with_limit(q.clone(), 100).unwrap();
        sink.write_line("later");
        sink.flush();
        assert_eq!(fs::read_to_string(&q).unwrap(), "earlier\nlater\n");
        assert!(!rolled_path(&q).exists());
    }

    /// Settings → Debug → Clear empties the file behind the sink's
    /// back: its count is stale, and must not roll a short file over
    /// (which would also replace the older one with it).
    #[test]
    fn a_log_cleared_behind_the_sink_is_not_rolled_over() {
        let p = scratch("sink-clear");
        let sink = FileSink::with_limit(p.clone(), 100).unwrap();
        for i in 0..9 {
            sink.write_line(&format!("line {i:05}"));
        }
        clear(&p).unwrap();
        // By the sink's count these take it past the limit.
        sink.write_line("after 1");
        sink.write_line("after 2");
        sink.write_line("after 3");
        assert_eq!(
            fs::read_to_string(&p).unwrap(),
            "after 1\nafter 2\nafter 3\n"
        );
        assert!(!rolled_path(&p).exists());
    }

    #[test]
    fn clearing_empties_both_files_and_appending_starts_over() {
        let p = scratch("clear");
        fs::write(rolled_path(&p), "old\n").unwrap();
        // The host's own handle, open for appending as the loggers are.
        let mut host = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&p)
            .unwrap();
        host.write_all(b"before\n").unwrap();
        clear(&p).unwrap();
        assert!(!rolled_path(&p).exists());
        assert_eq!(total_len(&p), 0);
        host.write_all(b"after\n").unwrap();
        assert_eq!(fs::read_to_string(&p).unwrap(), "after\n");
    }
}
