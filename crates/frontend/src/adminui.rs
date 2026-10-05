//! The admin panel (spec §8): a window like settings, driven by the
//! admin wire. Each tab asks for its list (`AdminLogic.refresh` →
//! `send_admin(.., PendingAdmin::Panel(tab))`), the outcome fills the
//! tab's model, and every row action is the same `AdminCommand` the
//! slash command sends — after which the tab refreshes itself.

use std::rc::Rc;

use client::{network::NetworkControl, AdminCommand, AdminOutcome, AdminTarget};
use i18n::t;
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};
use ui::{
    AdminAuditRow, AdminBlobRow, AdminChannelRow, AdminInviteRow, AdminLogic, AdminRestrictionRow,
    AdminRoleRow, AdminState, AdminUserRow, AdminWindow,
};

use crate::app::{self, AppState, PendingAdmin};

pub const TAB_USERS: i32 = 0;
pub const TAB_ROLES: i32 = 1;
pub const TAB_RESTRICTIONS: i32 = 2;
pub const TAB_INVITES: i32 = 3;
pub const TAB_CHANNELS: i32 = 4;
pub const TAB_FILES: i32 = 5;
pub const TAB_AUDIT: i32 = 6;
pub const TAB_SERVER: i32 = 7;

/// Audit entries per page.
const AUDIT_PAGE: u16 = 50;

/// An action the panel asked about; it runs on "confirm" with the
/// reason typed into the dialog, when it takes one.
#[derive(Debug, Clone)]
pub enum PanelAction {
    Disconnect(AdminTarget),
    Kick(AdminTarget),
    Ban(AdminTarget),
    Purge(AdminTarget),
    ResetProfile(AdminTarget),
    DeleteRole(u32),
    DeleteBlob(client::protocol::BlobId),
    DeleteChannel(u16),
    RevokeInvite(String),
    Lift(u64),
}

/// Open the panel (creating it on first use) on `tab`.
pub fn open(app: &mut AppState, tab: i32) {
    if app::SINGLE_WINDOW_HOST {
        app.show_error(&t!("host-single-window"));
        return;
    }
    if app.admin_win.is_none() {
        let win = match AdminWindow::new() {
            Ok(w) => w,
            Err(e) => {
                app.show_error(&e.to_string());
                return;
            }
        };
        wire(&win);
        app::fill_tr(&win.global::<ui::Tr>());
        win.global::<ui::Theme>()
            .set_scale(app.settings.ui.text_scale);
        let weak = win.as_weak();
        let armed = Rc::new(std::cell::Cell::new(false));
        app::repaint_on_reveal(win.window(), armed, move || {
            if let Some(w) = weak.upgrade() {
                w.set_repaint_flip(!w.get_repaint_flip());
            }
        });
        app.admin_win = Some(win);
    }
    if let Some(win) = &app.admin_win {
        let s = win.global::<AdminState>();
        s.set_notice("".into());
        s.set_confirm_open(false);
        s.set_perm_names(ModelRc::new(VecModel::from(
            client::permissions::PERMISSION_NAMES
                .iter()
                .filter(|(_, bit)| *bit != client::permissions::ADMIN)
                .map(|(n, _)| SharedString::from(*n))
                .collect::<Vec<_>>(),
        )));
        s.set_role_icons(ModelRc::new(VecModel::from(
            client::permissions::ROLE_ICONS
                .iter()
                .map(|n| SharedString::from(*n))
                .collect::<Vec<_>>(),
        )));
        // Badges may be gold (spec §12, 1.1.5): the *name band* is the
        // permission, a badge's colour is whatever the admin says.
        s.set_role_colors(ModelRc::new(VecModel::from(
            badge_palette()
                .map(|c| slint::Color::from_argb_encoded(0xFF00_0000 | c))
                .collect::<Vec<_>>(),
        )));
        win.set_tab(tab);
        let _ = win.show();
    }
    refresh(app, tab);
}

fn wire(win: &AdminWindow) {
    let logic = win.global::<AdminLogic>();
    logic.on_refresh(|tab| app::with_current(move |app| refresh(app, tab)));
    logic.on_user_action(|user_id, action| {
        app::with_current(move |app| user_action(app, &user_id, action));
    });
    logic.on_user_set_role(|user_id, index| {
        app::with_current(move |app| {
            let Some(target) = target_of(&user_id) else {
                return;
            };
            // The dropdown lists the table in rank order.
            let ids = app.role_table_in_rank_order();
            let Some(role) = usize::try_from(index)
                .ok()
                .and_then(|i| ids.get(i).copied())
            else {
                return;
            };
            app.send_admin(
                AdminCommand::SetRoles {
                    target,
                    roles: vec![role],
                },
                PendingAdmin::Panel(TAB_USERS),
            );
        });
    });
    logic.on_role_select(|id| app::with_current(move |app| role_select(app, id)));
    logic.on_role_new(|| app::with_current(|app| role_select(app, -1)));
    logic.on_role_perm_toggled(|index, on| {
        app::with_current(move |app| {
            if let Some(win) = &app.admin_win
                && let Some(model) = win
                    .global::<AdminState>()
                    .get_perm_on()
                    .as_any()
                    .downcast_ref::<VecModel<bool>>()
                && let Ok(i) = usize::try_from(index)
                && i < model.row_count()
            {
                model.set_row_data(i, on);
            }
        });
    });
    logic.on_role_save(|| app::with_current(role_save));
    logic.on_role_delete(|id| {
        app::with_current(move |app| {
            if let Ok(role) = u32::try_from(id) {
                let name = app.role_name_of(role);
                ask(
                    app,
                    PanelAction::DeleteRole(role),
                    t!("admin-confirm-delete-role", "name" => name.as_str()),
                    t!("admin-confirm-delete-role-text"),
                    t!("admin-delete"),
                    false,
                );
            }
        });
    });
    logic.on_role_edit_cancel(|| {
        app::with_current(|app| {
            if let Some(win) = &app.admin_win {
                win.global::<AdminState>().set_role_edit_open(false);
            }
        });
    });
    logic.on_restriction_lift(|id| {
        app::with_current(move |app| {
            if let Ok(restriction_id) = u64::try_from(id) {
                ask(
                    app,
                    PanelAction::Lift(restriction_id),
                    t!("admin-confirm-lift"),
                    t!("admin-confirm-lift-text"),
                    t!("admin-lift"),
                    false,
                );
            }
        });
    });
    logic.on_invite_revoke(|code| {
        app::with_current(move |app| {
            ask(
                app,
                PanelAction::RevokeInvite(code.to_string()),
                t!("admin-confirm-revoke", "code" => code.as_str()),
                t!("admin-confirm-revoke-text"),
                t!("admin-revoke"),
                false,
            );
        });
    });
    logic.on_invite_new(|| {
        app::with_current(|app| {
            app.open_invite_dialog();
            app.show_main();
        });
    });
    logic.on_channel_edit(|id| {
        app::with_current(move |app| {
            if let Ok(id) = u16::try_from(id) {
                app.channel_edit(id);
                app.show_main();
            }
        });
    });
    logic.on_channel_delete(|id| {
        app::with_current(move |app| {
            if let Ok(id) = u16::try_from(id) {
                let name = app
                    .channels
                    .iter()
                    .find(|c| c.id == id)
                    .map_or_else(String::new, |c| c.name.clone());
                ask(
                    app,
                    PanelAction::DeleteChannel(id),
                    t!("admin-confirm-delete-channel", "name" => name.as_str()),
                    t!("admin-confirm-delete-channel-text"),
                    t!("admin-delete"),
                    false,
                );
            }
        });
    });
    logic.on_blob_delete(|hex| {
        app::with_current(move |app| {
            if let Some(blob_id) = app::parse_hex_id(&hex) {
                ask(
                    app,
                    PanelAction::DeleteBlob(blob_id),
                    t!("admin-confirm-delete-file"),
                    t!("admin-confirm-delete-file-text"),
                    t!("admin-delete"),
                    false,
                );
            }
        });
    });
    logic.on_confirm_accept(|| app::with_current(confirm_accept));
    logic.on_confirm_cancel(|| {
        app::with_current(|app| {
            app.admin_pending = None;
            if let Some(win) = &app.admin_win {
                win.global::<AdminState>().set_confirm_open(false);
            }
        });
    });
    logic.on_audit_more(|| {
        app::with_current(|app| {
            let before = app.admin_audit_oldest;
            app.send_admin(
                AdminCommand::AuditLog {
                    before,
                    limit: AUDIT_PAGE,
                },
                PendingAdmin::AuditMore,
            );
        });
    });
    logic.on_close(|| {
        app::with_current(|app| {
            if let Some(win) = &app.admin_win {
                let _ = win.hide();
            }
        });
    });
}

/// Ask the server for what `tab` shows. Channels come from what the
/// client already knows; the rest are admin commands.
pub fn refresh(app: &mut AppState, tab: i32) {
    let Some(win) = &app.admin_win else {
        return;
    };
    let s = win.global::<AdminState>();
    s.set_notice("".into());
    match tab {
        TAB_USERS => {
            s.set_role_names(ModelRc::new(VecModel::from(
                app.role_table_in_rank_order()
                    .into_iter()
                    .map(|id| SharedString::from(app.role_name_of(id)))
                    .collect::<Vec<_>>(),
            )));
            s.set_busy(true);
            app.send_admin(AdminCommand::ListUsers, PendingAdmin::Panel(tab));
        }
        TAB_ROLES => fill_roles(app),
        TAB_RESTRICTIONS => {
            s.set_busy(true);
            app.send_admin(AdminCommand::ListRestrictions, PendingAdmin::Panel(tab));
        }
        TAB_INVITES => {
            s.set_busy(true);
            app.send_admin(AdminCommand::ListInvites, PendingAdmin::Panel(tab));
        }
        TAB_CHANNELS => fill_channels(app),
        TAB_FILES => {
            s.set_busy(true);
            app.send_admin(AdminCommand::ListBlobs, PendingAdmin::Panel(tab));
        }
        TAB_AUDIT => {
            s.set_busy(true);
            app.admin_audit_oldest = None;
            app.send_admin(
                AdminCommand::AuditLog {
                    before: None,
                    limit: AUDIT_PAGE,
                },
                PendingAdmin::Panel(tab),
            );
        }
        TAB_SERVER => {
            s.set_busy(true);
            app.send_admin(AdminCommand::ServerStats, PendingAdmin::Panel(tab));
        }
        _ => {}
    }
}

/// An outcome the panel asked for.
pub fn take_outcome(app: &mut AppState, tab: i32, outcome: AdminOutcome) {
    let Some(win) = &app.admin_win else {
        return;
    };
    let s = win.global::<AdminState>();
    s.set_busy(false);
    match outcome {
        AdminOutcome::Users(users) => {
            let rows: Vec<AdminUserRow> = users
                .iter()
                .map(|u| AdminUserRow {
                    user_id: app::hex(&u.user_id).into(),
                    name: u.name.as_str().into(),
                    roles: u
                        .roles
                        .iter()
                        .map(|r| app.role_name_of(*r))
                        .collect::<Vec<_>>()
                        .join(", ")
                        .into(),
                    online: u.client_id.is_some(),
                    last_seen: if u.client_id.is_some() {
                        t!("admin-now").into()
                    } else {
                        app::describe_ago(u.last_seen).into()
                    },
                    keys: i32::try_from(u.keys.len()).unwrap_or(0),
                    role_index: top_role_index(app, &u.roles),
                })
                .collect();
            s.set_users(ModelRc::new(VecModel::from(rows)));
        }
        AdminOutcome::Restrictions(list) => {
            let rows: Vec<AdminRestrictionRow> = list
                .iter()
                .map(|r| AdminRestrictionRow {
                    id: i32::try_from(r.id).unwrap_or(-1),
                    kind: r.kind.name().into(),
                    who: r
                        .user_name
                        .clone()
                        .or_else(|| r.address.clone())
                        .or_else(|| r.key_hash.map(|k| app::short_id(&k[..8])))
                        .unwrap_or_default()
                        .into(),
                    until: app::describe_until(r.until).into(),
                    reason: r.reason.as_str().into(),
                    by: r.by.as_str().into(),
                })
                .collect();
            s.set_restrictions(ModelRc::new(VecModel::from(rows)));
        }
        AdminOutcome::Invites(list) => {
            let rows: Vec<AdminInviteRow> = list
                .iter()
                .map(|i| AdminInviteRow {
                    code: client::invite::display_code(&i.code).into(),
                    role: app.role_name_of(i.grants_role).into(),
                    expiry: app::describe_expiry(i.expires_at).into(),
                    uses: match i.max_uses {
                        Some(n) => format!("{} / {n}", i.uses),
                        None => t!("invite-uses-unlimited"),
                    }
                    .into(),
                    revoked: i.revoked,
                })
                .collect();
            s.set_invites(ModelRc::new(VecModel::from(rows)));
        }
        AdminOutcome::Blobs(list) => {
            let total: u64 = list.iter().map(|b| u64::from(b.size)).sum();
            let rows: Vec<AdminBlobRow> = list
                .iter()
                .map(|b| AdminBlobRow {
                    blob_hex: app::hex(&b.blob_id).into(),
                    name: b.name.as_str().into(),
                    kind: app_core::chat::FileMeta {
                        blob: b.blob_id,
                        name: b.name.clone(),
                        mime: b.mime.clone(),
                        size: b.size,
                    }
                    .kind_text()
                    .into(),
                    size: app_core::chat::human_size(u64::from(b.size)).into(),
                    by: b.created_by.clone().unwrap_or_default().into(),
                    when: app::describe_ago(b.created_at).into(),
                    refs: i32::try_from(b.refs).unwrap_or(0),
                })
                .collect();
            s.set_blobs(ModelRc::new(VecModel::from(rows)));
            let cap = app.admin_blob_cap.max(1);
            let used = app_core::chat::human_size(total);
            let cap_text = app_core::chat::human_size(cap);
            s.set_storage_line(
                t!("admin-storage", "used" => used.as_str(), "cap" => cap_text.as_str()).into(),
            );
            #[allow(clippy::cast_precision_loss)]
            s.set_storage_frac((total as f64 / cap as f64) as f32);
        }
        AdminOutcome::Audit(entries) => {
            let more = entries.len() >= usize::from(AUDIT_PAGE);
            let oldest = entries.last().map(|e| e.id);
            let fresh: Vec<AdminAuditRow> = entries
                .iter()
                .map(|e| AdminAuditRow {
                    when: app::format_wall_stamp(e.ts).into(),
                    actor: e
                        .actor
                        .clone()
                        .unwrap_or_else(|| t!("admin-actor-server"))
                        .into(),
                    action: audit_action_text(&e.action).into(),
                    target: e.target.clone().unwrap_or_default().into(),
                    detail: e.detail.clone().unwrap_or_default().into(),
                })
                .collect();
            if tab == TAB_AUDIT {
                s.set_audit(ModelRc::new(VecModel::from(fresh)));
            } else {
                let shown = s.get_audit();
                if let Some(model) = shown.as_any().downcast_ref::<VecModel<AdminAuditRow>>() {
                    for row in fresh {
                        model.push(row);
                    }
                }
            }
            if let Some(id) = oldest {
                app.admin_audit_oldest = Some(id);
            }
            s.set_audit_more(more);
        }
        AdminOutcome::Stats(st) => {
            app.admin_blob_cap = st.blob_cap_bytes;
            s.set_stat_version(format!("baucord {}", st.version).into());
            s.set_stat_uptime(app::describe_duration(st.uptime_secs).into());
            s.set_stat_online(st.online.to_string().into());
            s.set_stat_users(st.users.to_string().into());
            s.set_stat_messages(st.messages.to_string().into());
            let used = app_core::chat::human_size(st.blob_bytes);
            let cap_text = app_core::chat::human_size(st.blob_cap_bytes);
            s.set_stat_blobs(
                t!(
                    "admin-blobs-line",
                    "count" => st.blobs,
                    "used" => used.as_str(),
                    "cap" => cap_text.as_str()
                )
                .into(),
            );
            s.set_stat_persistence(
                if st.persistent {
                    t!("admin-persistent-on")
                } else {
                    t!("admin-persistent-off")
                }
                .into(),
            );
            s.set_stat_queue(format!("{} / {}", st.queue_depth, st.queue_capacity).into());
            s.set_stat_dropped(st.dropped_writes.to_string().into());
            s.set_stat_errors(st.write_errors.to_string().into());
        }
        AdminOutcome::Done | AdminOutcome::RoleCreated(_) => {
            // The action landed: the tab shows the new state.
            if tab == TAB_ROLES {
                s.set_role_edit_open(false);
            }
            refresh(app, tab);
        }
        AdminOutcome::Telemetry(_) => {}
    }
}

/// A refusal the panel asked for.
pub fn take_error(app: &mut AppState, why: &str) {
    if let Some(win) = &app.admin_win {
        let s = win.global::<AdminState>();
        s.set_busy(false);
        s.set_notice(t!("chat-admin-refused", "why" => why).into());
    }
}

/// The roles tab is the client's own table.
fn fill_roles(app: &mut AppState) {
    let Some(win) = &app.admin_win else {
        return;
    };
    let holders = |id: client::RoleId| {
        app.details
            .values()
            .filter(|(_, roles)| roles.contains(&id))
            .count()
    };
    let rows: Vec<AdminRoleRow> = app
        .role_table_in_rank_order()
        .into_iter()
        .filter_map(|id| app.roles.get(&id))
        .map(|r| AdminRoleRow {
            id: i32::try_from(r.id).unwrap_or(-1),
            name: r.name.as_str().into(),
            standing: app::describe_standing(r.rank).into(),
            builtin: r.builtin,
            permissions: client::permissions::permission_names(r.permissions)
                .join(", ")
                .into(),
            holders: i32::try_from(holders(r.id)).unwrap_or(0),
            icon: r.icon.as_str().into(),
            color: slint::Color::from_argb_encoded(0xFF00_0000 | r.color),
            has_color: r.color != 0,
        })
        .collect();
    win.global::<AdminState>()
        .set_roles(ModelRc::new(VecModel::from(rows)));
}

/// The channels tab is the client's own list.
/// The role-badge palette: the name colours plus gold (spec §12,
/// 1.1.5). One source for the swatches, the edit form's selection and
/// the value saved — an index into one that isn't an index into the
/// others is how gold came out grey.
fn badge_palette() -> impl Iterator<Item = u32> {
    crate::settingsui::NAME_COLORS
        .iter()
        .copied()
        .chain(std::iter::once(crate::settingsui::GOLD))
}

fn fill_channels(app: &mut AppState) {
    let Some(win) = &app.admin_win else {
        return;
    };
    let rows: Vec<AdminChannelRow> = app
        .channels
        .iter()
        .filter(|c| Some(c.name.as_str()) != app.lobby_name.as_deref())
        .map(|c| AdminChannelRow {
            id: i32::from(c.id),
            name: c.name.as_str().into(),
            members: i32::try_from(app.users.values().filter(|u| u.channel_id == c.id).count())
                .unwrap_or(0),
            owner: if c.owned {
                t!("admin-yours").into()
            } else {
                "".into()
            },
            config_owned: c.from_config,
        })
        .collect();
    win.global::<AdminState>()
        .set_channels(ModelRc::new(VecModel::from(rows)));
}

/// Open the editor on role `id` (-1: a new one).
fn role_select(app: &mut AppState, id: i32) {
    let Some(win) = &app.admin_win else {
        return;
    };
    let s = win.global::<AdminState>();
    let current = u32::try_from(id).ok().and_then(|id| app.roles.get(&id));
    let bits = current.map_or(client::permissions::GUEST_PERMISSIONS, |r| r.permissions);
    let on: Vec<bool> = client::permissions::PERMISSION_NAMES
        .iter()
        .filter(|(_, bit)| *bit != client::permissions::ADMIN)
        .map(|(_, bit)| client::permissions::has(bits, *bit))
        .collect();
    s.set_role_edit_id(id);
    s.set_role_edit_name(current.map_or("", |r| r.name.as_str()).into());
    s.set_role_edit_icon(current.map_or("", |r| r.icon.as_str()).into());
    let color = current.map_or(0, |r| r.color);
    s.set_role_edit_color(
        badge_palette()
            .position(|c| c == color)
            .and_then(|i| i32::try_from(i).ok())
            .unwrap_or(-1),
    );
    s.set_perm_on(ModelRc::new(VecModel::from(on)));
    s.set_role_edit_open(true);
}

fn role_save(app: &mut AppState) {
    let Some(win) = &app.admin_win else {
        return;
    };
    let s = win.global::<AdminState>();
    let name = s.get_role_edit_name().trim().to_string();
    let on = s.get_perm_on();
    let permissions: u64 = client::permissions::PERMISSION_NAMES
        .iter()
        .filter(|(_, bit)| *bit != client::permissions::ADMIN)
        .enumerate()
        .filter(|(i, _)| on.row_data(*i).unwrap_or(false))
        .map(|(_, (_, bit))| *bit)
        .fold(0, |acc, bit| acc | bit);
    let icon = s.get_role_edit_icon().to_string();
    let color = usize::try_from(s.get_role_edit_color())
        .ok()
        .and_then(|i| badge_palette().nth(i))
        .unwrap_or(0);
    let id = s.get_role_edit_id();
    let command = match u32::try_from(id) {
        Ok(role) => AdminCommand::EditRole {
            role,
            name: Some(name),
            permissions: Some(permissions),
            icon: Some(icon),
            color: Some(color),
        },
        Err(_) => AdminCommand::CreateRole {
            name,
            permissions,
            icon,
            color,
        },
    };
    app.send_admin(command, PendingAdmin::Panel(TAB_ROLES));
}

/// A row button on the Users tab: every one of them asks first. The
/// dialog says what the action does; the ones that carry a reason to
/// the person ask for it there.
fn user_action(app: &mut AppState, user_id: &str, action: i32) {
    let Some(target) = target_of(user_id) else {
        return;
    };
    let name = app
        .admin_win
        .as_ref()
        .and_then(|w| {
            let users = w.global::<AdminState>().get_users();
            (0..users.row_count())
                .filter_map(|i| users.row_data(i))
                .find(|u| u.user_id.as_str() == user_id)
                .map(|u| u.name.to_string())
        })
        .unwrap_or_default();
    let n = name.as_str();
    let (pending, title, text, label, reason) = match action {
        1 => (
            PanelAction::Disconnect(target),
            t!("admin-confirm-disconnect", "name" => n),
            t!("admin-confirm-disconnect-text"),
            t!("admin-disconnect"),
            true,
        ),
        2 => (
            PanelAction::Kick(target),
            t!("admin-confirm-kick", "name" => n),
            t!("admin-confirm-kick-text"),
            t!("admin-kick"),
            true,
        ),
        3 => (
            PanelAction::Ban(target),
            t!("admin-confirm-ban", "name" => n),
            t!("admin-confirm-ban-text"),
            t!("admin-ban"),
            true,
        ),
        4 => (
            PanelAction::Purge(target),
            t!("admin-confirm-purge", "name" => n),
            t!("admin-confirm-purge-text"),
            t!("admin-purge"),
            false,
        ),
        5 => (
            PanelAction::ResetProfile(target),
            t!("admin-confirm-reset-profile", "name" => n),
            t!("admin-confirm-reset-profile-text"),
            t!("admin-reset-profile"),
            false,
        ),
        _ => return,
    };
    ask(app, pending, title, text, label, reason);
}

/// Open the confirmation for `action`.
fn ask(
    app: &mut AppState,
    action: PanelAction,
    title: String,
    text: String,
    label: String,
    asks_reason: bool,
) {
    let Some(win) = &app.admin_win else {
        return;
    };
    let s = win.global::<AdminState>();
    s.set_confirm_title(title.into());
    s.set_confirm_text(text.into());
    s.set_confirm_label(label.into());
    s.set_confirm_asks_reason(asks_reason);
    s.set_confirm_reason("".into());
    s.set_confirm_danger(!matches!(action, PanelAction::Lift(_)));
    s.set_confirm_open(true);
    app.admin_pending = Some(action);
}

/// "Confirm": run the pending action with the reason typed.
fn confirm_accept(app: &mut AppState) {
    let Some(action) = app.admin_pending.take() else {
        return;
    };
    let reason = app
        .admin_win
        .as_ref()
        .map(|w| {
            let s = w.global::<AdminState>();
            s.set_confirm_open(false);
            s.get_confirm_reason().trim().to_string()
        })
        .unwrap_or_default();
    match action {
        PanelAction::Disconnect(target) => app.send_admin(
            AdminCommand::Disconnect { target, reason },
            PendingAdmin::Panel(TAB_USERS),
        ),
        PanelAction::Kick(target) => app.send_admin(
            AdminCommand::Kick { target, reason },
            PendingAdmin::Panel(TAB_USERS),
        ),
        PanelAction::Ban(target) => app.send_admin(
            AdminCommand::Ban {
                target,
                duration_secs: None,
                reason,
            },
            PendingAdmin::Panel(TAB_USERS),
        ),
        PanelAction::Purge(target) => {
            app.send_admin(
                AdminCommand::PurgeUser { target },
                PendingAdmin::Panel(TAB_USERS),
            );
        }
        PanelAction::ResetProfile(target) => app.send_admin(
            AdminCommand::ResetProfile {
                target,
                lock: false,
            },
            PendingAdmin::Panel(TAB_USERS),
        ),
        PanelAction::DeleteRole(role) => {
            app.send_admin(
                AdminCommand::DeleteRole { role },
                PendingAdmin::Panel(TAB_ROLES),
            );
        }
        PanelAction::DeleteBlob(blob_id) => app.send_admin(
            AdminCommand::DeleteBlob { blob_id },
            PendingAdmin::Panel(TAB_FILES),
        ),
        PanelAction::DeleteChannel(channel_id) => {
            app.network
                .tx
                .send(NetworkControl::DeleteChannel { channel_id })
                .ok();
        }
        PanelAction::RevokeInvite(code) => app.send_admin(
            AdminCommand::RevokeInvite { code },
            PendingAdmin::Panel(TAB_INVITES),
        ),
        PanelAction::Lift(restriction_id) => app.send_admin(
            AdminCommand::Lift { restriction_id },
            PendingAdmin::Panel(TAB_RESTRICTIONS),
        ),
    }
}

/// An audit action as a verb phrase; unknown ones read as stored.
pub(crate) fn audit_action_text(action: &str) -> String {
    let key = match action {
        "user.join" => "admin-act-join",
        "user.kick" => "admin-act-kick",
        "user.disconnect" => "admin-act-disconnect",
        "user.move" => "admin-act-move",
        "user.roles" => "admin-act-roles",
        "user.purge" => "admin-act-purge",
        "restrict" => "admin-act-restrict",
        "restriction.lift" => "admin-act-lift",
        "message.delete" => "admin-act-message-delete",
        "invite.create" => "admin-act-invite-create",
        "invite.revoke" => "admin-act-invite-revoke",
        "key.link" => "admin-act-key-link",
        "key.unlink" => "admin-act-key-unlink",
        "profile.reset" => "admin-act-profile-reset",
        "role.create" => "admin-act-role-create",
        "role.edit" => "admin-act-role-edit",
        "role.delete" => "admin-act-role-delete",
        "channel.create" => "admin-act-channel-create",
        "channel.edit" => "admin-act-channel-edit",
        "channel.delete" => "admin-act-channel-delete",
        "blob.delete" => "admin-act-blob-delete",
        _ => return action.to_string(),
    };
    t!(key)
}

/// The index, in the rank-ordered table, of the highest role a user
/// holds; -1 with none (the dropdown's placeholder).
fn top_role_index(app: &AppState, roles: &[client::RoleId]) -> i32 {
    let order = app.role_table_in_rank_order();
    order
        .iter()
        .rposition(|id| roles.contains(id))
        .and_then(|i| i32::try_from(i).ok())
        .unwrap_or(-1)
}

fn target_of(user_id: &str) -> Option<AdminTarget> {
    app::parse_hex_id(user_id).map(AdminTarget::User)
}
