//! Relay-side voice receiver report: per-sender sequence-gap tracking.
//!
//! A transport's loss figure is ack accounting over *every* packet a
//! client sends — acks, keepalives, DTX refreshes, voice alike. The
//! 2026-09-01 incident had clients reading 60–90 % "uplink loss" from
//! such a figure while their voice arrived intelligibly and the relay
//! host dropped nothing, so no transport counter can by itself say
//! whether a sender's *voice* is getting through. The relay can: it sees
//! every voice packet and the sender's monotonic sequence number, so
//! counting the holes is a direct measurement of what listeners miss —
//! the RTCP receiver-report idea, computed where the packets are.
//!
//! The per-window numbers ride back to the sender every two seconds as
//! [`protocol::ServerMessage::VoiceReceiverReport`] and are the *only*
//! loss input to its audio rate ladder (spec §6.1).

use std::collections::HashMap;

use protocol::ClientId;

/// A forward jump larger than this is treated as the sender restarting
/// its sequence (reconnect, encoder restart), not as that many lost
/// packets. Voice runs at 25–50 packets/s, so this is ~20–40 s of
/// silence-free streaming — no real gap is that long without the
/// connection itself timing out first.
const RESYNC_THRESHOLD: u32 = 1_000;

/// What arrived from one sender since the last report.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct VoiceWindow {
    /// Packets that arrived, in any order.
    pub received: u32,
    /// Packets the sequence numbers say were sent (highest seen minus
    /// the previous highest, plus one per resync).
    pub expected: u32,
    /// Arrivals at or below the highest sequence already seen:
    /// reordered or duplicated packets. They count toward `received`,
    /// so a late arrival fills the gap it was counted against.
    pub late: u32,
    /// Forward jumps past [`RESYNC_THRESHOLD`], counted instead of
    /// being read as loss.
    pub resyncs: u32,
}

impl VoiceWindow {
    /// Packets that never arrived. Saturating: duplicates can push
    /// `received` past `expected`, and that is not negative loss.
    #[must_use]
    pub fn lost(&self) -> u32 {
        self.expected.saturating_sub(self.received)
    }

    /// Loss as a percentage of `expected`; zero for an empty window.
    #[must_use]
    pub fn loss_pct(&self) -> f64 {
        if self.expected == 0 {
            return 0.0;
        }
        f64::from(self.lost()) * 100.0 / f64::from(self.expected)
    }
}

#[derive(Debug, Default)]
struct SenderSeq {
    /// Highest sequence seen so far, kept across windows so a gap that
    /// straddles a report boundary is still counted.
    last_seq: Option<u32>,
    window: VoiceWindow,
}

/// Per-sender sequence tracker for the voice relay.
#[derive(Debug, Default)]
pub struct VoiceSeqTracker {
    senders: HashMap<ClientId, SenderSeq>,
}

impl VoiceSeqTracker {
    /// Record one voice packet from `sender` carrying `seq`.
    pub fn note(&mut self, sender: ClientId, seq: u32) {
        let state = self.senders.entry(sender).or_default();
        state.window.received += 1;

        let Some(last) = state.last_seq else {
            state.last_seq = Some(seq);
            state.window.expected += 1;
            return;
        };

        // Wrapping arithmetic so a u32 rollover reads as a delta of 1,
        // and anything "behind" us lands in the top half of the range.
        let delta = seq.wrapping_sub(last);
        if delta == 0 || delta > u32::MAX / 2 {
            state.window.late += 1;
        } else if delta > RESYNC_THRESHOLD {
            state.window.resyncs += 1;
            state.window.expected += 1;
            state.last_seq = Some(seq);
        } else {
            state.window.expected += delta;
            state.last_seq = Some(seq);
        }
    }

    /// The window accumulated since the previous call for `sender`,
    /// resetting the counters but keeping the sequence position.
    /// `None` if this sender has never sent voice.
    pub fn take_window(&mut self, sender: ClientId) -> Option<VoiceWindow> {
        let state = self.senders.get_mut(&sender)?;
        Some(std::mem::take(&mut state.window))
    }

    /// Forget a sender (on disconnect).
    pub fn remove(&mut self, sender: ClientId) {
        self.senders.remove(&sender);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SENDER: ClientId = 7;

    fn tracker_with(seqs: &[u32]) -> VoiceSeqTracker {
        let mut tracker = VoiceSeqTracker::default();
        for &seq in seqs {
            tracker.note(SENDER, seq);
        }
        tracker
    }

    fn window_of(seqs: &[u32]) -> VoiceWindow {
        tracker_with(seqs)
            .take_window(SENDER)
            .expect("sender has sent voice")
    }

    #[test]
    fn in_order_sequence_should_report_no_loss() {
        assert_eq!(window_of(&[10, 11, 12, 13]).lost(), 0);
    }

    #[test]
    fn gap_should_count_missing_packets_as_lost() {
        // 11 and 12 never arrived.
        assert_eq!(window_of(&[10, 13]).lost(), 2);
    }

    #[test]
    fn late_packet_should_fill_its_gap() {
        // 11 arrives after 12: reordered, not lost.
        let window = window_of(&[10, 12, 11]);
        assert_eq!((window.lost(), window.late), (0, 1));
    }

    #[test]
    fn duplicate_should_not_produce_negative_loss() {
        assert_eq!(window_of(&[10, 10, 10]).lost(), 0);
    }

    #[test]
    fn sequence_wraparound_should_not_read_as_a_gap() {
        assert_eq!(window_of(&[u32::MAX - 1, u32::MAX, 0, 1]).lost(), 0);
    }

    #[test]
    fn large_forward_jump_should_be_a_resync_not_loss() {
        let window = window_of(&[10, 10 + RESYNC_THRESHOLD + 1]);
        assert_eq!((window.lost(), window.resyncs), (0, 1));
    }

    #[test]
    fn take_window_should_reset_counters() {
        let mut tracker = tracker_with(&[10, 11]);
        let _ = tracker.take_window(SENDER);
        assert_eq!(tracker.take_window(SENDER), Some(VoiceWindow::default()));
    }

    #[test]
    fn gap_across_window_boundary_should_count_in_the_later_window() {
        let mut tracker = tracker_with(&[10, 11]);
        let _ = tracker.take_window(SENDER);
        tracker.note(SENDER, 14); // 12 and 13 lost between reports
        let later = tracker.take_window(SENDER).expect("sender known");
        assert_eq!(later.lost(), 2);
    }

    #[test]
    fn unknown_sender_should_have_no_window() {
        assert_eq!(VoiceSeqTracker::default().take_window(SENDER), None);
    }

    #[test]
    fn removed_sender_should_have_no_window() {
        let mut tracker = tracker_with(&[10, 11]);
        tracker.remove(SENDER);
        assert_eq!(tracker.take_window(SENDER), None);
    }

    #[test]
    fn loss_pct_should_be_zero_for_empty_window() {
        assert!((VoiceWindow::default().loss_pct() - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn loss_pct_should_be_share_of_expected() {
        // 10 expected, 8 received → 20 %.
        let window = window_of(&[1, 2, 3, 4, 5, 6, 7, 10]);
        assert!((window.loss_pct() - 20.0).abs() < 1e-9);
    }
}
