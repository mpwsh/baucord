//! The Java side of Android's voice-call path (`AEC-SPEC.md` §9,
//! `ANDROID-SPEC.md` §6.2), through JNI on whichever thread asks — in
//! practice the voice thread, around stream (re)opens.
//!
//! Android only runs its echo canceller (and the rest of the voice
//! chain) when the capture stream uses the `VoiceCommunication` input
//! preset *and* the phone is in `MODE_IN_COMMUNICATION`; in that mode
//! a voice-communication stream goes to the earpiece unless the speaker
//! is asked for. This file holds those three switches plus the two
//! questions the policy needs answered (what is the sound coming out
//! of; does this phone have a canceller at all). The streams themselves
//! are `aaudio.rs`.
//!
//! Same JNI idioms as `crates/android/src/java.rs` and cpal's `AAudio`
//! host (jni 0.22): `attach_current_thread` with a closure — which
//! also catches a Java exception and hands it back as an error — and
//! `jni_str!` / `jni_sig!` for names. The context is the Activity
//! `android-activity` registered with `ndk-context`.

use jni::{
    errors::Result as JniResult,
    objects::{JObject, JObjectArray, JValue},
    sys::jobject,
    Env, JavaVM,
};
use log::{info, warn};

/// `AudioManager.MODE_NORMAL` / `MODE_IN_COMMUNICATION`.
const MODE_NORMAL: i32 = 0;
const MODE_IN_COMMUNICATION: i32 = 3;
/// `AudioManager.GET_DEVICES_OUTPUTS`.
const GET_DEVICES_OUTPUTS: i32 = 2;

/// Run `f` with a JNI environment and the app's `Context`.
fn with_context<R>(
    f: impl for<'j> FnOnce(&mut Env<'j>, &JObject<'j>) -> JniResult<R>,
) -> Result<R, String> {
    let ctx = ndk_context::android_context();
    // SAFETY: android-activity registered a live JavaVM pointer for the
    // process before `android_main`; `from_raw` only wraps it.
    let vm = unsafe { JavaVM::from_raw(ctx.vm().cast()) };
    let context_raw: jobject = ctx.context().cast();
    vm.attach_current_thread(|env: &mut Env<'_>| {
        // SAFETY: the NativeActivity's own global reference to the
        // Activity, valid while the process runs it.
        let context = unsafe { JObject::from_raw(env, context_raw) };
        f(env, &context)
    })
    .map_err(|e| e.to_string())
}

/// `context.getSystemService("audio")`.
fn audio_manager<'j>(env: &mut Env<'j>, context: &JObject<'j>) -> JniResult<JObject<'j>> {
    let name = env.new_string("audio")?;
    env.call_method(
        context,
        jni::jni_str!("getSystemService"),
        jni::jni_sig!("(Ljava/lang/String;)Ljava/lang/Object;"),
        &[(&name).into()],
    )?
    .l()
}

/// `AudioDeviceInfo.getType()` of every output device the system lists.
///
/// # Errors
/// Any JNI failure, as text.
pub fn output_device_types() -> Result<Vec<i32>, String> {
    with_context(output_types)
}

fn output_types<'j>(env: &mut Env<'j>, context: &JObject<'j>) -> JniResult<Vec<i32>> {
    let manager = audio_manager(env, context)?;
    let devices = env
        .call_method(
            &manager,
            jni::jni_str!("getDevices"),
            jni::jni_sig!("(I)[Landroid/media/AudioDeviceInfo;"),
            &[JValue::Int(GET_DEVICES_OUTPUTS)],
        )?
        .l()?;
    // SAFETY: getDevices returns an AudioDeviceInfo[]; the local
    // reference is re-typed, not duplicated.
    let devices = unsafe { JObjectArray::<JObject<'j>>::from_raw(env, devices.into_raw()) };
    let count = devices.len(env)?;
    let mut types = Vec::with_capacity(count);
    for i in 0..count {
        let device = devices.get_element(env, i)?;
        let kind = env
            .call_method(&device, jni::jni_str!("getType"), jni::jni_sig!("()I"), &[])?
            .i()?;
        types.push(kind);
    }
    Ok(types)
}

/// `AcousticEchoCanceler.isAvailable()`: whether this phone ships an
/// AEC effect at all. Reporting only — a phone that says no still gets
/// the voice-communication path, which may cancel in the HAL anyway.
///
/// # Errors
/// Any JNI failure, as text.
pub fn echo_canceller_available() -> Result<bool, String> {
    with_context(|env, _context| {
        env.call_static_method(
            jni::jni_str!("android/media/audiofx/AcousticEchoCanceler"),
            jni::jni_str!("isAvailable"),
            jni::jni_sig!("()Z"),
            &[],
        )?
        .z()
    })
}

fn set_mode_and_speaker(mode: i32, speaker: bool) -> Result<(), String> {
    with_context(|env, context| {
        let manager = audio_manager(env, context)?;
        env.call_method(
            &manager,
            jni::jni_str!("setMode"),
            jni::jni_sig!("(I)V"),
            &[JValue::Int(mode)],
        )?;
        // Deprecated in API 31 in favour of setCommunicationDevice, and
        // still implemented through it; one call covers 29–36.
        env.call_method(
            &manager,
            jni::jni_str!("setSpeakerphoneOn"),
            jni::jni_sig!("(Z)V"),
            &[JValue::Bool(speaker)],
        )?;
        Ok(())
    })
}

fn current_mode() -> Result<i32, String> {
    with_context(|env, context| {
        let manager = audio_manager(env, context)?;
        env.call_method(
            &manager,
            jni::jni_str!("getMode"),
            jni::jni_sig!("()I"),
            &[],
        )?
        .i()
    })
}

/// The phone in communication mode for as long as this lives: what
/// makes Android run its voice chain (AEC, NS, AGC) on a
/// `VoiceCommunication` capture stream. Entered *before* the streams
/// open (some OEMs pick the chain at open), left after they close —
/// `VoiceChat` owns it next to the streams and drops it last.
pub struct CommunicationMode {
    previous_mode: i32,
}

impl CommunicationMode {
    /// Enter `MODE_IN_COMMUNICATION`; `speaker` routes the call audio to
    /// the loudspeaker instead of the earpiece (a headset, when one is
    /// connected, wins either way).
    ///
    /// # Errors
    /// Any JNI failure, as text; the mode is left as it was.
    pub fn enter(speaker: bool) -> Result<Self, String> {
        let previous_mode = current_mode().unwrap_or(MODE_NORMAL);
        set_mode_and_speaker(MODE_IN_COMMUNICATION, speaker)?;
        info!(
            "Android audio: communication mode on (speaker {}, was mode {previous_mode})",
            if speaker { "on" } else { "off" }
        );
        Ok(Self { previous_mode })
    }
}

impl Drop for CommunicationMode {
    fn drop(&mut self) {
        // Never leave the phone in call mode: other apps' audio and the
        // volume keys follow it.
        let mode = if self.previous_mode == MODE_IN_COMMUNICATION {
            MODE_NORMAL
        } else {
            self.previous_mode
        };
        match set_mode_and_speaker(mode, false) {
            Ok(()) => info!("Android audio: communication mode off (mode {mode})"),
            Err(e) => warn!("Android audio: leaving communication mode failed: {e}"),
        }
    }
}
