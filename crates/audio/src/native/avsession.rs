//! iOS: the shared `AVAudioSession` as the voice engine needs it
//! (`IOS-SPEC.md` §6.2–6.5). The host sets the category once at startup
//! (`crates/ios/src/session.rs`: `PlayAndRecord`, the loudspeaker,
//! Bluetooth); this file adds what depends on the voice plan:
//!
//! - the **mode**: `voiceChat` on the voice-call path (what the system
//!   tunes its voice processing and routes for), `default` otherwise;
//! - the preferred rate and buffer before a voice-processing unit opens;
//! - what the route plays to, for the plan's output kind;
//! - keeping the loudspeaker when the mode change would have moved the
//!   call to the earpiece;
//! - the session events that stop a voice-processing unit — an
//!   interruption (a phone call, Siri) ending, the media services
//!   resetting — turned into the stream-failed flags `VoiceChat::service`
//!   already reopens on. cpal's own streams handle these themselves.

use std::{
    ptr::NonNull,
    sync::{atomic::Ordering, Arc},
};

use block2::RcBlock;
use log::{debug, info, warn};
use objc2::{rc::Retained, runtime::ProtocolObject};
use objc2_avf_audio::{
    AVAudioSession, AVAudioSessionCategoryOptions, AVAudioSessionCategoryPlayAndRecord,
    AVAudioSessionInterruptionNotification, AVAudioSessionInterruptionType,
    AVAudioSessionInterruptionTypeKey, AVAudioSessionMediaServicesWereResetNotification,
    AVAudioSessionModeDefault, AVAudioSessionModeVoiceChat, AVAudioSessionPortAirPlay,
    AVAudioSessionPortBluetoothA2DP, AVAudioSessionPortBluetoothHFP, AVAudioSessionPortBluetoothLE,
    AVAudioSessionPortBuiltInReceiver, AVAudioSessionPortBuiltInSpeaker,
    AVAudioSessionPortCarAudio, AVAudioSessionPortHDMI, AVAudioSessionPortHeadphones,
    AVAudioSessionPortOverride, AVAudioSessionPortUSBAudio,
};
use objc2_foundation::{
    NSNotification, NSNotificationCenter, NSNumber, NSObjectProtocol, NSString,
};

use crate::{
    aec::{self, ApplePort, OutputKind},
    stats::AudioStats,
};

/// The host's category options (`crates/ios/src/session.rs`), set again
/// with the mode: the loudspeaker rather than the earpiece, a Bluetooth
/// headset with its microphone or as output only.
fn options() -> AVAudioSessionCategoryOptions {
    AVAudioSessionCategoryOptions::DefaultToSpeaker
        | AVAudioSessionCategoryOptions::AllowBluetoothHFP
        | AVAudioSessionCategoryOptions::AllowBluetoothA2DP
}

/// `voiceChat` (the voice-call path) or `default`. Call with no stream
/// open: a category change reroutes, which a running stream reports as
/// a failure.
pub fn set_voice_mode(voice: bool) {
    // SAFETY: the shared session is a process-wide singleton; the
    // constants are immutable framework statics.
    let (session, category, mode) = unsafe {
        (
            AVAudioSession::sharedInstance(),
            AVAudioSessionCategoryPlayAndRecord,
            if voice {
                AVAudioSessionModeVoiceChat
            } else {
                AVAudioSessionModeDefault
            },
        )
    };
    let (Some(category), Some(mode)) = (category, mode) else {
        warn!("audio session: the SDK's category or mode constant is missing");
        return;
    };
    // SAFETY: valid category, mode and options for the shared session.
    match unsafe { session.setCategory_mode_options_error(category, mode, options()) } {
        Ok(()) => info!("audio session: mode {mode}"),
        Err(e) => warn!("audio session mode {mode}: {}", e.localizedDescription()),
    }
}

/// 48 kHz and 10 ms buffers, before a voice-processing unit opens
/// (spec §6.2). Preferences only: the hardware decides, and the unit
/// converts to the format we ask of it either way.
pub fn prefer_voice_io() {
    // SAFETY: the shared session; plain setters.
    let session = unsafe { AVAudioSession::sharedInstance() };
    if let Err(e) = unsafe { session.setPreferredSampleRate_error(48_000.0) } {
        debug!(
            "audio session: preferred rate: {}",
            e.localizedDescription()
        );
    }
    if let Err(e) = unsafe { session.setPreferredIOBufferDuration_error(0.01) } {
        debug!(
            "audio session: preferred buffer: {}",
            e.localizedDescription()
        );
    }
}

/// Activate the session (a unit starting would too, implicitly; doing
/// it first gives the error a log line of its own).
pub fn activate() {
    // SAFETY: the shared session.
    let session = unsafe { AVAudioSession::sharedInstance() };
    if let Err(e) = unsafe { session.setActive_error(true) } {
        warn!("audio session: activate: {}", e.localizedDescription());
    }
}

/// What an output port's type is, for [`aec::ios_output_kind`].
fn port_of(kind: &NSString) -> ApplePort {
    let is = |c: Option<&NSString>| c.is_some_and(|c| kind.isEqualToString(c));
    // SAFETY: the port-type constants are immutable framework statics.
    unsafe {
        if is(AVAudioSessionPortBuiltInSpeaker) {
            ApplePort::Speaker
        } else if is(AVAudioSessionPortBuiltInReceiver) {
            ApplePort::Receiver
        } else if is(AVAudioSessionPortHeadphones) {
            ApplePort::Headphones
        } else if is(AVAudioSessionPortBluetoothA2DP)
            || is(AVAudioSessionPortBluetoothHFP)
            || is(AVAudioSessionPortBluetoothLE)
        {
            ApplePort::Bluetooth
        } else if is(AVAudioSessionPortUSBAudio) {
            ApplePort::Usb
        } else if is(AVAudioSessionPortCarAudio)
            || is(AVAudioSessionPortAirPlay)
            || is(AVAudioSessionPortHDMI)
        {
            ApplePort::Room
        } else {
            ApplePort::Other
        }
    }
}

/// The current route's outputs: their kinds and names.
fn outputs() -> Vec<(ApplePort, String)> {
    // SAFETY: the shared session; reading its current route.
    let route = unsafe { AVAudioSession::sharedInstance().currentRoute() };
    let outputs = unsafe { route.outputs() };
    outputs
        .iter()
        .map(|port| {
            // SAFETY: plain getters of a port description.
            let (kind, name) = unsafe { (port.portType(), port.portName()) };
            (port_of(&kind), name.to_string())
        })
        .collect()
}

/// What the session plays to now, for the echo-cancellation plan.
pub fn output_kind() -> OutputKind {
    let kinds: Vec<ApplePort> = outputs().into_iter().map(|(k, _)| k).collect();
    aec::ios_output_kind(&kinds)
}

/// The route's output names, for the log and the report's reference
/// (`Speaker`, `AirPods Pro`).
pub fn route_label() -> String {
    let names: Vec<String> = outputs().into_iter().map(|(_, n)| n).collect();
    if names.is_empty() {
        "no output".to_string()
    } else {
        names.join(" + ")
    }
}

/// The host chose the loudspeaker (`DefaultToSpeaker`); a mode change
/// that put the call on the earpiece anyway is undone with the speaker
/// override. Nothing else is touched: a headset stays a headset.
pub fn keep_loudspeaker() {
    let on_earpiece = outputs()
        .first()
        .is_some_and(|(k, _)| *k == ApplePort::Receiver);
    if !on_earpiece {
        return;
    }
    // SAFETY: the shared session; a documented override for
    // `PlayAndRecord`.
    let session = unsafe { AVAudioSession::sharedInstance() };
    match unsafe { session.overrideOutputAudioPort_error(AVAudioSessionPortOverride::Speaker) } {
        Ok(()) => info!("audio session: the call went to the earpiece; back to the loudspeaker"),
        Err(e) => warn!(
            "audio session: speaker override: {}",
            e.localizedDescription()
        ),
    }
}

/// The number stored under `key` in a notification's `userInfo`.
fn user_info_number(note: &NSNotification, key: Option<&NSString>) -> Option<usize> {
    let info = note.userInfo()?;
    let value = info.objectForKey(key?)?;
    Some(value.downcast::<NSNumber>().ok()?.unsignedIntegerValue())
}

/// Observers for the events that stop a voice-processing unit, for as
/// long as the unit lives.
pub struct SessionWatch {
    observers: Vec<Retained<ProtocolObject<dyn NSObjectProtocol>>>,
}

// SAFETY: the tokens are opaque handles used only to remove the
// observers on drop, which `NSNotificationCenter` allows from any
// thread (cpal's iOS session manager holds them the same way).
unsafe impl Send for SessionWatch {}
// SAFETY: as above; nothing is read or written through them.
unsafe impl Sync for SessionWatch {}

impl SessionWatch {
    /// Start watching; a stopped unit is reported through `stats`.
    pub fn start(stats: &Arc<AudioStats>) -> Self {
        let center = NSNotificationCenter::defaultCenter();
        let mut observers = Vec::new();
        // SAFETY: immutable framework statics.
        let (interruption, reset) = unsafe {
            (
                AVAudioSessionInterruptionNotification,
                AVAudioSessionMediaServicesWereResetNotification,
            )
        };

        // A phone call or Siri stops the unit. Nothing to do while it
        // lasts (the session cannot be had back); when it ends, both
        // sides are reported failed and reopened together.
        let failed = stats.clone();
        let interrupted = RcBlock::new(move |note: NonNull<NSNotification>| {
            // SAFETY: the center hands a valid notification for the
            // duration of the call.
            let note = unsafe { note.as_ref() };
            // SAFETY: an immutable framework static.
            let key = unsafe { AVAudioSessionInterruptionTypeKey };
            let Some(kind) = user_info_number(note, key) else {
                return;
            };
            if AVAudioSessionInterruptionType(kind) == AVAudioSessionInterruptionType::Began {
                info!("voice processing: interrupted (a call, Siri, an alarm)");
            } else {
                info!("voice processing: interruption over; reopening");
                failed.input_stream_failed.store(true, Ordering::Relaxed);
                failed.output_stream_failed.store(true, Ordering::Relaxed);
            }
        });
        if let Some(name) = interruption {
            // SAFETY: the block is `'static` and runs on the posting
            // thread, which does not matter: atomics and the logger only.
            observers.push(unsafe {
                center.addObserverForName_object_queue_usingBlock(
                    Some(name),
                    None,
                    None,
                    &interrupted,
                )
            });
        }

        // The media server restarted: every unit is dead.
        let failed = stats.clone();
        let was_reset = RcBlock::new(move |_note: NonNull<NSNotification>| {
            warn!("voice processing: the media services were reset; reopening");
            failed.input_stream_failed.store(true, Ordering::Relaxed);
            failed.output_stream_failed.store(true, Ordering::Relaxed);
        });
        if let Some(name) = reset {
            // SAFETY: as above.
            observers.push(unsafe {
                center.addObserverForName_object_queue_usingBlock(
                    Some(name),
                    None,
                    None,
                    &was_reset,
                )
            });
        }

        Self { observers }
    }
}

impl Drop for SessionWatch {
    fn drop(&mut self) {
        let center = NSNotificationCenter::defaultCenter();
        for observer in self.observers.drain(..) {
            // SAFETY: a token `addObserverForName…` returned, removed once.
            unsafe { center.removeObserver(observer.as_ref()) };
        }
    }
}
