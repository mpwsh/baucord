//! The app's audio session, M0's minimum (IOS-SPEC.md §6.1).
//!
//! An iOS app can only record in a recording category; the default,
//! `SoloAmbient`, has no microphone at all, and cpal's iOS host opens
//! its `RemoteIO` unit without touching the category. So the host sets
//! `PlayAndRecord` once at startup and the existing cpal path can open
//! the microphone:
//!
//! - **`DefaultToSpeaker`**: the loudspeaker, not the earpiece — a phone
//!   on the table or at arm's length in a group call, as Discord does.
//!   (Without it `PlayAndRecord` plays to the earpiece and the app
//!   sounds silent.)
//! - **`AllowBluetoothHFP` / `AllowBluetoothA2DP`**: a Bluetooth headset
//!   with its microphone, or as output only.
//!
//! Nothing is activated here: the session becomes active when the first
//! stream starts — which is when other apps' audio stops — and the
//! system asks for microphone permission at that moment (the Info.plist
//! carries `NSMicrophoneUsageDescription`). There is no echo
//! cancellation on this path: that is M3's voice-processing backend,
//! with the `voiceChat` mode, activation on join, interruptions and
//! the route control (spec §6.2–6.5).

use objc2_avf_audio::{
    AVAudioSession, AVAudioSessionCategoryOptions, AVAudioSessionCategoryPlayAndRecord,
    AVAudioSessionModeDefault,
};

/// Put the shared audio session in `PlayAndRecord`. Logged, never
/// fatal: without it voice receives but cannot send.
pub fn configure() {
    // SAFETY: the shared session is a process-wide singleton, valid for
    // the process's lifetime; reading the framework's string constants
    // is reading immutable statics.
    let (session, category, mode) = unsafe {
        (
            AVAudioSession::sharedInstance(),
            AVAudioSessionCategoryPlayAndRecord,
            AVAudioSessionModeDefault,
        )
    };
    let (Some(category), Some(mode)) = (category, mode) else {
        log::warn!("audio session: the SDK's category or mode constant is missing");
        return;
    };
    let options = AVAudioSessionCategoryOptions::DefaultToSpeaker
        | AVAudioSessionCategoryOptions::AllowBluetoothHFP
        | AVAudioSessionCategoryOptions::AllowBluetoothA2DP;
    // SAFETY: valid category, mode and options for the shared session.
    match unsafe { session.setCategory_mode_options_error(category, mode, options) } {
        Ok(()) => log::info!("audio session: play and record (loudspeaker, Bluetooth allowed)"),
        Err(e) => log::warn!("audio session category: {}", e.localizedDescription()),
    }
}
