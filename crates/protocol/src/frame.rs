//! Stream framing: a 4-byte little-endian `u32` length prefix per
//! message.
//!
//! Fixed width on purpose — it parses incrementally without a varint
//! state machine, and the postcard payload inside is unchanged. Streams
//! carry `[tag][len][frame][len][frame]…`; the tag byte is consumed by
//! the transport before the [`Decoder`] ever sees the bytes.

use bytes::{Buf, Bytes, BytesMut};

/// Size of the length prefix.
pub const PREFIX_BYTES: usize = 4;

/// Encode `payload` as one frame: prefix + bytes, in a fresh buffer.
#[must_use]
pub fn encode(payload: &[u8]) -> Bytes {
    let mut out = BytesMut::with_capacity(PREFIX_BYTES + payload.len());
    out.extend_from_slice(&prefix(payload.len()));
    out.extend_from_slice(payload);
    out.freeze()
}

/// The prefix alone, for callers that write the payload separately.
///
/// # Panics
/// Never for payloads under 4 GiB; every lane budget is far smaller.
#[must_use]
pub fn prefix(len: usize) -> [u8; PREFIX_BYTES] {
    let len = u32::try_from(len).expect("frame length exceeds u32");
    len.to_le_bytes()
}

/// Why [`Decoder::next_frame`] refused the stream.
#[derive(Debug, PartialEq, Eq)]
pub enum DecodeError {
    /// A prefix announced more than the lane's budget — the peer is
    /// misbehaving; the caller stops the stream.
    Oversize { announced: usize, max: usize },
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecodeError::Oversize { announced, max } => {
                write!(
                    f,
                    "frame of {announced} bytes exceeds the {max}-byte lane budget"
                )
            }
        }
    }
}

impl std::error::Error for DecodeError {}

/// Incremental frame reader for one stream. Feed it whatever bytes the
/// transport delivers, in order; pull whole frames out.
#[derive(Debug)]
pub struct Decoder {
    buf: BytesMut,
    max_frame: usize,
    /// Length of the frame currently being assembled, once its prefix
    /// has been read.
    pending_len: Option<usize>,
}

impl Decoder {
    /// `max_frame` is the largest payload the lane allows.
    #[must_use]
    pub fn new(max_frame: usize) -> Self {
        Self {
            buf: BytesMut::new(),
            max_frame,
            pending_len: None,
        }
    }

    /// Append received bytes.
    pub fn push(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    /// Bytes buffered but not yet yielded (partial frame).
    #[must_use]
    pub fn buffered(&self) -> usize {
        self.buf.len()
    }

    /// The next complete frame, if one is buffered.
    ///
    /// # Errors
    /// [`DecodeError::Oversize`] when a prefix exceeds the lane budget;
    /// the stream should be stopped, nothing else is readable from it.
    pub fn next_frame(&mut self) -> Result<Option<Bytes>, DecodeError> {
        if self.pending_len.is_none() {
            if self.buf.len() < PREFIX_BYTES {
                return Ok(None);
            }
            let mut prefix = [0u8; PREFIX_BYTES];
            prefix.copy_from_slice(&self.buf[..PREFIX_BYTES]);
            let announced = u32::from_le_bytes(prefix) as usize;
            if announced > self.max_frame {
                return Err(DecodeError::Oversize {
                    announced,
                    max: self.max_frame,
                });
            }
            self.buf.advance(PREFIX_BYTES);
            self.pending_len = Some(announced);
        }
        let len = self.pending_len.unwrap_or(0);
        if self.buf.len() < len {
            return Ok(None);
        }
        self.pending_len = None;
        Ok(Some(self.buf.split_to(len).freeze()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_prefixes_little_endian_length() {
        let frame = encode(b"abc");
        assert_eq!(&frame[..], &[3, 0, 0, 0, b'a', b'b', b'c']);
    }

    #[test]
    fn decoder_should_yield_whole_frames_only() {
        let mut d = Decoder::new(64);
        let frame = encode(b"hello");
        d.push(&frame[..3]);
        assert_eq!(d.next_frame(), Ok(None));
        d.push(&frame[3..7]);
        assert_eq!(d.next_frame(), Ok(None));
        d.push(&frame[7..]);
        assert_eq!(d.next_frame().unwrap().as_deref(), Some(&b"hello"[..]));
        assert_eq!(d.next_frame(), Ok(None));
    }

    #[test]
    fn decoder_should_split_back_to_back_frames() {
        let mut d = Decoder::new(64);
        let mut bytes = encode(b"one").to_vec();
        bytes.extend_from_slice(&encode(b"two"));
        bytes.extend_from_slice(&encode(b""));
        d.push(&bytes);
        assert_eq!(d.next_frame().unwrap().as_deref(), Some(&b"one"[..]));
        assert_eq!(d.next_frame().unwrap().as_deref(), Some(&b"two"[..]));
        assert_eq!(d.next_frame().unwrap().as_deref(), Some(&b""[..]));
        assert_eq!(d.next_frame(), Ok(None));
    }

    #[test]
    fn decoder_should_reject_oversize_prefix_before_buffering_it() {
        let mut d = Decoder::new(8);
        d.push(&encode(&[0u8; 9]));
        assert_eq!(
            d.next_frame(),
            Err(DecodeError::Oversize {
                announced: 9,
                max: 8
            })
        );
    }

    #[test]
    fn empty_frame_roundtrips() {
        let mut d = Decoder::new(8);
        d.push(&encode(b""));
        assert_eq!(d.next_frame().unwrap().as_deref(), Some(&b""[..]));
    }
}
