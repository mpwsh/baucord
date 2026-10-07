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
#[cfg(any(test, feature = "sim"))]
pub mod sim;
pub mod udp;

pub use config::Budget;
pub use driver::{
    DisconnectReason, Driver, DriverHost, Event, InboundDatagram, LinkStats, PeerId, SendError,
    Track, Transmit, UploadId, VideoItem, OWN_TRACK,
};
pub use identity::{ClientKey, Fingerprint, Observation, PinnedVerifier, ServerIdentity, Trust};
pub use protocol::Lane;
#[cfg(unix)]
pub use udp::ReadWaiter;
pub use udp::{RecvOutcome, UdpIo};
