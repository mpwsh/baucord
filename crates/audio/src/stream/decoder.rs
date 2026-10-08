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
//! network → StreamAudioPacket → [decoder thread] → Sink ─→ StreamAudioOutput → cpal
//!                                                        └→ stream::mix → voice output
//! ```
//!
//! The sink is chosen per frame, so a stream follows the voice path as
//! it changes (headphones in, a re-plan): into the mix while a call-path
//! voice output runs, on its own output otherwise. Its own output lives
//! on this thread, opens on the first frame and reopens when its device
//! goes away (an Android reroute reports "Disconnected").

use std::{
    sync::{
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
        Arc,
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
        output::{StreamAudioFrame, StreamAudioOutput},
    },
    OPUS_SAMPLE_RATE,
};

/// Samples per channel per frame (20 ms at 48 kHz).
const STREAM_FRAME_SIZE: usize = 960;

/// Number of channels.
const STREAM_CHANNELS: usize = 2;

/// Interleaved decode buffer size: 960 samples/ch × 2 channels.
const DECODE_BUF_SIZE: usize = STREAM_FRAME_SIZE * STREAM_CHANNELS;

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

// ── Where decoded audio goes ────────────────────────────────────────────

/// The stream's playback: its own output, or the voice output's mix.
struct Sink {
    device_name: Option<String>,
    volume: Arc<AtomicU32>,
    is_muted: Arc<AtomicBool>,
    /// The stream's own output and the sender into it.
    own: Option<(StreamAudioOutput, Sender<StreamAudioFrame>)>,
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
        device_name: Option<String>,
        volume: Arc<AtomicU32>,
        is_muted: Arc<AtomicBool>,
        counters: Arc<Counters>,
    ) -> Self {
        Self {
            device_name,
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
    fn play(&mut self, stereo: &[f32], seq: u32) {
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
            self.play_own(stereo, seq, now);
        }
        #[allow(clippy::cast_possible_truncation)] // ms of audio
        let queued_ms = if let Some(source) = &self.mix {
            (source.queued() as u64 * 1000 / u64::from(OPUS_SAMPLE_RATE)) as u32
        } else {
            self.own
                .as_ref()
                .map_or(0, |(output, _)| output.queued_ms())
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
    fn play_own(&mut self, stereo: &[f32], seq: u32, now: Instant) {
        if self.mix.take().is_some() {
            info!("Stream audio: back on its own output");
        }
        if self.own.as_ref().is_some_and(|(output, _)| output.failed()) {
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
        if let Some((_, tx)) = &self.own {
            let _ = tx.try_send(StreamAudioFrame {
                samples: stereo.to_vec(),
                seq,
            });
        }
    }

    fn open_own(
        &self,
    ) -> Result<(StreamAudioOutput, Sender<StreamAudioFrame>), Box<dyn std::error::Error>> {
        let mut output = StreamAudioOutput::with_controls(
            self.device_name.as_deref(),
            self.volume.clone(),
            self.is_muted.clone(),
        )?;
        let tx = output.frame_sender();
        output.start()?;
        Ok((output, tx))
    }
}

// ── Decoder worker ──────────────────────────────────────────────────────

struct StreamDecoderWorker {
    decoder: Decoder,
    rx: Receiver<StreamAudioPacket>,
    sink: Sink,
    should_stop: Arc<AtomicBool>,

    last_seq: Option<u32>,
    frame_buffer: Vec<f32>,
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

    fn process_packet(&mut self, packet: &StreamAudioPacket) {
        let curr = packet.seq;
        let counters = Arc::clone(&self.sink.counters);
        counters.received.fetch_add(1, Ordering::Relaxed);

        // ── Sequence gap detection & PLC recovery ───────────────────
        if let Some(last) = self.last_seq {
            let expect = last.wrapping_add(1);
            let ahead = curr.wrapping_sub(expect);
            let behind = expect.wrapping_sub(curr);

            if behind > 0 && behind <= 10 {
                return; // late packet
            }

            if ahead > 0 && ahead <= 10 {
                let gap = ahead as usize;
                counters
                    .concealed
                    .fetch_add(u64::from(ahead), Ordering::Relaxed);
                for _ in 0..gap {
                    match self
                        .decoder
                        .decode_float(&[], &mut self.frame_buffer, false)
                    {
                        Ok(n) => {
                            let total = n * STREAM_CHANNELS;
                            self.sink.play(&self.frame_buffer[..total], 0);
                        }
                        Err(e) => {
                            warn!("Stream PLC decode failed: {e}");
                        }
                    }
                }
            } else if ahead > 10 {
                // Counted in the viewer's summary line (`skipped`).
                counters
                    .skipped
                    .fetch_add(u64::from(ahead), Ordering::Relaxed);
                debug!("Stream audio seq jumped by {ahead} — resetting decoder");
                match Decoder::new(OPUS_SAMPLE_RATE, Channels::Stereo) {
                    Ok(new_dec) => self.decoder = new_dec,
                    Err(e) => error!("Failed to reset stream decoder: {e}"),
                }
            }
        }

        // ── Decode current packet ───────────────────────────────────
        match self
            .decoder
            .decode_float(&packet.data, &mut self.frame_buffer, false)
        {
            Ok(samples_per_channel) => {
                let total = samples_per_channel * STREAM_CHANNELS;
                self.sink.play(&self.frame_buffer[..total], curr);
                self.last_seq = Some(curr);
            }
            Err(e) => {
                error!("Stream audio decode error seq {curr}: {e}");

                if let Ok(n) = self
                    .decoder
                    .decode_float(&[], &mut self.frame_buffer, false)
                {
                    let total = n * STREAM_CHANNELS;
                    self.sink.play(&self.frame_buffer[..total], curr);
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

impl StreamDecoder {
    /// Send a raw Opus-encoded packet for decoding and playback.
    pub fn send(&self, packet: StreamAudioPacket) {
        let _ = self.packet_tx.try_send(packet);
    }

    /// Set playback volume (0.0–2.0).
    pub fn set_volume(&self, volume: f32) {
        let clamped = volume.clamp(0.0, 2.0);
        self.volume.store(clamped.to_bits(), Ordering::Relaxed);
    }

    #[must_use]
    pub fn volume(&self) -> f32 {
        f32::from_bits(self.volume.load(Ordering::Relaxed))
    }

    /// Mute/unmute stream audio playback.
    pub fn set_muted(&self, muted: bool) {
        self.is_muted.store(muted, Ordering::Relaxed);
    }

    #[must_use]
    pub fn is_muted(&self) -> bool {
        self.is_muted.load(Ordering::Relaxed)
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
        // its source on the mix) on the way out.
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
/// - `device_name`: output device for the stream's own output (or `None`
///   for the system default)
/// - `volume`: initial playback volume (0.0–2.0)
///
/// # Errors
/// The Opus decoder could not be created. (Playback opens with the
/// first frame and retries on its own.)
pub fn spawn(
    device_name: Option<&str>,
    volume: f32,
) -> Result<StreamDecoder, Box<dyn std::error::Error>> {
    let decoder = Decoder::new(OPUS_SAMPLE_RATE, Channels::Stereo)?;
    let volume = Arc::new(AtomicU32::new(volume.clamp(0.0, 2.0).to_bits()));
    let is_muted = Arc::new(AtomicBool::new(false));

    let (packet_tx, packet_rx) = bounded::<StreamAudioPacket>(16);
    let should_stop = Arc::new(AtomicBool::new(false));

    let stop = should_stop.clone();
    let device_name = device_name.map(String::from);
    let (sink_volume, sink_muted) = (volume.clone(), is_muted.clone());
    let counters = Arc::new(Counters::default());
    let sink_counters = counters.clone();
    // The sink is built on the thread: its own output (a cpal stream,
    // not `Send` everywhere) is opened, used and dropped there.
    let decoder_thread = crate::platform::spawn_named("stream-decode", move || {
        StreamDecoderWorker {
            decoder,
            rx: packet_rx,
            sink: Sink::new(device_name, sink_volume, sink_muted, sink_counters),
            should_stop: stop,
            last_seq: None,
            frame_buffer: vec![0.0f32; DECODE_BUF_SIZE],
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
