//! baucord transport: QUIC without a runtime.
//!
//! `quinn-proto` is a pure state machine; this crate is the glue that
//! turns it into the `send(peer, lane, bytes)` API both ends program
//! against (spec §4), driven from the caller's own thread:
//!
//! - [`driver::Driver`] — endpoint, connections, timers, lane mapping
//!   (sans-IO: `now` in, datagrams in, datagrams out).
//! - [`udp::UdpIo`] — the real socket (`quinn-udp`, nonblocking).
//! - [`identity`] — the server's Ed25519 key, self-signed certificate
//!   and successor proofs; the client's pinning verifier.
//! - [`config::Budget`] — the §4.5 buffer budgets per side.
//! - [`sim`] (feature `sim`) — the in-process network harness.

pub mod config;
mod der;
pub mod driver;
pub mod identity;
pub mod sha256;
#[cfg(any(test, feature = "sim"))]
pub mod sim;
pub mod udp;

pub use config::Budget;
pub use driver::{
    frame_media, DisconnectReason, Driver, DriverHost, Event, InboundDatagram, LinkStats,
    MediaStream, PeerId, SendError, Shared, Track, TrackStats, Transmit, UploadId, VideoItem,
    WaitingRoom, OWN_TRACK,
};
#[cfg(any(test, feature = "server"))]
pub use identity::ServerIdentity;
pub use identity::{ClientKey, Fingerprint, Observation, PinnedVerifier, Trust};
pub use protocol::Lane;
pub use udp::{ReadWaiter, RecvOutcome, UdpIo};
