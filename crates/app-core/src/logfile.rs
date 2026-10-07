//! The app's own log file, as Settings → Debug sees it: the last lines
//! to show, the tail to send to the server, and clearing it.
//!
//! Each host opens `baucord.log` itself (the desktop's release build in
//! its working folder, Android in the app's private folder, iOS in
//! Documents) and says where with [`set_path`]; a build that logs only
//! to a terminal never does, and [`path`] stays `None`. The phone hosts
//! roll a log past 5 MB over to `baucord.log.1` at startup, so "the end
//! of the log" reads across both files, the rolled-over one first.
//!
//! Clearing truncates the file in place: every host keeps it open in
//! append mode, so its next line lands at the new end — the start.

use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::OnceLock,
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
pub fn rolled_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".1");
    path.with_file_name(name)
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
pub fn tail_bytes(path: &Path, max: usize) -> io::Result<Vec<u8>> {
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
