//! Stereo Opus decoder for incoming screen share audio.
//!
//! Receives raw Opus-encoded stereo packets from the network, decodes
//! them, and plays them: through a [`StreamAudioOutput`] of its own, or —
//! on a phone whose voice output is on the call path — through that
//! output's mix (`stream::mix`, ANDROID-SPEC.md D56, IOS-SPEC.md D52).
//!
//! ## Architecture
//!
//! ```text
//! network → StreamAudioPacket → [decoder thread] → Sink ─→ StreamAudioOutput's ring → its device callback
//!                                                        └→ stream::mix → voice output
//! ```
//!
//! The sink is chosen per frame, so a stream follows the voice path as
//! it changes (headphones in, a re-plan): into the mix while a call-path
//! voice output runs, on its own output otherwise. Its own output lives
//! on this thread, opens on the first frame on the voice output picked
//! in Settings ([`OutputSelection`]), and reopens when that pick changes
//! or its device goes away (an Android reroute reports "Disconnected").

use std::{
    sync::{
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
        Arc, Mutex, PoisonError,
    },
    time::{Duration, Instant},
};

use crossbeam_channel::{bounded, Receiver, Sender};
use log::{debug, error, info, warn};
use opus::{Channels, Decoder};

use crate::{
    stream::{
        downmix::Downmix,
        mix::{self, MixSource},
        output::StreamAudioOutput,
    },
    OPUS_SAMPLE_RATE,
};

/// Samples per channel per frame (20 ms at 48 kHz).
const STREAM_FRAME_SIZE: usize = 960;

/// Number of channels.
const STREAM_CHANNELS: usize = 2;

/// Samples per channel in Opus' longest frame (120 ms at 48 kHz).
const MAX_FRAME_SIZE: usize = 5_760;

/// Interleaved decode buffer size: room for Opus' longest frame. It held
/// exactly 20 ms, which is what we send; a longer frame, which the wire
/// allows, failed to decode and was concealed (review 2026-10, P3-29).
const DECODE_BUF_SIZE: usize = MAX_FRAME_SIZE * STREAM_CHANNELS;

/// How long after the voice output stopped mixing a stream stays on the
/// mix: a voice output reopening (a reroute, a re-plan) stops for a few
/// hundred milliseconds, and hopping onto an output of its own and back
/// for that would be worse than the gap.
const MIX_GRACE: Duration = Duration::from_millis(1500);

/// After the stream's own output lost its device: let the route settle.
const REOPEN_AFTER_LOSS: Duration = Duration::from_millis(300);

/// Between attempts to open an output that would not open.
const REOPEN_EVERY: Duration = Duration::from_secs(2);

// ── Incoming packet ─────────────────────────────────────────────────────

/// A raw Opus-encoded stream audio packet from the network.
#[derive(Debug, Clone)]
pub struct StreamAudioPacket {
    /// Opus-encoded stereo audio data.
    pub data: Vec<u8>,
    /// Sequence number for ordering and gap detection.
    pub seq: u32,
}

// ── Counters for the viewer's log ───────────────────────────────────────

/// What the stream's audio has been doing, for the viewer's summary
/// line (`client::screen`): written by the decoder thread, read by
/// anyone through [`StreamDecoder::stats`].
#[derive(Debug, Default)]
struct Counters {
    received: AtomicU64,
    /// Missing 20 ms frames filled in by the decoder (short gaps).
    concealed: AtomicU64,
    /// Frames skipped over by a reset (gaps longer than 200 ms).
    skipped: AtomicU64,
    /// Audio waiting to be played, ms (see [`StreamAudioStats`]).
    queued_ms: AtomicU32,
}

/// A snapshot of a stream's audio, cumulative since it started.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StreamAudioStats {
    pub received: u64,
    pub concealed: u64,
    pub skipped: u64,
    /// Audio decoded and waiting to be played, ms: the voice output's
    /// mix source on a phone's call path, else the stream's own output
    /// (its worker and ring; not the device's own buffer). With no
    /// timestamps on stream audio, this is its half of A/V sync.
    pub queued_ms: u32,
}

// ── Which output a stream plays on ──────────────────────────────────────

/// The output a watched stream's own output opens on: the voice output
/// the user picked in Settings, so a stream plays where the room does.
/// It was always the system default, so with a headset picked and
/// speakers as the default every stream played on the speakers — and
/// Automatic, seeing a headset, opened no canceller, so the room heard
/// it back (review P1-19).
///
/// One selection is shared by whoever knows the pick and every stream
/// decoder; a decoder looks at it with each frame and moves its output
/// when it changed. With no pick ("Default") a stream follows the device
/// the voice output opened ([`VOICE_OUTPUT`]): on Windows "Default" can
/// mean the communications device for voice and the console one for
/// cpal (`communications_default` in lib.rs). The phones ignore both:
/// their pick is a route for the call (`route`), not a device, and a
/// stream follows the call's route by itself.
#[derive(Clone, Default)]
pub struct OutputSelection(Arc<Selection>);

impl OutputSelection {
    /// Pick `name` (`None` = the default). A change moves every stream
    /// decoder's output with its next frame.
    pub fn set(&self, name: Option<&str>) {
        self.0.set(name);
    }
}

/// A device name and a generation bumped by every change, so a decoder
/// notices with one load.
#[derive(Default)]
pub(crate) struct Selection {
    generation: AtomicU64,
    name: Mutex<Option<String>>,
}

impl Selection {
    const fn new() -> Self {
        Self {
            generation: AtomicU64::new(0),
            name: Mutex::new(None),
        }
    }

    pub(crate) fn set(&self, name: Option<&str>) {
        let mut slot = self.name.lock().unwrap_or_else(PoisonError::into_inner);
        if slot.as_deref() != name {
            *slot = name.map(String::from);
            self.generation.fetch_add(1, Ordering::Release);
        }
    }

    fn get(&self) -> Option<String> {
        self.name
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }
}

/// The device the voice output opened, as its backend names it (the
/// endpoint's friendly name, which cpal uses too); set by `VoiceChat`
/// on the desktop each time playback opens.
pub(crate) static VOICE_OUTPUT: Selection = Selection::new();

/// Where a stream's own output should open, and the generations of the
/// pick and of the voice output that say so.
fn wanted_output(pick: &OutputSelection) -> ((u64, u64), Option<String>) {
    let seen = (pick.0.generation(), VOICE_OUTPUT.generation());
    // A phone's pick is a call route, not a device (see above).
    if cfg!(any(target_os = "android", target_os = "ios")) {
        return (seen, None);
    }
    (seen, pick.0.get().or_else(|| VOICE_OUTPUT.get()))
}

// ── Where decoded audio goes ────────────────────────────────────────────

/// The stream's playback: its own output, or the voice output's mix.
struct Sink {
    /// The Settings pick, the device the own output is for, and the
    /// generations (pick, voice output) it was worked out from.
    output: OutputSelection,
    device: Option<String>,
    output_seen: (u64, u64),
    volume: Arc<AtomicU32>,
    is_muted: Arc<AtomicBool>,
    /// The stream's own output.
    own: Option<StreamAudioOutput>,
    /// When the own output last opened (a device lost right after an
    /// open counts as a failed open: back off).
    opened_at: Option<Instant>,
    /// Not before this may the own output be (re)opened.
    next_open: Option<Instant>,
    /// Failed opens in a row (the first is a warning, the rest debug).
    open_failures: u32,
    /// The stream's source on the voice output's mix.
    mix: Option<MixSource>,
    /// Last time a voice output was mixing streams.
    last_mixing: Option<Instant>,
    /// Stereo → mono for the mix (phase-aligned: out-of-phase sound
    /// does not cancel), and its output, reused.
    downmix: Downmix,
    mono: Vec<f32>,
    counters: Arc<Counters>,
}

impl Sink {
    fn new(
        output: OutputSelection,
        volume: Arc<AtomicU32>,
        is_muted: Arc<AtomicBool>,
        counters: Arc<Counters>,
    ) -> Self {
        let (output_seen, device) = wanted_output(&output);
        Self {
            output,
            device,
            output_seen,
            volume,
            is_muted,
            own: None,
            opened_at: None,
            next_open: None,
            open_failures: 0,
            mix: None,
            last_mixing: None,
            downmix: Downmix::new(),
            mono: Vec::with_capacity(2 * STREAM_FRAME_SIZE),
            counters,
        }
    }

    /// Play one decoded frame (interleaved stereo, 48 kHz).
    fn play(&mut self, stereo: &[f32]) {
        let now = Instant::now();
        let mixing = mix::BUS.is_mixing();
        if mixing {
            self.last_mixing = Some(now);
        }
        let into_mix = self
            .last_mixing
            .is_some_and(|at| now.duration_since(at) < MIX_GRACE);
        if into_mix {
            self.play_mixed(stereo, mixing);
        } else {
            self.play_own(stereo, now);
        }
        #[allow(clippy::cast_possible_truncation)] // ms of audio
        let queued_ms = if let Some(source) = &self.mix {
            (source.queued() as u64 * 1000 / u64::from(OPUS_SAMPLE_RATE)) as u32
        } else {
            self.own.as_ref().map_or(0, StreamAudioOutput::queued_ms)
        };
        self.counters.queued_ms.store(queued_ms, Ordering::Relaxed);
    }

    /// Into the voice output's mix: mono (the call path is) through
    /// [`Downmix`] — a plain `(L + R) / 2` silenced whatever was out of
    /// phase between the channels — with the stream's volume and mute
    /// applied here. In the grace after the voice output stopped
    /// (`mixing` false) nothing is queued: nobody reads it, and it would
    /// play late when the next output starts.
    fn play_mixed(&mut self, stereo: &[f32], mixing: bool) {
        if self.own.take().is_some() {
            info!("Stream audio: now through the voice output (call path)");
        }
        let source = self.mix.get_or_insert_with(|| {
            info!("Stream audio: playing through the voice output (call path)");
            mix::BUS.source()
        });
        if !mixing {
            // Resumed later, it starts clean, not overlapping this.
            self.downmix.reset();
            return;
        }
        let gain = if self.is_muted.load(Ordering::Relaxed) {
            0.0
        } else {
            f32::from_bits(self.volume.load(Ordering::Relaxed))
        };
        self.mono.clear();
        self.downmix.process(stereo, &mut self.mono);
        for s in &mut self.mono {
            *s = (*s * gain).clamp(-0.99, 0.99);
        }
        source.push(&self.mono);
    }

    /// On the stream's own stereo output, opening or reopening it as
    /// needed.
    fn play_own(&mut self, stereo: &[f32], now: Instant) {
        if self.mix.take().is_some() {
            info!("Stream audio: back on its own output");
        }
        // Another output picked in Settings, or (no pick) the voice
        // output moved: follow it now, without the backoff a lost
        // device would get.
        let seen = (self.output.0.generation(), VOICE_OUTPUT.generation());
        if seen != self.output_seen {
            let (seen, device) = wanted_output(&self.output);
            self.output_seen = seen;
            if device != self.device {
                self.device = device;
                if self.own.take().is_some() {
                    info!("Stream audio: moving to the voice output");
                }
                self.next_open = None;
                self.open_failures = 0;
            }
        }
        if self.own.as_ref().is_some_and(StreamAudioOutput::failed) {
            self.own = None;
            // Lost right after opening: the device is coming and going —
            // a failed open, with its backoff and its quieter log.
            if self
                .opened_at
                .is_some_and(|at| now.duration_since(at) < REOPEN_EVERY)
            {
                if self.open_failures == 0 {
                    warn!("Stream audio output keeps losing its device; retrying");
                }
                self.open_failures = self.open_failures.saturating_add(1);
                self.next_open = Some(now + REOPEN_EVERY);
            } else {
                warn!("Stream audio output lost its device; reopening");
                // It had played a while: a fresh start for the backoff.
                self.open_failures = 0;
                self.next_open = Some(now + REOPEN_AFTER_LOSS);
            }
        }
        if self.own.is_none() && self.next_open.is_none_or(|at| now >= at) {
            match self.open_own() {
                Ok(own) => {
                    self.own = Some(own);
                    self.opened_at = Some(now);
                }
                Err(e) => {
                    if self.open_failures == 0 {
                        warn!("Stream audio output unavailable ({e}); retrying");
                    } else {
                        debug!("stream audio output still unavailable: {e}");
                    }
                    self.open_failures = self.open_failures.saturating_add(1);
                    self.next_open = Some(now + REOPEN_EVERY);
                }
            }
        }
        if let Some(output) = &mut self.own {
            output.push(stereo);
        }
    }

    /// Muted: let go of the own output (its device callback runs for
    /// nothing) and the mix source; [`Self::play`] brings back
    /// whichever is wanted.
    fn rest(&mut self) {
        // Only one is ever held; both are let go either way.
        let held = self.own.take().is_some() | self.mix.take().is_some();
        if held {
            debug!("Stream audio muted: output released");
        }
        self.downmix.reset();
        self.counters.queued_ms.store(0, Ordering::Relaxed);
    }

    fn open_own(&self) -> Result<StreamAudioOutput, Box<dyn std::error::Error>> {
        let open = |device: Option<&str>| -> Result<_, Box<dyn std::error::Error>> {
            let mut output = StreamAudioOutput::with_controls(
                device,
                self.volume.clone(),
                self.is_muted.clone(),
            )?;
            output.start()?;
            Ok(output)
        };
        match open(self.device.as_deref()) {
            // No pick, and cpal will not open the voice output's device
            // by that name: the default still plays.
            Err(e) if self.device.is_some() && self.output.0.get().is_none() => {
                debug!("stream audio on the voice output's device: {e}; the default instead");
                open(None)
            }
            opened => opened,
        }
    }
}

// ── Decoder worker ──────────────────────────────────────────────────────

/// A few packets at most are concealed, or dropped as late.
const NEAR: i32 = 10;

/// Where a packet stands against the one before it.
#[derive(Debug, PartialEq, Eq)]
enum Gap {
    /// The next one.
    Next,
    /// A few behind: already concealed, dropped.
    Late,
    /// This many are missing before it: concealed.
    Missing(u32),
    /// Too many are missing to conceal: the decoder starts over.
    Skipped(u32),
    /// Far behind: the sharer's capture restarted and counts from
    /// nothing again. The decoder starts over, and nothing was
    /// skipped — read as a jump forward it was four billion frames in
    /// the viewer's summary (review P2-22).
    Restarted,
}

impl Gap {
    fn between(last: u32, curr: u32) -> Self {
        // The distance from the expected number, wrapped into ±2³¹.
        #[allow(clippy::cast_possible_wrap)]
        let ahead = curr.wrapping_sub(last.wrapping_add(1)) as i32;
        match ahead {
            0 => Self::Next,
            1..=NEAR => Self::Missing(ahead.unsigned_abs()),
            a if a > NEAR => Self::Skipped(ahead.unsigned_abs()),
            a if a >= -NEAR => Self::Late,
            _ => Self::Restarted,
        }
    }
}

struct StreamDecoderWorker {
    decoder: Decoder,
    rx: Receiver<StreamAudioPacket>,
    sink: Sink,
    should_stop: Arc<AtomicBool>,

    last_seq: Option<u32>,
    frame_buffer: Vec<f32>,
    /// The stream is muted, and nothing of it is decoded or played.
    resting: bool,
}

impl StreamDecoderWorker {
    fn run(&mut self) {
        info!("Stream decoder thread started");

        while let Ok(packet) = self.rx.recv() {
            if self.should_stop.load(Ordering::Relaxed) {
                break;
            }
            self.process_packet(&packet);
        }

        info!("Stream decoder thread stopped");
    }

    /// A fresh decoder: what came before says nothing about what
    /// follows.
    fn reset_decoder(&mut self) {
        match Decoder::new(OPUS_SAMPLE_RATE, Channels::Stereo) {
            Ok(decoder) => self.decoder = decoder,
            Err(e) => error!("Failed to reset stream decoder: {e}"),
        }
    }

    fn process_packet(&mut self, packet: &StreamAudioPacket) {
        let curr = packet.seq;
        let counters = Arc::clone(&self.sink.counters);
        counters.received.fetch_add(1, Ordering::Relaxed);

        // Muted: counted, and nothing more. It was decoded, downmixed
        // and played as zeros, its output open all along (review
        // 2026-10, P3-27). Unmuted, the decoder starts over from the
        // packet in hand and the output opens again.
        if self.sink.is_muted.load(Ordering::Relaxed) {
            if !self.resting {
                self.resting = true;
                self.sink.rest();
            }
            self.last_seq = Some(curr);
            return;
        }
        if self.resting {
            self.resting = false;
            self.reset_decoder();
        }

        // ── Sequence gap detection & PLC recovery ───────────────────
        match self
            .last_seq
            .map_or(Gap::Next, |last| Gap::between(last, curr))
        {
            Gap::Next => {}
            Gap::Late => return,
            Gap::Missing(missing) => {
                counters
                    .concealed
                    .fetch_add(u64::from(missing), Ordering::Relaxed);
                for _ in 0..missing {
                    match self
                        .decoder
                        .decode_float(&[], &mut self.frame_buffer, false)
                    {
                        Ok(n) => {
                            let total = n * STREAM_CHANNELS;
                            self.sink.play(&self.frame_buffer[..total]);
                        }
                        Err(e) => {
                            warn!("Stream PLC decode failed: {e}");
                        }
                    }
                }
            }
            Gap::Skipped(skipped) => {
                // Counted in the viewer's summary line (`skipped`).
                counters
                    .skipped
                    .fetch_add(u64::from(skipped), Ordering::Relaxed);
                debug!("Stream audio seq jumped by {skipped} — resetting decoder");
                self.reset_decoder();
            }
            Gap::Restarted => {
                debug!("Stream audio seq went back to {curr}: the sharer's capture restarted");
                self.reset_decoder();
            }
        }

        // ── Decode current packet ───────────────────────────────────
        match self
            .decoder
            .decode_float(&packet.data, &mut self.frame_buffer, false)
        {
            Ok(samples_per_channel) => {
                let total = samples_per_channel * STREAM_CHANNELS;
                self.sink.play(&self.frame_buffer[..total]);
                self.last_seq = Some(curr);
            }
            Err(e) => {
                error!("Stream audio decode error seq {curr}: {e}");

                if let Ok(n) = self
                    .decoder
                    .decode_float(&[], &mut self.frame_buffer, false)
                {
                    let total = n * STREAM_CHANNELS;
                    self.sink.play(&self.frame_buffer[..total]);
                }

                self.last_seq = Some(curr);
            }
        }
    }
}

// ── Public handle ───────────────────────────────────────────────────────

/// Handle to a running stream audio decoder + playback pipeline.
///
/// Dropping this handle stops the decoder thread, and with it playback.
pub struct StreamDecoder {
    packet_tx: Sender<StreamAudioPacket>,
    volume: Arc<AtomicU32>,
    is_muted: Arc<AtomicBool>,
    should_stop: Arc<AtomicBool>,
    decoder_thread: Option<std::thread::JoinHandle<()>>,
    counters: Arc<Counters>,
}

/// Where a stream's packets go, from any thread: the network thread
/// hands them to the decoder itself instead of through the screen
/// thread, a wake of that thread per 20 ms packet (review 2026-10,
/// P2-45). Holding one does not keep the decoder running.
#[derive(Clone)]
pub struct StreamAudioSender(Sender<StreamAudioPacket>);

impl StreamAudioSender {
    /// Queue `packet` for the decoder; dropped when 16 already wait.
    pub fn send(&self, packet: StreamAudioPacket) {
        let _ = self.0.try_send(packet);
    }
}

impl StreamDecoder {
    /// Send a raw Opus-encoded packet for decoding and playback.
    pub fn send(&self, packet: StreamAudioPacket) {
        let _ = self.packet_tx.try_send(packet);
    }

    /// A sender for another thread (see [`StreamAudioSender`]).
    #[must_use]
    pub fn sender(&self) -> StreamAudioSender {
        StreamAudioSender(self.packet_tx.clone())
    }

    /// Set playback volume (0.0–2.0).
    pub fn set_volume(&self, volume: f32) {
        let clamped = volume.clamp(0.0, 2.0);
        self.volume.store(clamped.to_bits(), Ordering::Relaxed);
    }

    /// Mute/unmute stream audio playback.
    pub fn set_muted(&self, muted: bool) {
        self.is_muted.store(muted, Ordering::Relaxed);
    }

    /// What the stream's audio has done so far (see
    /// [`StreamAudioStats`]).
    #[must_use]
    pub fn stats(&self) -> StreamAudioStats {
        let c = &self.counters;
        StreamAudioStats {
            received: c.received.load(Ordering::Relaxed),
            concealed: c.concealed.load(Ordering::Relaxed),
            skipped: c.skipped.load(Ordering::Relaxed),
            queued_ms: c.queued_ms.load(Ordering::Relaxed),
        }
    }
}

impl Drop for StreamDecoder {
    fn drop(&mut self) {
        self.should_stop.store(true, Ordering::Relaxed);
        // Replacing the sender closes the channel, which ends the
        // thread's `recv`; the thread drops the sink (its own output,
        // its source on the mix) on the way out. A `StreamAudioSender`
        // still held elsewhere keeps the channel open: an empty packet
        // wakes the thread to see the stop (a full queue wakes it too).
        let _ = self.packet_tx.try_send(StreamAudioPacket {
            data: Vec::new(),
            seq: 0,
        });
        let (dead_tx, _) = bounded::<StreamAudioPacket>(0);
        self.packet_tx = dead_tx;
        if let Some(thread) = self.decoder_thread.take() {
            let _ = thread.join();
        }
        info!("Stream decoder dropped");
    }
}

// ── Public spawn function ───────────────────────────────────────────────

/// Create and start a stream audio decoder + playback pipeline.
///
/// - `output`: where the stream's own output opens — the voice output
///   picked in Settings; followed when it changes
/// - `volume`: initial playback volume (0.0–2.0)
///
/// # Errors
/// The Opus decoder could not be created. (Playback opens with the
/// first frame and retries on its own.)
pub fn spawn(
    output: OutputSelection,
    volume: f32,
) -> Result<StreamDecoder, Box<dyn std::error::Error>> {
    let decoder = Decoder::new(OPUS_SAMPLE_RATE, Channels::Stereo)?;
    let volume = Arc::new(AtomicU32::new(volume.clamp(0.0, 2.0).to_bits()));
    let is_muted = Arc::new(AtomicBool::new(false));

    let (packet_tx, packet_rx) = bounded::<StreamAudioPacket>(16);
    let should_stop = Arc::new(AtomicBool::new(false));

    let stop = should_stop.clone();
    let (sink_volume, sink_muted) = (volume.clone(), is_muted.clone());
    let counters = Arc::new(Counters::default());
    let sink_counters = counters.clone();
    // The sink is built on the thread: its own output (a cpal stream,
    // not `Send` everywhere) is opened, used and dropped there.
    let decoder_thread = crate::platform::spawn_named("stream-decode", move || {
        StreamDecoderWorker {
            decoder,
            rx: packet_rx,
            sink: Sink::new(output, sink_volume, sink_muted, sink_counters),
            should_stop: stop,
            last_seq: None,
            frame_buffer: vec![0.0f32; DECODE_BUF_SIZE],
            resting: false,
        }
        .run();
    });

    Ok(StreamDecoder {
        packet_tx,
        volume,
        is_muted,
        should_stop,
        decoder_thread: Some(decoder_thread),
        counters,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_pick_is_noticed_and_the_same_one_is_not() {
        let selection = OutputSelection::default();
        let shared = selection.clone();
        let seen = shared.0.generation();
        selection.set(None);
        assert_eq!(shared.0.generation(), seen, "the default again");
        selection.set(Some("Headset"));
        assert_ne!(shared.0.generation(), seen);
        assert_eq!(shared.0.get().as_deref(), Some("Headset"));
        let seen = shared.0.generation();
        selection.set(Some("Headset"));
        assert_eq!(shared.0.generation(), seen, "the same pick again");
    }

    /// A pick wins; with none, the stream goes where the voice output
    /// opened (on Windows the communications default, which cpal's
    /// default need not be).
    #[test]
    fn the_pick_wins_and_default_follows_the_voice_output() {
        let pick = OutputSelection::default();
        VOICE_OUTPUT.set(Some("Speakers (Realtek)"));
        assert_eq!(
            wanted_output(&pick).1.as_deref(),
            Some("Speakers (Realtek)")
        );
        pick.set(Some("Headset"));
        let (seen, device) = wanted_output(&pick);
        assert_eq!(device.as_deref(), Some("Headset"));
        VOICE_OUTPUT.set(Some("Headset"));
        assert_ne!(wanted_output(&pick).0, seen, "noticed");
        assert_eq!(wanted_output(&pick).1.as_deref(), Some("Headset"));
        VOICE_OUTPUT.set(None);
    }
    /// A sender held by another thread (the network thread's route)
    /// does not keep a dropped decoder's thread waiting.
    #[test]
    fn a_held_sender_does_not_stall_the_drop() {
        let decoder = spawn(OutputSelection::default(), 1.0).unwrap();
        let sender = decoder.sender();
        drop(decoder);
        // The thread is gone: what is sent now goes nowhere.
        sender.send(StreamAudioPacket {
            data: vec![0; 8],
            seq: 1,
        });
    }

    /// The numbering of stream audio against the packet before: what is
    /// next, late, concealed, skipped — and a capture that restarted,
    /// which is none of those (review P2-22).
    #[test]
    fn a_restarted_capture_is_not_a_jump_forward() {
        assert_eq!(Gap::between(41, 42), Gap::Next);
        assert_eq!(Gap::between(41, 41), Gap::Late);
        assert_eq!(Gap::between(41, 32), Gap::Late);
        assert_eq!(Gap::between(41, 43), Gap::Missing(1));
        assert_eq!(Gap::between(41, 52), Gap::Missing(10));
        assert_eq!(Gap::between(41, 53), Gap::Skipped(11));
        assert_eq!(Gap::between(41, 5_000), Gap::Skipped(4_958));
        // The sharer's capture starts over from 0.
        assert_eq!(Gap::between(90_000, 0), Gap::Restarted);
        assert_eq!(Gap::between(90_000, 3), Gap::Restarted);
        // The counter wrapping is the next packet, not a restart.
        assert_eq!(Gap::between(u32::MAX, 0), Gap::Next);
        assert_eq!(Gap::between(u32::MAX - 1, 1), Gap::Missing(2));
    }
}
