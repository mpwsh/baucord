//! The Rust → Java calls (spec §10.2), through the `jni` crate on
//! whatever thread asks. The `JavaVM` and the `Activity` come from
//! `ndk-context`, which `android-activity` fills in before
//! `android_main` runs — the same road cpal's AAudio host takes to
//! `AudioManager`, so the idioms below are the ones that compile there
//! (jni 0.22: `attach_current_thread` takes a closure, `jni_str!` /
//! `jni_sig!` name methods and signatures).
//!
//! Only permissions for now. The foreground service, the audio mode
//! and route, audio focus and `ACTION_VIEW` (spec §10.2) are added
//! here as their milestones come; nothing else in the host should
//! touch JNI directly.

use jni::{
    Env, JavaVM,
    errors::Result as JniResult,
    objects::{JObject, JObjectArray, JString, JValue},
    sys::jobject,
};

/// `PackageManager.PERMISSION_GRANTED`.
const PERMISSION_GRANTED: i32 = 0;
/// Our one request code; NativeActivity never hands the result back to
/// native code anyway (the next `Resume` re-checks).
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
