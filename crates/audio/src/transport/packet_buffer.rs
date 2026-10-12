//! Per-peer packet playout buffer: encoded Opus packets in, PCM out at pull.
//!
//! Replaces two things at once:
//!   - `decoder.rs` (arrival-time decode, fixed 30 ms reorder hold,
//!     `Decoder::new()` on gaps > 200 ms), and
//!   - `PeerBuffer::samples: VecDeque<f32>` in `jitter_buffer.rs`.
//!
//! Packets are held sorted by sequence number and decoded only when the
//! playhead reaches their slot. That is the point at which we actually
//! know whether the packet arrived, whether its FEC arrived, or whether
//! concealment is needed — Perkins ch. 6/9: hold frames until playout
//! time so recovery data has a chance to arrive. Concretely:
//!
//!   - "late" means *behind the playhead*, so the adaptive target in
//!     `AdaptiveJitterBuffer` is the one and only late-tolerance knob;
//!   - in-band FEC (LBRR in packet N+1) is used the moment slot N comes
//!     due, not 30 ms after N+4 arrived;
//!   - reordering costs nothing (sorted insert), and the
//!     `decoder_to_audio` channel + per-frame `Vec<f32>` allocation go away;
//!   - long outages jump the playhead instead of throwing decoder state away;
//!   - talk-spurt boundaries (seq contiguous, media clock jumped — DTX) are
//!     detected and used as the place to resync, never concealed into.
//!
//! What stays in `AdaptiveJitterBuffer` untouched: jitter estimation,
//! priming, deferred underrun classification, drift compensation, mix
//! normalisation, per-peer limiter, `adapt()`. Only the storage type and
//! the pull call change.
//!
use std::collections::VecDeque;

use opus::{Channels, Decoder};

use crate::{transport::packet::AudioPacket, FRAME_SIZE, OPUS_SAMPLE_RATE};

/// Largest single packet this buffer decodes: 40 ms at 48 kHz (the rate
/// controller's low-bitrate rungs use 40 ms frames). Bump to 2880 only if
/// 60 ms is ever allowed; every increase costs `MAX_FRAME * 4` bytes of
/// staging per peer.
pub const MAX_FRAME: usize = 1920;

/// Missing slots before a present packet beyond this are skipped (playhead
/// jumps forward) rather than concealed one by one. libopus PLC has faded
/// to silence well inside this; concealing further only delays the resume.
/// Replaces `decoder.rs` `MAX_GAP = 20` + `Decoder::new()`.
const MAX_PLC_SLOTS: u32 = 6;

/// Frames of PLC synthesised when the buffer is *empty* while the peer was
/// playing, before the caller's priming/underrun logic takes over. Only
/// used when the last real frame was loud: extrapolating a comfort-noise
/// refresh or the quiet tail before a DTX pause buys nothing (field log:
/// two silent peers' 400 ms refreshes produced ~7 "PLC" frames/s), while
/// a mid-word underrun gets 40 ms of real extrapolated speech instead of
/// a 2 ms decay tail.
const DRY_PLC_SLOTS: u32 = 2;

/// A packet this far *behind* the playhead is not late, it is a sender
/// that restarted its sequence space (crash, reconnect). Resync instead
/// of silently dropping everything for the next 30 s. 1000 slots = 20 s;
/// no real network delivers a packet 20 s late.
const RESTART_SLOTS: i32 = 1000;

/// Frames at or below this RMS may be dropped to shrink an over-deep buffer
/// mid-spurt — "vary the playout at times where error concealment is more
/// effective, for example during a period of relative quiet" (Perkins ch. 6).
/// ≈ −50 dBFS.
const QUIET_RMS: f32 = 0.003;

/// Outcome of offering a packet to the buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Push {
    Accepted,
    /// Its slot has already played or been concealed. Drop and count.
    Late,
    Duplicate,
}

/// What a `fill` call did, for the caller's stats and adaptation.
#[derive(Debug, Default, Clone, Copy)]
pub struct FillReport {
    /// Samples written to `out`. `< out.len()` means the buffer ran dry.
    pub filled: usize,
    /// Slots recovered from the following packet's LBRR.
    pub fec: u32,
    /// Slots concealed with PLC because a packet was missing at its slot.
    pub plc: u32,
    /// PLC frames produced because the buffer was empty (not a loss
    /// verdict — the caller's deferred underrun classification decides).
    pub dry_plc: u32,
    /// Slots skipped by a playhead jump (long outage or DTX resync).
    pub skipped: u32,
    /// Quiet frames dropped to shrink the buffer (`shrink == true`).
    pub dropped_quiet: u32,
}

struct Slot {
    seq: u32,
    /// 48 kHz media clock from the packet header.
    ts: u32,
    /// Payload moved straight out of `AudioPacket` — no extra allocation.
    data: Vec<u8>,
}

enum Refill {
    Real,
    Fec,
    Plc,
    DryPlc,
    Skipped(u32),
    Dry,
}

/// Wrapping sequence distance `a - b` as a signed value.
#[inline]
#[allow(clippy::cast_possible_wrap)] // the wrap IS the point
fn seq_dist(a: u32, b: u32) -> i32 {
    a.wrapping_sub(b) as i32
}

/// Duration in samples of an Opus packet, clamped to what we can stage.
fn frame_samples(data: &[u8]) -> usize {
    opus::packet::get_nb_samples(data, OPUS_SAMPLE_RATE)
        .map_or(FRAME_SIZE, |n| n.clamp(120, MAX_FRAME))
}

/// Whether the packet actually carries LBRR for the preceding frame.
/// `opus_packet_has_lbrr` is in libopus ≥ 1.5 and in opusic-sys 0.7.x but
/// not wrapped by opus-rs 0.4.0, hence the direct FFI call. Lets us pick
/// FEC vs PLC *before* decoding, and makes `fec_recoveries` honest.
fn packet_has_lbrr(data: &[u8]) -> bool {
    let Ok(len) = i32::try_from(data.len()) else {
        return false;
    };
    // SAFETY: read-only scan of a valid, initialised byte slice; libopus
    // does not retain the pointer.
    unsafe { opusic_sys::opus_packet_has_lbrr(data.as_ptr(), len) == 1 }
}

#[allow(clippy::cast_precision_loss)] // len ≤ MAX_FRAME
fn is_quiet(frame: &[f32]) -> bool {
    if frame.is_empty() {
        return true;
    }
    let sum_sq: f32 = frame.iter().map(|s| s * s).sum();
    (sum_sq / frame.len() as f32).sqrt() <= QUIET_RMS
}

/// A decoded frame whose loudest sample is at or below this (−80 dBFS)
/// is digital silence for every purpose here: a closed noise gate, a
/// muted microphone that still sends.
const SILENT_PEAK: f32 = 1e-4;

fn is_silent(frame: &[f32]) -> bool {
    frame.iter().all(|s| s.abs() <= SILENT_PEAK)
}

pub struct PacketBuffer {
    decoder: Decoder,
    /// Ascending by sequence (relative to the playhead). Tiny: a 200 ms
    /// ceiling is 10–20 packets, so a linear insert beats any map.
    slots: VecDeque<Slot>,
    /// Playhead: the slot we will produce next.
    started: bool,
    next_seq: u32,
    next_ts: u32,
    /// PLC frames spent since the last real frame while the buffer was empty.
    dry_plc_used: u32,
    /// Whether the last decoded real frame was at/below `QUIET_RMS`.
    /// Gates dry-PLC (nothing worth extrapolating after silence) and
    /// enables comfort-noise continuation instead.
    last_real_quiet: bool,
    /// The last decoded real frame was silence ([`SILENT_PEAK`]):
    /// continuing it is continuing nothing, so [`Self::comfort_noise`]
    /// does not run the decoder for it — it did, fifty times a second
    /// for every peer sitting quiet in the room (review P2-34).
    last_real_silent: bool,
    /// Duration of the last decoded real frame, for the caller's
    /// loss-protection floor.
    last_real_samples: usize,
    /// Decoded PCM for the current slot, consumed by `fill`.
    staging: [f32; MAX_FRAME],
    staged: usize,
    pos: usize,
}

impl PacketBuffer {
    pub fn new() -> opus::Result<Self> {
        Ok(Self {
            decoder: Decoder::new(OPUS_SAMPLE_RATE, Channels::Mono)?,
            slots: VecDeque::with_capacity(24),
            started: false,
            next_seq: 0,
            next_ts: 0,
            dry_plc_used: 0,
            last_real_quiet: true,
            last_real_silent: true,
            last_real_samples: FRAME_SIZE,
            staging: [0.0; MAX_FRAME],
            staged: 0,
            pos: 0,
        })
    }

    // ── Ingest ──────────────────────────────────────────────────────

    /// Whether [`Self::push`] would accept a packet numbered `seq`: not
    /// behind the playhead, not held already. Asked before a RED
    /// bundle's redundant block is copied out — nearly every one is of
    /// a packet that arrived (review P3-21).
    #[must_use]
    pub fn wants(&self, seq: u32) -> bool {
        if self.started {
            let behind = seq_dist(seq, self.next_seq);
            if behind < -RESTART_SLOTS {
                return true;
            }
            if behind < 0 {
                return false;
            }
        }
        !self.slots.iter().any(|s| s.seq == seq)
    }

    /// Offer a packet. Jitter estimation and underrun classification stay
    /// in the caller (they need `arrival`, which this buffer does not).
    pub fn push(&mut self, pkt: AudioPacket) -> Push {
        let seq = pkt.header.seq;

        if self.started {
            let behind = seq_dist(seq, self.next_seq);
            if behind < -RESTART_SLOTS {
                self.clear();
            } else if behind < 0 {
                return Push::Late;
            }
        }

        // Insertion point by signed distance from the playhead (or from
        // the current head before playout starts).
        let base = if self.started {
            self.next_seq
        } else {
            self.slots.front().map_or(seq, |s| s.seq)
        };
        let key = seq_dist(seq, base);
        let mut idx = self.slots.len();
        for (i, s) in self.slots.iter().enumerate() {
            let k = seq_dist(s.seq, base);
            if k == key {
                return Push::Duplicate;
            }
            if k > key {
                idx = i;
                break;
            }
        }
        self.slots.insert(
            idx,
            Slot {
                seq,
                ts: pkt.header.timestamp,
                data: pkt.opus_data,
            },
        );
        Push::Accepted
    }

    // ── Depth ───────────────────────────────────────────────────────

    /// Nothing buffered and nothing staged.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty() && self.pos >= self.staged
    }

    /// Media time (samples) between the playhead and the end of the newest
    /// packet, plus staged PCM not yet consumed. Missing slots inside that
    /// span count — they will be concealed and occupy playout time. A DTX
    /// pause between the playhead and the head does not count.
    ///
    /// This is the drop-in for every `peer.samples.len()` in
    /// `jitter_buffer.rs` (priming, headroom, hard cap).
    #[must_use]
    pub fn buffered_media(&self) -> usize {
        let staged_left = self.staged.saturating_sub(self.pos);
        let (Some(head), Some(last)) = (self.slots.front(), self.slots.back()) else {
            return staged_left;
        };
        let origin = if self.started && self.is_contiguous_with_playhead(head) {
            self.next_ts
        } else {
            head.ts
        };
        let end = last.ts.wrapping_add(frame_samples(&last.data) as u32);
        end.wrapping_sub(origin) as usize + staged_left
    }

    /// Whether the slots between the playhead and `head` are plain loss
    /// (media clock advanced by the missing packets' duration) rather
    /// than a DTX pause (media clock jumped further).
    fn is_contiguous_with_playhead(&self, head: &Slot) -> bool {
        let gap = u64::from(head.seq.wrapping_sub(self.next_seq));
        let media_gap = u64::from(head.ts.wrapping_sub(self.next_ts));
        media_gap <= gap * MAX_FRAME as u64
    }

    /// Emergency cap for a frozen output (mirrors the old `hard_cap`
    /// drain): drop the oldest packets until at most `max_media` samples
    /// remain, and move the playhead to the new head.
    pub fn truncate_to(&mut self, max_media: usize) {
        while self.buffered_media() > max_media && self.slots.len() > 1 {
            self.slots.pop_front();
            if let Some(head) = self.slots.front() {
                self.next_seq = head.seq;
                self.next_ts = head.ts;
            }
        }
    }

    /// Drop everything and forget the playhead (mute, or a peer reset).
    /// The next packet defines the new playout position.
    pub fn clear(&mut self) {
        self.slots.clear();
        self.started = false;
        self.dry_plc_used = 0;
        self.last_real_quiet = true;
        self.last_real_silent = true;
        self.staged = 0;
        self.pos = 0;
    }

    /// Comfort-noise continuation for a peer in a DTX pause.
    ///
    /// Opus DTX sends one refresh frame every 400 ms; between them the
    /// decoder is meant to keep being called with no packet, which
    /// extrapolates that frame's noise floor and lets it decay (RFC 7587
    /// §3.1.3 / libopus PLC) instead of playing a 20 ms blip every 400 ms
    /// with digital silence between. Applies only when the last real
    /// frame was quiet (a refresh, or a closed-gate frame) and the buffer
    /// is empty. Does **not** advance the playhead — the next real packet
    /// resyncs exactly as it would after silence. Returns `false` when not
    /// applicable (the caller then contributes nothing) — which includes
    /// a last frame that was silence: there is no floor to continue.
    pub fn comfort_noise(&mut self, out: &mut [f32]) -> bool {
        if !self.started || !self.last_real_quiet || self.last_real_silent || !self.slots.is_empty()
        {
            return false;
        }
        let mut filled = 0;
        while filled < out.len() {
            if self.pos >= self.staged {
                self.pos = 0;
                self.plc_into_staging(FRAME_SIZE);
            }
            let take = (self.staged - self.pos).min(out.len() - filled);
            out[filled..filled + take].copy_from_slice(&self.staging[self.pos..self.pos + take]);
            self.pos += take;
            filled += take;
        }
        true
    }

    // ── Playout ─────────────────────────────────────────────────────

    /// Fill `out` from the playhead, decoding/concealing as required.
    ///
    /// - `plc_on_dry`: synthesise up to `DRY_PLC_SLOTS` frames of PLC when
    ///   the buffer is empty (pass the peer's `playing` flag).
    /// - `shrink`: the caller has decided the buffer is sustainedly too
    ///   deep; drop at most one quiet frame this call to catch up.
    ///
    /// `report.filled < out.len()` is the "ran dry" signal.
    pub fn fill(&mut self, out: &mut [f32], plc_on_dry: bool, shrink: bool) -> FillReport {
        let mut rep = FillReport::default();
        let mut dropped_this_call = false;

        while rep.filled < out.len() {
            if self.pos >= self.staged {
                match self.refill(plc_on_dry) {
                    Refill::Dry => break,
                    Refill::Skipped(n) => {
                        rep.skipped += n;
                        continue;
                    }
                    Refill::Real => {
                        if shrink && !dropped_this_call && is_quiet(&self.staging[..self.staged]) {
                            // Adjust the playout point during quiet: discard
                            // this frame and go straight to the next one.
                            self.pos = self.staged;
                            dropped_this_call = true;
                            rep.dropped_quiet += 1;
                            continue;
                        }
                    }
                    Refill::Fec => rep.fec += 1,
                    Refill::Plc => rep.plc += 1,
                    Refill::DryPlc => rep.dry_plc += 1,
                }
            }
            let take = (self.staged - self.pos).min(out.len() - rep.filled);
            out[rep.filled..rep.filled + take]
                .copy_from_slice(&self.staging[self.pos..self.pos + take]);
            self.pos += take;
            rep.filled += take;
        }
        rep
    }

    /// Produce the next slot into `staging`. This is the decision tree the
    /// whole redesign exists for.
    fn refill(&mut self, plc_on_dry: bool) -> Refill {
        self.pos = 0;
        self.staged = 0;

        let Some(head) = self.slots.front() else {
            if plc_on_dry
                && self.started
                && !self.last_real_quiet
                && self.dry_plc_used < DRY_PLC_SLOTS
            {
                // Buffer empty mid-playout. Extrapolate rather than cut.
                // The playhead does NOT advance: if the packet turns up
                // late it is played (playout delay grew by the PLC we
                // inserted, which is exactly what an underrun should do);
                // if this was a DTX pause the next spurt resyncs anyway.
                self.dry_plc_used += 1;
                self.plc_into_staging(FRAME_SIZE);
                return Refill::DryPlc;
            }
            return Refill::Dry;
        };
        let head_seq = head.seq;
        let head_ts = head.ts;
        let head_dur = frame_samples(&head.data);

        if !self.started {
            self.started = true;
            self.next_seq = head_seq;
            self.next_ts = head_ts;
        }

        let gap = head_seq.wrapping_sub(self.next_seq); // ≥ 0: late ones never enter
        let media_gap = head_ts.wrapping_sub(self.next_ts);

        if gap == 0 {
            // In order. If the media clock jumped, this is the first packet
            // of a talk spurt after DTX — snap, nothing to conceal.
            if media_gap != 0 {
                self.next_ts = head_ts;
            }
            let Some(slot) = self.slots.pop_front() else {
                return Refill::Dry; // unreachable, keeps the borrow checker calm
            };
            return self.decode_real(&slot);
        }

        // gap ≥ 1: packets are missing in front of head.
        let contiguous = u64::from(media_gap) <= u64::from(gap) * MAX_FRAME as u64;
        if !contiguous {
            // The missing packets straddle a DTX pause: whatever was lost
            // was a spurt edge, and head's LBRR (if any) covers the DTX
            // frame before it, not speech. Resync at head.
            self.next_seq = head_seq;
            self.next_ts = head_ts;
            return Refill::Skipped(gap);
        }

        if gap > MAX_PLC_SLOTS {
            // Long outage: PLC only the last MAX_PLC_SLOTS (it has faded to
            // silence by then), jump the rest. Decoder state is kept — Opus
            // is designed to resume after loss; it is not designed to be
            // recreated mid-stream.
            // Land exactly MAX_PLC_SLOTS before head, measured from head
            // (not from where we were), so an irregular timestamp in the
            // gap cannot leave the remaining slots looking non-contiguous.
            let jump = gap - MAX_PLC_SLOTS;
            self.next_seq = head_seq.wrapping_sub(MAX_PLC_SLOTS);
            self.next_ts = head_ts.wrapping_sub(MAX_PLC_SLOTS * head_dur as u32);
            return Refill::Skipped(jump);
        }

        if gap == 1 && packet_has_lbrr(&self.slots[0].data) {
            // head's LBRR is exactly the frame we are missing. frame_size
            // must be the missing duration (docs: "exactly the duration of
            // audio that is missing"), assumed equal to head's.
            let n = self
                .decoder
                .decode_float(&self.slots[0].data, &mut self.staging[..head_dur], true)
                .unwrap_or(0);
            if n > 0 {
                self.staged = n;
                self.advance(n);
                return Refill::Fec;
            }
            // fall through to PLC
        }

        // One missing slot, no usable LBRR (or an earlier slot of a
        // multi-packet gap — the last one gets the FEC attempt above).
        self.plc_into_staging(head_dur);
        self.advance(head_dur);
        Refill::Plc
    }

    fn decode_real(&mut self, slot: &Slot) -> Refill {
        match self
            .decoder
            .decode_float(&slot.data, &mut self.staging, false)
        {
            Ok(n) if n > 0 => {
                self.staged = n;
                self.dry_plc_used = 0;
                self.last_real_quiet = is_quiet(&self.staging[..n]);
                self.last_real_silent = is_silent(&self.staging[..n]);
                self.last_real_samples = n;
                self.next_seq = slot.seq.wrapping_add(1);
                self.next_ts = slot.ts.wrapping_add(n as u32);
                Refill::Real
            }
            _ => {
                // Corrupt or oversize payload: conceal its slot, keep timeline.
                let dur = frame_samples(&slot.data);
                self.plc_into_staging(dur);
                self.next_seq = slot.seq.wrapping_add(1);
                self.next_ts = slot.ts.wrapping_add(dur as u32);
                Refill::Plc
            }
        }
    }

    /// PLC exactly `dur` samples (must be a multiple of 2.5 ms — every
    /// Opus frame size is). Zeros on the (near-impossible) decoder error so
    /// the timeline never stalls.
    fn plc_into_staging(&mut self, dur: usize) {
        let dur = dur.min(MAX_FRAME);
        match self
            .decoder
            .decode_float(&[], &mut self.staging[..dur], false)
        {
            Ok(n) if n > 0 => self.staged = n,
            _ => {
                self.staging[..dur].fill(0.0);
                self.staged = dur;
            }
        }
    }

    #[inline]
    fn advance(&mut self, samples: usize) {
        self.next_seq = self.next_seq.wrapping_add(1);
        self.next_ts = self.next_ts.wrapping_add(samples as u32);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::packet::AudioPacket;

    fn enc() -> opus::Encoder {
        let mut e = opus::Encoder::new(48_000, Channels::Mono, opus::Application::Voip).unwrap();
        e.set_inband_fec(true).unwrap();
        e.set_packet_loss_perc(20).unwrap(); // force LBRR on
        e
    }

    fn pkt(e: &mut opus::Encoder, seq: u32, ts: u32) -> AudioPacket {
        let pcm: Vec<f32> = (0..FRAME_SIZE)
            .map(|i| {
                ((ts as usize + i) as f32 * 220.0 * std::f32::consts::TAU / 48_000.0).sin() * 0.3
            })
            .collect();
        let mut buf = vec![0u8; 512];
        let n = e.encode_float(&pcm, &mut buf).unwrap();
        AudioPacket::new(seq, ts, 1, buf[..n].to_vec())
    }

    #[test]
    fn reordered_pair_plays_in_order_with_no_concealment() {
        let mut e = enc();
        let mut b = PacketBuffer::new().unwrap();
        let p0 = pkt(&mut e, 0, 0);
        let p1 = pkt(&mut e, 1, FRAME_SIZE as u32);
        let p2 = pkt(&mut e, 2, 2 * FRAME_SIZE as u32);
        assert_eq!(b.push(p0), Push::Accepted);
        assert_eq!(b.push(p2), Push::Accepted);
        assert_eq!(b.push(p1), Push::Accepted);
        let mut out = [0.0f32; FRAME_SIZE * 3];
        let r = b.fill(&mut out, true, false);
        assert_eq!(r.filled, FRAME_SIZE * 3);
        assert_eq!((r.fec, r.plc, r.skipped), (0, 0, 0));
    }

    #[test]
    fn single_loss_is_recovered_with_fec_when_successor_present() {
        let mut e = enc();
        let mut b = PacketBuffer::new().unwrap();
        // Warm the encoder so LBRR is actually coded.
        for s in 0..10 {
            b.push(pkt(&mut e, s, s * FRAME_SIZE as u32));
        }
        let mut out = [0.0f32; FRAME_SIZE * 10];
        b.fill(&mut out, true, false);
        let _lost = pkt(&mut e, 10, 10 * FRAME_SIZE as u32); // never pushed
        b.push(pkt(&mut e, 11, 11 * FRAME_SIZE as u32));
        let mut out = [0.0f32; FRAME_SIZE * 2];
        let r = b.fill(&mut out, true, false);
        assert_eq!(r.filled, FRAME_SIZE * 2);
        assert_eq!(r.fec + r.plc, 1, "exactly one concealed slot");
    }

    #[test]
    fn dry_plc_only_follows_a_loud_frame() {
        let mut e = enc();
        let mut b = PacketBuffer::new().unwrap();
        // Digital silence (a DTX refresh / closed-gate frame): no tail.
        let silent = AudioPacket::new(0, 0, 1, {
            let mut buf = vec![0u8; 512];
            let n = e.encode_float(&[0.0f32; FRAME_SIZE], &mut buf).unwrap();
            buf[..n].to_vec()
        });
        b.push(silent);
        let mut out = vec![0.0f32; FRAME_SIZE * 3];
        let r = b.fill(&mut out, true, false);
        assert_eq!(r.dry_plc, 0, "quiet frame must not be extrapolated");
        assert_eq!(r.filled, FRAME_SIZE);
        // A loud frame mid-stream: extrapolate up to DRY_PLC_SLOTS.
        b.push(pkt(&mut e, 1, FRAME_SIZE as u32));
        let r = b.fill(&mut out, true, false);
        assert_eq!(r.dry_plc, DRY_PLC_SLOTS);
        assert_eq!(r.filled, FRAME_SIZE * 3);
    }

    /// Comfort noise continues a quiet floor — not silence: after a
    /// frame that decoded to nothing the decoder is not run at all,
    /// where it produced 20 ms of zeros fifty times a second for every
    /// peer sitting quiet (review P2-34).
    #[test]
    fn comfort_noise_does_not_continue_silence() {
        let mut e = enc();
        let frame = |e: &mut opus::Encoder, seq: u32, level: f32| {
            let pcm: Vec<f32> = (0..FRAME_SIZE)
                .map(|i| ((i * 7919 + seq as usize * 31) % 200) as f32 / 100.0 - 1.0)
                .map(|noise| noise * level)
                .collect();
            let mut buf = vec![0u8; 512];
            let n = e.encode_float(&pcm, &mut buf).unwrap();
            AudioPacket::new(seq, seq * FRAME_SIZE as u32, 1, buf[..n].to_vec())
        };
        let mut out = vec![0.0f32; FRAME_SIZE];

        // Digital silence, played out: nothing to continue.
        let mut b = PacketBuffer::new().unwrap();
        b.push(frame(&mut e, 0, 0.0));
        b.fill(&mut out, false, false);
        assert!(b.last_real_quiet && b.last_real_silent);
        assert!(!b.comfort_noise(&mut out));

        // A faint room floor (under the quiet line, over silence): kept
        // going between refreshes, as before.
        let mut e = enc();
        let mut b = PacketBuffer::new().unwrap();
        for seq in 0..20 {
            b.push(frame(&mut e, seq, 0.004));
        }
        let mut played = vec![0.0f32; FRAME_SIZE * 20];
        b.fill(&mut played, false, false);
        assert!(
            b.last_real_quiet && !b.last_real_silent,
            "a floor, not silence"
        );
        assert!(b.comfort_noise(&mut out));
    }

    /// `wants` answers as `push` would, without taking the packet
    /// (review P3-21: asked before a redundant copy is made).
    #[test]
    fn wants_agrees_with_push() {
        let mut e = enc();
        let mut b = PacketBuffer::new().unwrap();
        assert!(b.wants(5), "anything, before the first packet");
        b.push(pkt(&mut e, 5, 5 * FRAME_SIZE as u32));
        assert!(!b.wants(5), "held already");
        assert!(b.wants(4) && b.wants(7), "not played yet, not held");
        let mut out = [0.0f32; FRAME_SIZE];
        b.fill(&mut out, true, false);
        assert!(!b.wants(4) && !b.wants(5), "behind the playhead");
        assert_eq!(b.push(pkt(&mut e, 4, 4 * FRAME_SIZE as u32)), Push::Late);
        assert!(b.wants(6));
        assert_eq!(
            b.push(pkt(&mut e, 6, 6 * FRAME_SIZE as u32)),
            Push::Accepted
        );
        assert!(!b.wants(6));
        assert_eq!(
            b.push(pkt(&mut e, 6, 6 * FRAME_SIZE as u32)),
            Push::Duplicate
        );
    }

    #[test]
    fn packet_behind_playhead_is_late() {
        let mut e = enc();
        let mut b = PacketBuffer::new().unwrap();
        b.push(pkt(&mut e, 5, 5 * FRAME_SIZE as u32));
        let mut out = [0.0f32; FRAME_SIZE];
        b.fill(&mut out, true, false);
        assert_eq!(b.push(pkt(&mut e, 4, 4 * FRAME_SIZE as u32)), Push::Late);
    }

    #[test]
    fn dtx_boundary_is_not_concealed() {
        let mut e = enc();
        let mut b = PacketBuffer::new().unwrap();
        b.push(pkt(&mut e, 0, 0));
        let mut out = [0.0f32; FRAME_SIZE];
        b.fill(&mut out, true, false);
        // seq contiguous, media clock jumped 2 s: talk-spurt start.
        b.push(pkt(&mut e, 1, 96_000));
        let r = b.fill(&mut out, true, false);
        assert_eq!(r.filled, FRAME_SIZE);
        assert_eq!((r.fec, r.plc, r.skipped), (0, 0, 0));
    }

    #[test]
    fn long_gap_jumps_instead_of_concealing_everything() {
        let mut e = enc();
        let mut b = PacketBuffer::new().unwrap();
        b.push(pkt(&mut e, 0, 0));
        let mut out = [0.0f32; FRAME_SIZE];
        b.fill(&mut out, true, false);
        b.push(pkt(&mut e, 30, 30 * FRAME_SIZE as u32)); // 29 missing, media-contiguous
        let mut out = vec![0.0f32; FRAME_SIZE * 8];
        let r = b.fill(&mut out, false, false); // no dry-PLC: count exact frames
        assert_eq!(r.skipped, 29 - MAX_PLC_SLOTS);
        assert_eq!(r.fec + r.plc, MAX_PLC_SLOTS);
        assert_eq!(r.filled, FRAME_SIZE * 7); // 6 concealed + 1 real, then dry
    }
}
