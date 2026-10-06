//! The Rust → Java calls (spec §10.2), through the `jni` crate on
//! whatever thread asks. The `JavaVM` and the `Activity` come from
//! `ndk-context`, which `android-activity` fills in before
//! `android_main` runs — the same road cpal's `AAudio` host takes to
//! `AudioManager`, so the idioms below are the ones that compile there
//! (jni 0.22: `attach_current_thread` takes a closure, `jni_str!` /
//! `jni_sig!` name methods and signatures).
//!
//! Permissions, and the file and link helpers `BaucordActivity`
//! (`android/app/src/main/java/dev/baucord/`) carries for `host.rs`:
//! the system file picker (whose answer comes back through the one
//! native method, registered here), the public Downloads, `ACTION_VIEW`.
//! The foreground service and audio focus (spec §10.2) are added here
//! as their milestones come; nothing else in the host should touch JNI
//! directly. (The audio route lives with the audio engine:
//! `audio/src/native/android.rs`.)

use jni::{
    Env, JavaVM,
    errors::Result as JniResult,
    objects::{JObject, JObjectArray, JString, JValue},
    sys::jobject,
};
#[cfg(feature = "app")]
use jni::{errors::LogErrorAndDefault, objects::JClass, refs::Reference};

/// `PackageManager.PERMISSION_GRANTED`.
const PERMISSION_GRANTED: i32 = 0;
/// Our one request code; `NativeActivity` never hands the result back to
/// native code anyway (the next `Resume` re-checks).
// Only the smoke test never asks; the imports it shares stay used.
#[cfg_attr(feature = "hello", allow(dead_code))]
const PERMISSION_REQUEST_CODE: i32 = 1;

/// Run `f` with a JNI environment and the current Activity.
fn with_activity<R>(
    f: impl for<'j> FnOnce(&mut Env<'j>, &JObject<'j>) -> JniResult<R>,
) -> JniResult<R> {
    let ctx = ndk_context::android_context();
    // SAFETY: android-activity registered a live JavaVM pointer for
    // this process; `from_raw` only wraps it.
    let vm = unsafe { JavaVM::from_raw(ctx.vm().cast()) };
    let activity_raw: jobject = ctx.context().cast();
    vm.attach_current_thread(|env: &mut Env<'_>| {
        // SAFETY: the context pointer is the NativeActivity's own
        // global reference to the Activity, valid while it lives.
        let activity = unsafe { JObject::from_raw(env, activity_raw) };
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

/// `Activity.requestPermissions(String[], int)`: one system dialog for
/// all of `names` (API 23+). Returns as soon as the dialog is queued.
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
        env.call_method(
            activity,
            jni::jni_str!("requestPermissions"),
            jni::jni_sig!("([Ljava/lang/String;I)V"),
            &[(&array).into(), JValue::Int(PERMISSION_REQUEST_CODE)],
        )?;
        Ok(())
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

/// Bind `nativeFilePicked` on the running Activity's class.
///
/// # Errors
/// The Activity is not a `BaucordActivity` (an old manifest), or any
/// JNI failure, as text.
#[cfg(feature = "app")]
pub fn register_natives() -> Result<(), String> {
    with_activity(|env, activity| {
        let class = env.get_object_class(activity)?;
        // SAFETY: FILE_PICKED is built by `native_method!`, which checks
        // the Rust function against the declared static signature.
        unsafe { env.register_native_methods(&class, &[FILE_PICKED]) }
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
