//! SHA-256, once for the whole tree: `ring`'s, which TLS brings into
//! every build anyway. `sha2` used to sit beside it as a second
//! implementation, in three crates, for exactly this (review P3-50).

use ring::digest::{Context, SHA256};

/// The digest of `data`.
#[must_use]
pub fn digest(data: &[u8]) -> [u8; 32] {
    let mut hasher = Hasher::new();
    hasher.update(data);
    hasher.finish()
}

/// A digest fed piece by piece: a file that arrives in chunks is
/// hashed as it comes in.
#[derive(Clone)]
pub struct Hasher(Context);

impl Default for Hasher {
    fn default() -> Self {
        Self(Context::new(&SHA256))
    }
}

impl Hasher {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn update(&mut self, data: &[u8]) {
        self.0.update(data);
    }

    /// The digest of everything fed in.
    #[must_use]
    pub fn finish(self) -> [u8; 32] {
        let mut out = [0u8; 32];
        out.copy_from_slice(self.0.finish().as_ref());
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        use std::fmt::Write as _;
        bytes.iter().fold(String::new(), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
    }

    #[test]
    fn the_standard_vectors() {
        // FIPS 180-4's examples: what every stored hash and every pinned
        // fingerprint in the field was computed as.
        assert_eq!(
            hex(&digest(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            hex(&digest(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn pieces_hash_as_the_whole() {
        let data: Vec<u8> = (0..10_000u32).map(|i| (i % 251) as u8).collect();
        let mut hasher = Hasher::new();
        for chunk in data.chunks(777) {
            hasher.update(chunk);
        }
        assert_eq!(hasher.finish(), digest(&data));
    }
}
