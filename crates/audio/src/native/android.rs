//! The Java side of Android's voice-call path (`AEC-SPEC.md` §9,
//! `ANDROID-SPEC.md` §6.2), through JNI on whichever thread asks — in
//! practice the voice thread, around stream (re)opens.
//!
//! Android only runs its echo canceller (and the rest of the voice
//! chain) when the capture stream uses the `VoiceCommunication` input
//! preset *and* the phone is in `MODE_IN_COMMUNICATION`; in that mode
//! everything — the call and media alike, a watched stream's sound
//! included — goes to the communication device, the earpiece unless
//! the app picks another. This file holds the mode, the route
//! (`setCommunicationDevice` from API 31, `setSpeakerphoneOn` before;
//! which device is `crate::route`'s decision) and the questions the
//! policy needs answered (where can the sound go; does this phone have
//! a canceller at all). The streams themselves are `aaudio.rs`.
//!
//! Same JNI idioms as `crates/android/src/java.rs` and cpal's `AAudio`
//! host (jni 0.22): `attach_current_thread` with a closure — which
//! also catches a Java exception and hands it back as an error — and
//! `jni_str!` / `jni_sig!` for names. The context is the Activity
//! `android-activity` registered with `ndk-context`.

use jni::{
    errors::Result as JniResult,
    objects::{JObject, JObjectArray, JString, JValue},
    refs::Reference,
    sys::jobject,
    Env, JavaVM,
};
use log::{info, warn};

use crate::route::{self, RouteDevice};

/// `AudioManager.MODE_NORMAL` / `MODE_IN_COMMUNICATION`.
const MODE_NORMAL: i32 = 0;
const MODE_IN_COMMUNICATION: i32 = 3;
/// `AudioManager.GET_DEVICES_OUTPUTS`.
const GET_DEVICES_OUTPUTS: i32 = 2;
/// `Build.VERSION_CODES.S`: `setCommunicationDevice` and friends.
const API_COMMUNICATION_DEVICE: i32 = 31;

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

/// `Build.VERSION.SDK_INT`.
fn sdk_int(env: &mut Env<'_>) -> JniResult<i32> {
    env.get_static_field(
        jni::jni_str!("android/os/Build$VERSION"),
        jni::jni_str!("SDK_INT"),
        jni::jni_sig!("I"),
    )?
    .i()
}

/// `AudioDeviceInfo` → what routing needs to know of it.
fn route_device<'j>(env: &mut Env<'j>, device: &JObject<'j>) -> JniResult<RouteDevice> {
    let id = env
        .call_method(device, jni::jni_str!("getId"), jni::jni_sig!("()I"), &[])?
        .i()?;
    let kind = env
        .call_method(device, jni::jni_str!("getType"), jni::jni_sig!("()I"), &[])?
        .i()?;
    let product = env
        .call_method(
            device,
            jni::jni_str!("getProductName"),
            jni::jni_sig!("()Ljava/lang/CharSequence;"),
            &[],
        )?
        .l()?;
    let product = if product.is_null() {
        String::new()
    } else {
        let text = env
            .call_method(
                &product,
                jni::jni_str!("toString"),
                jni::jni_sig!("()Ljava/lang/String;"),
                &[],
            )?
            .l()?;
        env.cast_local::<JString>(text)?.try_to_string(env)?
    };
    Ok(RouteDevice { id, kind, product })
}

/// The devices communication audio can be routed to, as Java objects
/// with their description: `getAvailableCommunicationDevices()` from
/// API 31; before it, the output devices of the kinds a call can use.
fn communication_device_objects<'j>(
    env: &mut Env<'j>,
    manager: &JObject<'j>,
    sdk: i32,
) -> JniResult<Vec<(JObject<'j>, RouteDevice)>> {
    let mut out = Vec::new();
    if sdk >= API_COMMUNICATION_DEVICE {
        let list = env
            .call_method(
                manager,
                jni::jni_str!("getAvailableCommunicationDevices"),
                jni::jni_sig!("()Ljava/util/List;"),
                &[],
            )?
            .l()?;
        let count = env
            .call_method(&list, jni::jni_str!("size"), jni::jni_sig!("()I"), &[])?
            .i()?;
        for i in 0..count {
            let device = env
                .call_method(
                    &list,
                    jni::jni_str!("get"),
                    jni::jni_sig!("(I)Ljava/lang/Object;"),
                    &[JValue::Int(i)],
                )?
                .l()?;
            let described = route_device(env, &device)?;
            out.push((device, described));
        }
    } else {
        let devices = env
            .call_method(
                manager,
                jni::jni_str!("getDevices"),
                jni::jni_sig!("(I)[Landroid/media/AudioDeviceInfo;"),
                &[JValue::Int(GET_DEVICES_OUTPUTS)],
            )?
            .l()?;
        // SAFETY: getDevices returns an AudioDeviceInfo[]; the local
        // reference is re-typed, not duplicated.
        let devices = unsafe { JObjectArray::<JObject<'j>>::from_raw(env, devices.into_raw()) };
        for i in 0..devices.len(env)? {
            let device = devices.get_element(env, i)?;
            let described = route_device(env, &device)?;
            if route::output_kind(described.kind) != crate::aec::OutputKind::Unknown {
                out.push((device, described));
            }
        }
    }
    Ok(out)
}

/// Where communication audio can go right now (Settings → Audio →
/// Output on a phone, and the automatic choice).
///
/// # Errors
/// Any JNI failure, as text.
pub fn communication_devices() -> Result<Vec<RouteDevice>, String> {
    with_context(|env, context| {
        let manager = audio_manager(env, context)?;
        let sdk = sdk_int(env)?;
        Ok(communication_device_objects(env, &manager, sdk)?
            .into_iter()
            .map(|(_, d)| d)
            .collect())
    })
}

/// Point communication audio at the device `selection` means
/// (`route::choose`). Returns what it chose, for the log.
fn apply_route<'j>(
    env: &mut Env<'j>,
    manager: &JObject<'j>,
    selection: Option<&str>,
) -> JniResult<String> {
    let sdk = sdk_int(env)?;
    let devices = communication_device_objects(env, manager, sdk)?;
    let described: Vec<RouteDevice> = devices.iter().map(|(_, d)| d.clone()).collect();
    let Some(index) = route::choose(selection, &described) else {
        return Ok("no communication device listed; the system's choice".to_string());
    };
    let chosen = &described[index];
    let name = route::label(chosen);
    if sdk >= API_COMMUNICATION_DEVICE {
        let accepted = env
            .call_method(
                manager,
                jni::jni_str!("setCommunicationDevice"),
                jni::jni_sig!("(Landroid/media/AudioDeviceInfo;)Z"),
                &[(&devices[index].0).into()],
            )?
            .z()?;
        Ok(format!(
            "{name} (type {}, id {}){}",
            chosen.kind,
            chosen.id,
            if accepted {
                ""
            } else {
                " — refused by the system"
            }
        ))
    } else {
        // Before API 31: the loudspeaker by its switch; a wired headset
        // takes over by itself; a Bluetooth headset only once SCO is
        // started (call mode never uses A2DP).
        let speaker = chosen.kind == route::kinds::BUILTIN_SPEAKER;
        let bluetooth = matches!(
            chosen.kind,
            route::kinds::BLUETOOTH_SCO | route::kinds::BLUETOOTH_A2DP
        );
        env.call_method(
            manager,
            jni::jni_str!("setSpeakerphoneOn"),
            jni::jni_sig!("(Z)V"),
            &[JValue::Bool(speaker)],
        )?;
        if bluetooth {
            env.call_method(
                manager,
                jni::jni_str!("startBluetoothSco"),
                jni::jni_sig!("()V"),
                &[],
            )?;
            env.call_method(
                manager,
                jni::jni_str!("setBluetoothScoOn"),
                jni::jni_sig!("(Z)V"),
                &[JValue::Bool(true)],
            )?;
        }
        Ok(format!(
            "{name} (speakerphone {}{})",
            if speaker { "on" } else { "off" },
            if bluetooth { ", Bluetooth SCO" } else { "" }
        ))
    }
}

/// Undo [`apply_route`]: back to the system's own routing.
fn clear_route<'j>(env: &mut Env<'j>, manager: &JObject<'j>) -> JniResult<()> {
    if sdk_int(env)? >= API_COMMUNICATION_DEVICE {
        env.call_method(
            manager,
            jni::jni_str!("clearCommunicationDevice"),
            jni::jni_sig!("()V"),
            &[],
        )?;
    } else {
        env.call_method(
            manager,
            jni::jni_str!("setSpeakerphoneOn"),
            jni::jni_sig!("(Z)V"),
            &[JValue::Bool(false)],
        )?;
        // Harmless when SCO was never started.
        env.call_method(
            manager,
            jni::jni_str!("setBluetoothScoOn"),
            jni::jni_sig!("(Z)V"),
            &[JValue::Bool(false)],
        )?;
        env.call_method(
            manager,
            jni::jni_str!("stopBluetoothSco"),
            jni::jni_sig!("()V"),
            &[],
        )?;
    }
    Ok(())
}

fn set_mode(env: &mut Env<'_>, manager: &JObject<'_>, mode: i32) -> JniResult<()> {
    env.call_method(
        manager,
        jni::jni_str!("setMode"),
        jni::jni_sig!("(I)V"),
        &[JValue::Int(mode)],
    )?;
    Ok(())
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

/// The device communication audio is going to now (API 31+), for the
/// log after the streams open: whether the route held.
///
/// # Errors
/// Any JNI failure, as text.
pub fn current_route() -> Result<String, String> {
    with_context(|env, context| {
        let manager = audio_manager(env, context)?;
        if sdk_int(env)? < API_COMMUNICATION_DEVICE {
            return Ok("unknown before API 31".to_string());
        }
        let device = env
            .call_method(
                &manager,
                jni::jni_str!("getCommunicationDevice"),
                jni::jni_sig!("()Landroid/media/AudioDeviceInfo;"),
                &[],
            )?
            .l()?;
        if device.is_null() {
            return Ok("none".to_string());
        }
        let d = route_device(env, &device)?;
        Ok(format!(
            "{} (type {}, id {})",
            route::label(&d),
            d.kind,
            d.id
        ))
    })
}

/// The phone in communication mode for as long as this lives: what
/// makes Android run its voice chain (AEC, NS, AGC) on a
/// `VoiceCommunication` capture stream. Entered *before* the streams
/// open (some OEMs pick the chain at open), left after they close —
/// `VoiceChat` owns it next to the streams and drops it last.
///
/// In this mode Android sends call audio, and media too (a watched
/// stream's sound), to the communication device, which is the earpiece
/// unless asked otherwise — so entering also picks the route
/// (`route::choose`: the user's choice, else a headset, else the
/// loudspeaker).
pub struct CommunicationMode {
    // Not constructible outside `enter`.
    _entered: (),
}

impl CommunicationMode {
    /// Enter `MODE_IN_COMMUNICATION` and route to what `selection` (the
    /// output picker's choice, `None` = automatic) means. Returns the
    /// mode guard and a description of the route. A route that fails
    /// (a headset gone between listing and setting) leaves the system's
    /// choice and says so; the guard exists either way, so the mode is
    /// always left again.
    ///
    /// # Errors
    /// The mode could not be set (any JNI failure, as text); nothing
    /// changed.
    pub fn enter(selection: Option<&str>) -> Result<(Self, String), String> {
        let previous_mode = current_mode().unwrap_or(MODE_NORMAL);
        with_context(|env, context| {
            let manager = audio_manager(env, context)?;
            set_mode(env, &manager, MODE_IN_COMMUNICATION)
        })?;
        let guard = Self { _entered: () };
        let route = with_context(|env, context| {
            let manager = audio_manager(env, context)?;
            apply_route(env, &manager, selection)
        })
        .unwrap_or_else(|e| format!("the system's choice (routing failed: {e})"));
        info!("Android audio: communication mode on, route {route} (was mode {previous_mode})");
        Ok((guard, route))
    }
}

impl Drop for CommunicationMode {
    fn drop(&mut self) {
        // Back to MODE_NORMAL, never to the mode found on entry: since
        // API 31 the mode is tracked per app, and "restoring" another
        // app's (a ringtone's, a call's) would make us hold it.
        let left = with_context(|env, context| {
            let manager = audio_manager(env, context)?;
            clear_route(env, &manager)?;
            set_mode(env, &manager, MODE_NORMAL)
        });
        match left {
            Ok(()) => info!("Android audio: communication mode off"),
            Err(e) => warn!("Android audio: leaving communication mode failed: {e}"),
        }
    }
}
