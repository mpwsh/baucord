// ui/chat.rs
use crate::ui::chat_view::ChatView;
use crate::ui::emoji;
use crate::ui::messages::UiMessage;
use fltk::{
    image::{PngImage, RgbImage},
    prelude::*,
    *,
};
use i18n::t;
use std::cell::RefCell;
use std::io::Cursor;
use std::rc::Rc;
use ui::theme;
use ui::widgets::{controls, style};

/// Hard cap on an encoded outgoing image. Keeps a single chat message
/// well within renet's reliable-channel budget; the paste pipeline
/// downscales until it fits (or refuses).
const MAX_IMAGE_BYTES: usize = 1024 * 1024;
/// First-pass bound for a pasted image's longest edge.
const MAX_IMAGE_DIM: u32 = 1280;

pub struct Chat {
    // UI elements kept alive for method access.
    // sender, message_input, char_count, and the emoji button are
    // captured by closures in new() and don't need to live on the
    // struct.
    group: group::Flex,
    view: ChatView,
    /// Currently open emoji picker popup, if any.
    picker: Rc<RefCell<Option<window::Window>>>,
}

impl Chat {
    pub fn new(sender: app::Sender<UiMessage>) -> Self {
        let mut chat_col = group::Flex::default().column();
        chat_col.set_margins(10, 10, 10, 10);
        chat_col.set_spacing(10);

        // Rich message history (text + links + inline images).
        let view = ChatView::new();

        // Input row: [😀] [message input      ] [0/400] [Send]
        let mut input_row = group::Flex::default().row();
        chat_col.fixed(&input_row, 40);
        input_row.set_spacing(10);

        let mut emoji_btn = controls::button("");
        match crate::ui::emoji_assets::rgb_image("\u{1F642}", 20) {
            Some(img) => emoji_btn.set_image(Some(img)),
            None => {
                emoji_btn.set_label("\u{1F642}");
                emoji_btn.set_label_size(16);
            }
        }
        emoji_btn.set_tooltip("Emoji — or type :name: codes like :joy:");
        input_row.fixed(&emoji_btn, 40);

        let mut message_input = input::Input::default();
        message_input.set_maximum_size(400);
        style::style_input(&mut message_input);

        let mut char_count = frame::Frame::default().with_label("0/400");
        input_row.fixed(&char_count, 60);
        char_count.set_label_color(theme::TEXT_MUTED);
        char_count.set_label_size(11);

        let mut send_btn = controls::accent_button(&t!("send"), theme::ACCENT);
        input_row.fixed(&send_btn, 80);

        input_row.end();
        chat_col.end();

        let picker: Rc<RefCell<Option<window::Window>>> = Rc::new(RefCell::new(None));

        // ── Emoji picker toggle ─────────────────────────────────────
        let picker_toggle = picker.clone();
        let input_for_picker = message_input.clone();
        let count_for_picker = char_count.clone();
        emoji_btn.set_callback(move |btn| {
            // Second click (or a stale hidden popup) closes it.
            if let Some(mut open) = picker_toggle.borrow_mut().take() {
                let was_visible = open.visible();
                open.hide();
                app::delete_widget(open);
                if was_visible {
                    return;
                }
            }

            // Anchor the palette just above the button.
            let (px, py) = match btn.top_window() {
                Some(w) => (w.x() + btn.x(), w.y() + btn.y()),
                None => (app::event_x_root(), app::event_y_root()),
            };
            let x = px;
            let y = (py - emoji::picker_height() - 6).max(0);

            let mut input = input_for_picker.clone();
            let mut count = count_for_picker.clone();
            let win = emoji::open_picker(x, y, move |e| {
                input.insert(e).ok();
                let _ = input.take_focus();
                update_char_count(&input, &mut count);
            });
            *picker_toggle.borrow_mut() = Some(win);
        });

        // ── Input: counter, Enter-to-send, image paste ──────────────
        let mut count_clone = char_count.clone();
        let picker_input = picker.clone();
        message_input.handle(move |input, event| match event {
            enums::Event::KeyUp | enums::Event::KeyDown => {
                scrub_control_chars(input);
                update_char_count(input, &mut count_clone);

                if event == enums::Event::KeyDown {
                    // Ctrl/Cmd+V with an image on the clipboard becomes
                    // an image send; text pastes fall through to the
                    // default Fl_Input handler.
                    let paste_combo = app::event_state().contains(enums::EventState::Command)
                        && app::event_key() == enums::Key::from_char('v');
                    if paste_combo && app::clipboard_contains(app::ClipboardContent::Image) {
                        app::paste_image(input);
                        return true;
                    }

                    // Windows synthesizes an extra key event carrying the
                    // literal SYN control char (0x16) for Ctrl+V, which
                    // Fl_Input would insert as a visible "^V". The only
                    // ^V-carrying event that may fall through is a
                    // genuine text paste (modifier held AND text on the
                    // clipboard); everything else is a stray duplicate —
                    // swallow it. Note the image-paste combo already
                    // returned above, so this can't shadow it.
                    if app::event_text() == "\u{16}" {
                        let genuine_text_paste = app::event_state()
                            .contains(enums::EventState::Command)
                            && app::clipboard_contains(app::ClipboardContent::Text);
                        if !genuine_text_paste {
                            return true;
                        }
                    }

                    if app::event_key() == enums::Key::Enter {
                        close_picker(&picker_input);
                        send_current(input, &mut count_clone, &sender);
                        return true;
                    }
                }
                false
            }
            // The input regains focus right after the preview dialog
            // closes (Enter, click, or Escape alike) — scrub then, so a
            // stray ^V never survives to be seen or typed around.
            enums::Event::Focus => {
                scrub_control_chars(input);
                update_char_count(input, &mut count_clone);
                false // let Fl_Input handle focus normally
            }
            enums::Event::Paste => match app::event_clipboard() {
                Some(app::ClipboardEvent::Image(Some(img))) => {
                    confirm_and_send_image(img, sender);
                    true
                }
                // Text paste (or empty image): default handling.
                _ => false,
            },
            _ => false,
        });

        // Send button
        let mut input_clone = message_input.clone();
        let mut count_clone = char_count.clone();
        let picker_send = picker.clone();
        send_btn.set_callback(move |_| {
            close_picker(&picker_send);
            send_current(&mut input_clone, &mut count_clone, &sender);
        });

        Self {
            group: chat_col,
            view,
            picker,
        }
    }

    pub fn create_ui(&mut self) -> group::Flex {
        self.group.clone()
    }

    pub fn handle_system_message(&mut self, text: String) {
        self.view.push_system(&text);
    }

    pub fn handle_connected(&mut self, server_name: String, greeting: String) {
        self.view.clear();
        self.view.push_system(&format!("{server_name}: {greeting}"));
    }

    pub fn handle_disconnected(&mut self, reason: String) {
        close_picker(&self.picker);
        self.view
            .push_system(&t!("chat-disconnected", "reason" => reason.as_str()));
    }

    pub fn handle_user_joined(&mut self, username: String) {
        self.view
            .push_system(&t!("chat-user-joined", "name" => username.as_str()));
    }

    pub fn handle_user_left(&mut self, username: String) {
        self.view
            .push_system(&t!("chat-user-left", "name" => username.as_str()));
    }

    pub fn handle_text_message(&mut self, username: String, text: String) {
        let timestamp = chrono::Local::now().format("%H:%M").to_string();
        // Render shortcodes from clients that send them raw.
        let text = emoji::replace_shortcodes(&text);
        self.view.push_message(&timestamp, &username, &text);
    }

    /// An image message arrived (PNG bytes).
    pub fn handle_image_message(&mut self, username: String, data: Vec<u8>) {
        let timestamp = chrono::Local::now().format("%H:%M").to_string();
        if !self.view.push_image(&timestamp, &username, data) {
            self.view
                .push_system(&format!("{username} sent an image that could not be displayed"));
        }
    }
}

impl Drop for Chat {
    fn drop(&mut self) {
        // The main view is rebuilt per connection; don't leave a
        // floating emoji palette behind.
        close_picker(&self.picker);
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────

fn close_picker(picker: &Rc<RefCell<Option<window::Window>>>) {
    if let Some(mut w) = picker.borrow_mut().take() {
        w.hide();
        app::delete_widget(w);
    }
}

/// Remove any control characters that slipped into the input (e.g. the
/// literal ^V Windows can synthesize around Ctrl+V), keeping the cursor
/// where the user left it. Reactive backstop: whatever the insertion
/// mechanism, the character is gone by the next event the input sees.
fn scrub_control_chars(input: &mut input::Input) {
    let v = input.value();
    if !v.contains(|c: char| c.is_control()) {
        return;
    }
    let pos = (input.position().max(0) as usize).min(v.len());
    // Control chars are single-byte in UTF-8, so the cursor shifts left
    // by exactly the number removed before it.
    let removed_before = v
        .get(..pos)
        .unwrap_or("")
        .chars()
        .filter(|c| c.is_control())
        .count();
    let cleaned: String = v.chars().filter(|c| !c.is_control()).collect();
    input.set_value(&cleaned);
    let _ = input.set_position((pos - removed_before) as i32);
}

fn update_char_count(input: &input::Input, count: &mut frame::Frame) {
    let len = input.value().len();
    count.set_label(&format!("{len}/400"));
    if len > 380 {
        count.set_label_color(theme::WARNING);
    } else {
        count.set_label_color(theme::TEXT_MUTED);
    }
    count.redraw();
}

/// Convert shortcodes, validate, send, and reset the input.
fn send_current(
    input: &mut input::Input,
    count: &mut frame::Frame,
    sender: &app::Sender<UiMessage>,
) {
    // Strip control characters first: stray ones (e.g. a ^V that slipped
    // past the paste interception) must never reach the wire.
    let cleaned: String = input.value().chars().filter(|c| !c.is_control()).collect();
    let text = emoji::replace_shortcodes(cleaned.trim());
    if !text.is_empty() && text.len() <= 400 {
        sender.send(UiMessage::SendChatMessage(text));
        input.set_value("");
        count.set_label("0/400");
        count.set_label_color(theme::TEXT_MUTED);
        count.redraw();
    }
}

// ── Image paste pipeline ────────────────────────────────────────────────

/// Convert a clipboard image to PNG (downscaling as needed) and show a
/// send/cancel preview dialog.
fn confirm_and_send_image(img: RgbImage, sender: app::Sender<UiMessage>) {
    let png = match encode_clipboard_image(&img) {
        Ok(png) => png,
        Err(e) => {
            log::warn!("Clipboard image could not be encoded: {e}");
            return;
        }
    };
    show_image_preview_dialog(png, sender);
}

/// fltk clipboard image → bounded PNG bytes.
fn encode_clipboard_image(img: &RgbImage) -> Result<Vec<u8>, String> {
    // Normalise to packed RGB8 regardless of source depth/stride.
    let rgb = img
        .convert(enums::ColorDepth::Rgb8)
        .map_err(|e| e.to_string())?;
    let (w, h) = (rgb.data_w() as u32, rgb.data_h() as u32);
    let data = rgb.to_rgb_data();

    let buf: ::image::RgbImage =
        ::image::ImageBuffer::from_raw(w, h, data).ok_or("bad clipboard image buffer")?;

    // Downscale until the encoded PNG fits the wire budget.
    let mut bound = MAX_IMAGE_DIM;
    loop {
        let scaled = if buf.width().max(buf.height()) > bound {
            ::image::imageops::resize(
                &buf,
                (buf.width() * bound / buf.width().max(buf.height())).max(1),
                (buf.height() * bound / buf.width().max(buf.height())).max(1),
                ::image::imageops::FilterType::Triangle,
            )
        } else {
            buf.clone()
        };

        let mut out = Cursor::new(Vec::new());
        ::image::DynamicImage::ImageRgb8(scaled)
            .write_to(&mut out, ::image::ImageFormat::Png)
            .map_err(|e| e.to_string())?;
        let bytes = out.into_inner();

        if bytes.len() <= MAX_IMAGE_BYTES {
            return Ok(bytes);
        }
        if bound <= 320 {
            return Err(format!(
                "image is too large to send ({} KB after downscaling)",
                bytes.len() / 1024
            ));
        }
        bound /= 2;
    }
}

/// Modal preview with Send / Cancel, WhatsApp-style.
fn show_image_preview_dialog(png: Vec<u8>, sender: app::Sender<UiMessage>) {
    let Ok(mut preview) = PngImage::from_data(&png) else {
        return;
    };

    group::Group::set_current(None::<&group::Group>);

    // Scale the preview into a sane dialog.
    let (dw, dh) = (preview.data_w().max(1), preview.data_h().max(1));
    let scale = f64::min(1.0, f64::min(440.0 / dw as f64, 300.0 / dh as f64));
    let (pw, ph) = (
        ((dw as f64 * scale) as i32).max(80),
        ((dh as f64 * scale) as i32).max(60),
    );
    preview.scale(pw, ph, true, true);

    let win_w = (pw + 40).max(280);
    let win_h = ph + 96;
    let mut win = window::Window::default()
        .with_size(win_w, win_h)
        .with_label("Send image");
    win.set_color(theme::BG_PRIMARY);
    win.make_modal(true);

    let mut img_frame = frame::Frame::new(win_w / 2 - pw / 2, 12, pw, ph, None);
    img_frame.set_frame(enums::FrameType::BorderBox);
    img_frame.set_color(theme::SURFACE);
    img_frame.set_image(Some(preview));

    let mut caption = frame::Frame::new(0, ph + 16, win_w, 20, None).with_label(&format!(
        "{}×{} — {} KB",
        dw,
        dh,
        (png.len() / 1024).max(1)
    ));
    caption.set_label_color(theme::TEXT_MUTED);
    caption.set_label_size(11);

    let btn_y = ph + 44;
    let mut cancel_btn = controls::button("Cancel");
    cancel_btn.resize(win_w / 2 - 130, btn_y, 120, 30);

    // ReturnButton: FLTK's default button — Enter anywhere in the
    // dialog triggers it, so the flow is Ctrl+V → Enter. Styled to
    // match `controls::accent_button`.
    let mut send_btn = button::ReturnButton::default().with_label("Send");
    send_btn.set_color(theme::ACCENT);
    send_btn.set_label_color(theme::BUTTON_TEXT);
    send_btn.set_selection_color(theme::ACCENT);
    send_btn.set_frame(enums::FrameType::FlatBox);
    send_btn.set_label_size(11);
    send_btn.resize(win_w / 2 + 10, btn_y, 120, 30);

    win.end();
    win.show();

    // The dialog is created mid key-dispatch (paste delivery is
    // synchronous), so shortcut routing to the ReturnButton can miss
    // the very first Enter before focus settles. Focus Send explicitly
    // and catch Enter at the window level as a backstop — whichever
    // path fires first closes the dialog, so double-send is impossible.
    let _ = send_btn.take_focus();
    let mut send_for_keys = send_btn.clone();
    win.handle(move |_, ev| {
        if ev == enums::Event::KeyDown
            && (app::event_key() == enums::Key::Enter
                || app::event_key() == enums::Key::KPEnter)
        {
            send_for_keys.do_callback();
            return true;
        }
        false
    });

    // Escape and the window-manager close both fire the window
    // callback: cancel and free the dialog instead of leaking it
    // hidden.
    win.set_callback(|w| {
        w.hide();
        app::delete_widget(w.clone());
    });

    let mut win_cancel = win.clone();
    cancel_btn.set_callback(move |_| {
        win_cancel.hide();
        app::delete_widget(win_cancel.clone());
    });

    let mut win_send = win.clone();
    send_btn.set_callback(move |_| {
        sender.send(UiMessage::SendChatImage(png.clone()));
        win_send.hide();
        app::delete_widget(win_send.clone());
    });
}
