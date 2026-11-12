//! The recorder's mixer: voice packets in, one mono timeline out.
//!
//! Sans-IO, like the transport: the caller says where on the timeline
//! each packet arrived (in output samples since the recording began)
//! and how far the timeline may be written; nothing here reads a clock.
//! That is what lets a four-hour recording be tested in milliseconds.
//!
//! Per speaker, packets wait up to [`REORDER`] for their turn by
//! sequence number. A hole is repaired from a redundant copy in a RED
//! bundle, else from Opus in-band FEC carried by the next packet, else
//! by up to [`MAX_CONCEALED`] frames of concealment, else left silent.
//! A talk spurt is placed where its first packet arrived; within it,
//! packets follow the sender's 48 kHz timestamps, pulled back by a
//! frame whenever the sender's clock has drifted [`DRIFT`] from arrival
//! time (a bot that never pauses). Speakers are summed, limited, and
//! handed out [`HOLD`] behind the newest arrival, so memory stays
//! constant however long the recording runs.

use std::collections::{BTreeMap, HashMap, VecDeque};

use protocol::{parse_voice, ClientId};

/// How long a packet waits for those before it.
pub const REORDER_MS: u64 = 400;
/// How far behind the newest arrival the timeline is written.
pub const HOLD_MS: u64 = 1_000;
/// Drift from arrival time that moves a spurt by one frame.
pub const DRIFT_MS: u64 = 60;
/// A pause in a sender's timestamps this long starts a new spurt.
const SPURT_GAP_MS: u64 = 200;
/// Frames of concealment before a hole is left silent.
pub const MAX_CONCEALED: usize = 3;
/// A forward jump in sequence numbers past this is a restarted
/// sender, not a loss (as the relay's receiver report reads it).
const RESYNC: u64 = 1_000;

/// What the mixer did, for the log.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct MixStats {
    pub packets: u64,
    pub malformed: u64,
    pub repaired_red: u64,
    pub repaired_fec: u64,
    pub concealed: u64,
    pub lost: u64,
    /// Arrived after its place on the timeline was written.
    pub too_late: u64,
}

struct Held {
    ts: u64,
    arrival: u64,
    payload: Vec<u8>,
    red: bool,
}

struct Speaker {
    decoder: opus::Decoder,
    held: BTreeMap<u64, Held>,
    /// Last sequence number seen, raw and extended to 64 bits.
    seq_raw: u32,
    seq_ext: u64,
    ts_raw: u32,
    ts_ext: u64,
    seen: bool,
    next: Option<u64>,
    /// (extended timestamp, output position) the spurt is laid from.
    anchor: Option<(u64, u64)>,
    /// Extended 48 kHz timestamp where the last frame placed ended.
    ts_end: u64,
    /// The last frame's length, output samples.
    frame: usize,
    concealed_run: usize,
}

fn extend(raw: u32, last_raw: u32, last_ext: u64) -> u64 {
    let delta = i64::from(raw.wrapping_sub(last_raw) as i32);
    last_ext.saturating_add_signed(delta)
}

/// One recording's timeline.
pub struct Mixer {
    rate: u32,
    speakers: HashMap<ClientId, Speaker>,
    buf: VecDeque<i32>,
    buf_start: u64,
    newest: u64,
    scratch: Vec<i16>,
    pub stats: MixStats,
}

impl Mixer {
    /// A mixer writing `rate` samples a second: 48, 24 or 16 kHz, which
    /// Opus decodes to directly.
    #[must_use]
    pub fn new(rate: u32) -> Self {
        Self {
            rate,
            speakers: HashMap::new(),
            buf: VecDeque::new(),
            buf_start: 0,
            newest: 0,
            // 120 ms, the longest Opus frame.
            scratch: vec![0; rate as usize * 120 / 1000],
            stats: MixStats::default(),
        }
    }

    /// Milliseconds as timeline samples.
    #[must_use]
    pub fn samples(&self, ms: u64) -> u64 {
        ms * u64::from(self.rate) / 1000
    }

    /// A voice packet from `sender` (the relay's bytes, without the
    /// sender envelope) that arrived at timeline position `arrival`.
    pub fn push(&mut self, sender: ClientId, arrival: u64, packet: &[u8]) {
        let Some((header, payload)) = parse_voice(packet) else {
            self.stats.malformed += 1;
            return;
        };
        self.stats.packets += 1;
        self.newest = self.newest.max(arrival);
        let rate = self.rate;
        let speaker = match self.speakers.entry(sender) {
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::hash_map::Entry::Vacant(v) => {
                let Ok(decoder) = opus::Decoder::new(rate, opus::Channels::Mono) else {
                    return;
                };
                v.insert(Speaker {
                    decoder,
                    held: BTreeMap::new(),
                    seq_raw: header.seq,
                    seq_ext: u64::from(header.seq) + (1 << 32),
                    ts_raw: header.timestamp,
                    ts_ext: u64::from(header.timestamp) + (1 << 32),
                    seen: false,
                    next: None,
                    anchor: None,
                    ts_end: 0,
                    frame: rate as usize / 50,
                    concealed_run: 0,
                })
            }
        };
        let (seq, ts) = if speaker.seen {
            (
                extend(header.seq, speaker.seq_raw, speaker.seq_ext),
                extend(header.timestamp, speaker.ts_raw, speaker.ts_ext),
            )
        } else {
            speaker.seen = true;
            (speaker.seq_ext, speaker.ts_ext)
        };
        if seq > speaker.seq_ext {
            speaker.seq_raw = header.seq;
            speaker.seq_ext = seq;
        }
        if ts > speaker.ts_ext {
            speaker.ts_raw = header.timestamp;
            speaker.ts_ext = ts;
        }
        // A restarted sender: whatever waits is from the old stream.
        let restarted = speaker.next.is_some_and(|n| seq > n + RESYNC);
        if restarted {
            let old = std::mem::take(&mut speaker.held);
            for (key, h) in old {
                self.place_packet(sender, key, &h, None);
            }
            if let Some(sp) = self.speakers.get_mut(&sender) {
                sp.next = None;
                sp.anchor = None;
            }
        }
        let Some(speaker) = self.speakers.get_mut(&sender) else {
            return;
        };
        if speaker.next.is_some_and(|n| seq < n) {
            // A duplicate, or later than its turn: already concealed.
            self.stats.too_late += 1;
            return;
        }
        speaker.held.insert(
            seq,
            Held {
                ts,
                arrival,
                payload: payload.to_vec(),
                red: header.is_red(),
            },
        );
        self.release(sender, arrival);
    }

    /// Place every packet of `sender` whose turn has come at `now`.
    fn release(&mut self, sender: ClientId, now: u64) {
        let reorder = self.samples(REORDER_MS);
        loop {
            let Some(speaker) = self.speakers.get_mut(&sender) else {
                return;
            };
            let Some((&seq, first)) = speaker.held.iter().next() else {
                return;
            };
            let in_turn = speaker.next.is_none_or(|n| seq == n);
            if !in_turn && now.saturating_sub(first.arrival) < reorder {
                return;
            }
            let gap_from = speaker.next.filter(|n| seq > *n);
            let held = speaker.held.remove(&seq).expect("just seen");
            self.place_packet(sender, seq, &held, gap_from);
        }
    }

    /// Decode one packet onto the timeline, repairing the frames from
    /// `gap_from` up to it first.
    fn place_packet(&mut self, sender: ClientId, seq: u64, held: &Held, gap_from: Option<u64>) {
        let rate = self.rate;
        let spurt_gap = 48 * SPURT_GAP_MS;
        let drift = self.samples(DRIFT_MS);
        let buf_start = self.buf_start;
        let Some(sp) = self.speakers.get_mut(&sender) else {
            return;
        };
        let (primary, red) = if held.red {
            let Some(bundle) = protocol::red::decode(&held.payload) else {
                self.stats.malformed += 1;
                sp.next = Some(seq + 1);
                return;
            };
            (bundle.primary, Some(bundle))
        } else {
            (&held.payload[..], None)
        };
        // A new spurt starts where its first packet arrived.
        let new_spurt = sp.anchor.is_none() || held.ts > sp.ts_end + spurt_gap;
        if new_spurt {
            sp.anchor = Some((held.ts, held.arrival.max(buf_start)));
            sp.concealed_run = 0;
        }
        let (a_ts, mut a_pos) = sp.anchor.expect("set above");
        let mut pos = a_pos + (held.ts.saturating_sub(a_ts)) * u64::from(rate) / 48_000;
        // The sender's clock drifting from arrival time: one frame back
        // toward it.
        let frame = sp.frame as u64;
        if pos + drift < held.arrival {
            a_pos += frame;
            pos += frame;
        } else if pos > held.arrival + drift && a_pos >= frame {
            a_pos -= frame;
            pos -= frame;
        }
        sp.anchor = Some((a_ts, a_pos));

        let mut repaired: Vec<(u64, Vec<i16>)> = Vec::new();
        if let Some(from) = gap_from.filter(|_| !new_spurt) {
            let missing = seq - from;
            for k in (1..=missing).rev() {
                // Frame `seq - k`.
                let block = red
                    .as_ref()
                    .and_then(|b| b.redundant.iter().find(|x| u64::from(x.seq_distance) == k));
                let at = pos.saturating_sub(k * sp.frame as u64);
                if let Some(block) = block {
                    let out = &mut self.scratch;
                    if let Ok(n) = sp.decoder.decode(block.data, out, false) {
                        let block_pos = pos
                            .saturating_sub(u64::from(block.ts_offset) * u64::from(rate) / 48_000);
                        repaired.push((block_pos, out[..n].to_vec()));
                        sp.frame = n.max(1);
                        self.stats.repaired_red += 1;
                        sp.concealed_run = 0;
                        continue;
                    }
                }
                if k == 1 {
                    let n = sp.frame.min(self.scratch.len());
                    let out = &mut self.scratch[..n];
                    if let Ok(n) = sp.decoder.decode(primary, out, true) {
                        repaired.push((at, out[..n].to_vec()));
                        self.stats.repaired_fec += 1;
                        sp.concealed_run = 0;
                        continue;
                    }
                }
                if sp.concealed_run < MAX_CONCEALED {
                    let n = sp.frame.min(self.scratch.len());
                    let out = &mut self.scratch[..n];
                    if let Ok(n) = sp.decoder.decode(&[], out, false) {
                        repaired.push((at, out[..n].to_vec()));
                    }
                    sp.concealed_run += 1;
                    self.stats.concealed += 1;
                } else {
                    self.stats.lost += 1;
                }
            }
        }
        let decoded = if let Ok(n) = sp.decoder.decode(primary, &mut self.scratch, false) {
            sp.frame = n.max(1);
            sp.concealed_run = 0;
            Some(self.scratch[..n].to_vec())
        } else {
            self.stats.malformed += 1;
            None
        };
        sp.next = Some(seq + 1);
        if let Some(samples) = &decoded {
            sp.ts_end = held.ts + samples.len() as u64 * 48_000 / u64::from(rate);
        }
        for (at, samples) in repaired {
            self.add(at, &samples);
        }
        if let Some(samples) = decoded {
            self.add(pos, &samples);
        }
    }

    /// Sum `samples` into the timeline from `pos`.
    fn add(&mut self, pos: u64, samples: &[i16]) {
        let end = pos + samples.len() as u64;
        if end <= self.buf_start {
            self.stats.too_late += 1;
            return;
        }
        let skip = usize::try_from(self.buf_start.saturating_sub(pos)).unwrap_or(usize::MAX);
        let from = usize::try_from(pos.saturating_sub(self.buf_start)).unwrap_or(usize::MAX);
        let needed = from + samples.len() - skip;
        if self.buf.len() < needed {
            self.buf.resize(needed, 0);
        }
        for (i, s) in samples.iter().skip(skip).enumerate() {
            self.buf[from + i] += i32::from(*s);
        }
    }

    /// Everything before `upto` is final: hand it out, limited, into
    /// `out`. The caller writes it to the file.
    pub fn drain_until(&mut self, upto: u64, out: &mut Vec<i16>) {
        // Holes held longer than the reorder window are given up on
        // first, for every speaker, so their frames land in time.
        let senders: Vec<ClientId> = self.speakers.keys().copied().collect();
        for s in senders {
            self.release(s, upto + self.samples(HOLD_MS));
        }
        while self.buf_start < upto {
            let v = self.buf.pop_front().unwrap_or(0);
            out.push(limit(v));
            self.buf_start += 1;
        }
    }

    /// What may be written now: up to [`HOLD_MS`] before the newest
    /// arrival.
    pub fn drain_ready(&mut self, out: &mut Vec<i16>) {
        let upto = self.newest.saturating_sub(self.samples(HOLD_MS));
        if upto > self.buf_start {
            self.drain_until(upto, out);
        }
    }

    /// The recording ended at `end`: place what still waits, and hand
    /// out the timeline to there.
    pub fn finish(&mut self, end: u64, out: &mut Vec<i16>) {
        let senders: Vec<ClientId> = self.speakers.keys().copied().collect();
        for s in senders {
            self.release(s, u64::MAX / 2);
        }
        self.drain_until(end, out);
        self.buf.clear();
    }

    /// Where the timeline has been written to.
    #[must_use]
    pub fn written(&self) -> u64 {
        self.buf_start
    }
}

/// A soft limiter to −1 dBFS: transparent below 0.7 of full scale,
/// a smooth knee above, never past 0.891.
fn limit(v: i32) -> i16 {
    const KNEE: f32 = 0.7;
    const CEIL: f32 = 0.891;
    #[allow(clippy::cast_precision_loss)]
    let x = v as f32 / 32_768.0;
    let a = x.abs();
    let y = if a <= KNEE {
        a
    } else {
        KNEE + (CEIL - KNEE) * ((a - KNEE) / (CEIL - KNEE)).tanh()
    };
    (y.copysign(x) * 32_767.0).round() as i16
}

#[cfg(test)]
mod tests {
    use protocol::pack_voice_with_flags;

    use super::*;

    const RATE: u32 = 48_000;
    const FRAME: usize = 960;

    /// An encoder making 20 ms frames of a tone, packed as the client
    /// sends them.
    struct Talker {
        enc: opus::Encoder,
        seq: u32,
        ts: u32,
        phase: f32,
        freq: f32,
    }

    impl Talker {
        fn new(freq: f32) -> Self {
            let mut enc =
                opus::Encoder::new(RATE, opus::Channels::Mono, opus::Application::Voip).unwrap();
            enc.set_inband_fec(true).unwrap();
            enc.set_packet_loss_perc(20).unwrap();
            Self {
                enc,
                seq: 0,
                ts: 0,
                phase: 0.0,
                freq,
            }
        }

        fn frame_bytes(&mut self) -> Vec<u8> {
            let pcm: Vec<i16> = (0..FRAME)
                .map(|_| {
                    self.phase += self.freq / RATE as f32;
                    ((self.phase * std::f32::consts::TAU).sin() * 8_000.0) as i16
                })
                .collect();
            let mut out = vec![0u8; 400];
            let n = self.enc.encode(&pcm, &mut out).unwrap();
            out.truncate(n);
            out
        }

        /// The next packet, plain.
        fn next(&mut self) -> Vec<u8> {
            let payload = self.frame_bytes();
            let p = pack_voice_with_flags(self.seq, self.ts, 0, &payload).unwrap();
            self.seq = self.seq.wrapping_add(1);
            self.ts = self.ts.wrapping_add(FRAME as u32);
            p
        }

        /// A silent pause: timestamps run on, sequence numbers hold.
        fn pause(&mut self, ms: u32) {
            self.ts = self.ts.wrapping_add(ms * 48);
        }
    }

    fn energy(s: &[i16]) -> f64 {
        s.iter().map(|v| f64::from(*v).powi(2)).sum::<f64>() / s.len().max(1) as f64
    }

    #[test]
    fn two_speakers_land_where_they_arrived() {
        let mut mix = Mixer::new(RATE);
        let mut a = Talker::new(440.0);
        let mut b = Talker::new(660.0);
        // A speaks from 0 to 1 s; B from 2 s to 3 s.
        for i in 0..50u64 {
            mix.push(1, i * FRAME as u64, &a.next());
        }
        for i in 0..50u64 {
            mix.push(2, 2 * 48_000 + i * FRAME as u64, &b.next());
        }
        let mut out = Vec::new();
        mix.finish(4 * 48_000, &mut out);
        assert_eq!(out.len(), 4 * 48_000);
        let sec = |k: usize| energy(&out[k * 48_000 + 4_800..(k + 1) * 48_000 - 4_800]);
        assert!(sec(0) > 1e5, "A in the first second");
        assert!(sec(1) < 1e3, "silence between");
        assert!(sec(2) > 1e5, "B in the third second");
        assert!(sec(3) < 1e3, "silence after");
    }

    #[test]
    fn a_pause_is_silence_of_its_length() {
        let mut mix = Mixer::new(RATE);
        let mut a = Talker::new(440.0);
        for i in 0..25u64 {
            mix.push(1, i * 960, &a.next());
        }
        a.pause(1_500);
        // The spurt after the pause arrives 2 s after the first.
        for i in 0..25u64 {
            mix.push(1, 96_000 + i * 960, &a.next());
        }
        let mut out = Vec::new();
        mix.finish(3 * 48_000, &mut out);
        assert!(energy(&out[2_000..20_000]) > 1e5);
        assert!(energy(&out[30_000..90_000]) < 1e3);
        assert!(energy(&out[98_000..116_000]) > 1e5);
    }

    #[test]
    fn reordered_packets_land_in_order_and_a_lost_one_is_repaired() {
        let mut mix = Mixer::new(RATE);
        let mut a = Talker::new(300.0);
        let packets: Vec<Vec<u8>> = (0..40).map(|_| a.next()).collect();
        for (i, p) in packets.iter().enumerate() {
            if i == 10 {
                continue; // lost
            }
            // 11 arrives before 9.
            let arrival = if i == 9 {
                12
            } else if i == 11 {
                9
            } else {
                i
            } as u64
                * 960;
            mix.push(1, arrival, p);
        }
        let mut out = Vec::new();
        mix.finish(48_000, &mut out);
        assert_eq!(mix.stats.repaired_fec + mix.stats.concealed, 1);
        assert_eq!(mix.stats.too_late, 0);
        // No hole: the tone runs through frame 10.
        assert!(energy(&out[9_600..10_560]) > 1e5);
    }

    #[test]
    fn a_red_copy_repairs_exactly() {
        let mut mix = Mixer::new(RATE);
        let mut a = Talker::new(500.0);
        let frames: Vec<(u32, Vec<u8>)> = (0..20)
            .map(|_| {
                let ts = a.ts;
                let f = a.frame_bytes();
                a.ts += FRAME as u32;
                (ts, f)
            })
            .collect();
        for (i, (ts, primary)) in frames.iter().enumerate() {
            if i == 7 {
                continue;
            }
            let mut payload = Vec::new();
            let red: Vec<(u32, &[u8])> = if i > 0 {
                vec![(FRAME as u32, &frames[i - 1].1[..])]
            } else {
                Vec::new()
            };
            protocol::red::encode(primary, &red, &mut payload);
            let p = pack_voice_with_flags(i as u32, *ts, protocol::voice::VOICE_FLAG_RED, &payload)
                .unwrap();
            mix.push(1, i as u64 * 960, &p);
        }
        let mut out = Vec::new();
        mix.finish(24_000, &mut out);
        assert_eq!(mix.stats.repaired_red, 1);
        assert_eq!(mix.stats.concealed, 0);
    }

    #[test]
    fn memory_stays_flat_over_a_long_recording() {
        let mut mix = Mixer::new(16_000);
        let mut a = Talker::new(200.0);
        let mut out = Vec::new();
        let mut written = 0usize;
        // Ten minutes of a sender that never pauses, drained as it goes.
        for i in 0..30_000u64 {
            mix.push(1, i * 320, &a.next());
            mix.drain_ready(&mut out);
            written += out.len();
            out.clear();
            assert!(
                mix.buf.len() < 16_000 * 2,
                "the timeline holds about a second"
            );
        }
        mix.finish(30_000 * 320, &mut out);
        written += out.len();
        assert_eq!(written, 30_000 * 320);
    }

    #[test]
    fn the_limiter_never_clips() {
        assert_eq!(limit(0), 0);
        assert_eq!(limit(1000), 1000);
        assert!(limit(200_000) <= 29_200);
        assert!(limit(-200_000) >= -29_200);
    }

    #[test]
    fn the_live_path_keeps_the_sound() {
        // As the recorder thread drives it: a drain after every push.
        let mut mix = Mixer::new(RATE);
        let mut a = Talker::new(440.0);
        let mut out = Vec::new();
        for i in 0..150u64 {
            let mut chunk = Vec::new();
            mix.push(1, i * FRAME as u64 + 5_000, &a.next());
            mix.drain_ready(&mut chunk);
            out.extend(chunk);
        }
        let mut tail = Vec::new();
        mix.finish(150 * FRAME as u64 + 10_000, &mut tail);
        out.extend(tail);
        let loud = out.iter().filter(|s| s.unsigned_abs() > 2_000).count();
        eprintln!("len {} loud {} stats {:?}", out.len(), loud, mix.stats);
        assert!(loud > 48_000, "silent: {loud}");
    }
}
