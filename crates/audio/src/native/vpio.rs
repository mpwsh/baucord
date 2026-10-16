//! iOS: Apple's Voice-Processing I/O unit (VPIO,
//! `kAudioUnitSubType_VoiceProcessingIO`) — the platform's echo
//! canceller (`IOS-SPEC.md` §6.3, `AEC-SPEC.md` E8). Used only when the
//! session wants the voice-call path (`aec::wants_aec`); otherwise cpal's
//! `RemoteIO` runs exactly as before, as on Android.
//!
//! VPIO is **one unit for both directions**: what it plays on bus 0 is
//! the reference it cancels from what bus 1 records — so both sides
//! must be on it, or neither. `VoiceChat::plan_aec` opens the unit
//! first ([`Prepared`]) and keeps it for the plan's life; if it will
//! not open, both sides take cpal's plain streams. The engine's two
//! sides (`AudioInput`, `AudioOutput`) still open and close on their
//! own and attach to the running unit (a process-wide `Weak`): each
//! hands its half — the ring, the wake latch, the meters — to the
//! unit's callback through a [`Mailbox`], and a side that goes away
//! flags its half, which the callback then gives back; so either side
//! can be reopened while the other keeps running.
//!
//! Both callbacks honour the `native/mod.rs` contract through the
//! shared [`super::pump`] halves. The unit's client format is 48 kHz mono
//! `f32` both ways — it converts to and from the hardware — so the
//! workers never resample. The callbacks run on the I/O thread: no
//! blocking, no logging, no freeing there — the mailbox is `try_lock`
//! behind an atomic flag, and a half the callback lets go of goes back
//! through a channel to be freed on the control side.

use std::{
    ptr::{self, addr_of},
    slice,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex, PoisonError, Weak,
    },
};

use coreaudio::audio_unit::{
    audio_format::LinearPcmFlags,
    render_callback::{data, ActionFlags, Args},
    AudioUnit, Element, IOType, SampleFormat, Scope, StreamFormat,
};
use crossbeam_channel::{bounded, Receiver, Sender};
use log::{debug, info, warn};
use objc2_audio_toolbox::{
    kAUVoiceIOProperty_BypassVoiceProcessing, kAUVoiceIOProperty_MuteOutput,
    kAUVoiceIOProperty_OtherAudioDuckingConfiguration, kAudioOutputUnitProperty_EnableIO,
    kAudioUnitProperty_MaximumFramesPerSlice, AUVoiceIOOtherAudioDuckingConfiguration,
    AUVoiceIOOtherAudioDuckingLevel,
};
use objc2_core_audio_types::AudioBufferList;
use rtrb::{Consumer, Producer};

use super::{
    avsession,
    pump::{CapturePump, RenderPump},
    CaptureStartError, RenderStartError,
};
use crate::{
    aec::reasons, input_config::InputConfig, latch::Ring, stats::AudioStats, OPUS_SAMPLE_RATE,
};

/// The largest slice the unit may ask for: with the screen locked iOS
/// moves to 4096-frame buffers, and a unit left at its default refuses
/// to render them (Technical Q&A QA1606).
const MAX_FRAMES_PER_SLICE: u32 = 4096;

// ── The shared unit ─────────────────────────────────────────────────────

/// A one-value handoff into a real-time callback: the control side
/// [`put`](Self::put)s, replacing anything not yet collected; the
/// callback [`take`](Self::take)s without ever blocking.
struct Slot<T> {
    pending: AtomicBool,
    value: Mutex<Option<T>>,
}

impl<T> Slot<T> {
    fn new() -> Self {
        Self {
            pending: AtomicBool::new(false),
            value: Mutex::new(None),
        }
    }

    fn put(&self, value: T) {
        // A value not collected yet is dropped here, off the I/O thread.
        *self.value.lock().unwrap_or_else(PoisonError::into_inner) = Some(value);
        self.pending.store(true, Ordering::Release);
    }

    /// Real-time safe: an atomic load, and `try_lock` only when
    /// something is pending. A lost race is retried next callback.
    fn take(&self) -> Option<T> {
        if !self.pending.load(Ordering::Acquire) {
            return None;
        }
        let mut value = self.value.try_lock().ok()?;
        self.pending.store(false, Ordering::Relaxed);
        value.take()
    }
}

/// A side's half as the callback holds it: given back once `attached`
/// goes false (the side was dropped).
struct Half<P> {
    attached: Arc<AtomicBool>,
    pump: P,
}

/// Halves the callback may hand back before the control side sweeps:
/// one per reopen, so a few is plenty.
const RETIRED: usize = 4;

/// One side's way into the unit's callback, and back out.
struct Mailbox<P> {
    slot: Slot<Half<P>>,
    /// Halves the callback let go of (replaced or detached), to be
    /// freed off the I/O thread.
    retired_tx: Sender<Half<P>>,
    retired_rx: Receiver<Half<P>>,
}

impl<P> Mailbox<P> {
    fn new() -> Self {
        let (retired_tx, retired_rx) = bounded(RETIRED);
        Self {
            slot: Slot::new(),
            retired_tx,
            retired_rx,
        }
    }

    /// Hand a new half to the callback (control side), freeing what it
    /// gave back meanwhile.
    fn attach(&self, half: Half<P>) {
        while self.retired_rx.try_recv().is_ok() {}
        self.slot.put(half);
    }

    /// Give a half back (I/O thread). Only a full channel — four reopens
    /// nobody swept — frees it here.
    fn retire(&self, half: Half<P>) {
        let _ = self.retired_tx.try_send(half);
    }

    /// The callback's current half: a new one from the slot replaces
    /// it, a detached one is given back.
    fn current<'a>(&self, held: &'a mut Option<Half<P>>) -> Option<&'a mut P> {
        if let Some(new) = self.slot.take()
            && let Some(old) = held.replace(new)
        {
            self.retire(old);
        }
        if held
            .as_ref()
            .is_some_and(|h| !h.attached.load(Ordering::Relaxed))
            && let Some(old) = held.take()
        {
            self.retire(old);
        }
        held.as_mut().map(|h| &mut h.pump)
    }
}

/// The open, running VPIO unit.
struct Unit {
    /// Stops, uninitialises and disposes of the unit — and frees its
    /// callbacks and whatever halves they hold — on drop. Behind a
    /// mutex so the shared `Arc<Unit>` is `Sync`, and for the mute
    /// property, set from the control side.
    io: Mutex<AudioUnit>,
    /// The microphone is muted on the unit (`set_mic_muted`).
    mic_muted: AtomicBool,
    capture: Arc<Mailbox<CapturePump>>,
    render: Arc<Mailbox<RenderPump>>,
    /// Voice processing confirmed on after initialisation (the bypass
    /// property read back as 0).
    processing: bool,
    /// The route when the unit opened, for names and the log.
    route: String,
    _watch: avsession::SessionWatch,
}

/// The unit both sides use, while either holds it.
static LIVE: Mutex<Weak<Unit>> = Mutex::new(Weak::new());

/// The unit, opened for a voice plan before either side starts, and
/// held by `VoiceChat` for as long as the plan lasts: both sides then
/// attach to it, never to a unit of their own.
pub struct Prepared {
    unit: Arc<Unit>,
}

impl Prepared {
    /// Mute or unmute the microphone on the unit
    /// (`kAUVoiceIOProperty_MuteOutput`: the unit stops delivering the
    /// microphone, and the system no longer shows the app as recording —
    /// what `AVAudioEngine`'s `voiceProcessingInputMuted` sets; iOS 17+
    /// may play its own mute tone). Playback and the canceller's state
    /// carry on, so unmuting is immediate (IOS-SPEC.md D51). Logged on
    /// a change; a refusal leaves the samples dropped by the capture
    /// side's own mute, as before.
    pub fn set_mic_muted(&self, muted: bool) {
        self.unit.set_mic_muted(muted);
    }
}

impl Unit {
    fn set_mic_muted(&self, muted: bool) {
        if self.mic_muted.load(Ordering::Relaxed) == muted {
            return;
        }
        let result = set_mute(
            &mut self.io.lock().unwrap_or_else(PoisonError::into_inner),
            muted,
        );
        match result {
            Ok(()) => {
                // Only a change that took: a refusal is tried again on
                // the next call.
                self.mic_muted.store(muted, Ordering::Relaxed);
                info!(
                    "VPIO microphone {}",
                    if muted {
                        "muted on the unit"
                    } else {
                        "unmuted"
                    }
                );
            }
            Err(e) => warn!("VPIO microphone mute ({muted}): {e}"),
        }
    }
}

/// Open the unit (or take the one running) for a voice plan, its
/// microphone muted on the unit when `mic_muted` (IOS-SPEC.md D51).
///
/// # Errors
/// The unit would not open: the plan takes plain streams instead.
pub fn prepare(stats: &Arc<AudioStats>, mic_muted: bool) -> Result<Prepared, String> {
    let unit = shared_unit(stats, mic_muted)?;
    unit.set_mic_muted(mic_muted);
    Ok(Prepared { unit })
}

/// The running unit, opening it if nobody holds one — muted from the
/// start when `mic_muted`, so a muted plan never records a moment.
fn shared_unit(stats: &Arc<AudioStats>, mic_muted: bool) -> Result<Arc<Unit>, String> {
    let mut live = LIVE.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(unit) = live.upgrade() {
        return Ok(unit);
    }
    let unit = Arc::new(Unit::open(stats, mic_muted)?);
    *live = Arc::downgrade(&unit);
    Ok(unit)
}

/// The unit's microphone mute (`kAUVoiceIOProperty_MuteOutput`).
fn set_mute(unit: &mut AudioUnit, muted: bool) -> Result<(), coreaudio::Error> {
    let value = u32::from(muted);
    unit.set_property(
        kAUVoiceIOProperty_MuteOutput,
        Scope::Global,
        Element::Output,
        Some(&value),
    )
}

fn ca(what: &str) -> impl FnOnce(coreaudio::Error) -> String + '_ {
    move |e| format!("VPIO {what}: {e}")
}

impl Unit {
    fn open(stats: &Arc<AudioStats>, mic_muted: bool) -> Result<Self, String> {
        avsession::prefer_voice_io();
        avsession::activate();
        // Before the unit runs, so the reroute does not hit it.
        avsession::keep_loudspeaker();

        let mut unit =
            AudioUnit::new_uninitialized(IOType::VoiceProcessingIO).map_err(ca("unit"))?;
        // Bus 1 is the microphone, bus 0 the speaker; both on.
        let on: u32 = 1;
        unit.set_property(
            kAudioOutputUnitProperty_EnableIO,
            Scope::Input,
            Element::Input,
            Some(&on),
        )
        .map_err(ca("enable input"))?;
        unit.set_property(
            kAudioOutputUnitProperty_EnableIO,
            Scope::Output,
            Element::Output,
            Some(&on),
        )
        .map_err(ca("enable output"))?;

        // What we read from the microphone (bus 1's output side) and
        // what we hand the speaker (bus 0's input side).
        let format = StreamFormat {
            sample_rate: f64::from(OPUS_SAMPLE_RATE),
            sample_format: SampleFormat::F32,
            flags: LinearPcmFlags::IS_FLOAT | LinearPcmFlags::IS_PACKED,
            channels: 1,
        };
        unit.set_stream_format(format, Scope::Output, Element::Input)
            .map_err(ca("microphone format"))?;
        unit.set_stream_format(format, Scope::Input, Element::Output)
            .map_err(ca("speaker format"))?;
        if let Err(e) = unit.set_property(
            kAudioUnitProperty_MaximumFramesPerSlice,
            Scope::Global,
            Element::Output,
            Some(&MAX_FRAMES_PER_SLICE),
        ) {
            debug!("VPIO max frames per slice: {e}");
        }

        // Processing on (the default, said anyway); its AGC left on.
        let bypass: u32 = 0;
        unit.set_property(
            kAUVoiceIOProperty_BypassVoiceProcessing,
            Scope::Global,
            Element::Output,
            Some(&bypass),
        )
        .map_err(ca("voice processing"))?;
        // VPIO lowers other apps' sound while it runs; as little as it
        // allows, and only while someone talks (iOS 17+; older systems
        // refuse the property and keep their default).
        let ducking = AUVoiceIOOtherAudioDuckingConfiguration {
            mEnableAdvancedDucking: 1,
            mDuckingLevel: AUVoiceIOOtherAudioDuckingLevel::Min,
        };
        if let Err(e) = unit.set_property(
            kAUVoiceIOProperty_OtherAudioDuckingConfiguration,
            Scope::Global,
            Element::Output,
            Some(&ducking),
        ) {
            debug!("VPIO ducking configuration: {e}");
        }

        let capture = Arc::new(Mailbox::new());
        let render = Arc::new(Mailbox::new());
        unit.set_input_callback(capture_callback(capture.clone()))
            .map_err(ca("input callback"))?;
        unit.set_render_callback(render_callback(render.clone()))
            .map_err(ca("render callback"))?;
        unit.initialize().map_err(ca("initialize"))?;
        let processing = unit
            .get_property::<u32>(
                kAUVoiceIOProperty_BypassVoiceProcessing,
                Scope::Global,
                Element::Output,
            )
            .map_or_else(
                |e| {
                    debug!("VPIO bypass read-back: {e}");
                    true
                },
                |b| b == 0,
            );
        // Muted before it runs, when the plan wants no microphone; the
        // flag says what took, so a refusal is tried again later.
        let muted = mic_muted
            && set_mute(&mut unit, true)
                .inspect_err(|e| warn!("VPIO microphone mute before start: {e}"))
                .is_ok();
        // Observers first: an interruption between start and here would
        // otherwise go unseen.
        let watch = avsession::SessionWatch::start(stats);
        unit.start().map_err(ca("start"))?;
        // And once more in case starting moved the call (a no-op unless
        // it is on the earpiece).
        avsession::keep_loudspeaker();
        let route = avsession::route_label();
        info!(
            "VPIO voice processing {}: {} Hz mono both ways, route {route}",
            if processing { "on" } else { "BYPASSED" },
            OPUS_SAMPLE_RATE,
        );
        Ok(Self {
            io: Mutex::new(unit),
            mic_muted: AtomicBool::new(muted),
            capture,
            render,
            processing,
            route,
            _watch: watch,
        })
    }
}

/// The first `AudioBuffer` of a callback's list as `f32` samples, and
/// its channel count. Empty for a null list or buffer.
///
/// # Safety
/// `list` is null or the buffer list Core Audio (or coreaudio-rs's
/// input callback, which rendered the microphone into it) handed the
/// callback: valid for the call, in the `f32` client format.
unsafe fn first_buffer<'a>(list: *mut AudioBufferList, frames: usize) -> (&'a mut [f32], usize) {
    if list.is_null() {
        return (&mut [], 1);
    }
    // SAFETY: a valid list (the contract); read unaligned as cpal does,
    // since the list is a C struct Core Audio owns.
    let buffer = unsafe { ptr::read_unaligned(addr_of!((*list).mBuffers[0])) };
    if buffer.mData.is_null() {
        return (&mut [], 1);
    }
    let channels = (buffer.mNumberChannels as usize).max(1);
    let len = (buffer.mDataByteSize as usize / size_of::<f32>()).min(frames * channels);
    // SAFETY: the buffer holds `mDataByteSize` bytes of `f32` (the
    // contract), of which `len` samples are used.
    (
        unsafe { slice::from_raw_parts_mut(buffer.mData.cast::<f32>(), len) },
        channels,
    )
}

// ── Callbacks ───────────────────────────────────────────────────────────

/// The microphone: coreaudio-rs has already pulled the processed
/// (echo-cancelled) signal into its buffer with `AudioUnitRender`.
fn capture_callback(
    mailbox: Arc<Mailbox<CapturePump>>,
) -> impl FnMut(Args<data::Raw>) -> Result<(), ()> {
    let mut held: Option<Half<CapturePump>> = None;
    move |args| {
        let Some(pump) = mailbox.current(&mut held) else {
            return Ok(());
        };
        // SAFETY: coreaudio-rs's rendered input list, valid for this call,
        // in the client format set in `Unit::open`.
        let (samples, channels) = unsafe { first_buffer(args.data.data, args.num_frames) };
        pump.push(samples, channels);
        Ok(())
    }
}

/// The speaker: fill the unit's buffer from the output worker's ring;
/// silence (flagged as such) while no output side is attached.
fn render_callback(
    mailbox: Arc<Mailbox<RenderPump>>,
) -> impl FnMut(Args<data::Raw>) -> Result<(), ()> {
    let mut held: Option<Half<RenderPump>> = None;
    move |mut args| {
        // SAFETY: Core Audio's buffer list for this render, valid for the
        // call, in the client format set in `Unit::open`.
        let (out, channels) = unsafe { first_buffer(args.data.data, args.num_frames) };
        if let Some(pump) = mailbox.current(&mut held) {
            pump.pull(out, channels);
        } else {
            out.fill(0.0);
            args.flags.insert(ActionFlags::OUTPUT_IS_SILENCE);
        }
        Ok(())
    }
}

// ── Capture ─────────────────────────────────────────────────────────────

/// The microphone half on the shared unit. Dropping it detaches the
/// half (the unit stops once the render side is gone too).
pub struct NativeCapture {
    attached: Arc<AtomicBool>,
    _unit: Arc<Unit>,
    /// Rate of the samples arriving in the ring.
    pub sample_rate: u32,
    /// What the log calls it.
    pub device_name: String,
    /// Whether voice processing is on this microphone, and if not why.
    pub aec_verdict: Result<(), &'static str>,
}

impl NativeCapture {
    /// Attach the microphone to the voice-processing unit — the plan's
    /// ([`prepare`]), or one opened here if there is none.
    ///
    /// # Errors
    /// The unit would not open; the ring and latch come back in the
    /// error.
    pub fn start(
        producer: Producer<f32>,
        wake_tx: Ring,
        stats: Arc<AudioStats>,
        config: Arc<InputConfig>,
    ) -> Result<Self, CaptureStartError> {
        let unit = match shared_unit(&stats, false) {
            Ok(unit) => unit,
            Err(reason) => {
                return Err(CaptureStartError {
                    reason,
                    producer,
                    wake_tx,
                })
            }
        };
        let attached = Arc::new(AtomicBool::new(true));
        unit.capture.attach(Half {
            attached: attached.clone(),
            pump: CapturePump {
                producer,
                wake_tx,
                stats,
                config,
            },
        });
        let aec_verdict = if unit.processing {
            Ok(())
        } else {
            Err(reasons::PATH_REFUSED)
        };
        Ok(Self {
            attached,
            sample_rate: OPUS_SAMPLE_RATE,
            device_name: format!("voice processing microphone ({})", unit.route),
            aec_verdict,
            _unit: unit,
        })
    }
}

impl Drop for NativeCapture {
    fn drop(&mut self) {
        self.attached.store(false, Ordering::Relaxed);
    }
}

// ── Render ──────────────────────────────────────────────────────────────

/// The speaker half on the shared unit. Dropping it detaches the half.
pub struct NativeRender {
    attached: Arc<AtomicBool>,
    _unit: Arc<Unit>,
    /// Rate the ring must be filled at.
    pub sample_rate: u32,
    pub device_name: String,
}

impl NativeRender {
    /// Attach the speaker to the voice-processing unit — the plan's
    /// ([`prepare`]), or one opened here if there is none.
    ///
    /// # Errors
    /// The unit would not open; the ring and latch come back in the
    /// error.
    pub fn start(
        consumer: Consumer<f32>,
        wake_tx: Ring,
        stats: Arc<AudioStats>,
        callback_frames: Arc<AtomicUsize>,
    ) -> Result<Self, RenderStartError> {
        let unit = match shared_unit(&stats, false) {
            Ok(unit) => unit,
            Err(reason) => {
                return Err(RenderStartError {
                    reason,
                    consumer,
                    wake_tx,
                })
            }
        };
        let attached = Arc::new(AtomicBool::new(true));
        unit.render.attach(Half {
            attached: attached.clone(),
            pump: RenderPump::new(consumer, wake_tx, stats, Some(callback_frames)),
        });
        Ok(Self {
            attached,
            sample_rate: OPUS_SAMPLE_RATE,
            device_name: format!("voice processing ({})", unit.route),
            _unit: unit,
        })
    }
}

impl Drop for NativeRender {
    fn drop(&mut self) {
        self.attached.store(false, Ordering::Relaxed);
    }
}
