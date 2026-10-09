//! Blocking, reporting and deleting one's own account (1.2, feature bit
//! [`crate::features::SAFETY`]): what Google Play and the App Store
//! require of an app where people talk to each other.
//!
//! - **Block** is per account and lasts until undone: the blocked
//!   person's messages are hidden from the blocker, their voice and
//!   stream sound never reach the blocker's sessions, and direct
//!   messages between the two are refused both ways. The blocked
//!   person is not told; a DM they try fails like any unavailable one.
//! - **Report** sends a message or a person to the server's moderators
//!   with a reason; the server keeps the reported text as it was, and
//!   tells the moderators online.
//! - **Delete account** removes the account and everything it posted
//!   from the server, at once.
//!
//! Every type here is new in 1.2 and travels only between peers that
//! announced the bit. Like everything on the wire, enums are
//! append-only.

use serde::{Deserialize, Serialize};

use crate::messages::{ConvRef, UserId};

/// Longest note a report may carry, in bytes.
pub const MAX_REPORT_NOTE_BYTES: usize = 500;
/// Longest excerpt of a reported message the server keeps, in bytes.
pub const MAX_REPORT_EXCERPT_BYTES: usize = 2000;

/// Someone this user blocked, as Settings lists them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockedUser {
    pub user_id: UserId,
    /// Their name of record, so the list reads even when they are
    /// offline.
    pub name: String,
}

/// What a report is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReportTarget {
    /// One message, in General or in a DM the reporter is part of.
    Message { conv: ConvRef, msg_id: u64 },
    /// A person, for something that is not one message (their name,
    /// their avatar, what they say in voice).
    User { user: UserId },
}

/// Why something is reported. Append-only; the discriminants are the
/// database encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReportReason {
    Spam = 0,
    Harassment = 1,
    Hate = 2,
    Sexual = 3,
    Violence = 4,
    SelfHarm = 5,
    Illegal = 6,
    Other = 7,
}

impl ReportReason {
    /// Every reason, in the order a picker offers them.
    pub const ALL: [ReportReason; 8] = [
        Self::Spam,
        Self::Harassment,
        Self::Hate,
        Self::Sexual,
        Self::Violence,
        Self::SelfHarm,
        Self::Illegal,
        Self::Other,
    ];

    #[must_use]
    pub fn from_db(value: i64) -> Option<Self> {
        Self::ALL.into_iter().find(|r| r.to_db() == value)
    }

    #[must_use]
    pub fn to_db(self) -> i64 {
        self as i64
    }

    /// For logs and the audit line (the client translates its own).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Spam => "spam",
            Self::Harassment => "harassment",
            Self::Hate => "hate",
            Self::Sexual => "sexual",
            Self::Violence => "violence",
            Self::SelfHarm => "self-harm",
            Self::Illegal => "illegal",
            Self::Other => "other",
        }
    }
}

/// Why a report was not filed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReportRefusal {
    /// No such message, or a DM the reporter is not part of.
    NoSuchMessage,
    NoSuchUser,
    /// One's own message, or oneself.
    Yourself,
    /// Too many reports too quickly.
    TooMany,
}

/// Why `DeleteAccount` was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeleteRefusal {
    /// The server's only administrator: make someone else admin first.
    LastAdmin,
}

/// One report, as the admin panel's Reports tab shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportSummary {
    pub id: u64,
    /// Milliseconds since the Unix epoch.
    pub ts: u64,
    /// The reporter's name; `None` once they deleted their account.
    pub reporter: Option<String>,
    /// The reported person; `None` once they deleted their account.
    pub target_user: Option<UserId>,
    pub target_name: String,
    /// The reported message, when the report is about one.
    pub msg_id: Option<u64>,
    /// Whether that message was a direct message.
    pub in_dm: bool,
    /// The message's text as it was when reported.
    pub excerpt: String,
    pub reason: ReportReason,
    pub note: String,
    pub resolved: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reasons_round_trip_through_the_database_encoding() {
        for reason in ReportReason::ALL {
            assert_eq!(ReportReason::from_db(reason.to_db()), Some(reason));
        }
        assert_eq!(ReportReason::from_db(99), None);
        // The encoding is the declaration order, never renumbered.
        assert_eq!(ReportReason::Spam.to_db(), 0);
        assert_eq!(ReportReason::Other.to_db(), 7);
    }
}
