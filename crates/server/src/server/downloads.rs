//! Streamed downloads (1.5, feature bit `LARGE_BLOBS`): a blob of any
//! size, in pieces, resumable.
//!
//! `FetchBlob` reads a whole file into memory and sends it on one paced
//! stream, which is right for a picture and wrong for an hour of WAV.
//! Here the persistence thread reads one [`PIECE`] at a time, the piece
//! goes out as `BlobPart`s on a paced Media stream, and the next is read
//! once that stream has been handed to QUIC — so a download holds one
//! piece in memory, whatever the file's size. The client writes each
//! part at its offset and checks the SHA-256 at the end; a broken
//! connection resumes from what it has (`FetchBlobFrom { offset }`).

use std::time::Duration;

use log::{debug, warn};
use protocol::{BlobId, ClientId, ServerMessage, IMAGE_CHUNK_BYTES};
use transport::UploadId;

use super::{ChatServer, Waiting};
use crate::persistence::{Answer, Outcome, Query};

/// Bytes read from disk at a time for one download.
pub(super) const PIECE: usize = 256 * 1024;
/// How long one piece may take to read.
const PIECE_DEADLINE: Duration = Duration::from_secs(5);

/// One download in progress.
#[derive(Debug, Clone, Copy)]
pub(super) struct Download {
    /// The next byte to read.
    next: u64,
    /// The file's size, once known.
    total: Option<u64>,
    /// The stream carrying the last piece.
    inflight: Option<UploadId>,
    /// A piece is being read.
    reading: bool,
}

impl ChatServer {
    /// `FetchBlobFrom`.
    pub(super) fn handle_fetch_blob_from(
        &mut self,
        client: ClientId,
        blob_id: BlobId,
        offset: u64,
    ) {
        if !self.may_fetch(client, &blob_id) {
            self.send_msg(client, &ServerMessage::BlobMissing { blob_id });
            return;
        }
        // A blob held in memory (an avatar, or anything on a server
        // without persistence) goes in one piece.
        if let Some(bytes) = self.blob_bytes.get(&blob_id).cloned() {
            let from = usize::try_from(offset)
                .unwrap_or(usize::MAX)
                .min(bytes.len());
            self.send_parts(client, blob_id, offset, &bytes[from..], bytes.len() as u64);
            return;
        }
        if !self.policy.blobs.contains_key(&blob_id) || !self.persist.is_persistent() {
            self.send_msg(client, &ServerMessage::BlobMissing { blob_id });
            return;
        }
        self.downloads.insert(
            (client, blob_id),
            Download {
                next: offset,
                total: None,
                inflight: None,
                reading: false,
            },
        );
        self.pump_downloads();
    }

    /// Every tick while a download runs: read the next piece of each
    /// whose last one has left.
    pub(super) fn pump_downloads(&mut self) {
        if self.downloads.is_empty() {
            return;
        }
        let keys: Vec<(ClientId, BlobId)> = self.downloads.keys().copied().collect();
        for key in keys {
            let (client, blob_id) = key;
            let Some(d) = self.downloads.get(&key).copied() else {
                continue;
            };
            if d.reading {
                continue;
            }
            if let Some(upload) = d.inflight
                && self
                    .driver
                    .upload_progress(client, upload)
                    .is_some_and(|(written, total)| written < total)
            {
                continue;
            }
            if d.total.is_some_and(|t| d.next >= t) {
                self.downloads.remove(&key);
                continue;
            }
            let ask = self.persist.ask(
                Query::BlobRange {
                    blob_id,
                    offset: d.next,
                    len: PIECE,
                },
                self.now + PIECE_DEADLINE,
            );
            self.waiting
                .insert(ask, Waiting::BlobRange { client, blob_id });
            if let Some(d) = self.downloads.get_mut(&key) {
                d.reading = true;
            }
        }
    }

    /// A piece was read (or not).
    pub(super) fn resolve_blob_range(
        &mut self,
        client: ClientId,
        blob_id: BlobId,
        outcome: Outcome,
    ) {
        let key = (client, blob_id);
        let Some(d) = self.downloads.get(&key).copied() else {
            return;
        };
        match outcome {
            Outcome::Answered(Answer::BlobRange(Some((bytes, total)))) => {
                let offset = d.next;
                let upload = self.send_parts(client, blob_id, offset, &bytes, total);
                let next = offset + bytes.len() as u64;
                if upload.is_none() || bytes.is_empty() || next >= total {
                    self.downloads.remove(&key);
                    return;
                }
                if let Some(d) = self.downloads.get_mut(&key) {
                    d.next = next;
                    d.total = Some(total);
                    d.inflight = upload;
                    d.reading = false;
                }
            }
            Outcome::Answered(Answer::Failed(e)) => {
                warn!("download read failed: {e}");
                self.downloads.remove(&key);
                self.send_msg(client, &ServerMessage::BlobMissing { blob_id });
            }
            _ => {
                self.downloads.remove(&key);
                self.send_msg(client, &ServerMessage::BlobMissing { blob_id });
            }
        }
    }

    /// `body` (the blob's bytes from `offset` on) as `BlobPart`s on
    /// one paced Media stream.
    fn send_parts(
        &mut self,
        client: ClientId,
        blob_id: BlobId,
        offset: u64,
        body: &[u8],
        total: u64,
    ) -> Option<UploadId> {
        let base = offset;
        let mut frames = Vec::with_capacity(body.len().div_ceil(IMAGE_CHUNK_BYTES).max(1));
        let mut at = base;
        for chunk in body.chunks(IMAGE_CHUNK_BYTES) {
            let msg = ServerMessage::BlobPart {
                blob_id,
                offset: at,
                total,
                data: chunk.to_vec(),
            };
            let Some(encoded) = protocol::encode_server_message(&msg) else {
                warn!("blob part failed to encode; dropping the download");
                return None;
            };
            frames.push(protocol::Bytes::from(encoded));
            at += chunk.len() as u64;
        }
        if frames.is_empty() {
            // Nothing past the offset: say where the end is.
            let msg = ServerMessage::BlobPart {
                blob_id,
                offset: total,
                total,
                data: Vec::new(),
            };
            frames.push(protocol::Bytes::from(protocol::encode_server_message(
                &msg,
            )?));
        }
        self.stats.bytes_sent += body.len() as u64;
        match self.driver.send_media(self.now, client, frames) {
            Ok(id) => Some(id),
            Err(e) => {
                debug!("download to {client:#x} failed: {e}");
                None
            }
        }
    }

    /// A session ended: its downloads go with it.
    pub(super) fn forget_downloads(&mut self, client: ClientId) {
        self.downloads.retain(|(c, _), _| *c != client);
    }
}
