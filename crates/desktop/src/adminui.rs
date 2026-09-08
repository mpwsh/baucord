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

/// Open the panel (creating it on first use) on `tab`.
pub fn open(app: &mut AppState, tab: i32) {
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
        s.set_confirming("".into());
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
        s.set_role_colors(ModelRc::new(VecModel::from(
            crate::settingsui::NAME_COLORS
                .iter()
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
            let Some(role) = usize::try_from(index).ok().and_then(|i| ids.get(i).copied()) else {
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
                app.send_admin(AdminCommand::DeleteRole { role }, PendingAdmin::Panel(TAB_ROLES));
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
                app.send_admin(
                    AdminCommand::Lift { restriction_id },
                    PendingAdmin::Panel(TAB_RESTRICTIONS),
                );
            }
        });
    });
    logic.on_invite_revoke(|code| {
        app::with_current(move |app| {
            app.send_admin(
                AdminCommand::RevokeInvite {
                    code: code.to_string(),
                },
                PendingAdmin::Panel(TAB_INVITES),
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
                app.network
                    .tx
                    .send(NetworkControl::DeleteChannel { channel_id: id })
                    .ok();
            }
        });
    });
    logic.on_blob_delete(|hex| {
        app::with_current(move |app| {
            if let Some(blob_id) = app::parse_hex_id(&hex) {
                app.send_admin(
                    AdminCommand::DeleteBlob { blob_id },
                    PendingAdmin::Panel(TAB_FILES),
                );
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
                    when: app::format_wall_time(e.ts).into(),
                    actor: e.actor.clone().unwrap_or_else(|| "server".into()).into(),
                    action: e.action.as_str().into(),
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
fn fill_channels(app: &mut AppState) {
    let Some(win) = &app.admin_win else {
        return;
    };
    let rows: Vec<AdminChannelRow> = app
        .channels
        .iter()
        .map(|c| AdminChannelRow {
            id: i32::from(c.id),
            name: c.name.as_str().into(),
            members: i32::try_from(app.users.values().filter(|u| u.channel_id == c.id).count())
                .unwrap_or(0),
            owner: if c.owned { t!("admin-yours").into() } else { "".into() },
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
        crate::settingsui::NAME_COLORS
            .iter()
            .position(|c| *c == color)
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
        .and_then(|i| crate::settingsui::NAME_COLORS.get(i).copied())
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

fn user_action(app: &mut AppState, user_id: &str, action: i32) {
    let Some(target) = target_of(user_id) else {
        return;
    };
    let reason = app
        .admin_win
        .as_ref()
        .map(|w| w.global::<AdminState>().get_reason().to_string())
        .unwrap_or_default();
    let command = match action {
        1 => AdminCommand::Disconnect { target, reason },
        2 => AdminCommand::Kick { target, reason },
        3 => AdminCommand::Ban {
            target,
            duration_secs: None,
            reason,
        },
        4 => AdminCommand::PurgeUser { target },
        5 => AdminCommand::ResetProfile {
            target,
            lock: false,
        },
        6 => AdminCommand::ResetProfile { target, lock: true },
        _ => return,
    };
    app.send_admin(command, PendingAdmin::Panel(TAB_USERS));
}

fn target_of(user_id: &str) -> Option<AdminTarget> {
    app::parse_hex_id(user_id).map(AdminTarget::User)
}
