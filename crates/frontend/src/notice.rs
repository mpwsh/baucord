//! What the app asks a phone's notification system for (ANDROID-SPEC.md
//! D52, IOS-SPEC.md D50): the types both `host` twins share. The policy —
//! when a DM or a starting call becomes a notification, and how an alert
//! chime plays — is `app::alerts`; the hosts only carry it out.

/// Which kind of notification: each is its own channel in Android's
/// settings, so a user can silence one and keep the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeKind {
    /// A direct message (and the iOS sound-only notice for a chime).
    Message,
    /// Voice activity: someone started a call.
    Activity,
}

/// One notification.
#[derive(Debug, Clone)]
pub struct Notice {
    pub kind: NoticeKind,
    /// Identifies the notification: a newer one with the same key
    /// replaces the older (one per DM conversation, `dm:<user hex>`;
    /// one for the call starting, `join`), and `host::cancel_notice`
    /// takes it down.
    pub key: String,
    /// The channel's name in the system settings (Android).
    pub channel: String,
    pub title: String,
    pub body: String,
    /// A tap opens this DM conversation (the user id in hex).
    pub open_dm: Option<String>,
    /// Whether it may make a sound at all; the phone's own modes
    /// (silent, vibrate, Do Not Disturb, Focus) still decide.
    pub sound: bool,
    /// The app is on screen and only the sound is wanted — how iOS
    /// plays an alert chime under its ring/silent switch
    /// ([`ChimeRoute::System`]). Android never gets one.
    pub on_screen: bool,
}

/// How an alert chime (a DM, a mention) plays on this host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChimeRoute {
    /// Through the voice engine's mix: the desktop, and a phone whose
    /// ringer is on.
    App,
    /// Not at all: the phone is on silent or vibrate, or in Do Not
    /// Disturb (Android).
    Muted,
    /// Through the system, as a sound-only notice
    /// ([`Notice::on_screen`]): iOS, where nothing else honours the
    /// ring/silent switch — the voice session's audio ignores it.
    System,
}
