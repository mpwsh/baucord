//! The wire, pinned (review P2-62).
//!
//! `postcard` is positional: an enum variant is its index, a struct is
//! its fields in order, and nothing on the wire names either. A
//! variant inserted in the middle of an enum, or a field added to a
//! message that has shipped, still round-trips in every other test —
//! both ends of the test are the new build — and breaks every peer
//! that is not.
//!
//! So each wire type is decoded here from two fixed byte patterns, and
//! what comes out is compared with tables written down when the
//! protocol shipped:
//!
//! - an enum, tag by tag until one does not decode: the variants'
//!   names in wire order (the table's length is the variant count);
//! - every variant and struct: how many bytes an all-zero body takes
//!   (every list empty, every option `None`) and an all-one body (one
//!   of everything), and a hash of the latter's `Debug` — the fields,
//!   their order and their types.
//!
//! **When this fails:** something old peers decode has changed. New
//! variants go at the *end* of their enum; new data goes in a *new
//! message* behind a feature bit (`protocol::features`). Add the new
//! row and leave every existing row alone. The one harmless cause is a
//! rename of a field or variant (it changes `Debug`, not the wire):
//! then, and only then, update that row.

use std::fmt::Debug;

use protocol::{admin::*, messages::*, safety::*};
use serde::de::DeserializeOwned;

/// A variant or struct: its name, the bytes an all-zero body takes,
/// and the bytes and `Debug` hash of an all-one body (`None` where a
/// one is not a valid body: an enum inside with a single variant).
type Row = (&'static str, usize, Option<(usize, u32)>);

/// FNV-1a: stable across builds and platforms, which `Hash` is not.
fn fnv(text: &str) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for byte in text.bytes() {
        hash ^= u32::from(byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

/// What `bytes` decode to as a `T`: its leading name, the bytes it
/// took, and its `Debug` text.
fn shape<T: DeserializeOwned + Debug>(bytes: &[u8]) -> Option<(String, usize, String)> {
    let (value, rest) = postcard::take_from_bytes::<T>(bytes).ok()?;
    let debug = format!("{value:?}");
    let name = debug
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    Some((name, bytes.len() - rest.len(), debug))
}

/// Compare one decoded pattern pair with its row.
fn check_row(what: &str, zeros: &[u8], ones: &[u8], shapes: Shapes, row: &Row) {
    let (name, zero_len, one) = *row;
    let (zero_shape, one_shape) = shapes;
    let (got_name, got_zero_len, _) =
        zero_shape.unwrap_or_else(|| panic!("{what}: `{name}` no longer decodes from {zeros:?}"));
    assert_eq!(
        got_name, name,
        "{what}: the wire tag that was `{name}` now decodes as `{got_name}` — a variant was \
         inserted, removed or moved; old peers read every later variant as its neighbour"
    );
    assert_eq!(
        got_zero_len, zero_len,
        "{what}: `{name}` changed size on the wire — a field was added, removed or retyped"
    );
    match (one, one_shape) {
        (None, None) => {}
        (Some((len, hash)), Some((_, got_len, debug))) => assert!(
            got_len == len && fnv(&debug) == hash,
            "{what}: the layout of `{name}` changed — fields added, removed, reordered or \
             retyped; old peers cannot decode it. It now reads, from {} one-bytes ({len} \
             before, hash {:#06x}_{:04x} now): {debug}",
            got_len,
            fnv(&debug) >> 16,
            fnv(&debug) & 0xffff,
        ),
        (expected, got) => panic!(
            "{what}: `{name}` from {ones:?}: expected {expected:?}, decoded {:?}",
            got.map(|s| s.2)
        ),
    }
}

type Shapes = (
    Option<(String, usize, String)>,
    Option<(String, usize, String)>,
);

/// An enum: every tag in order, and no tag past the table.
fn check_enum<T: DeserializeOwned + Debug>(what: &str, golden: &[Row]) {
    for (tag, row) in golden.iter().enumerate() {
        let mut zeros = vec![0u8; 4096];
        let mut ones = vec![1u8; 4096];
        zeros[0] = u8::try_from(tag).expect("fewer than 128 variants: one tag byte");
        ones[0] = zeros[0];
        let shapes = (shape::<T>(&zeros), shape::<T>(&ones));
        check_row(what, &zeros[..1], &ones[..1], shapes, row);
    }
    let mut past = vec![0u8; 4096];
    past[0] = u8::try_from(golden.len()).expect("one tag byte");
    if let Some((name, ..)) = shape::<T>(&past) {
        panic!(
            "{what}: a variant `{name}` was appended. Add its row at the end of the table \
             (and make sure the peer is only sent it behind a feature bit)"
        );
    }
}

/// A struct: one row.
fn check_struct<T: DeserializeOwned + Debug>(row: &Row) {
    let zeros = vec![0u8; 4096];
    let ones = vec![1u8; 4096];
    let shapes = (shape::<T>(&zeros), shape::<T>(&ones));
    check_row(row.0, &[], &[], shapes, row);
}

#[rustfmt::skip]
const CLIENT_MESSAGE: &[Row] = &[
    ("Hello", 7, Some((42, 0x1e0e_d47c))),
    ("Chat", 2, Some((3, 0x7265_e04c))),
    ("Speaking", 2, Some((2, 0x9078_9f6c))),
    ("AudioState", 3, Some((3, 0x3da7_7461))),
    ("ScreenShareStart", 4, Some((4, 0x6966_1f42))),
    ("ScreenShareStop", 1, Some((1, 0x8f09_db1c))),
    ("SubscribeScreen", 2, Some((2, 0xa322_a8f1))),
    ("UnsubscribeScreen", 2, Some((2, 0x794e_ad72))),
    ("RequestKeyframe", 2, Some((2, 0x3399_1a22))),
    ("ChatImageChunk", 5, Some((6, 0x40aa_2799))),
    ("JoinChannel", 3, Some((5, 0x8f24_3f4a))),
    ("Telemetry", 25, Some((73, 0x4ace_35a3))),
    ("TelemetryEvent", 5, Some((2, 0x6bab_2cc4))),
    ("LogUploadChunk", 5, Some((6, 0x4da3_8681))),
    ("CreateInvite", 4, Some((7, 0x8046_e04e))),
    ("Admin", 3, Some((3, 0xba4d_1e00))),
    ("FetchHistory", 4, Some((37, 0xb8ce_e806))),
    ("EditMessage", 3, Some((4, 0x05f5_f5c5))),
    ("DeleteMessage", 2, Some((2, 0x4b24_2843))),
    ("CreateChannel", 7, Some((13, 0xcb15_9020))),
    ("EditChannel", 8, Some((14, 0x523b_ee3c))),
    ("DeleteChannel", 2, Some((2, 0x5fac_71f9))),
    ("ChannelKick", 34, Some((34, 0x8223_a043))),
    ("SetProfile", 4, Some((37, 0xa221_e3dd))),
    ("SetNickname", 2, Some((3, 0xf038_cb00))),
    ("UploadBegin", 37, Some((39, 0x47f7_909d))),
    ("UploadChunk", 5, Some((6, 0x8678_b3bd))),
    ("FetchBlob", 33, Some((33, 0x1f94_ecfd))),
    ("DirectMessage", 35, Some((36, 0xa457_e38b))),
    ("MarkRead", 3, Some((35, 0x2cce_c9f2))),
    ("SendFile", 35, Some((67, 0x93e9_e5d8))),
    ("Block", 33, Some((33, 0x0fe2_1797))),
    ("Unblock", 33, Some((33, 0x5022_502c))),
    ("Report", 6, Some((37, 0x6fa6_6a30))),
    ("DeleteAccount", 1, Some((1, 0x5380_0ba5))),
];

#[rustfmt::skip]
const SERVER_MESSAGE: &[Row] = &[
    ("Welcome", 4, Some((36, 0x5788_a3cf))),
    ("Refused", 2, Some((2, 0x6b1c_3d89))),
    ("Init", 7, Some((20, 0x52b9_851d))),
    ("UserJoined", 4, Some((5, 0x5a5c_6775))),
    ("UserLeft", 2, Some((2, 0xf8d4_7fb7))),
    ("Chat", 5, Some((6, 0x8753_b3fa))),
    ("Speaking", 3, Some((3, 0x85ec_66c2))),
    ("AudioState", 4, Some((4, 0xdfd4_d56b))),
    ("ScreenShareStarted", 5, Some((5, 0x0b19_fffb))),
    ("ScreenShareStopped", 2, Some((2, 0x43dd_ade5))),
    ("ShareDenied", 2, Some((2, 0x8d7a_172a))),
    ("SubscribeDenied", 3, Some((3, 0x8b6f_515f))),
    ("KeyframeRequested", 2, Some((2, 0x6048_4586))),
    ("StreamHealth", 10, Some((10, 0x4c18_2eeb))),
    ("ChatImageChunk", 40, Some((41, 0x6b0d_c575))),
    ("UserMovedChannel", 3, Some((3, 0x0001_003a))),
    ("JoinChannelDenied", 3, Some((3, 0xd850_4e95))),
    ("ChannelListUpdated", 3, Some((11, 0x2af3_b339))),
    ("VoiceReceiverReport", 6, Some((6, 0xd67a_3da3))),
    ("Session", 35, Some((35, 0x62ad_97d8))),
    ("Removed", 4, Some((6, 0x023a_88c1))),
    ("InviteCreated", 5, Some((8, 0x6418_98d1))),
    ("InviteDenied", 2, Some((2, 0x0dbb_edbc))),
    ("UserDetails", 36, Some((37, 0x10ab_4496))),
    ("Restricted", 4, Some((6, 0x4693_5e2a))),
    ("RestrictionLifted", 2, Some((2, 0xd8ec_0d5d))),
    ("AdminResult", 4, Some((4, 0x890b_b07f))),
    ("History", 4, Some((75, 0xd302_a84d))),
    ("HistoryUnavailable", 3, Some((35, 0x1b89_b77b))),
    ("MessageEdited", 3, Some((4, 0x2322_8e1a))),
    ("MessageDeleted", 2, Some((2, 0x12a2_d3e3))),
    ("ChatRateLimited", 2, Some((2, 0x9b39_0fb3))),
    ("ProgressUpdate", 3, Some((3, 0x160f_e9af))),
    ("ChannelDetails", 8, Some((42, 0xef8e_7c32))),
    ("ChannelDenied", 2, Some((2, 0x4cb2_0c5f))),
    ("MovedIdle", 2, Some((2, 0xa0b9_e888))),
    ("UserProfile", 5, Some((38, 0x8a84_016c))),
    ("ProfileUpdated", 36, Some((69, 0x6078_cf73))),
    ("ProfileDenied", 2, Some((2, 0x5936_1851))),
    ("UserRenamed", 3, Some((4, 0x7928_0b1d))),
    ("RenameDenied", 2, Some((2, 0x9388_0247))),
    ("UploadAccept", 3, Some((3, 0xea96_d71f))),
    ("UploadDone", 34, Some((34, 0x47fb_f085))),
    ("UploadRejected", 3, Some((3, 0x47b6_f9f5))),
    ("BlobChunk", 36, Some((37, 0x8eda_ae6a))),
    ("BlobMissing", 33, Some((33, 0xeba1_c2cb))),
    ("Direct", 37, Some((38, 0x0fa6_f8c3))),
    ("MessageAck", 3, Some((3, 0xc4a9_9bc9))),
    ("MessageFailed", 3, Some((3, 0xdf01_3b7f))),
    ("Conversations", 2, Some((39, 0xe8c0_28c2))),
    ("UserProgress", 4, Some((4, 0x3bae_40c3))),
    ("FileMessage", 73, Some((108, 0xd2d2_1161))),
    ("HistoryAttachments", 3, Some((73, 0x7076_0486))),
    ("AuditLine", 7, Some((14, 0xe8f9_473f))),
    ("RoleList", 2, Some((11, 0x5b03_188f))),
    ("Blocks", 2, Some((36, 0x5837_152c))),
    ("ReportAck", 2, Some((3, 0x1035_d98a))),
    ("AccountDeleted", 1, Some((1, 0x75eb_2109))),
    ("AccountDeleteRefused", 2, None),
    ("ChatFrom", 37, Some((38, 0x0902_53ea))),
    ("Authors", 2, Some((74, 0x8a02_ff18))),
    ("AdminRows", 3, Some((78, 0xf8e7_e989))),
];

#[rustfmt::skip]
const ADMIN_COMMAND: &[Row] = &[
    ("ListUsers", 1, Some((1, 0x29de_d7c7))),
    ("ListRestrictions", 1, Some((1, 0x015a_4f7e))),
    ("ListInvites", 1, Some((1, 0xdf82_176d))),
    ("SetRoles", 35, Some((36, 0x8632_9a15))),
    ("Restrict", 37, Some((39, 0xbf00_1c97))),
    ("Lift", 2, Some((2, 0xb9ad_adfd))),
    ("Unmute", 35, Some((35, 0x61d1_9e84))),
    ("Disconnect", 35, Some((36, 0x61ef_d9fe))),
    ("Kick", 35, Some((36, 0x17a2_8e72))),
    ("Ban", 36, Some((38, 0x2dd7_9b4a))),
    ("Unban", 34, Some((34, 0x9231_1258))),
    ("MoveUser", 35, Some((35, 0x746c_da7c))),
    ("RevokeInvite", 2, Some((3, 0xa5e2_474f))),
    ("LinkKey", 66, Some((66, 0xffa6_2ebd))),
    ("UnlinkKey", 33, Some((33, 0xefa1_7485))),
    ("DeleteMessage", 2, Some((2, 0x4b24_2843))),
    ("PurgeUser", 34, Some((34, 0x8a49_65a0))),
    ("LastTelemetry", 34, Some((34, 0x2331_baa7))),
    ("ResetProfile", 35, Some((35, 0x9285_93d3))),
    ("AuditLog", 3, Some((4, 0xa994_5c61))),
    ("CreateRole", 5, Some((7, 0xe849_46cc))),
    ("EditRole", 6, Some((12, 0xd9eb_f3e9))),
    ("DeleteRole", 2, Some((2, 0xb3a0_c4dd))),
    ("ListBlobs", 1, Some((1, 0x6bac_ac6f))),
    ("DeleteBlob", 33, Some((33, 0x4d8c_cef4))),
    ("ServerStats", 1, Some((1, 0x007e_5661))),
    ("ListReports", 2, Some((2, 0x32aa_a226))),
    ("ResolveReport", 2, Some((2, 0x1e5c_05ad))),
];

#[rustfmt::skip]
const ADMIN_OUTCOME: &[Row] = &[
    ("Done", 1, Some((1, 0x8dd3_1791))),
    ("Users", 2, Some((76, 0x25ca_58f8))),
    ("Restrictions", 2, Some((83, 0x75f9_4531))),
    ("Invites", 2, Some((15, 0x6645_1b27))),
    ("Telemetry", 2, Some((5, 0xfaa5_7b9d))),
    ("Audit", 2, Some((15, 0xc7a4_c752))),
    ("Blobs", 2, Some((44, 0x6534_ca0e))),
    ("Stats", 14, Some((15, 0x289f_d0a6))),
    ("RoleCreated", 2, Some((2, 0xecbb_91a7))),
    ("Reports", 2, Some((51, 0x261a_7c93))),
];

#[rustfmt::skip]
const ADMIN_ERROR: &[Row] = &[
    ("NoPermission", 1, Some((1, 0x1a57_f57d))),
    ("NoSuchUser", 1, Some((1, 0x5b12_af76))),
    ("NoSuchRestriction", 1, Some((1, 0x96ab_9591))),
    ("NoSuchInvite", 1, Some((1, 0xe81d_44c4))),
    ("NoSuchRole", 1, Some((1, 0xc755_ddb5))),
    ("NoSuchChannel", 1, Some((1, 0xd43e_6e40))),
    ("Outranked", 1, Some((1, 0x4111_c124))),
    ("LastAdmin", 1, Some((1, 0x1d7b_1518))),
    ("Yourself", 1, Some((1, 0x8227_c80c))),
    ("Invalid", 2, Some((3, 0x30cc_13b5))),
];

#[rustfmt::skip]
const ADMIN_TARGET: &[Row] = &[
    ("User", 33, Some((33, 0x471f_4241))),
    ("Key", 33, Some((33, 0xce9a_cda7))),
    ("Address", 2, Some((3, 0x9b98_fef4))),
    ("Name", 2, Some((3, 0x4047_53b3))),
];

#[rustfmt::skip]
const RESTRICTION_KIND: &[Row] = &[
    ("Server", 1, Some((1, 0x6d73_02f2))),
    ("Voice", 1, Some((1, 0x27d1_a499))),
    ("GeneralText", 1, Some((1, 0x8545_b05c))),
    ("Dm", 1, Some((1, 0x63ce_bc5a))),
    ("Upload", 1, Some((1, 0x826f_7f7c))),
    ("Share", 1, Some((1, 0x4f85_6dd8))),
    ("Profile", 1, Some((1, 0xcd17_328e))),
];

#[rustfmt::skip]
const REFUSED_REASON: &[Row] = &[
    ("WrongPassword", 1, Some((1, 0x878b_a42b))),
    ("ServerFull", 1, Some((1, 0x8e2b_f86b))),
    ("RateLimited", 1, Some((1, 0xfc9e_d593))),
    ("BadHello", 1, Some((1, 0xa99b_4200))),
    ("NeedInvite", 2, Some((2, 0xc90d_8d4e))),
    ("InviteInvalid", 2, Some((2, 0x9ae0_1977))),
    ("NameInUse", 1, Some((1, 0x6279_bbc0))),
    ("Banned", 3, Some((5, 0x986c_6edb))),
];

#[rustfmt::skip]
const NEED_INVITE_REASON: &[Row] = &[
    ("UnknownKey", 1, Some((1, 0xec0f_d190))),
    ("KeyRetired", 1, Some((1, 0x4585_a3dd))),
];

#[rustfmt::skip]
const INVITE_INVALID_REASON: &[Row] = &[
    ("Expired", 1, Some((1, 0xbca9_f3d2))),
    ("Revoked", 1, Some((1, 0xe58f_e295))),
    ("Exhausted", 1, Some((1, 0x7634_cdec))),
    ("Unknown", 1, Some((1, 0xcc20_0e59))),
];

#[rustfmt::skip]
const AUTH: &[Row] = &[
    ("Shared", 2, Some((4, 0xdb6f_8fca))),
    ("Key", 1, Some((1, 0xcd1a_c90c))),
    ("Invite", 2, Some((3, 0xdcac_3c6d))),
];

#[rustfmt::skip]
const DENY_REASON: &[Row] = &[
    ("ShareLimitReached", 1, Some((1, 0xaba3_fe3b))),
    ("ViewerLimitReached", 1, Some((1, 0xb7a4_4948))),
    ("NotSharing", 1, Some((1, 0x9e24_04b2))),
    ("InvalidDimensions", 1, Some((1, 0xb419_464d))),
    ("NotInChannel", 1, Some((1, 0x8ee2_4274))),
    ("NotPermitted", 1, Some((1, 0xaa97_c04e))),
];

#[rustfmt::skip]
const SHARE_SCOPE: &[Row] = &[
    ("Channel", 1, Some((1, 0x655d_8a44))),
    ("Everyone", 1, Some((1, 0x2521_9998))),
];

#[rustfmt::skip]
const CHANNEL_DENY_REASON: &[Row] = &[
    ("NoSuchChannel", 1, Some((1, 0xd43e_6e40))),
    ("WrongPassword", 1, Some((1, 0x878b_a42b))),
    ("ChannelFull", 1, Some((1, 0xa358_1409))),
    ("NotPermitted", 1, Some((1, 0xaa97_c04e))),
];

#[rustfmt::skip]
const CHANNEL_UI_DENY_REASON: &[Row] = &[
    ("NameTaken", 1, Some((1, 0x4d75_03b9))),
    ("NoPermission", 1, Some((1, 0x1a57_f57d))),
    ("ConfigOwned", 1, Some((1, 0xd497_fca0))),
    ("Limit", 1, Some((1, 0xd896_4ad4))),
    ("NoSuchChannel", 1, Some((1, 0xd43e_6e40))),
    ("Invalid", 1, Some((1, 0xfc7c_63d0))),
];

#[rustfmt::skip]
const TELEMETRY_EVENT: &[Row] = &[
    ("RateRungChanged", 4, Some((4, 0x61b0_878b))),
    ("IdentityPromptShown", 1, Some((1, 0xff05_9c2a))),
    ("DeviceChanged", 2, Some((3, 0x1592_672d))),
    ("InputDiscontinuityBurst", 2, Some((2, 0x9ceb_e315))),
    ("SendBufferPressure", 1, Some((1, 0xc704_96e2))),
    ("ShareStarted", 3, Some((5, 0xd98f_f686))),
    ("ShareStopped", 1, Some((1, 0xe3fb_d351))),
    ("Disconnected", 2, Some((3, 0xfae3_6598))),
];

#[rustfmt::skip]
const CONV_REF: &[Row] = &[
    ("General", 1, Some((1, 0x5561_fd0b))),
    ("Dm", 33, Some((33, 0xf470_1f59))),
];

#[rustfmt::skip]
const DM_FAIL_REASON: &[Row] = &[
    ("Offline", 1, Some((1, 0xc47c_37c8))),
    ("NoPermission", 1, Some((1, 0x1a57_f57d))),
    ("NoSuchUser", 1, Some((1, 0x5b12_af76))),
    ("Store", 1, Some((1, 0x6da3_2dce))),
    ("NoSuchBlob", 1, Some((1, 0x34af_d86e))),
    ("Unavailable", 1, Some((1, 0xda4b_aaa9))),
];

#[rustfmt::skip]
const NAME_STYLE: &[Row] = &[
    ("Normal", 1, Some((1, 0x58de_2772))),
    ("Bold", 1, Some((1, 0x4576_8d96))),
    ("Italic", 1, Some((1, 0x8db0_c08d))),
];

#[rustfmt::skip]
const PROFILE_DENY_REASON: &[Row] = &[
    ("ReservedColor", 1, Some((1, 0xcff8_e650))),
    ("LowContrast", 1, Some((1, 0x15cd_d92f))),
    ("RateLimited", 1, Some((1, 0xfc9e_d593))),
    ("Locked", 1, Some((1, 0x406a_5eb3))),
    ("BadAvatar", 1, Some((1, 0xa904_ce31))),
];

#[rustfmt::skip]
const RENAME_DENY_REASON: &[Row] = &[
    ("Taken", 1, Some((1, 0xd615_2dc8))),
    ("Invalid", 1, Some((1, 0xfc7c_63d0))),
    ("RateLimited", 1, Some((1, 0xfc9e_d593))),
];

#[rustfmt::skip]
const UPLOAD_REJECT_REASON: &[Row] = &[
    ("TooLarge", 1, Some((1, 0xcb13_b5a0))),
    ("BadType", 1, Some((1, 0x28a1_9970))),
    ("HashMismatch", 1, Some((1, 0x4a4e_cbe5))),
    ("NotAnnounced", 1, Some((1, 0xf68e_f067))),
    ("Refused", 1, Some((1, 0x9155_ad83))),
    ("Quota", 1, Some((1, 0x56be_502b))),
    ("Full", 1, Some((1, 0x7fa5_ff5c))),
    ("NoPermission", 1, Some((1, 0x1a57_f57d))),
];

#[rustfmt::skip]
const REMOVED_KIND: &[Row] = &[
    ("Disconnected", 1, Some((1, 0x2d10_72ca))),
    ("Kicked", 1, Some((1, 0x2f1b_eefe))),
    ("Banned", 2, Some((3, 0x0764_37a3))),
    ("Replaced", 1, Some((1, 0x42cb_6805))),
];

#[rustfmt::skip]
const INVITE_DENY_REASON: &[Row] = &[
    ("NoPermission", 1, Some((1, 0x1a57_f57d))),
    ("RoleNotAllowed", 1, Some((1, 0x6c22_5152))),
];

#[rustfmt::skip]
const REPORT_TARGET: &[Row] = &[
    ("Message", 3, Some((35, 0xdd7f_4822))),
    ("User", 33, Some((33, 0x5de9_6c07))),
];

#[rustfmt::skip]
const REPORT_REASON: &[Row] = &[
    ("Spam", 1, Some((1, 0xa8a8_3328))),
    ("Harassment", 1, Some((1, 0x6805_fc61))),
    ("Hate", 1, Some((1, 0x49fe_8805))),
    ("Sexual", 1, Some((1, 0x33fc_239b))),
    ("Violence", 1, Some((1, 0xb02a_9830))),
    ("SelfHarm", 1, Some((1, 0x3822_6e7d))),
    ("Illegal", 1, Some((1, 0x97c8_5679))),
    ("Other", 1, Some((1, 0x6e38_ff95))),
];

#[rustfmt::skip]
const REPORT_REFUSAL: &[Row] = &[
    ("NoSuchMessage", 1, Some((1, 0x049c_39a0))),
    ("NoSuchUser", 1, Some((1, 0x5b12_af76))),
    ("Yourself", 1, Some((1, 0x8227_c80c))),
    ("TooMany", 1, Some((1, 0x6e05_423e))),
];

#[rustfmt::skip]
const DELETE_REFUSAL: &[Row] = &[
    ("LastAdmin", 1, Some((1, 0x1d7b_1518))),
];

#[rustfmt::skip]
const STRUCTS: &[Row] = &[
    ("ChatMessage", 4, Some((5, 0xf0f4_bea3))),
    ("ChannelInfo", 4, Some((6, 0x9304_4f28))),
    ("ClientKeyProof", 33, Some((34, 0x50c7_5ee7))),
    ("ClientInfo", 4, Some((7, 0x9229_d715))),
    ("PathSample", 17, Some((17, 0x4459_a292))),
    ("AudioSample", 24, Some((27, 0xcc13_aa10))),
    ("SharingSample", 11, Some((13, 0xd3ef_a1b1))),
    ("ViewingSample", 5, Some((5, 0x46b4_58f1))),
    ("TelemetrySample", 24, Some((72, 0x7789_6573))),
    ("HistoryMessage", 37, Some((39, 0x9473_f6a6))),
    ("ChannelSpec", 6, Some((12, 0x05b2_4d76))),
    ("Attachment", 35, Some((37, 0x1ea9_c44e))),
    ("RoleInfo", 7, Some((9, 0xb5dc_feb0))),
    ("DmSummary", 36, Some((37, 0x4fda_a27e))),
    ("AuthorInfo", 37, Some((72, 0x6e3c_3a90))),
    ("Profile", 3, Some((36, 0xf4be_d255))),
    ("AuditEntry", 6, Some((13, 0x16cb_0bac))),
    ("BlobSummary", 38, Some((42, 0x8c36_1ab9))),
    ("StatsSummary", 13, Some((14, 0x1aad_9ad2))),
    ("KeySummary", 34, Some((34, 0x6423_fc00))),
    ("UserSummary", 37, Some((74, 0x53d2_1aad))),
    ("RestrictionSummary", 10, Some((81, 0x3ff1_33a5))),
    ("InviteSummary", 8, Some((13, 0x709c_2f82))),
    ("TelemetrySummary", 2, Some((3, 0x201d_0704))),
    ("BlockedUser", 33, Some((34, 0x4128_7078))),
    ("ReportSummary", 11, Some((49, 0x97b9_bf19))),
];

#[test]
fn every_wire_enum_keeps_its_variants_where_they_shipped() {
    check_enum::<ClientMessage>("ClientMessage", CLIENT_MESSAGE);
    check_enum::<ServerMessage>("ServerMessage", SERVER_MESSAGE);
    check_enum::<AdminCommand>("AdminCommand", ADMIN_COMMAND);
    check_enum::<AdminOutcome>("AdminOutcome", ADMIN_OUTCOME);
    check_enum::<AdminError>("AdminError", ADMIN_ERROR);
    check_enum::<AdminTarget>("AdminTarget", ADMIN_TARGET);
    check_enum::<RestrictionKind>("RestrictionKind", RESTRICTION_KIND);
    check_enum::<RefusedReason>("RefusedReason", REFUSED_REASON);
    check_enum::<NeedInviteReason>("NeedInviteReason", NEED_INVITE_REASON);
    check_enum::<InviteInvalidReason>("InviteInvalidReason", INVITE_INVALID_REASON);
    check_enum::<Auth>("Auth", AUTH);
    check_enum::<DenyReason>("DenyReason", DENY_REASON);
    check_enum::<ShareScope>("ShareScope", SHARE_SCOPE);
    check_enum::<ChannelDenyReason>("ChannelDenyReason", CHANNEL_DENY_REASON);
    check_enum::<ChannelUiDenyReason>("ChannelUiDenyReason", CHANNEL_UI_DENY_REASON);
    check_enum::<TelemetryEvent>("TelemetryEvent", TELEMETRY_EVENT);
    check_enum::<ConvRef>("ConvRef", CONV_REF);
    check_enum::<DmFailReason>("DmFailReason", DM_FAIL_REASON);
    check_enum::<NameStyle>("NameStyle", NAME_STYLE);
    check_enum::<ProfileDenyReason>("ProfileDenyReason", PROFILE_DENY_REASON);
    check_enum::<RenameDenyReason>("RenameDenyReason", RENAME_DENY_REASON);
    check_enum::<UploadRejectReason>("UploadRejectReason", UPLOAD_REJECT_REASON);
    check_enum::<RemovedKind>("RemovedKind", REMOVED_KIND);
    check_enum::<InviteDenyReason>("InviteDenyReason", INVITE_DENY_REASON);
    check_enum::<ReportTarget>("ReportTarget", REPORT_TARGET);
    check_enum::<ReportReason>("ReportReason", REPORT_REASON);
    check_enum::<ReportRefusal>("ReportRefusal", REPORT_REFUSAL);
    check_enum::<DeleteRefusal>("DeleteRefusal", DELETE_REFUSAL);
}

#[test]
fn every_wire_struct_keeps_its_layout() {
    let mut rows = STRUCTS.iter();
    let mut next = || rows.next().expect("a row per struct, in this order");
    check_struct::<ChatMessage>(next());
    check_struct::<ChannelInfo>(next());
    check_struct::<ClientKeyProof>(next());
    check_struct::<ClientInfo>(next());
    check_struct::<PathSample>(next());
    check_struct::<AudioSample>(next());
    check_struct::<SharingSample>(next());
    check_struct::<ViewingSample>(next());
    check_struct::<TelemetrySample>(next());
    check_struct::<HistoryMessage>(next());
    check_struct::<ChannelSpec>(next());
    check_struct::<Attachment>(next());
    check_struct::<RoleInfo>(next());
    check_struct::<DmSummary>(next());
    check_struct::<AuthorInfo>(next());
    check_struct::<Profile>(next());
    check_struct::<AuditEntry>(next());
    check_struct::<BlobSummary>(next());
    check_struct::<StatsSummary>(next());
    check_struct::<KeySummary>(next());
    check_struct::<UserSummary>(next());
    check_struct::<RestrictionSummary>(next());
    check_struct::<InviteSummary>(next());
    check_struct::<TelemetrySummary>(next());
    check_struct::<BlockedUser>(next());
    check_struct::<ReportSummary>(next());
    assert!(rows.next().is_none(), "a row with no struct checked");
}

/// The server sends a message only to a session that announced the
/// bit the message came with: one bit, one the server offers, and
/// nothing for what every client decodes.
#[test]
fn every_server_message_names_one_feature_bit_or_none() {
    use protocol::features;
    let mut gated = 0;
    for tag in 0..SERVER_MESSAGE.len() {
        let mut zeros = vec![0u8; 4096];
        zeros[0] = u8::try_from(tag).expect("one tag byte");
        let (msg, _) = postcard::take_from_bytes::<ServerMessage>(&zeros).expect("decodes");
        let bit = msg.required_feature();
        assert!(bit == 0 || bit.is_power_of_two(), "{msg:?}: {bit:#x}");
        assert_eq!(
            bit & !features::SERVER_1_1,
            0,
            "{msg:?} needs a bit no server offers"
        );
        gated += usize::from(bit != 0);
    }
    // The 1.0 messages: `Welcome` … `VoiceReceiverReport`.
    assert_eq!(SERVER_MESSAGE.len() - gated, 19);
    let needs = |msg: ServerMessage| msg.required_feature();
    assert_eq!(needs(ServerMessage::UserLeft { client_id: 1 }), 0);
    assert_eq!(
        needs(ServerMessage::Session {
            user_id: [0; 32],
            permissions: 0,
            persistent: true,
        }),
        features::INVITES
    );
    assert_eq!(
        needs(ServerMessage::UserProgress {
            client_id: 1,
            xp: 0,
            level: 0,
        }),
        features::PROGRESS_ALL,
        "its own bit: a 1.1.0 client negotiated PROGRESS and cannot decode this"
    );
    assert_eq!(needs(ServerMessage::AccountDeleted), features::SAFETY);
}
