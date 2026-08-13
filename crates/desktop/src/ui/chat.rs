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
/// Wire budget for a chat image/GIF — the protocol's own constant
/// (re-exported through `client`), so UI and codec can never drift.
const MAX_IMAGE_BYTES: usize = client::MAX_CHAT_IMAGE_BYTES;
/// First-pass bound for a pasted image's longest edge.
const MAX_IMAGE_DIM: u32 = 1280;

/// Dimension cap for *animated* GIFs, tighter than for stills: every
/// viewer that plays one decodes W×H×4 bytes per frame, so an 800px
/// 50-frame GIF is already ~128 MB of transient frame data.
const MAX_ANIM_GIF_DIM: u32 = 800;

type PickerSlot = Rc<RefCell<Option<(window::Window, Vec<RgbImage>)>>>;

pub struct Chat {
    // UI elements kept alive for method access.
    // sender, message_input, char_count, and the emoji button are
    // captured by closures in new() and don't need to live on the
    // struct.
    group: group::Flex,
    view: ChatView,
    /// Keeps the bar-icon Fl images alive (see construction).
    _icon_images: Vec<RgbImage>,
    /// Muted one-line upload indicator between history and input.
    upload_label: frame::Frame,
    
    /// The open picker window plus the image wrappers its cells
    /// display — the images must live exactly as long as the window
    /// (set_image does not own the underlying Fl image).
    picker: PickerSlot,
}

impl Chat {
    pub fn new(sender: app::Sender<UiMessage>) -> Self {
        let mut chat_col = group::Flex::default().column();
        chat_col.set_margins(10, 10, 10, 10);
        chat_col.set_spacing(10);

        // Rich message history (text + links + inline images).
        let mut view = ChatView::new();
        view.accept_image_drops(sender);

        // Input row: [😀] [📎] [message input      ] [0/400] [Send]
        let mut input_row = group::Flex::default().row();
        chat_col.fixed(&input_row, 40);
        input_row.set_spacing(10);

        // Bar-icon images are kept on the struct: set_image does not
        // own the underlying Fl image, so dropping the wrapper here
        // would leave the button drawing freed memory.
        let mut icon_images: Vec<RgbImage> = Vec::new();
        let mut emoji_btn = controls::button("");
        match crate::ui::emoji_assets::rgb_image("\u{1F642}", 20) {
            Some(img) => {
                emoji_btn.set_image(Some(img.clone()));
                icon_images.push(img);
            }
            None => {
                emoji_btn.set_label("\u{1F642}");
                emoji_btn.set_label_size(16);
            }
        }
        // ImageBackdrop centers the icon in the button; the default
        // image-above-label layout reserves a text line even for an
        // empty label, pushing the icon visibly off-center.
        emoji_btn.set_align(enums::Align::Center | enums::Align::ImageBackdrop);
        emoji_btn.set_tooltip(&t!("chat-emoji-tooltip"));
        input_row.fixed(&emoji_btn, 40);

        // Attach-image button — file browser → same preview/send flow
        // as clipboard paste.
        let mut attach_btn = controls::button("");
        match crate::ui::emoji_assets::rgb_image("\u{1F4CE}", 20) {
            Some(img) => {
                attach_btn.set_image(Some(img.clone()));
                icon_images.push(img);
            }
            None => {
                attach_btn.set_label("\u{1F4CE}");
                attach_btn.set_label_size(16);
            }
        }
        attach_btn.set_align(enums::Align::Center | enums::Align::ImageBackdrop);
        attach_btn.set_tooltip(&t!("chat-attach-tooltip"));
        input_row.fixed(&attach_btn, 40);

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

        // Thin, muted status line for upload progress, below the input.
        // Its 12 px are reserved PERMANENTLY (empty when idle) so
        // showing/hiding the text never resizes anything above it.
        let mut upload_label = frame::Frame::default();
        upload_label.set_label_size(11);
        upload_label.set_label_color(theme::TEXT_MUTED);
        upload_label.set_align(enums::Align::Left | enums::Align::Inside);
        chat_col.fixed(&upload_label, 12);
        chat_col.end();

        let picker: PickerSlot = Rc::new(RefCell::new(None));
        // Bumped on every open; a scheduled dismissal only acts if the
        // generation is still the one it was created for, so a stale
        // deferred close can never tear down a reopened picker.
        let picker_gen: Rc<std::cell::Cell<u64>> = Rc::new(std::cell::Cell::new(0));
        // When the picker closes itself on Unfocus, a click on the 😀
        // button IS a click outside: Unfocus fires first, and by the
        // time this callback runs the picker is gone — so a naive
        // toggle would instantly reopen it. The Unfocus handler stamps
        // the close time; a click arriving right after it is treated
        // as the closing click.
        let picker_closed_at: Rc<RefCell<Option<std::time::Instant>>> =
            Rc::new(RefCell::new(None));

        // ── Emoji picker toggle ─────────────────────────────────────
        let picker_toggle = picker.clone();
        let closed_at_toggle = picker_closed_at.clone();
        let input_for_picker = message_input.clone();
        let count_for_picker = char_count.clone();
        emoji_btn.set_callback(move |btn| {
            // Second click (or a stale hidden popup) closes it. Take
            // on its own statement so the RefCell borrow ends before
            // hide() — which can reenter via the Unfocus handler.
            let open = picker_toggle.borrow_mut().take();
            if let Some((mut open, _images)) = open {
                if !open.was_deleted() {
                    let was_visible = open.visible();
                    open.hide();
                    app::delete_widget(open);
                    if was_visible {
                        return;
                    }
                }
            }
            // The Unfocus self-close just handled this same click.
            let recently_closed = closed_at_toggle
                .borrow_mut()
                .take()
                .is_some_and(|t| t.elapsed().as_millis() < 300);
            if recently_closed {
                return;
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
            // Dismissal (Escape, click outside, app switch — and via
            // take_focus below, picking an emoji) stamps the time
            // immediately but DEFERS the actual teardown one event-loop
            // pass. Unfocus can arrive synchronously in the middle of
            // the picker's own event dispatch (e.g. inside a cell's
            // click callback); freeing the cell images and scheduling
            // deletion right there leaves FLTK's post-event bookkeeping
            // running against a dying widget. On the deferred pass the
            // stack is clean. The generation guard keeps a stale
            // deferred close from hitting a reopened picker.
            let generation = picker_gen.get().wrapping_add(1);
            picker_gen.set(generation);
            let picker_dismiss = picker_toggle.clone();
            let closed_at_dismiss = closed_at_toggle.clone();
            let gen_dismiss = picker_gen.clone();
            // Cocoa re-keys the main window while finishing the very
            // mouse-up that opened the picker, delivering a spurious
            // Unfocus right after show() — which would dismiss the
            // picker in the same breath. Ignore dismissal events in
            // the first instant after opening; a genuine click-outside
            // or Escape always comes on a human timescale.
            let opened_at = std::time::Instant::now();
            let (win, images) = emoji::open_picker(
                x,
                y,
                move |e| {
                    input.insert(e).ok();
                    update_char_count(&input, &mut count);
                    // Deferred: take_focus runs Fl::focus's unfocus
                    // walk over the OLD focus chain — the crash logs
                    // show that walk detonating when it overlaps
                    // picker teardown. One pass later the stack is
                    // clean and any dismissal has fully settled.
                    let input = input.clone();
                    app::add_timeout3(0.0, move |_| {
                        let mut input = input.clone();
                        if !input.was_deleted() {
                            let _ = input.take_focus();
                        }
                    });
                },
                move || {
                    if opened_at.elapsed().as_millis() < 250 {
                        return; // show-time focus churn, not a dismissal
                    }
                    *closed_at_dismiss.borrow_mut() = Some(std::time::Instant::now());
                    let slot = picker_dismiss.clone();
                    let gen_check = gen_dismiss.clone();
                    app::add_timeout3(0.0, move |_| {
                        if gen_check.get() == generation {
                            close_picker(&slot);
                        }
                    });
                },
            );
            *picker_toggle.borrow_mut() = Some((win, images));
        });

        // ── Attach image from disk ──────────────────────────────────
        attach_btn.set_callback(move |_| {
            attach_image_from_disk(sender);
        });

        // ── Input: counter, Enter-to-send, image paste ──────────────
        let mut count_clone = char_count.clone();
        let picker_input = picker.clone();
        // cfltk2 defaults to base-class-first dispatch (both handlers
        // always run). This input's closure *intercepts* — right-click
        // menu, Cmd+V, DND — so it must run first and be able to
        // suppress Fl_Input's own handling (notably its built-in
        // text-only right-click menu).
        message_input.super_handle_first(false);
        message_input.handle(move |input, event| match event {
            enums::Event::KeyUp | enums::Event::KeyDown => {
                scrub_control_chars(input);
                update_char_count(input, &mut count_clone);

                if event == enums::Event::KeyDown {
                    // Ctrl/Cmd+V: one chain for every clipboard flavor
                    // — copied file → native PNG → FLTK image → text
                    // (text still lands through the default Fl_Input
                    // handler, via paste_text inside the chain). This
                    // check runs FIRST: on Windows the genuine Ctrl+V
                    // keydown itself carries ^V (0x16) in event_text,
                    // so a blanket SYN swallow above it eats the real
                    // paste (as v0.1.38 painfully demonstrated).
                    let paste_combo = app::event_state().contains(enums::EventState::Command)
                        && app::event_key() == enums::Key::from_char('v');
                    if paste_combo {
                        paste_into_chat(input, sender);
                        return true;
                    }

                    // Windows also synthesizes a WM_CHAR-derived event
                    // carrying the literal SYN char, which doesn't
                    // match the combo (different keysym) but would be
                    // inserted as a visible "^V" by the base handler.
                    // Swallow it.
                    if app::event_text() == "\u{16}" {
                        return true;
                    }

                    if app::event_key() == enums::Key::Enter
                        || app::event_key() == enums::Key::KPEnter
                    {
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
            // Right-click: Cut / Copy / Paste context menu. FLTK's
            // Fl_Input has no native menu, and the menu paste requests
            // the *text* clipboard target — so images only work when we
            // route them ourselves (see show_input_context_menu).
            enums::Event::Push
                if app::event_mouse_button() == app::MouseButton::Right =>
            {
                let _ = input.take_focus();
                show_input_context_menu(input, sender);
                true
            }
            // Drag & drop: accept the drag, and the drop then arrives
            // as an FL_PASTE whose text is newline-separated paths
            // (mac/win) or file:// URIs (X11) — routed by the sniffer
            // in the Paste arm below.
            enums::Event::DndEnter | enums::Event::DndDrag | enums::Event::DndRelease => true,
            enums::Event::Paste => match app::event_clipboard() {
                Some(app::ClipboardEvent::Image(Some(img))) => {
                    confirm_and_send_image(img, sender);
                    true
                }
                // Text paste (or empty image): default handling inserts
                // after we return false. The counter refresh is deferred
                // one event-loop pass so it runs after the insertion —
                // menu pastes produce no key event to trigger it later.
                _ => {
                    // Dropped files and Linux clipboard file copies
                    // arrive here as path/URI text — route the first
                    // image to the attach flow instead of inserting
                    // the path string into the message.
                    if let Some(path) = first_image_path_in(&app::event_text()) {
                        attach_path(&path, sender);
                        return true;
                    }
                    // A pasted (or browser-dragged) image URL loads
                    // the image itself instead of the link text. Also
                    // the answer to Chrome's "Copy image" on GIFs,
                    // which only carries one frame as a bitmap: copy
                    // the image address instead and paste it here.
                    if let Some(url) = image_url_in(&app::event_text()) {
                        fetch_image_url(url, sender);
                        return true;
                    }
                    let input_after = input.clone();
                    let count_after = count_clone.clone();
                    app::add_timeout3(0.0, move |_| {
                        let mut input = input_after.clone();
                        let mut count = count_after.clone();
                        scrub_control_chars(&mut input);
                        update_char_count(&input, &mut count);
                    });
                    false
                }
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
            _icon_images: icon_images,
            upload_label,
            picker,
        }
    }

    /// Close any floating helper windows owned by the chat (currently
    /// the emoji picker). Called when the main window hides/minimizes.
    pub fn close_popups(&mut self) {
        close_picker(&self.picker);
    }

    /// Show/update the outbound-upload indicator. 100 (or anything
    /// above) clears it — the network thread reports 100 exactly once,
    /// when the media channel has fully drained. Only the text changes;
    /// the line's height is reserved permanently so nothing resizes.
    pub fn set_upload_progress(&mut self, pct: u8) {
        if pct >= 100 {
            self.upload_label.set_label("");
        } else {
            self.upload_label.set_label(&t!(
                "chat-upload-progress",
                "pct" => u32::from(pct)
            ));
        }
        self.upload_label.redraw();
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
                .push_system(&t!("chat-image-undisplayable", "name" => username.as_str()));
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

fn close_picker(picker: &PickerSlot) {
    // Take first, on its own statement: an `if let` on
    // `borrow_mut().take()` keeps the RefCell guard alive through the
    // whole body, and `hide()` can synchronously deliver Unfocus to
    // the picker — whose handler calls back in here. With the borrow
    // already released, the reentrant call just sees None. Dropping
    // the taken tuple also frees the cell images, deterministically.
    let taken = picker.borrow_mut().take();
    if let Some((mut w, _images)) = taken {
        if !w.was_deleted() {
            w.hide();
            app::delete_widget(w);
        }
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
    // Characters, not bytes: FLTK's `maximum_size` limits the input in
    // UTF-8 *characters* (see Fl_Input_::replace), so byte counting
    // both over-reported for emoji ("600/400" on a legal message) and
    // hid that `send_current`'s old byte check would silently drop it.
    let len = input.value().chars().count();
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
    // Character limit, matching the input widget's own maximum_size
    // (which FLTK enforces in UTF-8 characters). The old byte check
    // silently refused emoji-heavy messages the input had accepted.
    if !text.is_empty() && text.chars().count() <= 400 {
        sender.send(UiMessage::SendChatMessage(text));
        input.set_value("");
        count.set_label("0/400");
        count.set_label_color(theme::TEXT_MUTED);
        count.redraw();
    }
}

// ── Input context menu ──────────────────────────────────────────────────

/// Right-click menu on the message input: Cut / Copy / Paste.
///
/// Paste routes by clipboard content: images go through
/// `app::paste_image` (delivering an `Event::Paste` the input handler
/// turns into the preview dialog — the same path as Ctrl+V), text
/// through `app::paste_text` (default Fl_Input insertion).
///
/// `MenuItem::new` demands `&'static str`, so the items are built with
/// static placeholders and relabeled via `set_label` (which copies) —
/// the localized strings then also serve to match the user's choice.
fn show_input_context_menu(input: &mut input::Input, sender: app::Sender<UiMessage>) {
    let cut = t!("menu-cut");
    let copy = t!("menu-copy");
    let paste = t!("menu-paste");

    let menu = menu::MenuItem::new(&["cut", "copy", "paste"]);
    for (i, label) in [&cut, &copy, &paste].into_iter().enumerate() {
        if let Some(mut item) = menu.at(i as i32) {
            item.set_label(label);
        }
    }

    let Some(choice) = menu.popup(app::event_x(), app::event_y()) else {
        return;
    };
    let label = choice.label();
    let label = label.as_deref();

    if label == Some(cut.as_str()) {
        input.cut().ok();
    } else if label == Some(copy.as_str()) {
        input.copy().ok();
    } else if label == Some(paste.as_str()) {
        // Deferred one event-loop pass: the paste then runs on a clean
        // stack, fully outside the popup's teardown — and outside this
        // widget's own handle dispatch. (The observed macOS failure
        // mode: FLTK's image paste silently delivers nothing when
        // invoked here directly; see paste_into_chat's logging.)
        let mut input = input.clone();
        app::add_timeout3(0.0, move |_| {
            paste_into_chat(&mut input, sender);
        });
    }
}

/// One paste, best source wins: a copied *file* (which FLTK would
/// render as its icon), then native PNG data (which FLTK's TIFF-side
/// reader can't see), then FLTK's own image paste, then text. Shared
/// by the context menu and Ctrl/Cmd+V so the two can't drift.
fn paste_into_chat(input: &mut input::Input, sender: app::Sender<UiMessage>) {
    if let Some(path) = crate::clipboard::clipboard_file() {
        if is_image_path(&path) {
            log::debug!("paste: clipboard file {}", path.display());
            attach_path(&path, sender);
        } else {
            log::debug!("paste: clipboard file is not an image, ignoring");
        }
        return;
    }
    if let Some(png) = crate::clipboard::clipboard_png() {
        log::debug!("paste: native PNG data ({} bytes)", png.len());
        match load_image_bytes(png) {
            Ok(bytes) => show_image_preview_dialog(bytes, sender),
            Err(e) => notify_image_error(&e, sender),
        }
        return;
    }
    let has_fltk_image = app::clipboard_contains(app::ClipboardContent::Image);
    log::debug!(
        "paste: no native media; fltk sees image={has_fltk_image} text={}",
        app::clipboard_contains(app::ClipboardContent::Text)
    );
    if has_fltk_image {
        app::paste_image(input);
    } else {
        app::paste_text(input);
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

    encode_bounded_png(buf)
}

/// Encode to PNG, downscaling until the result fits the wire budget.
/// Shared by the clipboard-paste and attach-from-disk paths.
fn encode_bounded_png(buf: ::image::RgbImage) -> Result<Vec<u8>, String> {
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

// ── Attach image from disk ──────────────────────────────────────────────

/// Open a file browser, load the chosen image, and show the same
/// send/cancel preview as clipboard paste.
fn attach_image_from_disk(sender: app::Sender<UiMessage>) {
    let mut dialog =
        fltk::dialog::NativeFileChooser::new(fltk::dialog::FileDialogType::BrowseFile);
    dialog.set_filter(&format!(
        "{}\t*.{{png,jpg,jpeg,gif,webp,bmp,tif,tiff}}",
        t!("chat-image-filter")
    ));
    dialog.show();
    let path = dialog.filename();
    if path.as_os_str().is_empty() {
        return;
    }
    attach_path(&path, sender);
}

/// Read an image file into protocol-ready bytes (PNG, or GIF as-is).
fn load_image_file(path: &std::path::Path) -> Result<Vec<u8>, String> {
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    load_image_bytes(bytes)
}

/// Turn raw image bytes of any supported format into protocol-ready
/// bytes.
///
/// - Animated GIFs pass through untouched (animation can't survive a
///   PNG transcode) — enforced against the wire byte cap and the
///   tighter [`MAX_ANIM_GIF_DIM`], read from the GIF header without
///   decoding.
/// - PNGs already inside both wire budgets pass through untouched.
/// - Everything else (JPEG/WebP/BMP/TIFF, oversized or single-frame
///   GIFs, oversized PNGs) decodes — behind explicit limits so a
///   crafted file can't OOM us — and goes through the bounded-PNG
///   pipeline. The dimension check matters even for small files:
///   receivers decode at full resolution.
fn load_image_bytes(bytes: Vec<u8>) -> Result<Vec<u8>, String> {
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        // Logical screen size sits at bytes 6..10 of the header,
        // u16 LE each — no decode needed.
        if bytes.len() >= 10 {
            let w = u16::from_le_bytes([bytes[6], bytes[7]]) as u32;
            let h = u16::from_le_bytes([bytes[8], bytes[9]]) as u32;
            if gif_is_animated(&bytes) {
                if bytes.len() > MAX_IMAGE_BYTES {
                    return Err(t!("chat-gif-too-large",
                        "mb" => format!("{}", MAX_IMAGE_BYTES / (1024 * 1024))));
                }
                if w.max(h) > MAX_ANIM_GIF_DIM {
                    return Err(t!("chat-gif-too-big",
                        "dim" => format!("{MAX_ANIM_GIF_DIM}")));
                }
                return Ok(bytes);
            }
        }
        // Single-frame (or unparseable) GIF: fall through to the
        // static pipeline below.
    }

    let is_png = ::image::guess_format(&bytes)
        .map(|f| f == ::image::ImageFormat::Png)
        .unwrap_or(false);

    let decoded = decode_bounded(&bytes)?;

    if is_png
        && bytes.len() <= MAX_IMAGE_BYTES
        && decoded.width().max(decoded.height()) <= MAX_IMAGE_DIM
    {
        return Ok(bytes);
    }

    encode_bounded_png(decoded.to_rgb8())
}

/// Decode with explicit limits so a crafted file (e.g. a 30000×30000
/// bomb that compresses to nothing) can't allocate gigabytes before
/// any of our dimension checks run.
fn decode_bounded(bytes: &[u8]) -> Result<::image::DynamicImage, String> {
    let mut limits = ::image::Limits::default();
    limits.max_image_width = Some(16_384);
    limits.max_image_height = Some(16_384);
    limits.max_alloc = Some(512 * 1024 * 1024);

    let mut reader = ::image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| e.to_string())?;
    reader.limits(limits);
    reader.decode().map_err(|e| e.to_string())
}

/// Does this GIF contain more than one image descriptor? Scans for the
/// second `0x2C` image-separator at block level — cheap and
/// false-positive-free compared to decoding. Conservative on parse
/// confusion: treats "can't tell" as animated so we never strip frames.
fn gif_is_animated(bytes: &[u8]) -> bool {
    // Skip header (6) + logical screen descriptor (7).
    let Some(mut i) = 13usize.checked_add(gif_gct_len(bytes)) else {
        return true;
    };
    let mut images = 0u32;
    while i < bytes.len() {
        match bytes[i] {
            0x3B => return false, // trailer before a 2nd image
            0x2C => {
                images += 1;
                if images >= 2 {
                    return true;
                }
                // Image descriptor: 10 bytes + optional local color table.
                if i + 9 >= bytes.len() {
                    return true;
                }
                let flags = bytes[i + 9];
                let lct = if flags & 0x80 != 0 {
                    3usize << ((flags & 0x07) + 1)
                } else {
                    0
                };
                i += 10 + lct + 1; // + LZW min-code-size byte
                i = skip_gif_subblocks(bytes, i);
            }
            0x21 => {
                // Extension: label byte then sub-blocks.
                i += 2;
                i = skip_gif_subblocks(bytes, i);
            }
            _ => return true, // lost sync — assume animated
        }
    }
    images >= 2
}

/// Global color table length from the logical screen descriptor.
fn gif_gct_len(bytes: &[u8]) -> usize {
    match bytes.get(10) {
        Some(&flags) if flags & 0x80 != 0 => 3usize << ((flags & 0x07) + 1),
        _ => 0,
    }
}

/// Advance past a chain of GIF data sub-blocks (len byte + payload,
/// terminated by a zero-length block). Returns an out-of-range index
/// on truncation, which callers treat as "assume animated".
fn skip_gif_subblocks(bytes: &[u8], mut i: usize) -> usize {
    while let Some(&len) = bytes.get(i) {
        i += 1 + len as usize;
        if len == 0 {
            return i;
        }
    }
    bytes.len() + 1
}

// ── File paths from pastes and drops ────────────────────────────────────

/// Extensions the attach/paste/drop pipeline accepts.
const IMAGE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "bmp", "tif", "tiff"];

/// Does this path look like an image we can load?
fn is_image_path(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| IMAGE_EXTENSIONS.iter().any(|x| e.eq_ignore_ascii_case(x)))
}

/// `file://` URI or plain path → filesystem path. Percent-decodes URIs
/// (X11 drops encode spaces as `%20`), rejects remote `file://host/`
/// forms, and only answers for paths that exist.
///
/// Used by the DND/text sniffer here and the native clipboard readers.
pub fn file_url_to_path(s: &str) -> Option<std::path::PathBuf> {
    let s = s.trim();
    let path = if let Some(rest) = s.strip_prefix("file://") {
        // Strip an authority component; only localhost/empty accepted.
        let rest = rest.strip_prefix("localhost").unwrap_or(rest);
        if !rest.starts_with('/') {
            return None;
        }
        std::path::PathBuf::from(percent_decode(rest))
    } else if s.starts_with('/') || (s.len() > 2 && s.as_bytes()[1] == b':') {
        // Absolute POSIX path (macOS drops) or Windows drive path.
        std::path::PathBuf::from(s)
    } else {
        return None;
    };
    path.is_file().then_some(path)
}

/// Minimal %XX decoder for file URIs (UTF-8 paths).
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// First image file among newline-separated dropped/pasted paths or
/// URIs, if any. This is the single sniffer serving drag&drop on all
/// three platforms (FLTK delivers drops as FL_PASTE text) and Linux
/// clipboard file copies.
/// The pasted text as a single http(s) image URL, if that's what it
/// is: one token, no whitespace, an image extension on the URL path
/// (query/fragment ignored).
fn image_url_in(text: &str) -> Option<String> {
    let t = text.trim();
    if !(t.starts_with("https://") || t.starts_with("http://")) || t.contains(char::is_whitespace)
    {
        return None;
    }
    let path = t.split(['?', '#']).next().unwrap_or(t);
    is_image_path(std::path::Path::new(path)).then(|| t.to_string())
}

/// Download a pasted image URL on a background thread, capped at the
/// wire budget, and hand the bytes back to the UI thread for the
/// normal validate-and-preview flow. Failures become a chat notice.
fn fetch_image_url(url: String, sender: app::Sender<UiMessage>) {
    std::thread::spawn(move || {
        let fetched = (|| -> Result<Vec<u8>, String> {
            let mut resp = ureq::get(&url).call().map_err(|e| e.to_string())?;
            let bytes = resp
                .body_mut()
                .with_config()
                // +1 so an over-budget file is detectable vs truncated.
                .limit(MAX_IMAGE_BYTES as u64 + 1)
                .read_to_vec()
                .map_err(|e| e.to_string())?;
            if bytes.len() > MAX_IMAGE_BYTES {
                return Err(format!("image is over {} MB", MAX_IMAGE_BYTES / (1024 * 1024)));
            }
            Ok(bytes)
        })();
        // fltk-rs senders are backed by the awake queue and are safe
        // to use from worker threads.
        match fetched {
            Ok(bytes) => sender.send(UiMessage::ImageUrlFetched(bytes)),
            Err(e) => sender.send(UiMessage::ShowChatNotice(
                t!("chat-image-load-error", "error" => e.as_str()),
            )),
        }
    });
}

pub(crate) fn first_image_path_in(text: &str) -> Option<std::path::PathBuf> {
    text.lines()
        .filter_map(file_url_to_path)
        .find(|p| is_image_path(p))
}

/// Load a path and hand it to the preview dialog, reporting failures
/// as a chat notice.
pub(crate) fn attach_path(path: &std::path::Path, sender: app::Sender<UiMessage>) {
    match load_image_file(path) {
        Ok(bytes) => show_image_preview_dialog(bytes, sender),
        Err(e) => notify_image_error(&e, sender),
    }
}

/// Localized in-chat notice for a failed image load.
fn notify_image_error(error: &str, sender: app::Sender<UiMessage>) {
    log::warn!("Image load failed: {error}");
    sender.send(UiMessage::ShowChatNotice(
        t!("chat-image-load-error", "error" => error),
    ));
}

/// Modal preview with Send / Cancel, WhatsApp-style.
/// Validate URL-fetched bytes exactly like any attach (cap, dims, GIF
/// rules) and open the send-preview; failures become a chat notice.
pub(crate) fn preview_fetched_image(bytes: Vec<u8>, sender: app::Sender<UiMessage>) {
    match load_image_bytes(bytes) {
        Ok(ready) => show_image_preview_dialog(ready, sender),
        Err(e) => notify_image_error(&e, sender),
    }
}

fn show_image_preview_dialog(png: Vec<u8>, sender: app::Sender<UiMessage>) {
    // GIFs preview as their first frame; PNGs as themselves. Either
    // way we normalize to an RgbImage for uniform scaling below.
    let is_gif = png.starts_with(b"GIF8");
    let decoded: Option<RgbImage> = if is_gif {
        // fltk's GifImage can't convert to RGB (it's pixmap-based and
        // to_rgb_image refuses count() != 1), so decode the first
        // frame with the image crate instead.
        decode_bounded(&png).ok().and_then(|d| {
            let rgb = d.to_rgb8();
            #[allow(clippy::cast_possible_wrap)]
            let (w, h) = (rgb.width() as i32, rgb.height() as i32);
            RgbImage::new(rgb.as_raw(), w, h, enums::ColorDepth::Rgb8).ok()
        })
    } else {
        PngImage::from_data(&png)
            .ok()
            .and_then(|p| p.to_rgb_image().ok())
    };
    let Some(mut preview) = decoded else {
        log::warn!("image preview: could not decode {} bytes for display", png.len());
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
        .with_label(&t!("image-preview-title"));
    if let Some(pos) = client::config::window_pos("preview") {
        let (x, y) = super::chat_view::clamp_win_pos(pos);
        win.set_pos(x, y);
    }
    win.set_color(theme::BG_PRIMARY);
    win.make_modal(true);

    let mut img_frame = frame::Frame::new(win_w / 2 - pw / 2, 12, pw, ph, None);
    img_frame.set_frame(enums::FrameType::BorderBox);
    img_frame.set_color(theme::SURFACE);
    // Neither widget->image() nor set_image owns the Fl image — the
    // last Rust wrapper to drop deletes it. The keeper is shared by
    // every close path (window callback, Cancel, Send), each of which
    // must stop the animation BEFORE deleting widgets: its frame timer
    // holds a raw canvas pointer with no deletion guard.
    let media = Rc::new(RefCell::new(PreviewMedia {
        anim: None,
        still: None,
    }));
    if png.starts_with(b"GIF8") {
        // Play the GIF in the preview; it binds itself to the frame.
        if let Ok(mut anim) = image::AnimGifImage::from_data(
            &png,
            &mut img_frame,
            image::AnimGifImageFlags::DONT_RESIZE_CANVAS,
        ) {
            anim.scale(pw, ph, true, true);
            media.borrow_mut().anim = Some(anim);
        }
    }
    if media.borrow().anim.is_none() {
        img_frame.set_image(Some(preview.clone()));
        media.borrow_mut().still = Some(preview);
    }

    let mut caption = frame::Frame::new(0, ph + 16, win_w, 20, None).with_label(&format!(
        "{}×{} — {} KB",
        dw,
        dh,
        (png.len() / 1024).max(1)
    ));
    caption.set_label_color(theme::TEXT_MUTED);
    caption.set_label_size(11);

    let btn_y = ph + 44;
    let mut cancel_btn = controls::button(&t!("cancel"));
    cancel_btn.resize(win_w / 2 - 130, btn_y, 120, 30);

    // ReturnButton: FLTK's default button — Enter anywhere in the
    // dialog triggers it, so the flow is Ctrl+V → Enter. Styled to
    // match `controls::accent_button`.
    let mut send_btn = button::ReturnButton::default().with_label(&t!("send"));
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
    let media_cb = media.clone();
    win.set_callback(move |w| {
        media_cb.borrow_mut().release();
        client::config::set_window_pos("preview", (w.x(), w.y()));
        w.hide();
        app::delete_widget(w.clone());
    });

    let mut win_cancel = win.clone();
    let media_cancel = media.clone();
    cancel_btn.set_callback(move |_| {
        media_cancel.borrow_mut().release();
        client::config::set_window_pos("preview", (win_cancel.x(), win_cancel.y()));
        win_cancel.hide();
        app::delete_widget(win_cancel.clone());
    });

    let mut win_send = win.clone();
    let media_send = media;
    send_btn.set_callback(move |_| {
        sender.send(UiMessage::SendChatImage(png.clone()));
        media_send.borrow_mut().release();
        client::config::set_window_pos("preview", (win_send.x(), win_send.y()));
        win_send.hide();
        app::delete_widget(win_send.clone());
    });
}

/// The preview dialog's displayed image(s). Shared by every close path
/// so whichever fires first can stop the animation before widget
/// deletion — the anim timer has no canvas-deletion guard.
struct PreviewMedia {
    anim: Option<image::AnimGifImage>,
    still: Option<RgbImage>,
}

impl PreviewMedia {
    fn release(&mut self) {
        if let Some(mut anim) = self.anim.take() {
            anim.stop();
        }
        self.still.take();
    }
}
