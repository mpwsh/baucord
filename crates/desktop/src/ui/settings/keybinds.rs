// ui/settings/keybinds.rs — Keyboard shortcut configuration panel.
//
// Key capture uses a global FLTK event handler (app::add_handler) with
// static state, because add_handler requires a plain fn pointer (no
// closures with captures). A static Mutex holds the recording state
// and an FLTK sender to push captured keybinds back to the UI.

use fltk::{prelude::*, *};
use i18n::t;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Mutex, Once};

use crate::ui::messages::UiMessage;
use client::config::{BindableAction, build_bind_string};
use ui::theme;
use ui::widgets::labels;

// ── Static capture state (for add_handler fn pointer) ───────────────

/// What the global handler captured.
struct CapturedKey {
    bind: String,
    action: BindableAction,
}

static HANDLER_INIT: Once = Once::new();

/// All capture state behind one lock, so the invariants (recording is
/// exclusive with a pending result; at most one of captured /
/// cancelled / cleared is set per capture) live in one place instead
/// of four independent Mutexes.
#[derive(Default)]
struct CaptureShared {
    /// The action currently being recorded, if any.
    recording: Option<BindableAction>,
    /// A completed capture waiting for the panel's bridge timer.
    captured: Option<CapturedKey>,
    /// Escape was pressed — reset the row buttons.
    cancelled: bool,
    /// Backspace was pressed — clear this action's binding.
    cleared: Option<BindableAction>,
}

static CAPTURE: Mutex<CaptureShared> = Mutex::new(CaptureShared {
    recording: None,
    captured: None,
    cancelled: false,
    cleared: None,
});

/// Abort any in-progress keybind recording and drop pending results.
///
/// Called by App when the settings window closes: the global key
/// handler is installed for the process lifetime, so a recording left
/// armed would swallow the next unconsumed keypress anywhere in the
/// app and silently apply it as a keybind on the next settings-open.
pub fn cancel_recording() {
    if let Ok(mut g) = CAPTURE.lock() {
        *g = CaptureShared::default();
    }
}

/// Global FLTK event handler — captures keys while recording is active.
/// Installed once via app::add_handler.
fn global_key_handler(ev: enums::Event) -> bool {
    if ev != enums::Event::Shortcut && ev != enums::Event::KeyDown {
        return false;
    }

    let current_action = match CAPTURE.lock().ok().and_then(|g| g.recording) {
        Some(a) => a,
        None => return false,
    };

    let key = app::event_key();

    // Skip bare modifier presses
    if is_modifier_key(key) {
        return true;
    }

    // Escape = cancel
    if key == enums::Key::Escape {
        if let Ok(mut g) = CAPTURE.lock() {
            g.recording = None;
            g.cancelled = true;
        }
        return true;
    }

    // Backspace = clear binding
    if key == enums::Key::BackSpace {
        if let Ok(mut g) = CAPTURE.lock() {
            g.recording = None;
            g.cleared = Some(current_action);
        }
        return true;
    }

    // Capture the key combination
    let state = app::event_state();
    let ctrl =
        state.contains(enums::EventState::Ctrl) || state.contains(enums::EventState::Command);
    let shift = state.contains(enums::EventState::Shift);
    let alt = state.contains(enums::EventState::Alt);

    if let Some(code) = fltk_key_to_code_string(key) {
        let bind = build_bind_string(ctrl, alt, shift, &code);

        if let Ok(mut g) = CAPTURE.lock() {
            g.recording = None;
            g.captured = Some(CapturedKey {
                bind,
                action: current_action,
            });
        }
    }

    true
}

// ── Per-row UI widgets ──────────────────────────────────────────────

struct RowWidgets {
    action: BindableAction,
    display: frame::Frame,
    button: button::Button,
}

pub fn build(
    sender: app::Sender<UiMessage>,
    settings: &client::config::Settings,
    x: i32,
    y: i32,
    w: i32,
) -> group::Flex {
    let mut col = group::Flex::default()
        .with_pos(x, y)
        .with_size(w, 400)
        .column();
    col.set_spacing(8);

    labels::section_header(&mut col, "\u{2328}", &t!("keybinds-header"));

    // Install the global handler once per process. `add_handler`
    // appends — calling it on every settings-open used to accumulate
    // duplicate handlers for the lifetime of the app.
    HANDLER_INIT.call_once(|| {
        app::add_handler(global_key_handler);
    });

    let rows: Rc<RefCell<Vec<RowWidgets>>> = Rc::new(RefCell::new(Vec::new()));

    for &action in BindableAction::ALL {
        let bind_str = settings.bind_for(action).to_string();
        let (display, button) = build_keybind_row(&mut col, action, &bind_str, rows.clone());
        rows.borrow_mut().push(RowWidgets {
            action,
            display,
            button,
        });
    }

    labels::hint_label(&mut col, &t!("keybinds-hint"));

    col.end();

    // ── Timer: poll static capture state at 20Hz ────────────────
    // This bridges the static fn handler → FLTK widget updates + UiMessage sends.

    let rows_poll = rows.clone();
    let alive_probe = col.clone();
    app::add_timeout3(0.05, move |handle| {
        // Stop polling once the settings window is gone — otherwise every
        // settings-open would leak a permanent 20 Hz timer. (The panel
        // itself may be hidden by tab switching, so check the window.)
        if !alive_probe.window().map(|w| w.visible()).unwrap_or(false) {
            return;
        }

        // Take all pending results under one lock.
        let (captured, cancelled, cleared) = match CAPTURE.lock() {
            Ok(mut g) => (
                g.captured.take(),
                std::mem::take(&mut g.cancelled),
                g.cleared.take(),
            ),
            Err(_) => (None, false, None),
        };

        // Check for captured key
        if let Some(cap) = captured {
            for row in rows_poll.borrow().iter() {
                if row.action == cap.action {
                    let mut disp = row.display.clone();
                    let mut btn = row.button.clone();
                    disp.set_label(&cap.bind);
                    disp.set_label_color(theme::ACCENT);
                    reset_button(&mut btn);
                } else if row.display.label() == cap.bind {
                    // The key was bound to another action — steal it.
                    // Registering the same hotkey twice fails at the OS
                    // level, so a duplicate would silently leave the
                    // other action without a working key while its row
                    // still showed the bind.
                    let mut disp = row.display.clone();
                    let mut btn = row.button.clone();
                    disp.set_label(&t!("keybind-not-set"));
                    disp.set_label_color(theme::TEXT_MUTED);
                    reset_button(&mut btn);
                    sender.send(UiMessage::SetKeybind {
                        action: row.action,
                        bind: String::new(),
                    });
                }
            }
            sender.send(UiMessage::SetKeybind {
                action: cap.action,
                bind: cap.bind,
            });
        }

        // Check for cancel
        if cancelled {
            for row in rows_poll.borrow().iter() {
                let mut btn = row.button.clone();
                reset_button(&mut btn);
            }
        }

        // Check for clear
        if let Some(action) = cleared {
            for row in rows_poll.borrow().iter() {
                if row.action == action {
                    let mut disp = row.display.clone();
                    let mut btn = row.button.clone();
                    disp.set_label(&t!("keybind-not-set"));
                    disp.set_label_color(theme::TEXT_MUTED);
                    reset_button(&mut btn);
                }
            }
            sender.send(UiMessage::SetKeybind {
                action,
                bind: String::new(),
            });
        }

        app::repeat_timeout3(0.05, handle);
    });

    col
}

fn reset_button(btn: &mut button::Button) {
    btn.set_label(&t!("keybind-record"));
    btn.set_color(theme::BG_TERTIARY);
    btn.set_label_color(theme::TEXT_PRIMARY);
}

fn build_keybind_row(
    col: &mut group::Flex,
    action: BindableAction,
    bind_str: &str,
    all_rows: Rc<RefCell<Vec<RowWidgets>>>,
) -> (frame::Frame, button::Button) {
    let mut row = group::Flex::default().row();
    col.fixed(&row, 35);
    row.set_spacing(10);

    let mut label = frame::Frame::default()
        .with_label(&action_label(action))
        .with_align(enums::Align::Left | enums::Align::Inside);
    label.set_label_color(theme::TEXT_PRIMARY);
    label.set_label_size(13);
    row.fixed(&label, 150);

    let display_text = if bind_str.is_empty() {
        t!("keybind-not-set")
    } else {
        bind_str.to_string()
    };
    let mut binding_display = frame::Frame::default()
        .with_label(&display_text)
        .with_align(enums::Align::Center);
    binding_display.set_label_color(if bind_str.is_empty() {
        theme::TEXT_MUTED
    } else {
        theme::ACCENT
    });
    binding_display.set_label_size(13);
    binding_display.set_frame(enums::FrameType::BorderBox);
    binding_display.set_color(theme::SURFACE);

    let mut record_btn = button::Button::default().with_label(&t!("keybind-record"));
    row.fixed(&record_btn, 80);
    record_btn.set_color(theme::BG_TERTIARY);
    record_btn.set_label_color(theme::TEXT_PRIMARY);
    record_btn.set_label_size(12);
    record_btn.set_frame(enums::FrameType::FlatBox);

    row.end();

    let display_ret = binding_display.clone();
    let btn_ret = record_btn.clone();

    let mut disp = binding_display.clone();
    record_btn.set_callback(move |btn| {
        let was_recording = CAPTURE
            .lock()
            .ok()
            .map(|g| g.recording == Some(action))
            .unwrap_or(false);

        // Cancel any other active recording first
        if !was_recording {
            for row in all_rows.borrow().iter() {
                if row.action != action {
                    let mut other_btn = row.button.clone();
                    reset_button(&mut other_btn);
                }
            }
        }

        if was_recording {
            // Cancel
            if let Ok(mut g) = CAPTURE.lock() {
                g.recording = None;
            }
            reset_button(btn);
        } else {
            // Start recording
            if let Ok(mut g) = CAPTURE.lock() {
                g.recording = Some(action);
            }
            btn.set_label("...");
            btn.set_color(theme::ACCENT_HOVER);
            btn.set_label_color(theme::BUTTON_TEXT);
            disp.set_label(&t!("keybind-press-combo"));
            disp.set_label_color(theme::WARNING);
        }
    });

    (display_ret, btn_ret)
}

fn is_modifier_key(key: enums::Key) -> bool {
    matches!(
        key,
        enums::Key::ShiftL
            | enums::Key::ShiftR
            | enums::Key::ControlL
            | enums::Key::ControlR
            | enums::Key::AltL
            | enums::Key::AltR
    )
}

fn fltk_key_to_code_string(key: enums::Key) -> Option<String> {
    match key {
        enums::Key::F1 => return Some("F1".into()),
        enums::Key::F2 => return Some("F2".into()),
        enums::Key::F3 => return Some("F3".into()),
        enums::Key::F4 => return Some("F4".into()),
        enums::Key::F5 => return Some("F5".into()),
        enums::Key::F6 => return Some("F6".into()),
        enums::Key::F7 => return Some("F7".into()),
        enums::Key::F8 => return Some("F8".into()),
        enums::Key::F9 => return Some("F9".into()),
        enums::Key::F10 => return Some("F10".into()),
        enums::Key::F11 => return Some("F11".into()),
        enums::Key::F12 => return Some("F12".into()),
        enums::Key::Enter => return Some("Enter".into()),
        enums::Key::Tab => return Some("Tab".into()),
        enums::Key::Insert => return Some("Insert".into()),
        enums::Key::Delete => return Some("Delete".into()),
        enums::Key::Home => return Some("Home".into()),
        enums::Key::End => return Some("End".into()),
        enums::Key::PageUp => return Some("PageUp".into()),
        enums::Key::PageDown => return Some("PageDown".into()),
        enums::Key::Up => return Some("ArrowUp".into()),
        enums::Key::Down => return Some("ArrowDown".into()),
        enums::Key::Left => return Some("ArrowLeft".into()),
        enums::Key::Right => return Some("ArrowRight".into()),
        enums::Key::KPEnter => return Some("NumpadEnter".into()),
        _ => {}
    }

    let bits = key.bits();
    if bits > 0 && bits < 128 {
        let ch = (bits as u8) as char;
        match ch {
            'a'..='z' => return Some(format!("Key{}", ch.to_ascii_uppercase())),
            'A'..='Z' => return Some(format!("Key{}", ch)),
            '0'..='9' => return Some(format!("Digit{}", ch)),
            ' ' => return Some("Space".into()),
            '-' => return Some("Minus".into()),
            '=' => return Some("Equal".into()),
            '[' => return Some("BracketLeft".into()),
            ']' => return Some("BracketRight".into()),
            '\\' => return Some("Backslash".into()),
            ';' => return Some("Semicolon".into()),
            '\'' => return Some("Quote".into()),
            ',' => return Some("Comma".into()),
            '.' => return Some("Period".into()),
            '/' => return Some("Slash".into()),
            '`' => return Some("Backquote".into()),
            _ => {}
        }
    }
    let kp_base = enums::Key::KP.bits();
    let kp_last = enums::Key::KPLast.bits();
    if bits > kp_base && bits <= kp_last {
        let kp_char = (bits - kp_base) as u8 as char;
        let name = match kp_char {
            '0'..='9' => format!("Numpad{kp_char}"),
            '+' => "NumpadAdd".into(),
            '-' => "NumpadSubtract".into(),
            '*' => "NumpadMultiply".into(),
            '/' => "NumpadDivide".into(),
            '.' => "NumpadDecimal".into(),
            _ => return None,
        };
        return Some(name);
    }

    None
}

/// Localized display names for keybindable actions. The core's
/// `BindableAction::label()` stays English on purpose — it feeds log
/// lines, and logs are developer-facing.
fn action_label(action: BindableAction) -> String {
    t!(match action {
        BindableAction::ToggleMute => "action-toggle-mute",
        BindableAction::ToggleDeafen => "action-toggle-deafen",
        BindableAction::ToggleScreenShare => "action-toggle-share",
        BindableAction::PushToTalk => "action-push-to-talk",
    })
}
