//! Streamed downloads (1.5, feature bit `LARGE_BLOBS`): a file of any
//! size — a recording — written to disk as it arrives.
//!
//! The server sends `BlobPart`s, each with its offset; they are written
//! where they belong in a `.part` file, so pieces that arrive out of
//! order land right. Once every byte is there, the file is hashed on a
//! worker thread (`download-check`: an hour of WAV is a third of a
//! gigabyte, not something the network thread may hold voice for) and
//! the app is told it is done, or that it failed.

use std::collections::{BTreeMap, HashMap};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use log::warn;

/// One download being written.
pub(crate) struct Download {
    file: File,
    path: PathBuf,
    total: u64,
    /// Byte ranges received, start → end, merged as they touch.
    have: BTreeMap<u64, u64>,
    last_pct: u8,
}

impl Download {
    /// Create (or truncate) `path` for a file of `total` bytes.
    pub(crate) fn create(path: &Path, total: u64) -> std::io::Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(path)?;
        file.set_len(total)?;
        Ok(Self {
            file,
            path: path.to_path_buf(),
            total,
            have: BTreeMap::new(),
            last_pct: 0,
        })
    }

    /// Write one part; the new percentage when it moved.
    pub(crate) fn write(&mut self, offset: u64, data: &[u8]) -> std::io::Result<Option<u8>> {
        if data.is_empty() {
            return Ok(None);
        }
        let end = offset.saturating_add(data.len() as u64).min(self.total);
        if offset >= end {
            return Ok(None);
        }
        let take = usize::try_from(end - offset).unwrap_or(data.len());
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.write_all(&data[..take])?;
        self.note(offset, end);
        let pct =
            u8::try_from(self.received().saturating_mul(100) / self.total.max(1)).unwrap_or(100);
        if pct != self.last_pct {
            self.last_pct = pct;
            return Ok(Some(pct));
        }
        Ok(None)
    }

    fn note(&mut self, start: u64, end: u64) {
        let mut start = start;
        let mut end = end;
        // Merge with every range it touches.
        let touching: Vec<(u64, u64)> = self
            .have
            .range(..=end)
            .filter(|(s, e)| **e >= start && **s <= end)
            .map(|(s, e)| (*s, *e))
            .collect();
        for (s, e) in touching {
            self.have.remove(&s);
            start = start.min(s);
            end = end.max(e);
        }
        self.have.insert(start, end);
    }

    /// Bytes received so far, without counting overlap twice.
    pub(crate) fn received(&self) -> u64 {
        self.have.iter().map(|(s, e)| e - s).sum()
    }

    /// Every byte is here.
    pub(crate) fn complete(&self) -> bool {
        self.have.get(&0).is_some_and(|e| *e >= self.total)
    }

    /// Close the file and hand back where it is.
    pub(crate) fn finish(mut self) -> std::io::Result<PathBuf> {
        self.file.flush()?;
        self.file.sync_all()?;
        Ok(self.path)
    }
}

/// Every download of this session, by blob.
#[derive(Default)]
pub(crate) struct Downloads {
    pub(crate) running: HashMap<[u8; 32], Download>,
}

/// Whether the file at `path` hashes to `blob_id` (on a worker).
pub(crate) fn check(path: &Path, blob_id: &[u8; 32]) -> bool {
    let Ok(mut file) = File::open(path) else {
        return false;
    };
    let mut hasher = transport::sha256::Hasher::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => hasher.update(&buf[..n]),
            Err(e) => {
                warn!("download check failed: {e}");
                return false;
            }
        }
    }
    hasher.finish() == *blob_id
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parts_out_of_order_complete_the_file() {
        let dir = std::env::temp_dir().join(format!("baucord-dl-{}", std::process::id()));
        let path = dir.join("x.part");
        let body: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
        let mut d = Download::create(&path, body.len() as u64).unwrap();
        let mut pieces: Vec<(usize, &[u8])> = body
            .chunks(16_384)
            .enumerate()
            .map(|(i, c)| (i * 16_384, c))
            .collect();
        pieces.swap(1, 4);
        for (at, part) in &pieces[..pieces.len() - 1] {
            d.write(*at as u64, part).unwrap();
            assert!(!d.complete());
        }
        // A part twice changes nothing.
        d.write(0, pieces[0].1).unwrap();
        let (at, part) = pieces[pieces.len() - 1];
        d.write(at as u64, part).unwrap();
        assert!(d.complete());
        assert_eq!(d.received(), body.len() as u64);
        let path = d.finish().unwrap();
        assert!(check(&path, &transport::sha256::digest(&body)));
        std::fs::remove_dir_all(&dir).ok();
    }
}
