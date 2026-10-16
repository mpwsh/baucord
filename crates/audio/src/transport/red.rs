//! Redundant audio bundles — RFC 2198 in shape, trimmed for one codec.
//!
//! Each RED payload carries the packet's own Opus frame (the *primary*)
//! plus full copies of one or two preceding packets. A receiver missing
//! packet N recovers it exactly from packet N+1 (or N+2). Perkins ch. 9:
//! the redundancy may be delayed more than one packet to survive bursts,
//! at the cost of the receiver holding playout for that long — which is
//! why the jitter buffer lifts its target by the redundancy span while
//! loss is being observed (`AdaptiveJitterBuffer`).
//!
//! Unlike RFC 2198 there is no payload type per block (it is always Opus)
//! and no 14-bit limit on the timestamp offset. Layout:
//!
//! ```text
//! u8   count             number of redundant blocks (0..=MAX_BLOCKS)
//! (u16 ts_offset, u16 len) × count   oldest first; offsets in 48 kHz samples
//! bytes[len] × count     redundant Opus packets, oldest first
//! bytes[...]             primary Opus packet (to end of payload)
//! ```
//!
//! A block at position `i` (0 = oldest) covers sequence
//! `primary_seq - (count - i)`: the sender only bundles copies of packets
//! it actually transmitted, consecutively, so the sequence distance is
//! implicit. Across a DTX pause the sender simply sends `count = 0`.

/// Most redundant copies one bundle may carry.
pub const MAX_BLOCKS: usize = 2;

const HEADER_BYTES_PER_BLOCK: usize = 4;

/// A redundant block as parsed from a bundle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Block<'a> {
    /// Samples before the primary's timestamp this block's audio starts.
    pub ts_offset: u32,
    /// Sequence distance before the primary (1 = the packet just before).
    pub seq_distance: u32,
    pub data: &'a [u8],
}

/// The redundant blocks of one bundle, oldest first: at most
/// [`MAX_BLOCKS`], so they live in the bundle itself. They were a `Vec`
/// — an allocation per packet received, as the two lists [`encode`]
/// built were per packet sent (review P3-21). Reads as a slice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Blocks<'a> {
    blocks: [Block<'a>; MAX_BLOCKS],
    len: usize,
}

impl<'a> std::ops::Deref for Blocks<'a> {
    type Target = [Block<'a>];

    fn deref(&self) -> &Self::Target {
        &self.blocks[..self.len]
    }
}

impl<'a, 'b> IntoIterator for &'b Blocks<'a> {
    type Item = &'b Block<'a>;
    type IntoIter = std::slice::Iter<'b, Block<'a>>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

/// Parsed bundle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bundle<'a> {
    pub primary: &'a [u8],
    /// Oldest first.
    pub redundant: Blocks<'a>,
}

/// Build a bundle. `redundant` is oldest first, each `(ts_offset, data)`.
/// Blocks beyond `MAX_BLOCKS`, empty blocks, or blocks over 65535 bytes /
/// 65535 samples of offset are dropped (the primary always goes out).
pub fn encode(primary: &[u8], redundant: &[(u32, &[u8])], out: &mut Vec<u8>) {
    out.clear();
    // Walked three times rather than collected once: no allocation.
    let blocks = || {
        redundant
            .iter()
            .filter(|(off, d)| !d.is_empty() && d.len() <= 0xFFFF && *off <= 0xFFFF)
            .take(MAX_BLOCKS)
    };
    #[allow(clippy::cast_possible_truncation)] // ≤ MAX_BLOCKS
    out.push(blocks().count() as u8);
    for (off, d) in blocks() {
        #[allow(clippy::cast_possible_truncation)] // filtered to u16 range
        {
            out.extend_from_slice(&(*off as u16).to_be_bytes());
            out.extend_from_slice(&(d.len() as u16).to_be_bytes());
        }
    }
    for (_, d) in blocks() {
        out.extend_from_slice(d);
    }
    out.extend_from_slice(primary);
}

/// The size [`encode`] makes of these: its header, the blocks it keeps
/// and the primary.
#[must_use]
pub fn encoded_len(primary: &[u8], redundant: &[(u32, &[u8])]) -> usize {
    let kept = redundant
        .iter()
        .filter(|(off, d)| !d.is_empty() && d.len() <= 0xFFFF && *off <= 0xFFFF)
        .take(MAX_BLOCKS);
    1 + kept
        .map(|(_, d)| HEADER_BYTES_PER_BLOCK + d.len())
        .sum::<usize>()
        + primary.len()
}

/// Parse a bundle. `None` if the payload is malformed.
#[must_use]
pub fn decode(payload: &[u8]) -> Option<Bundle<'_>> {
    let (&count, rest) = payload.split_first()?;
    let count = usize::from(count);
    if count > MAX_BLOCKS {
        return None;
    }
    let header_len = count * HEADER_BYTES_PER_BLOCK;
    if rest.len() < header_len {
        return None;
    }
    let (headers, mut body) = rest.split_at(header_len);
    let mut redundant = Blocks {
        blocks: [Block {
            ts_offset: 0,
            seq_distance: 0,
            data: &[],
        }; MAX_BLOCKS],
        len: 0,
    };
    for (i, h) in headers.chunks_exact(HEADER_BYTES_PER_BLOCK).enumerate() {
        let ts_offset = u32::from(u16::from_be_bytes([h[0], h[1]]));
        let len = usize::from(u16::from_be_bytes([h[2], h[3]]));
        if body.len() < len || len == 0 {
            return None;
        }
        let (data, tail) = body.split_at(len);
        body = tail;
        #[allow(clippy::cast_possible_truncation)] // count ≤ MAX_BLOCKS
        let block = Block {
            ts_offset,
            seq_distance: (count - i) as u32,
            data,
        };
        redundant.blocks[i] = block;
        redundant.len = i + 1;
    }
    if body.is_empty() {
        return None;
    }
    Some(Bundle {
        primary: body,
        redundant,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_length_is_what_encode_makes() {
        let (a, b, p) = ([1u8; 40], [2u8; 70], [3u8; 90]);
        let blocks: [(u32, &[u8]); 3] = [(1920, &a), (960, &b), (0, &[])];
        let mut out = Vec::new();
        encode(&p, &blocks, &mut out);
        assert_eq!(encoded_len(&p, &blocks), out.len());
        encode(&p, &[], &mut out);
        assert_eq!(encoded_len(&p, &[]), out.len());
    }

    #[test]
    fn round_trips_primary_and_two_blocks() {
        let mut out = Vec::new();
        encode(b"PRIMARY", &[(1920, b"older"), (960, b"newer")], &mut out);
        let b = decode(&out).expect("valid");
        assert_eq!(b.primary, b"PRIMARY");
        assert_eq!(b.redundant.len(), 2);
        assert_eq!(
            b.redundant[0],
            Block {
                ts_offset: 1920,
                seq_distance: 2,
                data: b"older"
            }
        );
        assert_eq!(
            b.redundant[1],
            Block {
                ts_offset: 960,
                seq_distance: 1,
                data: b"newer"
            }
        );
    }

    #[test]
    fn zero_blocks_is_just_a_prefixed_primary() {
        let mut out = Vec::new();
        encode(b"P", &[], &mut out);
        assert_eq!(out, [0u8, b'P']);
        assert_eq!(decode(&out).expect("valid").primary, b"P");
    }

    #[test]
    fn rejects_truncated_bundles() {
        let mut out = Vec::new();
        encode(b"PRIMARY", &[(960, b"copy")], &mut out);
        for cut in 0..out.len() - 1 {
            // Any cut that removes the primary or part of a block is invalid;
            // cuts that leave a valid shorter primary are still parseable.
            let _ = decode(&out[..cut]);
        }
        assert!(decode(&[]).is_none());
        assert!(decode(&[3]).is_none(), "count above MAX_BLOCKS");
        assert!(
            decode(&[1, 0, 0, 0, 9, b'x']).is_none(),
            "block length past end"
        );
    }

    #[test]
    fn drops_blocks_that_do_not_fit_the_header() {
        let big = vec![0u8; 70_000];
        let mut out = Vec::new();
        encode(b"P", &[(960, &big), (960, b"ok")], &mut out);
        let b = decode(&out).expect("valid");
        assert_eq!(b.redundant.len(), 1);
        assert_eq!(b.redundant[0].data, b"ok");
    }
}
