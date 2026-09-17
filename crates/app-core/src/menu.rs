//! Context menus as data.
//!
//! The shell draws menus itself (the toolkit's own context menu is
//! native on macOS and Windows and cannot be restyled), so a menu is
//! a list of [`Entry`]s: a [`Label`] the shell localizes, an
//! [`Action`] it dispatches, and the flags the overlay draws from.
//! The builders here mirror what the server would allow (spec §8:
//! never advertise an action the server would refuse) from facts the
//! shell already has; targets — which roster row, which message —
//! stay with the shell, so an action carries only its own parameter.

use crate::roster::Role;

/// What an entry says. The shell maps each to its localized string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Label {
    ChannelConfigOwned,
    ChannelEdit,
    ChannelDelete,
    Volume,
    MuteForMe,
    UnmuteForMe,
    ResetAudio,
    WatchStream,
    Message,
    Role,
    RoleName(Role),
    MoveTo,
    /// Verbatim text — a channel's name in the "Move to" submenu.
    Text(String),
    ChannelKick,
    Mute,
    MuteVoice,
    MuteChat,
    UnmuteVoice,
    UnmuteChat,
    Disconnect,
    Kick,
    Ban,
    CopyMessage,
    Copy,
    SelectText,
    ShowFormatting,
    EditMessage,
    DeleteMessage,
    OpenImage,
    SaveImage,
    CopyImage,
    SaveFile,
    Cut,
    Paste,
}

/// What activating an entry does, against the target the shell
/// remembered when it opened the menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Informational or submenu header: nothing to do.
    None,
    ChannelEdit(u16),
    ChannelDelete(u16),
    /// The volume slider row: drawn inline, adjusted live; never a
    /// pick.
    Volume,
    MuteForMe,
    ResetAudio,
    Watch,
    Message,
    /// Make this the user's one role (the built-ins nest; a custom
    /// role is a whole set), by the server's role id.
    SetRole(u32),
    MoveTo(u16),
    ChannelKick,
    MuteVoice,
    MuteChat,
    UnmuteVoice,
    UnmuteChat,
    Disconnect,
    Kick,
    Ban,
    CopyText,
    /// Show a styled row as selectable plain text (`true`) or restore
    /// its formatting.
    SelectText(bool),
    EditMessage,
    DeleteMessage,
    OpenImage,
    SaveImage,
    /// Put the picture's pixels on the clipboard.
    CopyImage,
    /// Fetch a file card's blob into the Downloads folder.
    SaveFile,
    Cut,
    Copy,
    Paste,
}

/// One row of a menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub label: Label,
    pub action: Action,
    pub enabled: bool,
    /// A rule is drawn above this entry.
    pub separator: bool,
    /// Destructive: drawn in the danger color.
    pub danger: bool,
    /// A second column that opens beside this entry.
    pub submenu: Vec<Entry>,
}

impl Entry {
    fn new(label: Label, action: Action) -> Self {
        Self {
            label,
            action,
            enabled: true,
            separator: false,
            danger: false,
            submenu: Vec::new(),
        }
    }

    fn disabled(label: Label) -> Self {
        Self {
            enabled: false,
            ..Self::new(label, Action::None)
        }
    }

    fn submenu(label: Label, submenu: Vec<Entry>) -> Self {
        Self {
            submenu,
            ..Self::new(label, Action::None)
        }
    }

    fn danger(mut self) -> Self {
        self.danger = true;
        self
    }

    fn with_separator(mut self) -> Self {
        self.separator = true;
        self
    }
}

/// What this session may do (`Session.permissions` as the bools the
/// sidebar reads) and how high it ranks.
#[expect(
    clippy::struct_excessive_bools,
    reason = "one flag per Slint `perm-*` property; a bitset would be decoded at both ends"
)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Session {
    /// `MUTE` or `RESTRICT`.
    pub mute: bool,
    pub disconnect: bool,
    pub kick: bool,
    pub ban: bool,
    /// `MANAGE_CHANNELS`: move anyone, manage every user channel.
    pub move_users: bool,
    /// `MANAGE_ROLES`.
    pub roles: bool,
    pub dm: bool,
    pub manage_own_channels: bool,
    pub create_channels: bool,
    /// Our highest rank across our roles; the role submenu offers
    /// roles up to it.
    pub our_rank: u16,
}

/// A roster user row, as the menu sees it (the decorated row: the
/// rank rule against us is already applied).
#[expect(
    clippy::struct_excessive_bools,
    reason = "mirrors the roster row's flags one to one"
)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UserFacts {
    pub is_us: bool,
    pub role: Role,
    pub sharing: bool,
    pub local_muted: bool,
    /// Rank strictly below ours: mutes apply.
    pub outranked: bool,
    /// Holds the admin role: kick and ban never apply (spec §12).
    pub is_admin: bool,
    /// Rank at or below ours: the role submenu applies.
    pub role_editable: bool,
    /// We own their channel (or manage all): send to the default one.
    pub channel_kickable: bool,
    pub channel_id: u16,
}

/// A channel header row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChannelFacts {
    pub id: u16,
    pub owned: bool,
    /// The server's config file owns it: no rename, no delete.
    pub from_config: bool,
}

/// A channel the "Move to" submenu can name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelChoice {
    pub id: u16,
    pub name: String,
}

/// A role the "Role" submenu can offer: the server's table, in rank
/// order, with whether the user already holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleChoice {
    pub id: u32,
    /// The table's name; shown as-is for a custom role.
    pub name: String,
    pub rank: u16,
    /// The built-in this is, for a localized label; `None` for a
    /// custom role.
    pub builtin: Option<Role>,
    pub held: bool,
}

/// The menu for a user row. Empty for our own row.
#[must_use]
pub fn user_menu(
    session: &Session,
    user: &UserFacts,
    channels: &[ChannelChoice],
    roles: &[RoleChoice],
) -> Vec<Entry> {
    if user.is_us {
        return Vec::new();
    }
    let mut entries = vec![
        Entry::new(Label::Volume, Action::Volume),
        Entry::new(
            if user.local_muted {
                Label::UnmuteForMe
            } else {
                Label::MuteForMe
            },
            Action::MuteForMe,
        ),
        Entry::new(Label::ResetAudio, Action::ResetAudio),
    ];
    if user.sharing {
        entries.push(Entry::new(Label::WatchStream, Action::Watch));
    }
    // Direct messages: members to members (spec §6).
    if session.dm && user.role != Role::Guest {
        entries.push(Entry::new(Label::Message, Action::Message).with_separator());
    }

    // Moderation, permission- and rank-gated (spec §4.1, §8).
    let mut moderation = Vec::new();
    if session.roles && user.role_editable && !roles.is_empty() {
        moderation.push(Entry::submenu(
            Label::Role,
            roles
                .iter()
                .map(|r| Entry {
                    // Roles up to our rank; the one they hold is greyed.
                    enabled: r.rank <= session.our_rank && !r.held,
                    ..Entry::new(
                        r.builtin
                            .map_or_else(|| Label::Text(r.name.clone()), Label::RoleName),
                        Action::SetRole(r.id),
                    )
                })
                .collect(),
        ));
    }
    if session.move_users {
        moderation.push(Entry::submenu(
            Label::MoveTo,
            channels
                .iter()
                .map(|c| Entry {
                    enabled: c.id != user.channel_id,
                    ..Entry::new(Label::Text(c.name.clone()), Action::MoveTo(c.id))
                })
                .collect(),
        ));
    }
    if user.channel_kickable {
        moderation.push(Entry::new(Label::ChannelKick, Action::ChannelKick));
    }
    if user.outranked && session.mute {
        moderation.push(Entry::submenu(
            Label::Mute,
            vec![
                Entry::new(Label::MuteVoice, Action::MuteVoice),
                Entry::new(Label::MuteChat, Action::MuteChat),
                Entry::new(Label::UnmuteVoice, Action::UnmuteVoice).with_separator(),
                Entry::new(Label::UnmuteChat, Action::UnmuteChat),
            ],
        ));
    }
    // Disconnect reaches anyone; kick and ban spare administrators
    // (spec §12) — the server refuses the same.
    if session.disconnect {
        moderation.push(Entry::new(Label::Disconnect, Action::Disconnect));
    }
    if !user.is_admin {
        if session.kick {
            moderation.push(Entry::new(Label::Kick, Action::Kick).danger());
        }
        if session.ban {
            moderation.push(Entry::new(Label::Ban, Action::Ban).danger());
        }
    }
    if let Some(first) = moderation.first_mut() {
        first.separator = true;
    }
    entries.extend(moderation);
    entries
}

/// The menu for a channel header. Empty when there is nothing to say
/// about it.
#[must_use]
pub fn channel_menu(session: &Session, channel: &ChannelFacts) -> Vec<Entry> {
    if channel.from_config {
        // A one-line explanation for people who could otherwise
        // manage channels; nothing for everyone else.
        return if session.move_users || session.create_channels {
            vec![Entry::disabled(Label::ChannelConfigOwned)]
        } else {
            Vec::new()
        };
    }
    let manageable = (channel.owned && session.manage_own_channels) || session.move_users;
    if !manageable {
        return Vec::new();
    }
    vec![
        Entry::new(Label::ChannelEdit, Action::ChannelEdit(channel.id)),
        Entry::new(Label::ChannelDelete, Action::ChannelDelete(channel.id)).danger(),
    ]
}

/// What kind of chat row a menu is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageKind {
    Text,
    Code,
    Image,
    /// A file card (spec §7).
    File,
}

/// A chat row, as the menu sees it.
#[expect(
    clippy::struct_excessive_bools,
    reason = "mirrors the chat row's flags one to one"
)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MessageFacts {
    pub kind: MessageKind,
    /// Authored by this session: it may edit and delete.
    pub own: bool,
    pub deleted: bool,
    /// The body carries inline formatting (bold, italic, links).
    pub styled: bool,
    /// The styled body is currently shown as selectable plain text.
    pub selecting: bool,
    /// A direct message conversation: immutable in 1.1.1.
    pub in_dm: bool,
    /// `RESTRICT`: the session deletes anyone's message.
    pub delete_any: bool,
}

/// The menu for a chat row. Empty for a deleted message.
#[must_use]
pub fn message_menu(m: &MessageFacts) -> Vec<Entry> {
    if m.deleted {
        return Vec::new();
    }
    // A picture is a message with a blob (spec §12, 1.1.5): its own
    // entries first, then the same delete rule as text below.
    let mut entries = match m.kind {
        MessageKind::Image => vec![
            Entry::new(Label::OpenImage, Action::OpenImage),
            Entry::new(Label::SaveImage, Action::SaveImage),
            Entry::new(Label::CopyImage, Action::CopyImage),
        ],
        MessageKind::File => vec![Entry::new(Label::SaveFile, Action::SaveFile)],
        MessageKind::Code => vec![Entry::new(Label::Copy, Action::CopyText)],
        _ => vec![Entry::new(Label::CopyMessage, Action::CopyText)],
    };
    if m.kind == MessageKind::Text && m.styled {
        entries.push(if m.selecting {
            Entry::new(Label::ShowFormatting, Action::SelectText(false))
        } else {
            Entry::new(Label::SelectText, Action::SelectText(true))
        });
    }
    // Only the author edits; the author or a moderator deletes (the
    // server checks both; this mirrors it).
    if !m.in_dm {
        let mut editing = Vec::new();
        if m.own && !matches!(m.kind, MessageKind::File | MessageKind::Image) {
            editing.push(Entry::new(Label::EditMessage, Action::EditMessage));
        }
        if m.own || m.delete_any {
            editing.push(Entry::new(Label::DeleteMessage, Action::DeleteMessage).danger());
        }
        if let Some(first) = editing.first_mut() {
            first.separator = true;
        }
        entries.extend(editing);
    }
    entries
}

/// The composer's menu.
#[must_use]
pub fn composer_menu() -> Vec<Entry> {
    vec![
        Entry::new(Label::Cut, Action::Cut),
        Entry::new(Label::Copy, Action::Copy),
        Entry::new(Label::Paste, Action::Paste),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(entries: &[Entry]) -> Vec<Label> {
        entries.iter().map(|e| e.label.clone()).collect()
    }

    fn member() -> UserFacts {
        UserFacts {
            is_us: false,
            role: Role::Member,
            sharing: false,
            local_muted: false,
            outranked: false,
            is_admin: false,
            role_editable: false,
            channel_kickable: false,
            channel_id: 1,
        }
    }

    /// The four built-ins as a table, `held` for `member`.
    fn builtin_roles() -> Vec<RoleChoice> {
        [
            (1, "guest", 0, Role::Guest),
            (2, "member", 10, Role::Member),
            (3, "organizer", 20, Role::Organizer),
            (4, "admin", 30, Role::Admin),
        ]
        .into_iter()
        .map(|(id, name, rank, builtin)| RoleChoice {
            id,
            name: name.into(),
            rank,
            builtin: Some(builtin),
            held: builtin == Role::Member,
        })
        .collect()
    }

    #[test]
    fn own_row_has_no_menu() {
        let me = UserFacts {
            is_us: true,
            ..member()
        };
        assert!(user_menu(&Session::default(), &me, &[], &[]).is_empty());
    }

    #[test]
    fn plain_member_sees_only_audio_items() {
        let entries = user_menu(&Session::default(), &member(), &[], &[]);
        assert_eq!(
            labels(&entries),
            [Label::Volume, Label::MuteForMe, Label::ResetAudio]
        );
    }

    #[test]
    fn local_mute_flips_the_label() {
        let muted = UserFacts {
            local_muted: true,
            ..member()
        };
        let entries = user_menu(&Session::default(), &muted, &[], &[]);
        assert_eq!(entries[1].label, Label::UnmuteForMe);
    }

    #[test]
    fn watch_appears_only_while_sharing() {
        let sharing = UserFacts {
            sharing: true,
            ..member()
        };
        let entries = user_menu(&Session::default(), &sharing, &[], &[]);
        assert!(labels(&entries).contains(&Label::WatchStream));
    }

    #[test]
    fn message_needs_dm_on_both_ends() {
        let dm = Session {
            dm: true,
            ..Session::default()
        };
        let guest = UserFacts {
            role: Role::Guest,
            ..member()
        };
        let plain = user_menu(&Session::default(), &member(), &[], &[]);
        assert!(!labels(&plain).contains(&Label::Message));
        assert!(!labels(&user_menu(&dm, &guest, &[], &[])).contains(&Label::Message));
        let entries = user_menu(&dm, &member(), &[], &[]);
        let message = entries
            .iter()
            .find(|e| e.label == Label::Message)
            .expect("message entry");
        assert!(message.separator);
    }

    #[test]
    fn moderation_needs_permission_and_the_right_target() {
        let mod_ = Session {
            mute: true,
            disconnect: true,
            kick: true,
            ban: true,
            ..Session::default()
        };
        // Rank without permission: nothing.
        let outranked = UserFacts {
            outranked: true,
            ..member()
        };
        assert_eq!(
            user_menu(&Session::default(), &outranked, &[], &[]).len(),
            3
        );
        // Permission on an equal: mutes need rank; disconnect, kick and
        // ban do not.
        let entries = user_menu(&mod_, &member(), &[], &[]);
        assert_eq!(
            labels(&entries[3..]),
            [Label::Disconnect, Label::Kick, Label::Ban]
        );
        assert!(entries[3].separator && !entries[4].separator);
        // Outranked: the mute submenu joins them.
        let entries = user_menu(&mod_, &outranked, &[], &[]);
        assert_eq!(
            labels(&entries[3..]),
            [Label::Mute, Label::Disconnect, Label::Kick, Label::Ban]
        );
        assert!(entries[5].danger && entries[6].danger);
        // An administrator: disconnect only.
        let admin = UserFacts {
            is_admin: true,
            ..member()
        };
        assert_eq!(
            labels(&user_menu(&mod_, &admin, &[], &[])[3..]),
            [Label::Disconnect]
        );
    }

    #[test]
    fn role_submenu_offers_roles_up_to_ours() {
        let organizer = Session {
            roles: true,
            our_rank: 20,
            ..Session::default()
        };
        let editable = UserFacts {
            role_editable: true,
            ..member()
        };
        let entries = user_menu(&organizer, &editable, &[], &builtin_roles());
        let role = entries
            .iter()
            .find(|e| e.label == Label::Role)
            .expect("role submenu");
        let enabled: Vec<bool> = role.submenu.iter().map(|e| e.enabled).collect();
        // Guest yes, Member no (held), Organizer yes, Admin no (above us).
        assert_eq!(enabled, [true, false, true, false]);
        assert_eq!(role.submenu[2].action, Action::SetRole(3));
        assert_eq!(role.submenu[2].label, Label::RoleName(Role::Organizer));
    }

    #[test]
    fn custom_roles_appear_by_name_at_their_rank() {
        let admin = Session {
            roles: true,
            our_rank: 30,
            ..Session::default()
        };
        let editable = UserFacts {
            role_editable: true,
            ..member()
        };
        let mut roles = builtin_roles();
        roles.insert(
            2,
            RoleChoice {
                id: 7,
                name: "streamer".into(),
                rank: 15,
                builtin: None,
                held: false,
            },
        );
        let entries = user_menu(&admin, &editable, &[], &roles);
        let role = entries
            .iter()
            .find(|e| e.label == Label::Role)
            .expect("role submenu");
        assert_eq!(role.submenu[2].label, Label::Text("streamer".into()));
        assert_eq!(role.submenu[2].action, Action::SetRole(7));
        assert!(role.submenu[2].enabled);
        // No table at all (an older server): no submenu.
        assert!(!user_menu(&admin, &editable, &[], &[])
            .iter()
            .any(|e| e.label == Label::Role));
    }

    #[test]
    fn move_to_lists_channels_except_theirs() {
        let mover = Session {
            move_users: true,
            ..Session::default()
        };
        let channels = vec![
            ChannelChoice {
                id: 0,
                name: "General".into(),
            },
            ChannelChoice {
                id: 1,
                name: "Games".into(),
            },
        ];
        let entries = user_menu(&mover, &member(), &channels, &[]);
        let move_to = entries
            .iter()
            .find(|e| e.label == Label::MoveTo)
            .expect("move submenu");
        assert_eq!(move_to.submenu[0].label, Label::Text("General".into()));
        assert!(move_to.submenu[0].enabled);
        assert!(!move_to.submenu[1].enabled);
        assert_eq!(move_to.submenu[1].action, Action::MoveTo(1));
    }

    #[test]
    fn config_channel_explains_itself_only_to_managers() {
        let config = ChannelFacts {
            id: 0,
            owned: false,
            from_config: true,
        };
        assert!(channel_menu(&Session::default(), &config).is_empty());
        let creator = Session {
            create_channels: true,
            ..Session::default()
        };
        let entries = channel_menu(&creator, &config);
        assert_eq!(entries.len(), 1);
        assert!(!entries[0].enabled);
    }

    #[test]
    fn user_channel_edits_for_its_owner_or_a_manager() {
        let mine = ChannelFacts {
            id: 3,
            owned: true,
            from_config: false,
        };
        let theirs = ChannelFacts {
            owned: false,
            ..mine
        };
        let owner = Session {
            manage_own_channels: true,
            ..Session::default()
        };
        let manager = Session {
            move_users: true,
            ..Session::default()
        };
        assert!(channel_menu(&Session::default(), &mine).is_empty());
        assert!(channel_menu(&owner, &theirs).is_empty());
        let entries = channel_menu(&owner, &mine);
        assert_eq!(entries[0].action, Action::ChannelEdit(3));
        assert_eq!(entries[1].action, Action::ChannelDelete(3));
        assert!(entries[1].danger);
        assert_eq!(channel_menu(&manager, &theirs).len(), 2);
    }

    fn text_row() -> MessageFacts {
        MessageFacts {
            kind: MessageKind::Text,
            own: false,
            deleted: false,
            styled: false,
            selecting: false,
            in_dm: false,
            delete_any: false,
        }
    }

    #[test]
    fn deleted_message_has_no_menu() {
        let gone = MessageFacts {
            deleted: true,
            ..text_row()
        };
        assert!(message_menu(&gone).is_empty());
    }

    #[test]
    fn someone_elses_plain_message_only_copies() {
        assert_eq!(labels(&message_menu(&text_row())), [Label::CopyMessage]);
    }

    #[test]
    fn styled_message_toggles_its_plain_view() {
        let styled = MessageFacts {
            styled: true,
            ..text_row()
        };
        let entries = message_menu(&styled);
        assert_eq!(entries[1].action, Action::SelectText(true));
        let selecting = MessageFacts {
            selecting: true,
            ..styled
        };
        assert_eq!(message_menu(&selecting)[1].label, Label::ShowFormatting);
    }

    #[test]
    fn own_message_edits_and_deletes_outside_dms() {
        let own = MessageFacts {
            own: true,
            ..text_row()
        };
        let entries = message_menu(&own);
        assert_eq!(
            labels(&entries),
            [Label::CopyMessage, Label::EditMessage, Label::DeleteMessage]
        );
        assert!(entries[1].separator && entries[2].danger);
        let in_dm = MessageFacts { in_dm: true, ..own };
        assert_eq!(message_menu(&in_dm).len(), 1);
    }

    #[test]
    fn moderator_deletes_but_does_not_edit() {
        let mod_view = MessageFacts {
            delete_any: true,
            ..text_row()
        };
        assert_eq!(
            labels(&message_menu(&mod_view)),
            [Label::CopyMessage, Label::DeleteMessage]
        );
    }

    #[test]
    fn a_file_card_saves_and_deletes() {
        let own = MessageFacts {
            kind: MessageKind::File,
            own: true,
            ..text_row()
        };
        assert_eq!(
            labels(&message_menu(&own)),
            [Label::SaveFile, Label::DeleteMessage]
        );
        assert_eq!(message_menu(&own)[0].action, Action::SaveFile);
    }

    #[test]
    fn code_and_image_rows() {
        let code = MessageFacts {
            kind: MessageKind::Code,
            ..text_row()
        };
        assert_eq!(message_menu(&code)[0].label, Label::Copy);
        let image = MessageFacts {
            kind: MessageKind::Image,
            ..text_row()
        };
        assert_eq!(
            labels(&message_menu(&image)),
            [Label::OpenImage, Label::SaveImage, Label::CopyImage]
        );
    }

    #[test]
    fn own_picture_deletes_but_never_edits() {
        let own = MessageFacts {
            kind: MessageKind::Image,
            own: true,
            ..text_row()
        };
        assert_eq!(
            labels(&message_menu(&own)),
            [
                Label::OpenImage,
                Label::SaveImage,
                Label::CopyImage,
                Label::DeleteMessage
            ]
        );
        assert!(message_menu(&own)[3].danger);
        let moderated = MessageFacts {
            kind: MessageKind::Image,
            delete_any: true,
            ..text_row()
        };
        assert_eq!(labels(&message_menu(&moderated))[3], Label::DeleteMessage);
        let in_dm = MessageFacts {
            kind: MessageKind::Image,
            own: true,
            in_dm: true,
            ..text_row()
        };
        assert_eq!(message_menu(&in_dm).len(), 3);
    }
}
