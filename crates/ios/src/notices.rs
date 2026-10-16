//! Notifications on iOS (IOS-SPEC.md §14 D50): `UserNotifications`'
//! local notifications behind the frontend's `notify` hook — a direct
//! message or a call starting while the app is in the background, and
//! the message chime while it is on screen.
//!
//! That last one is why the chime goes through here at all: the voice
//! session's audio (`PlayAndRecord`) ignores the ring/silent switch, and
//! no public API reads the switch, so an on-screen chime is posted as a
//! notification the delegate presents with its sound only — and iOS
//! applies the switch and Focus to it as to any notification. Such a
//! notice's identifier starts with [`ON_SCREEN`]. The sounds are the
//! app's own chimes (a DM's, a mention's), written once per launch into
//! `Library/Sounds`, where notification sounds are looked up.
//!
//! The permission is asked on the first connect (`keep_alive`), not at
//! launch: by then the user knows what the app is for. What the user
//! answered (and whether sounds are on for the app) is read back at
//! launch, after the prompt and on every return to the foreground: while
//! notifications or their sounds are off, the chime goes back to the
//! voice engine ([`chime_route`]) rather than vanishing. Local
//! notifications only — no push entitlement, no `aps-environment`.

use std::{
    path::Path,
    ptr::NonNull,
    sync::atomic::{AtomicBool, Ordering},
};

use block2::{DynBlock, RcBlock};
use frontend::host::{ChimeRoute, Notice};
use objc2::{
    define_class, msg_send,
    rc::Retained,
    runtime::{Bool, NSObject, NSObjectProtocol, ProtocolObject},
    AllocAnyThread,
};
use objc2_foundation::{NSArray, NSError, NSString};
use objc2_user_notifications::{
    UNAuthorizationOptions, UNAuthorizationStatus, UNMutableNotificationContent, UNNotification,
    UNNotificationPresentationOptions, UNNotificationRequest, UNNotificationResponse,
    UNNotificationSetting, UNNotificationSettings, UNNotificationSound, UNUserNotificationCenter,
    UNUserNotificationCenterDelegate,
};

/// The identifier prefix of a sound-only notice (the app is on screen).
const ON_SCREEN: &str = "fg:";

/// The chimes' files in `Library/Sounds`, by tone: a mention rang
/// with the DM's chime (review 2026-10, P3-12).
fn sound_file(tone: frontend::host::Tone) -> String {
    format!("baucord-{}.wav", tone.name())
}

/// Whether the [`sound_file`]s were written this launch (else the system's
/// default sound plays).
static OWN_SOUND: AtomicBool = AtomicBool::new(false);

/// Whether the permission has been asked this launch.
static ASKED: AtomicBool = AtomicBool::new(false);

/// Whether notifications show (the user allowed them), and whether they
/// may sound — as last read by [`refresh`].
static ALLOWED: AtomicBool = AtomicBool::new(false);
static SOUNDS: AtomicBool = AtomicBool::new(false);

define_class!(
    // SAFETY: NSObject has no subclassing requirements; no `Drop`. Any
    // thread: the system calls the delegate on a queue of its choosing,
    // and both methods only hand off (`open_conversation` hops to the
    // UI itself) and call the completion handler.
    #[unsafe(super(NSObject))]
    struct NoticeDelegate;

    unsafe impl NSObjectProtocol for NoticeDelegate {}

    unsafe impl UNUserNotificationCenterDelegate for NoticeDelegate {
        /// A notice arriving while the app is on screen: a chime is
        /// played and not shown; anything else (posted in the
        /// background a moment before the app came back) shows.
        #[unsafe(method(userNotificationCenter:willPresentNotification:withCompletionHandler:))]
        fn will_present(
            &self,
            _center: &UNUserNotificationCenter,
            notification: &UNNotification,
            completion: &DynBlock<dyn Fn(UNNotificationPresentationOptions)>,
        ) {
            let id = notification.request().identifier().to_string();
            let options = if id.starts_with(ON_SCREEN) {
                UNNotificationPresentationOptions::Sound
            } else {
                UNNotificationPresentationOptions::Banner
                    | UNNotificationPresentationOptions::List
                    | UNNotificationPresentationOptions::Sound
            };
            completion.call((options,));
        }

        /// A tap: a message notice opens its conversation (the app is
        /// running — a notice only exists while it is connected).
        #[unsafe(method(userNotificationCenter:didReceiveNotificationResponse:withCompletionHandler:))]
        fn did_receive(
            &self,
            _center: &UNUserNotificationCenter,
            response: &UNNotificationResponse,
            completion: &DynBlock<dyn Fn()>,
        ) {
            let id = response.notification().request().identifier().to_string();
            let key = id.strip_prefix(ON_SCREEN).unwrap_or(&id);
            if let Some(dm) = key.strip_prefix("dm:") {
                frontend::host::open_conversation(dm.to_string());
            }
            completion.call(());
        }
    }
);

impl NoticeDelegate {
    fn new() -> Retained<Self> {
        let this = Self::alloc().set_ivars(());
        // SAFETY: NSObject's designated initialiser.
        unsafe { msg_send![super(this), init] }
    }
}

/// Become the notification center's delegate (before the app runs, so
/// no presentation is missed) and write the chime where notification
/// sounds are found. `config` is `Library/Application Support/baucord`.
pub fn install(config: Option<&Path>) {
    let delegate = NoticeDelegate::new();
    UNUserNotificationCenter::currentNotificationCenter()
        .setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
    // The delegate is a weak property: it lives as long as the process.
    std::mem::forget(delegate);

    let Some(sounds) = config
        .and_then(Path::parent)
        .and_then(Path::parent)
        .map(|library| library.join("Sounds"))
    else {
        return;
    };
    refresh();
    let written = std::fs::create_dir_all(&sounds).is_ok()
        && [
            frontend::host::Tone::DirectMessage,
            frontend::host::Tone::Mention,
        ]
        .into_iter()
        .all(|tone| {
            let wav = frontend::host::chime_wav(tone);
            !wav.is_empty() && std::fs::write(sounds.join(sound_file(tone)), wav).is_ok()
        });
    OWN_SOUND.store(written, Ordering::Relaxed);
    if !written {
        log::warn!(
            "notices: no chime file in {}; the default sound plays",
            sounds.display()
        );
    }
}

/// Ask for permission to notify, once a launch (iOS shows its prompt the
/// first time only and answers from the user's choice after that).
pub fn request_permission() {
    if ASKED.swap(true, Ordering::Relaxed) {
        return;
    }
    let done = RcBlock::new(|granted: Bool, error: *mut NSError| {
        refresh();
        if granted.as_bool() {
            log::info!("notices: allowed");
        } else if error.is_null() {
            log::info!("notices: not allowed (Settings › baucord › Notifications)");
        } else {
            // SAFETY: a non-null error is a valid NSError for the call.
            let error = unsafe { &*error };
            log::warn!(
                "notices: permission request failed: {}",
                error.localizedDescription()
            );
        }
    });
    UNUserNotificationCenter::currentNotificationCenter()
        .requestAuthorizationWithOptions_completionHandler(
            UNAuthorizationOptions::Alert | UNAuthorizationOptions::Sound,
            &done,
        );
}

/// Read what the user allows (Settings › baucord › Notifications) into
/// [`ALLOWED`] and [`SOUNDS`]; the answer arrives a moment later.
pub fn refresh() {
    let done = RcBlock::new(|settings: NonNull<UNNotificationSettings>| {
        // SAFETY: the framework hands a valid settings object to the
        // completion handler.
        let settings = unsafe { settings.as_ref() };
        let status = settings.authorizationStatus();
        let allowed = status == UNAuthorizationStatus::Authorized
            || status == UNAuthorizationStatus::Provisional
            || status == UNAuthorizationStatus::Ephemeral;
        let sounds = allowed && settings.soundSetting() == UNNotificationSetting::Enabled;
        let was = ALLOWED.swap(allowed, Ordering::Relaxed);
        let was_sounds = SOUNDS.swap(sounds, Ordering::Relaxed);
        if was != allowed || was_sounds != sounds {
            log::info!("notices: allowed {allowed}, sounds {sounds}");
        }
    });
    UNUserNotificationCenter::currentNotificationCenter()
        .getNotificationSettingsWithCompletionHandler(&done);
}

/// Whether a notification would show.
pub fn allowed() -> bool {
    ALLOWED.load(Ordering::Relaxed)
}

/// Through the system while its notifications may sound — the only
/// route that honours the ring/silent switch; else the voice engine, as
/// before notifications existed (a chime that ignores the switch beats
/// none).
pub fn chime_route() -> ChimeRoute {
    if ALLOWED.load(Ordering::Relaxed) && SOUNDS.load(Ordering::Relaxed) {
        ChimeRoute::System
    } else {
        ChimeRoute::App
    }
}

/// Post `notice` (replacing one with the same key).
pub fn post(notice: &Notice) {
    let content = UNMutableNotificationContent::new();
    content.setTitle(&NSString::from_str(&notice.title));
    content.setBody(&NSString::from_str(&notice.body));
    content.setThreadIdentifier(&NSString::from_str(&notice.key));
    if notice.sound {
        let sound = if OWN_SOUND.load(Ordering::Relaxed) {
            UNNotificationSound::soundNamed(&NSString::from_str(&sound_file(notice.tone)))
        } else {
            UNNotificationSound::defaultSound()
        };
        content.setSound(Some(&sound));
    }
    let id = if notice.on_screen {
        format!("{ON_SCREEN}{}", notice.key)
    } else {
        notice.key.clone()
    };
    let request = UNNotificationRequest::requestWithIdentifier_content_trigger(
        &NSString::from_str(&id),
        &content,
        None,
    );
    let key = notice.key.clone();
    let done = RcBlock::new(move |error: *mut NSError| {
        if let Some(error) = NonNull::new(error) {
            // SAFETY: a non-null error is a valid NSError for the call.
            let error = unsafe { error.as_ref() };
            log::warn!(
                "notices: {key} not posted: {}",
                error.localizedDescription()
            );
        }
    });
    UNUserNotificationCenter::currentNotificationCenter()
        .addNotificationRequest_withCompletionHandler(&request, Some(&done));
}

/// Take down what was posted under `key`, shown or still pending.
pub fn cancel(key: &str) {
    let ids = NSArray::from_retained_slice(&[
        NSString::from_str(key),
        NSString::from_str(&format!("{ON_SCREEN}{key}")),
    ]);
    let center = UNUserNotificationCenter::currentNotificationCenter();
    center.removeDeliveredNotificationsWithIdentifiers(&ids);
    center.removePendingNotificationRequestsWithIdentifiers(&ids);
}
