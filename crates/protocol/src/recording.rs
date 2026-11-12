//! Voice channel recording (1.5): the types its messages carry.
//!
//! A recording is made by the server from the voice it relays in one
//! channel, mixed to one mono WAV, and given to everyone who was in the
//! channel while it ran — and to nobody else. Those people find it in a
//! "Server" conversation of their own ([`RecordingItem`]); the bytes
//! are served to them alone. The messages are under the `RECORDING`
//! feature bit ([`crate::features::RECORDING`]); starting and stopping
//! needs the `record` permission ([`crate::permissions::RECORD`]).

use serde::{Deserialize, Serialize};

use crate::messages::BlobId;

/// The label every language shows on a recording channel. Not
/// translated: it is a mark, like LIVE.
pub const REC_LABEL: &str = "REC";

/// The longest recording the server will make, in minutes: a WAV of
/// 16-bit mono at 48 kHz reaches its 4 GiB limit at 12 h 25 min.
pub const MAX_RECORDING_MINUTES: u32 = 720;

/// Why a recording ended. The discriminants are the database
/// encoding; never renumber.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RecordingStopReason {
    /// Someone with `record` stopped it.
    Stopped = 0,
    /// The last member left the channel.
    Empty = 1,
    /// `recording.max_minutes` was reached.
    TimeLimit = 2,
    /// The recordings' disk cap was reached, or a write failed.
    StorageFull = 3,
    /// The channel was deleted or dropped by a config reload.
    ChannelGone = 4,
    /// The server shut down.
    ServerRestart = 5,
    /// The server stopped without closing it (a crash); the file was
    /// repaired at the next start.
    Interrupted = 6,
}

impl RecordingStopReason {
    /// The database encoding.
    #[must_use]
    pub const fn code(self) -> u8 {
        self as u8
    }

    /// The reason a database code stands for; `Interrupted` for one
    /// this build does not know.
    #[must_use]
    pub const fn from_code(code: u8) -> Self {
        match code {
            0 => Self::Stopped,
            1 => Self::Empty,
            2 => Self::TimeLimit,
            3 => Self::StorageFull,
            4 => Self::ChannelGone,
            5 => Self::ServerRestart,
            _ => Self::Interrupted,
        }
    }

    /// The reason as the log and the audit write it.
    #[must_use]
    pub const fn describe(self) -> &'static str {
        match self {
            Self::Stopped => "stopped",
            Self::Empty => "the channel emptied",
            Self::TimeLimit => "the time limit",
            Self::StorageFull => "storage full",
            Self::ChannelGone => "the channel went away",
            Self::ServerRestart => "server restart",
            Self::Interrupted => "interrupted",
        }
    }
}

/// Why a `StartRecording` or `StopRecording` was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RecordingDenyReason {
    /// No `record` permission.
    NoPermission,
    /// A start from outside the channel, or for the lobby or the AFK
    /// channel.
    NotInChannel,
    /// The channel is already recording.
    AlreadyRecording,
    /// A stop for a channel that is not recording.
    NotRecording,
    /// This server does not record (persistence off, or
    /// `recording.enabled: false`).
    Unavailable,
    /// `recording.max_concurrent` recordings are running.
    Busy,
    /// `recording.total_cap_gb` is reached.
    StorageFull,
    /// Someone in the channel runs a client that cannot show the
    /// recording label.
    OldClients,
}

impl RecordingDenyReason {
    /// Human-readable form, for the log.
    #[must_use]
    pub const fn describe(self) -> &'static str {
        match self {
            Self::NoPermission => "no permission to record",
            Self::NotInChannel => "not in that channel",
            Self::AlreadyRecording => "already recording",
            Self::NotRecording => "not recording",
            Self::Unavailable => "this server does not record",
            Self::Busy => "too many recordings at once",
            Self::StorageFull => "recording storage is full",
            Self::OldClients => "someone there cannot see the recording label",
        }
    }
}

/// One recording as its recipient sees it: a card in their "Server"
/// conversation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordingItem {
    /// The server's id for the recording.
    pub rec_id: u64,
    /// The channel's name when it started.
    pub channel: String,
    /// When it started, milliseconds since the Unix epoch.
    pub started_at: u64,
    /// How long it ran, milliseconds.
    pub duration_ms: u64,
    /// Who started it, by name at the time.
    pub started_by: String,
    /// Why it ended.
    pub reason: RecordingStopReason,
    /// The file: a blob only this recording's recipients may fetch.
    pub blob_id: BlobId,
    /// The file's name, made safe to save
    /// (`General_2026-11-12_21-04-33Z_1h05m00s.wav`).
    pub file_name: String,
    /// The file's size in bytes.
    pub size: u64,
    /// Samples per second of the WAV.
    pub sample_rate: u32,
    /// The recipient has seen it (no unread mark).
    pub seen: bool,
}

/// `General_2026-11-12_21-04-33Z_1h05m00s.wav`: a recording's file name
/// from its channel, start (ms since the epoch, UTC) and duration (ms).
#[must_use]
pub fn file_name(channel: &str, started_at_ms: u64, duration_ms: u64) -> String {
    let safe: String = channel
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .take(48)
        .collect();
    let safe = if safe.trim_matches('_').is_empty() {
        "channel".to_string()
    } else {
        safe
    };
    let (y, mo, d, h, mi, s) = civil(started_at_ms / 1000);
    let total = duration_ms / 1000;
    format!(
        "{safe}_{y:04}-{mo:02}-{d:02}_{h:02}-{mi:02}-{s:02}Z_{}h{:02}m{:02}s.wav",
        total / 3600,
        (total / 60) % 60,
        total % 60
    )
}

/// `HH:MM:SS` for a span of milliseconds; the hours grow past 99.
#[must_use]
pub fn clock_label(ms: u64) -> String {
    let secs = ms / 1000;
    format!(
        "{:02}:{:02}:{:02}",
        secs / 3600,
        (secs / 60) % 60,
        secs % 60
    )
}

/// UTC calendar fields of a Unix time (Howard Hinnant's algorithm), so
/// the protocol crate needs no date library.
fn civil(unix_secs: u64) -> (i64, u32, u32, u32, u32, u32) {
    let days = (unix_secs / 86_400) as i64;
    let rem = unix_secs % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (
        y,
        m,
        d,
        (rem / 3600) as u32,
        ((rem / 60) % 60) as u32,
        (rem % 60) as u32,
    )
}

/// The flag a downlink video track carries when it is someone's
/// camera rather than their screen (1.5): the relay keys a camera's
/// groups by the sender's id with this bit set, and client ids never
/// use it.
pub const CAMERA_TRACK_BIT: u64 = 1 << 63;

/// The uplink track a client sends its camera on (its screen is
/// track 0).
pub const CAMERA_UPLINK_TRACK: u64 = 1;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_names_carry_channel_start_and_duration() {
        // 2026-11-12 21:04:33 UTC.
        let start = 1_794_517_473_000;
        assert_eq!(
            file_name("General", start, 3_900_000),
            "General_2026-11-12_21-04-33Z_1h05m00s.wav"
        );
        assert_eq!(
            file_name("Movie night!", start, 7_000),
            "Movie_night__2026-11-12_21-04-33Z_0h00m07s.wav"
        );
        assert!(file_name("../..", start, 0).starts_with("channel_"));
    }

    #[test]
    fn clock_labels_are_hours_minutes_seconds() {
        assert_eq!(clock_label(7_000), "00:00:07");
        assert_eq!(clock_label(3_900_000), "01:05:00");
        assert_eq!(clock_label(360_000_000), "100:00:00");
    }

    #[test]
    fn stop_reasons_round_trip_their_codes() {
        for r in [
            RecordingStopReason::Stopped,
            RecordingStopReason::Empty,
            RecordingStopReason::TimeLimit,
            RecordingStopReason::StorageFull,
            RecordingStopReason::ChannelGone,
            RecordingStopReason::ServerRestart,
            RecordingStopReason::Interrupted,
        ] {
            assert_eq!(RecordingStopReason::from_code(r.code()), r);
        }
    }
}
