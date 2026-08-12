// ui/chat_view.rs — Rich chat history.
//
// Replaces the old TextDisplay-based log with a widget-per-message
// scroll view so the history can mix text, clickable links, and inline
// images (and, later, video embeds — add a new row kind).
//
// Layout model
// ------------
//   Scroll (vertical)
//     └─ Pack (vertical, one child per message row)
//          ├─ text row    — custom-drawn: [12:34] user: message …
//          ├─ system row  — custom-drawn, muted italic
//          └─ image row   — custom-drawn header + scaled inline image
//
// Text rows do their own word-wrap with the fltk draw primitives and
// record the on-screen rectangles of any URLs, so links are clickable
// and get a hand cursor. Right-click offers copy/save actions (the
// widget view has no native text selection, unlike TextDisplay).

use fltk::{
    app, enums, frame, group,
    image::{AnimGifImage, AnimGifImageFlags, GifImage, PngImage},
    menu, prelude::*, window,
};
use std::cell::RefCell;
use std::rc::Rc;

use i18n::t;
use ui::theme;

// ── Metrics ─────────────────────────────────────────────────────────────

const PAD_X: i32 = 10; // left/right padding inside a row
const PAD_Y: i32 = 3; //  top/bottom padding inside a row
const LINE_H: i32 = 19; // line height for 13 px text
const ROW_SPACING: i32 = 2;
const MAX_ROWS: usize = 500; // history cap — oldest rows are dropped
const IMG_MAX_W: i32 = 360; // inline image bounds (WhatsApp-ish)
const IMG_MAX_H: i32 = 280;
const AUTOSCROLL_SLACK: i32 = 40; // "at bottom" tolerance in px

const TS_SIZE: i32 = 12;
const NAME_SIZE: i32 = 13;
const BODY_SIZE: i32 = 13;

// ── Span / fragment model ───────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Timestamp,
    Username,
    Body,
    Link,
    System,
}

impl Kind {
    fn font(self) -> (enums::Font, i32) {
        match self {
            Kind::Timestamp => (enums::Font::Helvetica, TS_SIZE),
            Kind::Username => (enums::Font::HelveticaBold, NAME_SIZE),
            Kind::Body | Kind::Link => (enums::Font::Helvetica, BODY_SIZE),
            Kind::System => (enums::Font::HelveticaItalic, TS_SIZE),
        }
    }

    fn color(self) -> enums::Color {
        match self {
            Kind::Timestamp | Kind::System => theme::TEXT_MUTED,
            Kind::Username => theme::ACCENT,
            Kind::Body => theme::TEXT_SECONDARY,
            Kind::Link => theme::ACCENT,
        }
    }
}

/// A run of same-styled source text (pre-layout).
struct Span {
    kind: Kind,
    text: String,
    /// Target for `Kind::Link` (may differ from `text`, e.g. an
    /// `https://` prefix added to a bare `www.` link).
    url: Option<String>,
}

/// A positioned piece of text (post-layout, coordinates relative to the
/// row's content origin).
struct Frag {
    x: i32,
    y: i32,
    w: i32,
    text: String,
    kind: Kind,
    url: Option<String>,
    /// Draw as a Twemoji image instead of text (`text` holds the
    /// emoji string, `w` is the fixed image advance).
    emoji: bool,
}

/// Square size of an inline emoji image, fitted into `LINE_H`.
const EMOJI_SZ: i32 = 17;

/// Split a whitespace-free piece into alternating text/emoji runs
/// (`true` = bundled emoji). Longest match wins, so multi-codepoint
/// sequences (e.g. "\u{2764}\u{fe0f}") stay whole.
fn split_emoji(piece: &str) -> Vec<(bool, &str)> {
    let idx = crate::ui::emoji_assets::by_first_char();
    let mut out: Vec<(bool, &str)> = Vec::new();
    let mut text_start = 0;
    let mut i = 0;
    while i < piece.len() {
        let rest = &piece[i..];
        let c = rest.chars().next().unwrap();
        let hit = idx
            .get(&c)
            .and_then(|cands| cands.iter().find(|e| rest.starts_with(**e)));
        if let Some(e) = hit {
            if text_start < i {
                out.push((false, &piece[text_start..i]));
            }
            out.push((true, &piece[i..i + e.len()]));
            i += e.len();
            text_start = i;
        } else {
            i += c.len_utf8();
        }
    }
    if text_start < piece.len() {
        out.push((false, &piece[text_start..]));
    }
    out
}

/// Draw a bundled emoji at its chat-line size, decoding and caching on
/// first use. Returns false if the image is unavailable (caller falls
/// back to font rendering).
fn draw_emoji_img(emoji: &str, x: i32, y: i32) -> bool {
    use std::cell::RefCell;
    use std::collections::HashMap;
    thread_local! {
        static CACHE: RefCell<HashMap<String, Option<fltk::image::RgbImage>>> =
            RefCell::new(HashMap::new());
    }
    CACHE.with_borrow_mut(|cache| {
        let entry = cache.entry(emoji.to_string()).or_insert_with(|| {
            crate::ui::emoji_assets::rgb_image(emoji, EMOJI_SZ as u32)
        });
        match entry {
            Some(img) => {
                img.draw(x, y, EMOJI_SZ, EMOJI_SZ);
                true
            }
            None => false,
        }
    })
}

// ── URL detection ───────────────────────────────────────────────────────

/// Split a message body into Body/Link spans. Detects `http(s)://…`
/// and `www.…` words; trailing punctuation stays outside the link.
fn detect_links(text: &str, base: Kind) -> Vec<Span> {
    let mut spans: Vec<Span> = Vec::new();
    let mut plain = String::new();

    let flush = |plain: &mut String, spans: &mut Vec<Span>| {
        if !plain.is_empty() {
            spans.push(Span {
                kind: base,
                text: std::mem::take(plain),
                url: None,
            });
        }
    };

    for piece in split_keep_whitespace(text) {
        if piece.chars().next().is_some_and(char::is_whitespace) {
            plain.push_str(piece);
            continue;
        }

        let lower = piece.to_ascii_lowercase();
        let is_url = lower.starts_with("http://") || lower.starts_with("https://");
        let is_www = !is_url && lower.starts_with("www.") && piece.len() > 4;

        if !(is_url || is_www) {
            plain.push_str(piece);
            continue;
        }

        // Trim trailing punctuation out of the link.
        let trimmed = piece.trim_end_matches(['.', ',', ';', ':', '!', '?', ')', ']', '"', '\'']);
        if trimmed.len() < 8 && !is_www {
            // "http://" with nothing behind it — not a real link.
            plain.push_str(piece);
            continue;
        }
        let tail = &piece[trimmed.len()..];

        flush(&mut plain, &mut spans);
        let url = if is_www {
            format!("https://{trimmed}")
        } else {
            trimmed.to_string()
        };
        spans.push(Span {
            kind: Kind::Link,
            text: trimmed.to_string(),
            url: Some(url),
        });
        if !tail.is_empty() {
            plain.push_str(tail);
        }
    }
    flush(&mut plain, &mut spans);
    spans
}

/// Split into alternating word / whitespace pieces (both preserved).
fn split_keep_whitespace(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut in_ws: Option<bool> = None;
    for (i, c) in s.char_indices() {
        let ws = c.is_whitespace();
        match in_ws {
            None => in_ws = Some(ws),
            Some(prev) if prev != ws => {
                out.push(&s[start..i]);
                start = i;
                in_ws = Some(ws);
            }
            _ => {}
        }
    }
    if start < s.len() {
        out.push(&s[start..]);
    }
    out
}

// ── Layout ──────────────────────────────────────────────────────────────

/// Word-wrap `spans` into positioned fragments for a content width of
/// `cw`. Returns `(frags, content_height)`.
fn layout(spans: &[Span], cw: i32) -> (Vec<Frag>, i32) {
    let cw = cw.max(40);
    let mut frags = Vec::new();
    let (mut x, mut y) = (0, 0);

    let mut push = |text: &str, kind: Kind, url: &Option<String>, x: &mut i32, y: i32, emoji: bool| {
        let w = if emoji {
            EMOJI_SZ
        } else {
            let (f, s) = kind.font();
            fltk::draw::set_font(f, s);
            fltk::draw::width(text) as i32
        };
        frags.push(Frag {
            x: *x,
            y,
            w,
            text: text.to_string(),
            kind,
            url: url.clone(),
            emoji,
        });
        *x += w;
    };

    for span in spans {
        let (f, s) = span.kind.font();
        for piece in split_keep_whitespace(&span.text) {
            if piece.contains('\n') {
                // Forced breaks (multi-line text from other clients).
                for _ in piece.matches('\n') {
                    x = 0;
                    y += LINE_H;
                }
                continue;
            }
            if piece.chars().next().is_some_and(char::is_whitespace) {
                // Whitespace: advances the cursor, never starts a line.
                if x > 0 {
                    fltk::draw::set_font(f, s);
                    x += fltk::draw::width(piece) as i32;
                }
                continue;
            }

            fltk::draw::set_font(f, s);

            // Bundled emoji render as images with a fixed advance; the
            // rest of the piece wraps as text. Each run wraps like a
            // word of its own.
            for (is_emoji, seg) in split_emoji(piece) {
                if is_emoji {
                    if x > 0 && x + EMOJI_SZ > cw {
                        x = 0;
                        y += LINE_H;
                    }
                    push(seg, span.kind, &span.url, &mut x, y, true);
                    continue;
                }

                let w = fltk::draw::width(seg) as i32;

                if x > 0 && x + w > cw {
                    x = 0;
                    y += LINE_H;
                }

                if w <= cw {
                    push(seg, span.kind, &span.url, &mut x, y, false);
                } else {
                    // Word wider than the whole row (long URLs): hard-split
                    // by characters.
                    let mut chunk = String::new();
                    for c in seg.chars() {
                        let char_w = fltk::draw::width(&c.to_string()) as i32;
                        let chunk_w = fltk::draw::width(&chunk) as i32;
                        if x + chunk_w + char_w > cw {
                            if !chunk.is_empty() {
                                push(&chunk, span.kind, &span.url, &mut x, y, false);
                                chunk.clear();
                            }
                            x = 0;
                            y += LINE_H;
                        }
                        chunk.push(c);
                    }
                    if !chunk.is_empty() {
                        push(&chunk, span.kind, &span.url, &mut x, y, false);
                    }
                }
            }
        }
    }

    (frags, y + LINE_H)
}

// ── Text rows ───────────────────────────────────────────────────────────

struct TextRowState {
    spans: Vec<Span>,
    frags: Vec<Frag>,
    /// Raw text for the "Copy message" context action.
    copy_text: String,
}

/// Build a custom-drawn text row (message or system line) and return
/// `(widget, relayout_fn)`. The relayout fn re-wraps for a new width
/// and returns the new total row height.
fn make_text_row(spans: Vec<Span>, copy_text: String, width: i32) -> (frame::Frame, RelayoutFn) {
    let state = Rc::new(RefCell::new(TextRowState {
        spans,
        frags: Vec::new(),
        copy_text,
    }));

    let mut row = frame::Frame::default();
    row.set_frame(enums::FrameType::FlatBox);
    row.set_color(theme::SURFACE);

    // Initial layout.
    {
        let mut st = state.borrow_mut();
        let (frags, content_h) = layout(&st.spans, width - 2 * PAD_X);
        st.frags = frags;
        row.set_size(width, content_h + 2 * PAD_Y);
    }

    // Draw: render fragments; underline links.
    let st = state.clone();
    row.draw(move |r| {
        fltk::draw::draw_rect_fill(r.x(), r.y(), r.w(), r.h(), theme::SURFACE);
        let st = st.borrow();
        for frag in &st.frags {
            let (f, s) = frag.kind.font();
            fltk::draw::set_font(f, s);
            fltk::draw::set_draw_color(frag.kind.color());
            let baseline = r.y() + PAD_Y + frag.y + LINE_H - fltk::draw::descent() - 2;
            let fx = r.x() + PAD_X + frag.x;
            if frag.emoji {
                // Twemoji image, vertically centered in the line; font
                // glyph only if the image is somehow unavailable.
                if draw_emoji_img(&frag.text, fx, r.y() + PAD_Y + frag.y + 1) {
                    continue;
                }
            }
            fltk::draw::draw_text(&frag.text, fx, baseline);
            if frag.kind == Kind::Link {
                fltk::draw::draw_line(fx, baseline + 2, fx + frag.w, baseline + 2);
            }
        }
    });

    // Events: link clicks, hand cursor, context menu.
    let st = state.clone();
    row.handle(move |r, ev| {
        let link_under_cursor = |r: &frame::Frame| -> Option<String> {
            let (mx, my) = (app::event_x(), app::event_y());
            let (rx, ry) = (mx - r.x() - PAD_X, my - r.y() - PAD_Y);
            let st = st.borrow();
            st.frags
                .iter()
                .find(|f| {
                    f.url.is_some()
                        && rx >= f.x
                        && rx <= f.x + f.w
                        && ry >= f.y
                        && ry <= f.y + LINE_H
                })
                .and_then(|f| f.url.clone())
        };

        match ev {
            enums::Event::Push => {
                if app::event_mouse_button() == app::MouseButton::Right {
                    let link = link_under_cursor(r);
                    show_text_context_menu(&st.borrow().copy_text, link.as_deref());
                    return true;
                }
                if let Some(url) = link_under_cursor(r) {
                    let _ = open::that_detached(url);
                    return true;
                }
                false
            }
            enums::Event::Move => {
                let cursor = if link_under_cursor(r).is_some() {
                    enums::Cursor::Hand
                } else {
                    enums::Cursor::Default
                };
                if let Some(mut w) = r.window() {
                    w.set_cursor(cursor);
                }
                false
            }
            enums::Event::Leave => {
                if let Some(mut w) = r.window() {
                    w.set_cursor(enums::Cursor::Default);
                }
                false
            }
            _ => false,
        }
    });

    let st = state;
    let mut row_for_layout = row.clone();
    let relayout: RelayoutFn = Box::new(move |w| {
        let mut st = st.borrow_mut();
        let (frags, content_h) = layout(&st.spans, w - 2 * PAD_X);
        st.frags = frags;
        let h = content_h + 2 * PAD_Y;
        row_for_layout.set_size(w, h);
        h
    });

    (row, relayout)
}

fn show_text_context_menu(copy_text: &str, link: Option<&str>) {
    let copy_message = t!("menu-copy-message");
    let copy_link = t!("menu-copy-link");
    let open_link = t!("menu-open-link");

    // `MenuItem::new` demands `&'static str`; build with placeholders
    // and relabel (set_label copies), then match the localized strings.
    let items: &[&'static str] = if link.is_some() {
        &["copy-message", "copy-link", "open-link"]
    } else {
        &["copy-message"]
    };
    let menu = menu::MenuItem::new(items);
    let labels: &[&String] = if link.is_some() {
        &[&copy_message, &copy_link, &open_link]
    } else {
        &[&copy_message]
    };
    for (i, label) in labels.iter().enumerate() {
        if let Some(mut item) = menu.at(i as i32) {
            item.set_label(label);
        }
    }

    if let Some(choice) = menu.popup(app::event_x(), app::event_y()) {
        let label = choice.label();
        let label = label.as_deref();
        if label == Some(copy_message.as_str()) {
            app::copy(copy_text);
        } else if label == Some(copy_link.as_str()) {
            if let Some(url) = link {
                app::copy(url);
            }
        } else if label == Some(open_link.as_str()) {
            if let Some(url) = link {
                let _ = open::that_detached(url);
            }
        }
    }
}

// ── Image rows ──────────────────────────────────────────────────────────

/// The inline image of a chat row, in one of three states. GIF rows
/// move between `GifStill` (frozen first frame — the cheap steady
/// state) and `GifAnim` (live animation whose decoded frames exist
/// *only while it plays*): autoplay runs ~3 loops then freezes back,
/// hover replays. That policy is the memory model, not a nicety — a
/// decoded 100-frame GIF is tens of MB, and history keeps 500 rows.
enum RowImage {
    Png(PngImage),
    GifStill(GifImage),
    GifAnim(AnimGifImage),
}

impl RowImage {
    fn data_w(&self) -> i32 {
        match self {
            RowImage::Png(i) => i.data_w(),
            RowImage::GifStill(i) => i.data_w(),
            RowImage::GifAnim(i) => i.data_w(),
        }
    }
    fn data_h(&self) -> i32 {
        match self {
            RowImage::Png(i) => i.data_h(),
            RowImage::GifStill(i) => i.data_h(),
            RowImage::GifAnim(i) => i.data_h(),
        }
    }
    fn scale(&mut self, w: i32, h: i32) {
        match self {
            RowImage::Png(i) => i.scale(w, h, true, true),
            RowImage::GifStill(i) => i.scale(w, h, true, true),
            RowImage::GifAnim(i) => i.scale(w, h, true, true),
        }
    }
    fn draw(&mut self, x: i32, y: i32, w: i32, h: i32) {
        match self {
            RowImage::Png(i) => i.draw(x, y, w, h),
            RowImage::GifStill(i) => i.draw(x, y, w, h),
            RowImage::GifAnim(i) => i.draw(x, y, w, h),
        }
    }
}

/// Swap a GIF row to live animation, bound to the row widget so frame
/// advances damage exactly this row. `with_freeze_timer` arms the
/// autoplay budget (~3 loops); hover-initiated playback skips it and
/// freezes on Leave instead.
fn play_gif(state: &Rc<RefCell<ImageRowState>>, row: &frame::Frame, with_freeze_timer: bool) {
    let anim = {
        let st = state.borrow();
        let mut canvas = row.clone();
        AnimGifImage::from_data(
            &st.bytes,
            &mut canvas,
            // Coexist with the row's custom draw: don't hijack the
            // widget's image slot or size, just damage it per frame.
            // No OPTIMIZE_MEMORY: it stores frames at their partial
            // sizes and FLTK documents resize artifacts — which is
            // exactly our case (rows always draw scaled). Costs more
            // RAM per frame while playing; the 800 px cap bounds it.
            AnimGifImageFlags::DONT_SET_AS_IMAGE | AnimGifImageFlags::DONT_RESIZE_CANVAS,
        )
    };
    let Ok(mut anim) = anim else {
        return;
    };
    {
        let mut st = state.borrow_mut();
        let (dw, dh) = st.disp;
        anim.scale(dw, dh, true, true);
        st.img = RowImage::GifAnim(anim);
        st.hover_play = !with_freeze_timer;
    }
    if with_freeze_timer {
        let state = state.clone();
        let row = row.clone();
        // ~3 loops, but never more than 30 s of autoplay: a duration
        // misread (or a pathologically long GIF) must not turn into
        // minutes of animation. Hover still replays after the freeze.
        let budget = (3.0 * gif_loop_secs(&state.borrow().bytes) + 0.25).min(30.0);
        app::add_timeout3(budget, move |_| {
            if !row.was_deleted() {
                freeze_gif(&state, &row);
            }
        });
    }
}

/// Freeze a playing GIF back to its first-frame still, releasing every
/// decoded frame. No-op if the row is already still.
fn freeze_gif(state: &Rc<RefCell<ImageRowState>>, row: &frame::Frame) {
    {
        let st = state.borrow();
        if !matches!(st.img, RowImage::GifAnim(_)) {
            return;
        }
    }
    let still = {
        let st = state.borrow();
        GifImage::from_data(&st.bytes)
    };
    let mut st = state.borrow_mut();
    if let RowImage::GifAnim(anim) = &mut st.img {
        anim.stop();
    }
    if let Ok(mut still) = still {
        let (dw, dh) = st.disp;
        still.scale(dw, dh, true, true);
        st.img = RowImage::GifStill(still); // anim (and its frames) dropped here
    }
    st.hover_play = false;
    drop(st);
    let mut row = row.clone();
    row.redraw();
}

/// Total duration of one animation loop, from the GIF's own Graphic
/// Control Extension delays (centiseconds) — no pixel decoding. The
/// structural walk mirrors chat.rs's animation sniffer. Delays of
/// 0–1 cs render as ~100 ms everywhere (browser convention), so they
/// count as such. Falls back to 3 s if the parse loses sync.
fn gif_loop_secs(bytes: &[u8]) -> f64 {
    let gct = match bytes.get(10) {
        Some(&flags) if flags & 0x80 != 0 => 3usize << ((flags & 0x07) + 1),
        _ => 0,
    };
    let mut i = 13 + gct;
    let mut total_cs: u64 = 0;
    while i < bytes.len() {
        match bytes[i] {
            0x3B => break,
            0x21 => {
                let is_gce = bytes.get(i + 1) == Some(&0xF9);
                if is_gce && i + 4 < bytes.len() {
                    let d = u64::from(u16::from_le_bytes([bytes[i + 3], bytes[i + 4]]));
                    // Match Fl_Anim_GIF's convert_delay exactly: only
                    // a zero delay becomes 10 cs; small real delays
                    // play as-is. Clamping them up here made the
                    // 3-loop budget 3–5× too long for fast GIFs.
                    total_cs += if d == 0 { 10 } else { d };
                }
                i += 2;
                i = skip_subblocks(bytes, i);
            }
            0x2C => {
                if i + 9 >= bytes.len() {
                    break;
                }
                let flags = bytes[i + 9];
                let lct = if flags & 0x80 != 0 {
                    3usize << ((flags & 0x07) + 1)
                } else {
                    0
                };
                i += 10 + lct + 1;
                i = skip_subblocks(bytes, i);
            }
            _ => break,
        }
    }
    if total_cs == 0 {
        3.0
    } else {
        #[allow(clippy::cast_precision_loss)]
        {
            total_cs as f64 / 100.0
        }
    }
}

/// Advance past GIF data sub-blocks (len byte + payload, zero-length
/// terminator).
fn skip_subblocks(bytes: &[u8], mut i: usize) -> usize {
    while let Some(&len) = bytes.get(i) {
        i += 1 + len as usize;
        if len == 0 {
            return i;
        }
    }
    bytes.len() + 1
}

struct ImageRowState {
    header: Vec<Span>,
    header_frags: Vec<Frag>,
    img: RowImage,
    /// True when `bytes` is a GIF (drives replay-on-hover).
    gif: bool,
    /// The current animation was started by hover, so Leave freezes it.
    hover_play: bool,
    /// Displayed (scaled) size.
    disp: (i32, i32),
    /// Original PNG/GIF bytes, kept for "Save image…" and for
    /// rebuilding stills/animations without re-fetching.
    bytes: Vec<u8>,
    username: String,
}

fn make_image_row(
    timestamp: String,
    username: String,
    img: RowImage,
    bytes: Vec<u8>,
    width: i32,
    autoplay_gif: bool,
) -> (frame::Frame, RelayoutFn, Rc<RefCell<ImageRowState>>) {
    let header = vec![
        Span {
            kind: Kind::Timestamp,
            text: format!("[{timestamp}] "),
            url: None,
        },
        Span {
            kind: Kind::Username,
            text: format!("{username}:"),
            url: None,
        },
    ];

    let gif = bytes.starts_with(b"GIF8");
    let state = Rc::new(RefCell::new(ImageRowState {
        header,
        header_frags: Vec::new(),
        img,
        gif,
        hover_play: false,
        disp: (0, 0),
        bytes,
        username,
    }));

    let mut row = frame::Frame::default();
    row.set_frame(enums::FrameType::FlatBox);
    row.set_color(theme::SURFACE);

    // Compute display size for a given row width.
    fn fit(img: &RowImage, cw: i32) -> (i32, i32) {
        let (dw, dh) = (img.data_w().max(1), img.data_h().max(1));
        let max_w = IMG_MAX_W.min(cw).max(40);
        let scale = f64::min(
            1.0,
            f64::min(max_w as f64 / dw as f64, IMG_MAX_H as f64 / dh as f64),
        );
        (
            ((dw as f64 * scale) as i32).max(1),
            ((dh as f64 * scale) as i32).max(1),
        )
    }

    {
        let mut st = state.borrow_mut();
        let (frags, _) = layout(&st.header, width - 2 * PAD_X);
        st.header_frags = frags;
        let disp = fit(&st.img, width - 2 * PAD_X);
        st.img.scale(disp.0, disp.1);
        st.disp = disp;
        row.set_size(width, PAD_Y + LINE_H + disp.1 + PAD_Y + 2);
    }

    // Fresh GIFs animate on arrival (only when the chat is on screen),
    // then freeze after ~3 loops — see play_gif/freeze_gif.
    if autoplay_gif && gif {
        play_gif(&state, &row, true);
    }

    let st = state.clone();
    row.draw(move |r| {
        fltk::draw::draw_rect_fill(r.x(), r.y(), r.w(), r.h(), theme::SURFACE);
        let mut st = st.borrow_mut();
        for frag in &st.header_frags {
            let (f, s) = frag.kind.font();
            fltk::draw::set_font(f, s);
            fltk::draw::set_draw_color(frag.kind.color());
            let baseline = r.y() + PAD_Y + frag.y + LINE_H - fltk::draw::descent() - 2;
            let fx = r.x() + PAD_X + frag.x;
            if frag.emoji && draw_emoji_img(&frag.text, fx, r.y() + PAD_Y + frag.y + 1) {
                continue;
            }
            fltk::draw::draw_text(&frag.text, fx, baseline);
        }
        let (dw, dh) = st.disp;
        let (ix, iy) = (r.x() + PAD_X, r.y() + PAD_Y + LINE_H + 1);
        st.img.draw(ix, iy, dw, dh);
        fltk::draw::set_draw_color(theme::BORDER);
        fltk::draw::draw_rect(ix - 1, iy - 1, dw + 2, dh + 2);
    });

    let st = state.clone();
    row.handle(move |r, ev| match ev {
        enums::Event::Push => {
            let (mx, my) = (app::event_x(), app::event_y());
            let st_ref = st.borrow();
            let (dw, dh) = st_ref.disp;
            let (ix, iy) = (r.x() + PAD_X, r.y() + PAD_Y + LINE_H + 1);
            let over_image = mx >= ix && mx <= ix + dw && my >= iy && my <= iy + dh;

            if app::event_mouse_button() == app::MouseButton::Right {
                drop(st_ref);
                show_image_context_menu(&st);
                return true;
            }
            if over_image {
                drop(st_ref);
                open_image_viewer(&st.borrow());
                return true;
            }
            false
        }
        enums::Event::Move => {
            let (mx, my) = (app::event_x(), app::event_y());
            let st_ref = st.borrow();
            let (dw, dh) = st_ref.disp;
            let (ix, iy) = (r.x() + PAD_X, r.y() + PAD_Y + LINE_H + 1);
            let over = mx >= ix && mx <= ix + dw && my >= iy && my <= iy + dh;
            if let Some(mut w) = r.window() {
                w.set_cursor(if over {
                    enums::Cursor::Hand
                } else {
                    enums::Cursor::Default
                });
            }
            false
        }
        enums::Event::Enter => {
            // Replay a frozen GIF while hovered.
            let frozen = {
                let st_ref = st.borrow();
                st_ref.gif && matches!(st_ref.img, RowImage::GifStill(_))
            };
            if frozen {
                play_gif(&st, r, false);
                r.redraw();
            }
            false
        }
        enums::Event::Leave => {
            if let Some(mut w) = r.window() {
                w.set_cursor(enums::Cursor::Default);
            }
            // Hover-initiated playback ends with the hover; the
            // autoplay window is governed by its own timer instead.
            let hover_playing = st.borrow().hover_play;
            if hover_playing {
                freeze_gif(&st, r);
            }
            false
        }
        _ => false,
    });

    let media = state.clone();
    let st = state;
    let mut row_for_layout = row.clone();
    let relayout: RelayoutFn = Box::new(move |w| {
        let mut st = st.borrow_mut();
        let (frags, _) = layout(&st.header, w - 2 * PAD_X);
        st.header_frags = frags;
        let disp = fit(&st.img, w - 2 * PAD_X);
        st.img.scale(disp.0, disp.1);
        st.disp = disp;
        let h = PAD_Y + LINE_H + disp.1 + PAD_Y + 2;
        row_for_layout.set_size(w, h);
        h
    });

    (row, relayout, media)
}

fn show_image_context_menu(state: &Rc<RefCell<ImageRowState>>) {
    let open_image = t!("menu-open-image");
    let save_image = t!("menu-save-image");

    let menu = menu::MenuItem::new(&["open-image", "save-image"]);
    for (i, label) in [&open_image, &save_image].into_iter().enumerate() {
        if let Some(mut item) = menu.at(i as i32) {
            item.set_label(label);
        }
    }

    if let Some(choice) = menu.popup(app::event_x(), app::event_y()) {
        let label = choice.label();
        let label = label.as_deref();
        if label == Some(open_image.as_str()) {
            open_image_viewer(&state.borrow());
        } else if label == Some(save_image.as_str()) {
            let mut dialog =
                fltk::dialog::NativeFileChooser::new(fltk::dialog::FileDialogType::BrowseSaveFile);
            dialog.set_option(fltk::dialog::FileDialogOptions::SaveAsConfirm);
            let (ext, filter_label) = if state.borrow().gif {
                ("gif", t!("image-save-filter-gif"))
            } else {
                ("png", t!("image-save-filter"))
            };
            dialog.set_filter(&format!("{filter_label}\t*.{ext}"));
            dialog.set_preset_file(&format!("{}.{ext}", state.borrow().username));
            dialog.show();
            let path = dialog.filename();
            if !path.as_os_str().is_empty() {
                let mut path = path;
                if path.extension().is_none() {
                    path.set_extension(ext);
                }
                if let Err(e) = std::fs::write(&path, &state.borrow().bytes) {
                    log::warn!("Failed to save image: {e}");
                }
            }
        }
    }
}

/// Clamp a remembered window position onto the current screen, so a
/// monitor change can't strand a dialog off-screen.
pub(crate) fn clamp_win_pos(pos: (i32, i32)) -> (i32, i32) {
    let (sw, sh) = app::screen_size();
    #[allow(clippy::cast_possible_truncation)]
    (
        pos.0.clamp(0, (sw as i32 - 200).max(0)),
        pos.1.clamp(0, (sh as i32 - 200).max(0)),
    )
}

/// Open a chat image at (up to) full resolution in its own window.
fn open_image_viewer(state: &ImageRowState) {
    group::Group::set_current(None::<&group::Group>);

    let (dw, dh) = (state.img.data_w().max(1), state.img.data_h().max(1));
    let (sw, sh) = app::screen_size();
    let max_w = (sw as i32 - 120).max(320);
    let max_h = (sh as i32 - 120).max(240);
    let scale = f64::min(
        1.0,
        f64::min(max_w as f64 / dw as f64, max_h as f64 / dh as f64),
    );
    let (ww, wh) = (
        ((dw as f64 * scale) as i32).max(120),
        ((dh as f64 * scale) as i32).max(90),
    );

    let mut win = window::Window::default()
        .with_size(ww, wh)
        .with_label(&t!("image-window-title", "name" => state.username.as_str()));
    win.set_color(theme::BG_PRIMARY);
    win.make_resizable(true);
    if let Some(pos) = client::config::window_pos("viewer") {
        let (x, y) = clamp_win_pos(pos);
        win.set_pos(x, y);
    }

    let mut frame = frame::Frame::default_fill();
    // A fresh copy so viewer scaling never fights the inline row's.
    // Lifetime model: neither FLTK's widget->image() nor fltk-rs's
    // set_image takes ownership of the underlying Fl image — the last
    // Rust wrapper to drop deletes it (Fl_Image::release() is plain
    // `delete` outside Fl_Shared_Image). Letting these wrappers die at
    // the end of this function leaves the frame pointing at freed
    // memory; for GIFs the very first Resize during show() then
    // crashes inside Fl_Image_scale. The wrappers are therefore moved
    // into the window's close callback below, pinning them to the
    // window's lifetime. (Fl_Anim_GIF's destructor only removes its
    // timer — it never touches the canvas — so dropping after widget
    // deletion is safe.)
    let mut keep_anim: Option<AnimGifImage> = None;
    let mut keep_png: Option<PngImage> = None;
    if state.gif {
        if let Ok(mut anim) = AnimGifImage::from_data(
            &state.bytes,
            &mut frame,
            AnimGifImageFlags::DONT_RESIZE_CANVAS,
        ) {
            anim.scale(ww, wh, true, true);
            // Fl_Anim_GIF already bound itself via canvas->image().
            keep_anim = Some(anim);
        }
    } else if let Ok(mut full) = PngImage::from_data(&state.bytes) {
        full.scale(ww, wh, true, true);
        // The clone keeps the strong count above 1 across set_image's
        // internal drop; `full` then lives in the callback.
        frame.set_image(Some(full.clone()));
        keep_png = Some(full);
    }
    frame.handle(|f, ev| {
        if ev == enums::Event::Resize {
            if let Some(mut img) = f.image() {
                img.scale(f.w(), f.h(), true, true);
            }
        }
        false
    });

    win.end();
    let mut keep_anim = keep_anim;
    let mut keep_png = keep_png;
    win.set_callback(move |w| {
        // The animation's frame timer holds a raw canvas pointer with
        // no deletion guard (verified in Fl_Anim_GIF_Image.cxx), so it
        // must die BEFORE the widgets do — relying on the closure being
        // dropped during widget deletion demonstrably fires the timer
        // on a dead frame. stop() removes the pending timeout; the
        // drop frees the frames.
        if let Some(mut anim) = keep_anim.take() {
            anim.stop();
        }
        keep_png.take();
        client::config::set_window_pos("viewer", (w.x(), w.y()));
        w.hide();
        app::delete_widget(w.clone());
    });
    win.show();
}

// ── ChatView ────────────────────────────────────────────────────────────

type RelayoutFn = Box<dyn FnMut(i32) -> i32>;

/// Stop (and thereby unschedule) a row's GIF animation before its
/// canvas widget is deleted — the timer has no deletion guard and
/// would otherwise fire on a dead frame. Dropping the frames happens
/// with the state; stopping the timer is what prevents the crash.
fn stop_row_anim(row: &Row) {
    if let Some(state) = &row.media {
        if let RowImage::GifAnim(anim) = &mut state.borrow_mut().img {
            anim.stop();
        }
    }
}

struct Row {
    widget: frame::Frame,
    /// Image rows expose their state so deletion paths can stop a
    /// playing animation BEFORE the widget dies (see `stop_row_anim`).
    media: Option<Rc<RefCell<ImageRowState>>>,
    relayout: RelayoutFn,
    height: i32,
}

/// The scrollable message history.
pub struct ChatView {
    scroll: group::Scroll,
    pack: group::Pack,
    rows: Rc<RefCell<Vec<Row>>>,
}

impl ChatView {
    /// Build the view. Must be called inside the parent group's
    /// begin/end (it auto-parents like any FLTK widget).
    pub fn new() -> Self {
        let mut scroll = group::Scroll::default();
        scroll.set_type(group::ScrollType::Vertical);
        scroll.set_frame(enums::FrameType::BorderBox);
        scroll.set_color(theme::SURFACE);

        let mut pack = group::Pack::default();
        pack.set_type(group::PackType::Vertical);
        pack.set_spacing(ROW_SPACING);
        pack.end();
        scroll.end();

        let rows: Rc<RefCell<Vec<Row>>> = Rc::new(RefCell::new(Vec::new()));

        // Re-wrap all rows when the chat area is resized.
        let rows_cb = rows.clone();
        let mut pack_cb = pack.clone();
        scroll.resize_callback(move |s, _x, _y, _w, _h| {
            let cw = Self::content_width_for(s);
            let mut total = 0;
            let mut rows = rows_cb.borrow_mut();
            for row in rows.iter_mut() {
                row.height = (row.relayout)(cw);
                total += row.height + ROW_SPACING;
            }
            pack_cb.resize(s.x(), pack_cb.y(), cw, total.max(1));
            s.redraw();
        });

        Self { scroll, pack, rows }
    }

    fn content_width_for(scroll: &group::Scroll) -> i32 {
        (scroll.w() - app::scrollbar_size() - 4).max(60)
    }

    fn content_width(&self) -> i32 {
        Self::content_width_for(&self.scroll)
    }

    fn at_bottom(&self) -> bool {
        let content_h: i32 = self
            .rows
            .borrow()
            .iter()
            .map(|r| r.height + ROW_SPACING)
            .sum();
        content_h <= self.scroll.h() || self.scroll.yposition() >= content_h - self.scroll.h() - AUTOSCROLL_SLACK
    }

    fn append(
        &mut self,
        widget: frame::Frame,
        relayout: RelayoutFn,
        height: i32,
        media: Option<Rc<RefCell<ImageRowState>>>,
    ) {
        let stick = self.at_bottom();

        self.pack.add(&widget);
        self.rows.borrow_mut().push(Row {
            widget,
            relayout,
            height,
            media,
        });

        // History cap: drop the oldest row.
        if self.rows.borrow().len() > MAX_ROWS {
            let old = self.rows.borrow_mut().remove(0);
            stop_row_anim(&old);
            self.pack.remove(&old.widget);
            app::delete_widget(old.widget);
        }

        // Keep the pack sized to its content, pinned to the scroll top.
        let content_h: i32 = self
            .rows
            .borrow()
            .iter()
            .map(|r| r.height + ROW_SPACING)
            .sum();
        let cw = self.content_width();
        self.pack.resize(
            self.scroll.x(),
            self.scroll.y() - self.scroll.yposition(),
            cw,
            content_h.max(1),
        );

        if stick {
            let target = (content_h - self.scroll.h() + 4).max(0);
            self.scroll.scroll_to(0, target);
        }
        self.scroll.redraw();
    }

    /// Append a regular chat message.
    pub fn push_message(&mut self, timestamp: &str, username: &str, text: &str) {
        let mut spans = vec![
            Span {
                kind: Kind::Timestamp,
                text: format!("[{timestamp}] "),
                url: None,
            },
            Span {
                kind: Kind::Username,
                text: format!("{username}: "),
                url: None,
            },
        ];
        spans.extend(detect_links(text, Kind::Body));

        let copy_text = text.to_string();
        let cw = self.content_width();
        let (widget, relayout) = make_text_row(spans, copy_text, cw);
        let height = widget.h();
        self.append(widget, relayout, height, None);
    }

    /// Append a muted, italic system line.
    pub fn push_system(&mut self, text: &str) {
        let spans = detect_links(text, Kind::System);
        let cw = self.content_width();
        let (widget, relayout) = make_text_row(spans, text.to_string(), cw);
        let height = widget.h();
        self.append(widget, relayout, height, None);
    }

    /// Append an inline image from PNG bytes. Returns `false` when the
    /// bytes can't be decoded (caller may show a system line instead).
    pub fn push_image(&mut self, timestamp: &str, username: &str, media_bytes: Vec<u8>) -> bool {
        // The wire carries PNG or GIF; sniff the magic to pick the
        // decoder (matching the server's relay filter).
        let img = if media_bytes.starts_with(b"GIF8") {
            match GifImage::from_data(&media_bytes) {
                Ok(g) => RowImage::GifStill(g),
                Err(_) => return false,
            }
        } else {
            match PngImage::from_data(&media_bytes) {
                Ok(p) => RowImage::Png(p),
                Err(_) => return false,
            }
        };
        let cw = self.content_width();
        // Animate on arrival only when someone can see it — hidden or
        // minimized chat starts frozen and plays on hover instead.
        let autoplay = self.scroll.visible_r();
        let (widget, relayout, media) = make_image_row(
            timestamp.to_string(),
            username.to_string(),
            img,
            media_bytes,
            cw,
            autoplay,
        );
        let height = widget.h();
        self.append(widget, relayout, height, Some(media));
        true
    }

    /// Make the message area a drop target for image files. FLTK
    /// delivers the drop as an FL_PASTE whose text is paths (mac/win)
    /// or file:// URIs (X11); the shared sniffer routes the first
    /// image into the attach/preview flow. Rows under the mouse
    /// ignore DND events, so they bubble here.
    pub fn accept_image_drops(&mut self, sender: app::Sender<super::messages::UiMessage>) {
        self.scroll.handle(move |_, ev| match ev {
            enums::Event::DndEnter | enums::Event::DndDrag | enums::Event::DndRelease => true,
            enums::Event::Paste => {
                if let Some(path) = super::chat::first_image_path_in(&app::event_text()) {
                    super::chat::attach_path(&path, sender);
                    true
                } else {
                    false
                }
            }
            _ => false,
        });
    }

    /// Remove all rows (fresh connection).
    pub fn clear(&mut self) {
        for row in self.rows.borrow_mut().drain(..) {
            stop_row_anim(&row);
            self.pack.remove(&row.widget);
            app::delete_widget(row.widget);
        }
        self.pack
            .resize(self.scroll.x(), self.scroll.y(), self.content_width(), 1);
        self.scroll.scroll_to(0, 0);
        self.scroll.redraw();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn links_of(text: &str) -> Vec<(String, String)> {
        detect_links(text, Kind::Body)
            .into_iter()
            .filter(|s| s.kind == Kind::Link)
            .map(|s| (s.text, s.url.unwrap()))
            .collect()
    }

    #[test]
    fn detects_http_links() {
        let l = links_of("see https://example.com/x?a=1 now");
        assert_eq!(l, vec![("https://example.com/x?a=1".into(), "https://example.com/x?a=1".into())]);
    }

    #[test]
    fn trims_trailing_punctuation() {
        let l = links_of("go to http://a.io/path).");
        assert_eq!(l[0].0, "http://a.io/path");
    }

    #[test]
    fn www_gets_https_prefix() {
        let l = links_of("try www.rust-lang.org!");
        assert_eq!(l[0].1, "https://www.rust-lang.org");
    }

    #[test]
    fn plain_text_has_no_links() {
        assert!(links_of("nothing to see here http:// almost").is_empty());
        let spans = detect_links("hello world", Kind::Body);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].text, "hello world");
    }

    #[test]
    fn emoji_splitter_separates_runs() {
        let segs = split_emoji("hi\u{1F602}there");
        assert_eq!(
            segs,
            vec![(false, "hi"), (true, "\u{1F602}"), (false, "there")]
        );
        // Adjacent emoji stay individual fixed-advance runs.
        assert_eq!(
            split_emoji("\u{1F602}\u{1F602}"),
            vec![(true, "\u{1F602}"), (true, "\u{1F602}")]
        );
        // Multi-codepoint sequences match longest-first, staying whole.
        assert_eq!(split_emoji("x\u{2764}\u{FE0F}y").len(), 3);
        assert!(split_emoji("x\u{2764}\u{FE0F}y")[1].0);
        // Plain text passes through untouched.
        assert_eq!(split_emoji("plain"), vec![(false, "plain")]);
        // Round-trip: concatenation reproduces the input.
        for s in ["a\u{1F602}b", "\u{1F602}", "abc", "\u{2764}\u{FE0F}"] {
            let joined: String = split_emoji(s).iter().map(|(_, t)| *t).collect();
            assert_eq!(joined, s);
        }
    }

    #[test]
    fn whitespace_splitter_roundtrips() {
        for s in ["a  b\tc", "  lead", "trail  ", "", "one"] {
            assert_eq!(split_keep_whitespace(s).concat(), s);
        }
    }
}
