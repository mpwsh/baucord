//! The recording's file: RIFF/WAVE, PCM signed 16-bit little-endian,
//! one channel. Written as it goes with a placeholder header, patched
//! with the real lengths at the end — or at the next start, after a
//! crash ([`finish_spool`]).

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

/// The header's size: data starts here.
pub const HEADER_BYTES: u64 = 44;

/// The most a WAV's 32-bit sizes can say.
pub const MAX_DATA_BYTES: u64 = u32::MAX as u64 - HEADER_BYTES;

/// A 44-byte header for `data_bytes` of 16-bit mono at `rate`.
#[must_use]
pub fn header(rate: u32, data_bytes: u64) -> [u8; 44] {
    let data = u32::try_from(data_bytes.min(MAX_DATA_BYTES)).unwrap_or(u32::MAX);
    let mut h = [0u8; 44];
    h[0..4].copy_from_slice(b"RIFF");
    h[4..8].copy_from_slice(&(36u32.saturating_add(data)).to_le_bytes());
    h[8..12].copy_from_slice(b"WAVE");
    h[12..16].copy_from_slice(b"fmt ");
    h[16..20].copy_from_slice(&16u32.to_le_bytes());
    h[20..22].copy_from_slice(&1u16.to_le_bytes()); // PCM
    h[22..24].copy_from_slice(&1u16.to_le_bytes()); // mono
    h[24..28].copy_from_slice(&rate.to_le_bytes());
    h[28..32].copy_from_slice(&(rate * 2).to_le_bytes()); // byte rate
    h[32..34].copy_from_slice(&2u16.to_le_bytes()); // block align
    h[34..36].copy_from_slice(&16u16.to_le_bytes()); // bits
    h[36..40].copy_from_slice(b"data");
    h[40..44].copy_from_slice(&data.to_le_bytes());
    h
}

/// A WAV being written.
pub struct WavWriter {
    file: io::BufWriter<File>,
    rate: u32,
    data_bytes: u64,
}

impl WavWriter {
    /// Create `path` with a placeholder header.
    ///
    /// # Errors
    /// I/O.
    pub fn create(path: &Path, rate: u32) -> io::Result<Self> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let mut file = io::BufWriter::with_capacity(1 << 16, File::create(path)?);
        file.write_all(&header(rate, 0))?;
        Ok(Self {
            file,
            rate,
            data_bytes: 0,
        })
    }

    /// Append samples. Refuses past the format's limit.
    ///
    /// # Errors
    /// I/O, or the file is full ([`io::ErrorKind::FileTooLarge`]).
    pub fn write(&mut self, samples: &[i16]) -> io::Result<()> {
        let bytes = samples.len() as u64 * 2;
        if self.data_bytes + bytes > MAX_DATA_BYTES {
            return Err(io::Error::new(io::ErrorKind::FileTooLarge, "WAV limit"));
        }
        for s in samples {
            self.file.write_all(&s.to_le_bytes())?;
        }
        self.data_bytes += bytes;
        Ok(())
    }

    /// `n` samples of silence.
    ///
    /// # Errors
    /// As [`Self::write`].
    pub fn silence(&mut self, mut n: u64) -> io::Result<()> {
        const ZEROS: [i16; 4096] = [0; 4096];
        while n > 0 {
            let take = n.min(ZEROS.len() as u64);
            self.write(&ZEROS[..usize::try_from(take).unwrap_or(0)])?;
            n -= take;
        }
        Ok(())
    }

    /// Samples written so far.
    #[must_use]
    pub fn samples(&self) -> u64 {
        self.data_bytes / 2
    }

    /// Push what is buffered to the disk, so a crash loses at most
    /// what came after.
    ///
    /// # Errors
    /// I/O.
    pub fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }

    /// Patch the header and close. Returns the file's size.
    ///
    /// # Errors
    /// I/O.
    pub fn finish(mut self) -> io::Result<u64> {
        self.file.flush()?;
        let mut file = self
            .file
            .into_inner()
            .map_err(io::IntoInnerError::into_error)?;
        file.seek(SeekFrom::Start(0))?;
        file.write_all(&header(self.rate, self.data_bytes))?;
        file.sync_all()?;
        Ok(HEADER_BYTES + self.data_bytes)
    }
}

/// Hash a finished WAV and move it into the blob store as
/// `blobs/<sha256>`. Returns the hash and the size.
///
/// # Errors
/// I/O.
pub fn move_to_blobs(path: &Path, blobs: &Path) -> io::Result<([u8; 32], u64)> {
    let mut file = File::open(path)?;
    let mut hasher = transport::sha256::Hasher::new();
    let mut buf = vec![0u8; 1 << 16];
    let mut size = 0u64;
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        size += n as u64;
    }
    drop(file);
    let blob_id = hasher.finish();
    fs::create_dir_all(blobs)?;
    let dest = blobs.join(crate::persistence::hex(&blob_id));
    if dest.exists() {
        fs::remove_file(path)?;
    } else if fs::rename(path, &dest).is_err() {
        // Another file system (a spool on its own volume): copy.
        fs::copy(path, &dest)?;
        fs::remove_file(path)?;
    }
    Ok((blob_id, size))
}

/// Finish a spool a crash left behind: patch its header to the data
/// actually there (whole samples), then [`move_to_blobs`]. `None` when
/// there is no file, or nothing in it. Returns the hash, the size and
/// the duration in ms.
///
/// # Errors
/// I/O.
pub fn finish_spool(
    part: &Path,
    rate: u32,
    blobs: &Path,
) -> io::Result<Option<([u8; 32], u64, u64)>> {
    let Ok(meta) = fs::metadata(part) else {
        return Ok(None);
    };
    let len = meta.len();
    if len <= HEADER_BYTES {
        fs::remove_file(part).ok();
        return Ok(None);
    }
    let data = ((len - HEADER_BYTES) / 2 * 2).min(MAX_DATA_BYTES);
    {
        let mut file = OpenOptions::new().write(true).open(part)?;
        file.set_len(HEADER_BYTES + data)?;
        file.seek(SeekFrom::Start(0))?;
        file.write_all(&header(rate, data))?;
        file.sync_all()?;
    }
    let duration_ms = data / 2 * 1000 / u64::from(rate.max(1));
    let (blob_id, size) = move_to_blobs(part, blobs)?;
    Ok(Some((blob_id, size, duration_ms)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_written_file_reads_as_a_wav_of_its_samples() {
        let dir = std::env::temp_dir().join(format!("baucord-wav-{}", std::process::id()));
        let path = dir.join("1.wav.part");
        let mut w = WavWriter::create(&path, 16_000).unwrap();
        w.write(&[1, -1, 2]).unwrap();
        w.silence(5).unwrap();
        assert_eq!(w.samples(), 8);
        let size = w.finish().unwrap();
        assert_eq!(size, 44 + 16);
        let bytes = fs::read(&path).unwrap();
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(bytes[40..44].try_into().unwrap()), 16);
        assert_eq!(
            u32::from_le_bytes(bytes[24..28].try_into().unwrap()),
            16_000
        );
        assert_eq!(i16::from_le_bytes([bytes[46], bytes[47]]), -1);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_crashed_spool_is_patched_to_its_data() {
        let dir = std::env::temp_dir().join(format!("baucord-spool-{}", std::process::id()));
        let blobs = dir.join("blobs");
        fs::create_dir_all(&dir).unwrap();
        let part = dir.join("7.wav.part");
        // A placeholder header, 1000 samples, and half a sample.
        let mut bytes = header(48_000, 0).to_vec();
        bytes.extend(std::iter::repeat_n(0u8, 2001));
        fs::write(&part, bytes).unwrap();
        let (blob, size, ms) = finish_spool(&part, 48_000, &blobs).unwrap().unwrap();
        assert_eq!(size, 44 + 2000);
        assert_eq!(ms, 20);
        let stored = fs::read(blobs.join(crate::persistence::hex(&blob))).unwrap();
        assert_eq!(u32::from_le_bytes(stored[40..44].try_into().unwrap()), 2000);
        assert!(!part.exists());
        fs::remove_dir_all(&dir).ok();
    }
}
