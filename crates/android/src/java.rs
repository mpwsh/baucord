//! The Rust → Java calls (spec §10.2), through the `jni` crate on
//! whatever thread asks (jni 0.22: `attach_current_thread` takes a
//! closure, `jni_str!` / `jni_sig!` name methods and signatures).
//!
//! **The Activity comes from `AndroidApp`, not `ndk-context`** ([`init`],
//! from `android_main`): android-activity 0.6 registers the
//! *Application* with ndk-context, which is right for `AudioManager`
//! (cpal, the audio crate) but has neither `requestPermissions` nor our
//! `BaucordActivity` methods — on it the permission prompt never showed
//! and `pickFile` was "method not found" (ANDROID-SPEC.md D33).
//!
//! Permissions, and the file and link helpers `BaucordActivity`
//! (`android/app/src/main/java/dev/baucord/`) carries for `host.rs`:
//! the system file picker (whose answer comes back through the one
//! native method, registered here), the public Downloads, `ACTION_VIEW`.
//! The foreground service and audio focus (spec §10.2) are added here
//! as their milestones come; nothing else in the host should touch JNI
//! directly. (The audio route lives with the audio engine:
//! `audio/src/native/android.rs`.)

use std::{ffi::c_void, sync::OnceLock};

#[cfg(feature = "app")]
use jni::{errors::LogErrorAndDefault, objects::JClass, refs::Reference};
use jni::{
    errors::Result as JniResult,
    objects::{JObject, JObjectArray, JString, JValue},
    sys::jobject,
    Env, JavaVM,
};

/// `PackageManager.PERMISSION_GRANTED`.
const PERMISSION_GRANTED: i32 = 0;
/// Our one request code; the answer is not handed back to native code
/// (the next `Resume` re-checks, and the voice engine retries the
/// microphone every 2 s until it opens).
// Only the smoke test never asks; the imports it shares stay used.
#[cfg_attr(feature = "hello", allow(dead_code))]
const PERMISSION_REQUEST_CODE: i32 = 1;

/// The `JavaVM` and the Activity (a global reference android-activity
/// keeps alive for as long as the `AndroidApp` lives — the whole
/// process, D11), as addresses.
static ACTIVITY: OnceLock<(usize, usize)> = OnceLock::new();

/// Remember the Activity for every later call: `app.vm_as_ptr()` and
/// `app.activity_as_ptr()`, first thing in `android_main`.
pub fn init(vm: *mut c_void, activity: *mut c_void) {
    let _ = ACTIVITY.set((vm as usize, activity as usize));
}

/// Run `f` with a JNI environment and the current Activity.
fn with_activity<R>(
    f: impl for<'j> FnOnce(&mut Env<'j>, &JObject<'j>) -> JniResult<R>,
) -> JniResult<R> {
    let (vm_raw, activity_raw) = ACTIVITY.get().copied().unwrap_or_else(|| {
        // Before `init` (never, in practice): ndk-context's — the
        // Application, which answers the Context methods only.
        let ctx = ndk_context::android_context();
        (ctx.vm() as usize, ctx.context() as usize)
    });
    // SAFETY: a live JavaVM pointer for this process (android-activity's);
    // `from_raw` only wraps it.
    let vm = unsafe { JavaVM::from_raw(vm_raw as *mut jni::sys::JavaVM) };
    vm.attach_current_thread(|env: &mut Env<'_>| {
        // SAFETY: a global reference (android-activity's, see
        // `ACTIVITY`), valid while the process runs; wrapped, not owned.
        let activity = unsafe { JObject::from_raw(env, activity_raw as jobject) };
        f(env, &activity)
    })
}

/// `Activity.checkSelfPermission(name) == PERMISSION_GRANTED` (API 23+).
///
/// # Errors
/// Any JNI failure (a missing method, an exception), as text.
pub fn has_permission(name: &str) -> Result<bool, String> {
    with_activity(|env, activity| {
        let jname = env.new_string(name)?;
        let state = env
            .call_method(
                activity,
                jni::jni_str!("checkSelfPermission"),
                jni::jni_sig!("(Ljava/lang/String;)I"),
                &[(&jname).into()],
            )?
            .i()?;
        Ok(state == PERMISSION_GRANTED)
    })
    .map_err(|e| e.to_string())
}

/// The system's permission prompts for `names` (API 23+), one after
/// the other: `BaucordActivity.askPermissions`, which posts
/// `requestPermissions` to the UI thread (called from here at startup it
/// could land before the Activity is shown); the bare
/// `Activity.requestPermissions` if the Activity is not ours. Returns as
/// soon as the prompt is queued.
///
/// # Errors
/// Any JNI failure, as text.
#[cfg_attr(feature = "hello", allow(dead_code))]
pub fn request_permissions(names: &[&str]) -> Result<(), String> {
    with_activity(|env, activity| {
        // `String[]`, filled with nulls; the element type names the
        // class (`Reference::lookup_class`), as in cpal's device query.
        let array = JObjectArray::<JString>::new(env, names.len(), &JString::default())?;
        for (i, name) in names.iter().enumerate() {
            let jname = env.new_string(name)?;
            array.set_element(env, i, &jname)?;
        }
        let asked = env.call_method(
            activity,
            jni::jni_str!("askPermissions"),
            jni::jni_sig!("([Ljava/lang/String;)V"),
            &[(&array).into()],
        );
        if asked.is_err() {
            env.call_method(
                activity,
                jni::jni_str!("requestPermissions"),
                jni::jni_sig!("([Ljava/lang/String;I)V"),
                &[(&array).into(), JValue::Int(PERMISSION_REQUEST_CODE)],
            )?;
        }
        Ok(())
    })
    .map_err(|e| e.to_string())
}

/// `BaucordActivity.startVoiceService(channel, title, text)`: the
/// foreground service and its notification (D34). `Ok(false)` when the
/// system refused (asked from the background).
///
/// # Errors
/// Any JNI failure, as text.
#[cfg(feature = "app")]
pub fn start_voice_service(channel: &str, title: &str, text: &str) -> Result<bool, String> {
    with_activity(|env, activity| {
        let jchannel = env.new_string(channel)?;
        let jtitle = env.new_string(title)?;
        let jtext = env.new_string(text)?;
        env.call_method(
            activity,
            jni::jni_str!("startVoiceService"),
            jni::jni_sig!("(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;)Z"),
            &[(&jchannel).into(), (&jtitle).into(), (&jtext).into()],
        )?
        .z()
    })
    .map_err(|e| e.to_string())
}

/// `BaucordActivity.stopVoiceService()`.
///
/// # Errors
/// Any JNI failure, as text.
#[cfg(feature = "app")]
pub fn stop_voice_service() -> Result<(), String> {
    with_activity(|env, activity| {
        env.call_method(
            activity,
            jni::jni_str!("stopVoiceService"),
            jni::jni_sig!("()V"),
            &[],
        )?;
        Ok(())
    })
    .map_err(|e| e.to_string())
}

/// `BaucordActivity.keepScreenOn(on)`: the window's keep-screen-on flag
/// while a stream is watched (D45).
///
/// # Errors
/// Any JNI failure, as text.
#[cfg(feature = "app")]
pub fn keep_screen_on(on: bool) -> Result<(), String> {
    with_activity(|env, activity| {
        env.call_method(
            activity,
            jni::jni_str!("keepScreenOn"),
            jni::jni_sig!("(Z)V"),
            &[JValue::Bool(on)],
        )?;
        Ok(())
    })
    .map_err(|e| e.to_string())
}

/// `BaucordActivity.copyText(label, text)`: `text` onto the clipboard.
/// `Ok(false)`: the system refused it.
///
/// # Errors
/// Any JNI failure, as text.
pub fn copy_text(label: &str, text: &str) -> Result<bool, String> {
    with_activity(|env, activity| {
        let jlabel = env.new_string(label)?;
        let jtext = env.new_string(text)?;
        env.call_method(
            activity,
            jni::jni_str!("copyText"),
            jni::jni_sig!("(Ljava/lang/String;Ljava/lang/String;)Z"),
            &[(&jlabel).into(), (&jtext).into()],
        )?
        .z()
    })
    .map_err(|e| e.to_string())
}

// ── Files and links (BaucordActivity, ANDROID-SPEC.md D30) ─────────────

/// `BaucordActivity.nativeFilePicked(String path, String name, String
/// error)`: the end of a pick, on whichever Java thread finished it.
/// Registered from here rather than exported by name: `NativeActivity`
/// loads this library itself, so the Activity's class loader would not
/// find a `Java_…` symbol in it.
#[cfg(feature = "app")]
const FILE_PICKED: jni::NativeMethod = jni::native_method! {
    static fn native_file_picked(path: JString, name: JString, error: JString),
    fn = file_picked,
    // (An identifier in scope: the macro takes no path here.)
    error_policy = LogErrorAndDefault,
};

#[cfg(feature = "app")]
// The JNI signature hands the strings over by value.
#[allow(clippy::needless_pass_by_value)]
fn file_picked<'local>(
    env: &mut Env<'local>,
    _class: JClass<'local>,
    path: JString<'local>,
    name: JString<'local>,
    error: JString<'local>,
) -> JniResult<()> {
    let path = optional_string(env, &path)?;
    let name = optional_string(env, &name)?;
    let error = optional_string(env, &error)?;
    crate::host::picked(path, name, error);
    Ok(())
}

/// A Java string argument, `None` for null.
#[cfg(feature = "app")]
fn optional_string(env: &Env<'_>, s: &JString<'_>) -> JniResult<Option<String>> {
    if s.is_null() {
        Ok(None)
    } else {
        s.try_to_string(env).map(Some)
    }
}

/// `BaucordActivity.nativeOpenConversation(String dm)`: a message
/// notification was tapped while the app runs (D52).
#[cfg(feature = "app")]
const OPEN_CONVERSATION: jni::NativeMethod = jni::native_method! {
    static fn native_open_conversation(dm: JString),
    fn = open_conversation,
    error_policy = LogErrorAndDefault,
};

#[cfg(feature = "app")]
// The JNI signature hands the string over by value.
#[allow(clippy::needless_pass_by_value)]
fn open_conversation<'local>(
    env: &mut Env<'local>,
    _class: JClass<'local>,
    dm: JString<'local>,
) -> JniResult<()> {
    if let Some(dm) = optional_string(env, &dm)? {
        frontend::host::open_conversation(dm);
    }
    Ok(())
}

/// Bind `nativeFilePicked` and `nativeOpenConversation` on the running
/// Activity's class.
///
/// # Errors
/// The Activity is not a `BaucordActivity` (an old manifest), or any
/// JNI failure, as text.
#[cfg(feature = "app")]
pub fn register_natives() -> Result<(), String> {
    with_activity(|env, activity| {
        let class = env.get_object_class(activity)?;
        // SAFETY: both are built by `native_method!`, which checks the
        // Rust function against the declared static signature.
        unsafe { env.register_native_methods(&class, &[FILE_PICKED, OPEN_CONVERSATION]) }
    })
    .map_err(|e| e.to_string())
}

// ── Notifications (BaucordActivity → Notices, ANDROID-SPEC.md D52) ─────

/// `BaucordActivity.postNotice(kind, key, channelName, title, text, dm,
/// sound)`: a message or voice-activity notification. `dm` empty = a tap
/// only brings the app back.
///
/// # Errors
/// Any JNI failure, as text.
#[cfg(feature = "app")]
pub fn post_notice(notice: &frontend::notice::Notice) -> Result<(), String> {
    let kind = match notice.kind {
        frontend::notice::NoticeKind::Message => "message",
        frontend::notice::NoticeKind::Activity => "activity",
    };
    with_activity(|env, activity| {
        let jkind = env.new_string(kind)?;
        let jkey = env.new_string(&notice.key)?;
        let jchannel = env.new_string(&notice.channel)?;
        let jtitle = env.new_string(&notice.title)?;
        let jtext = env.new_string(&notice.body)?;
        let jdm = env.new_string(notice.open_dm.as_deref().unwrap_or_default())?;
        env.call_method(
            activity,
            jni::jni_str!("postNotice"),
            jni::jni_sig!(
                "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;\
                 Ljava/lang/String;Ljava/lang/String;Z)V"
            ),
            &[
                (&jkind).into(),
                (&jkey).into(),
                (&jchannel).into(),
                (&jtitle).into(),
                (&jtext).into(),
                (&jdm).into(),
                JValue::Bool(notice.sound),
            ],
        )?;
        Ok(())
    })
    .map_err(|e| e.to_string())
}

/// `BaucordActivity.noticesAllowed(kind)`: notifications allowed for
/// the app and the kind's channel not switched off.
///
/// # Errors
/// Any JNI failure, as text.
#[cfg(feature = "app")]
pub fn notices_allowed(kind: frontend::notice::NoticeKind) -> Result<bool, String> {
    let kind = match kind {
        frontend::notice::NoticeKind::Message => "message",
        frontend::notice::NoticeKind::Activity => "activity",
    };
    with_activity(|env, activity| {
        let jkind = env.new_string(kind)?;
        env.call_method(
            activity,
            jni::jni_str!("noticesAllowed"),
            jni::jni_sig!("(Ljava/lang/String;)Z"),
            &[(&jkind).into()],
        )?
        .z()
    })
    .map_err(|e| e.to_string())
}

/// `BaucordActivity.playChime(wav)`: the message chime as a
/// notification sound, while no voice stream is open (D53).
///
/// # Errors
/// Any JNI failure, as text.
#[cfg(feature = "app")]
pub fn play_chime(wav: &[u8]) -> Result<(), String> {
    with_activity(|env, activity| {
        let jwav = env.byte_array_from_slice(wav)?;
        env.call_method(
            activity,
            jni::jni_str!("playChime"),
            jni::jni_sig!("([B)V"),
            &[(&jwav).into()],
        )?;
        Ok(())
    })
    .map_err(|e| e.to_string())
}

/// `BaucordActivity.cancelNotice(key)`.
///
/// # Errors
/// Any JNI failure, as text.
#[cfg(feature = "app")]
pub fn cancel_notice(key: &str) -> Result<(), String> {
    with_activity(|env, activity| {
        let jkey = env.new_string(key)?;
        env.call_method(
            activity,
            jni::jni_str!("cancelNotice"),
            jni::jni_sig!("(Ljava/lang/String;)V"),
            &[(&jkey).into()],
        )?;
        Ok(())
    })
    .map_err(|e| e.to_string())
}

/// `BaucordActivity.chimeAllowed()`: the ringer is on and Do Not
/// Disturb is off.
///
/// # Errors
/// Any JNI failure, as text.
#[cfg(feature = "app")]
pub fn chime_allowed() -> Result<bool, String> {
    with_activity(|env, activity| {
        env.call_method(
            activity,
            jni::jni_str!("chimeAllowed"),
            jni::jni_sig!("()Z"),
            &[],
        )?
        .z()
    })
    .map_err(|e| e.to_string())
}

/// `BaucordActivity.pickFile(mime, maxBytes)`: the system picker comes
/// up; the answer arrives through `nativeFilePicked`.
///
/// # Errors
/// Any JNI failure, as text.
#[cfg(feature = "app")]
pub fn pick_file(mime: &str, max_bytes: u64) -> Result<(), String> {
    with_activity(|env, activity| {
        let jmime = env.new_string(mime)?;
        env.call_method(
            activity,
            jni::jni_str!("pickFile"),
            jni::jni_sig!("(Ljava/lang/String;J)V"),
            &[
                (&jmime).into(),
                JValue::Long(i64::try_from(max_bytes).unwrap_or(i64::MAX)),
            ],
        )?;
        Ok(())
    })
    .map_err(|e| e.to_string())
}

/// `BaucordActivity.saveDownload(name, mime, bytes)`: the file in the
/// public Downloads. Returns its `content://` URI and the name the user
/// will find it under (`Download/report (1).pdf`).
///
/// # Errors
/// What `MediaStore` said, or any JNI failure, as text.
#[cfg(feature = "app")]
pub fn save_download(name: &str, mime: &str, bytes: &[u8]) -> Result<(String, String), String> {
    let reply = with_activity(|env, activity| {
        let jname = env.new_string(name)?;
        let jmime = env.new_string(mime)?;
        let jbytes = env.byte_array_from_slice(bytes)?;
        let out = env
            .call_method(
                activity,
                jni::jni_str!("saveDownload"),
                jni::jni_sig!("(Ljava/lang/String;Ljava/lang/String;[B)Ljava/lang/String;"),
                &[(&jname).into(), (&jmime).into(), (&jbytes).into()],
            )?
            .l()?;
        let out = env.cast_local::<JString>(out)?;
        out.try_to_string(env)
    })
    .map_err(|e| e.to_string())?;
    match reply.split_once('\n') {
        Some(("error", message)) => Err(message.to_string()),
        Some((uri, shown)) => Ok((uri.to_string(), shown.to_string())),
        None => Err(reply),
    }
}

/// `BaucordActivity.openUri(uri, mime)`: `ACTION_VIEW` on a saved
/// download. `Ok(false)` when no installed app opens that type.
///
/// # Errors
/// Any JNI failure, as text.
#[cfg(feature = "app")]
pub fn open_uri(uri: &str, mime: &str) -> Result<bool, String> {
    with_activity(|env, activity| {
        let juri = env.new_string(uri)?;
        let jmime = env.new_string(mime)?;
        env.call_method(
            activity,
            jni::jni_str!("openUri"),
            jni::jni_sig!("(Ljava/lang/String;Ljava/lang/String;)Z"),
            &[(&juri).into(), (&jmime).into()],
        )?
        .z()
    })
    .map_err(|e| e.to_string())
}

/// `BaucordActivity.uriExists(uri)`: a saved download is still there.
///
/// # Errors
/// Any JNI failure, as text.
#[cfg(feature = "app")]
pub fn uri_exists(uri: &str) -> Result<bool, String> {
    with_activity(|env, activity| {
        let juri = env.new_string(uri)?;
        env.call_method(
            activity,
            jni::jni_str!("uriExists"),
            jni::jni_sig!("(Ljava/lang/String;)Z"),
            &[(&juri).into()],
        )?
        .z()
    })
    .map_err(|e| e.to_string())
}

/// `BaucordActivity.openUrl(url)`: the browser. `Ok(false)` when
/// nothing handles the link.
///
/// # Errors
/// Any JNI failure, as text.
#[cfg(feature = "app")]
pub fn open_url(url: &str) -> Result<bool, String> {
    with_activity(|env, activity| {
        let jurl = env.new_string(url)?;
        env.call_method(
            activity,
            jni::jni_str!("openUrl"),
            jni::jni_sig!("(Ljava/lang/String;)Z"),
            &[(&jurl).into()],
        )?
        .z()
    })
    .map_err(|e| e.to_string())
}
