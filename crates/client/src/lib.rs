//! baucord core — the UI-free client engine.
//!
//! Everything a frontend needs to run a baucord session: settings and
//! persistence ([`config`]), the network thread ([`network`]), the
//! voice-engine bridge ([`voice`]) and the screen-share thread
//! ([`screen`]).
//!
//! Frontends talk to the worker threads exclusively through their
//! control / event channels and inject a wake callback, so this crate
//! depends on no UI toolkit. The Slint desktop app lives in the sibling
//! `desktop` crate; a future mobile shell sits on the same seam.

use std::sync::Arc;

pub mod config;
mod inbound_wake;
pub mod network;
pub mod screen;
mod viewlog;
pub mod voice;

/// A callback the worker threads invoke to wake the UI event loop.
/// Injected at construction so this crate never imports a UI toolkit
/// (the desktop shell hands in a `slint::invoke_from_event_loop`
/// nudge; a headless test can pass a no-op).
///
/// Lives at the crate root because all three worker modules use it.
pub type WakeFn = Arc<dyn Fn() + Send + Sync>;

/// Wire budget for a chat image/GIF payload, re-exported so the UI
/// layer enforces the same cap the codec does — the numbers can't
/// drift because they are the same constant.
pub use protocol;
pub use protocol::{
    admin, invite, permissions, AdminCommand, AdminError, AdminOutcome, AdminTarget,
    ChannelDenyReason, ChannelInfo, ChannelSpec, ChannelUiDenyReason, ConvRef, DmFailReason,
    DmSummary, HistoryMessage, InviteDenyReason, InviteInvalidReason, NeedInviteReason,
    RefusedReason, RemovedKind, RestrictionKind, RoleId, ShareScope, UserId, MAX_CHAT_IMAGE_BYTES,
};
