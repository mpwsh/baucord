//! Blob uploads take turns.
//!
//! The server holds one announced upload a session (spec §7): a second
//! `UploadBegin` replaces the first, and the first's chunks are then
//! refused (`UploadRejectReason::NotAnnounced`). Two files dropped on
//! the window together were announced back to back, and the first one
//! failed (review P1-17's first symptom, end to end). So only one is
//! with the server at a time: the rest wait here, unannounced, in the
//! order they were asked for, and each is announced when the one
//! before it is over — stored, refused, already known, or cancelled.
//! Every server ever shipped works this way, so the turn is taken on
//! this side.

use std::collections::VecDeque;

/// A blob waiting for its turn.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct QueuedBlob {
    pub upload_id: u32,
    pub name: String,
    pub mime: String,
    pub data: Vec<u8>,
}

/// Whose turn it is, and who waits.
#[derive(Debug, Default)]
pub(crate) struct UploadTurns {
    /// The upload the server has our announcement for.
    current: Option<u32>,
    waiting: VecDeque<QueuedBlob>,
}

impl UploadTurns {
    /// A new upload: handed back to announce now, or kept for its turn.
    pub fn ask(&mut self, blob: QueuedBlob) -> Option<QueuedBlob> {
        if self.current.is_some() {
            self.waiting.push_back(blob);
            return None;
        }
        self.current = Some(blob.upload_id);
        Some(blob)
    }

    /// `upload_id` is over. If it held the turn, the next one to
    /// announce — whose turn it now is. An upload that is not the
    /// current one (a late answer about a cancelled one) changes
    /// nothing.
    pub fn over(&mut self, upload_id: u32) -> Option<QueuedBlob> {
        if self.current != Some(upload_id) {
            return None;
        }
        let next = self.waiting.pop_front();
        self.current = next.as_ref().map(|b| b.upload_id);
        next
    }

    /// Drop an upload that never got its turn. Whether it was waiting.
    pub fn forget(&mut self, upload_id: u32) -> bool {
        let before = self.waiting.len();
        self.waiting.retain(|b| b.upload_id != upload_id);
        self.waiting.len() != before
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blob(upload_id: u32) -> QueuedBlob {
        QueuedBlob {
            upload_id,
            name: format!("file-{upload_id}"),
            mime: String::new(),
            data: vec![0; 4],
        }
    }

    #[test]
    fn the_first_goes_at_once_and_the_rest_in_order() {
        let mut turns = UploadTurns::default();
        assert_eq!(turns.ask(blob(1)), Some(blob(1)));
        assert_eq!(turns.ask(blob(2)), None);
        assert_eq!(turns.ask(blob(3)), None);
        assert_eq!(turns.over(1), Some(blob(2)));
        assert_eq!(turns.over(2), Some(blob(3)));
        assert_eq!(turns.over(3), None);
        // Free again: the next asks and goes.
        assert_eq!(turns.ask(blob(4)), Some(blob(4)));
    }

    #[test]
    fn only_the_one_whose_turn_it_is_ends_the_turn() {
        let mut turns = UploadTurns::default();
        turns.ask(blob(1));
        turns.ask(blob(2));
        // A late refusal of something cancelled long ago, or the
        // answer about one still waiting: the turn stays.
        assert_eq!(turns.over(9), None);
        assert_eq!(turns.over(2), None);
        assert_eq!(turns.over(1), Some(blob(2)));
        // Said twice (a refusal, then our own cancel): once counts.
        assert_eq!(turns.over(1), None);
        assert_eq!(turns.over(2), None);
    }

    #[test]
    fn one_cancelled_while_waiting_never_goes() {
        let mut turns = UploadTurns::default();
        turns.ask(blob(1));
        turns.ask(blob(2));
        turns.ask(blob(3));
        assert!(turns.forget(2));
        assert!(!turns.forget(2));
        // The one in flight is not "waiting".
        assert!(!turns.forget(1));
        assert_eq!(turns.over(1), Some(blob(3)));
    }
}
