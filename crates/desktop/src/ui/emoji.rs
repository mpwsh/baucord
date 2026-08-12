// ui/emoji.rs — Emoji shortcodes and picker popup.
//
// Two entry points:
//   - `replace_shortcodes()` turns `:joy:`-style codes into Unicode
//     emoji. Applied to outgoing messages before sending and to
//     incoming messages before display (so older clients that send raw
//     shortcodes still render nicely).
//   - `open_picker()` builds the WhatsApp-style emoji palette as a
//     borderless popup window anchored above the emoji button.
//
// Rendering note: emoji glyphs come from the platform font fallback
// (FLTK 1.4+ draws colour emoji on Windows/macOS/Wayland; some X11
// setups render them monochrome).

use fltk::{enums, group, prelude::*, window};
use std::collections::HashMap;
use std::sync::OnceLock;
use ui::theme;

/// `(shortcode, emoji, section)` — section names group the picker.
/// The shortcode set follows the common Slack/Discord names.
#[rustfmt::skip]
pub const EMOJI: &[(&str, &str, &str)] = &[
    // ── Smileys ─────────────────────────────────────────────────────
    ("grinning", "😀", "Smileys"), ("smiley", "😃", "Smileys"), ("smile", "😄", "Smileys"),
    ("grin", "😁", "Smileys"), ("laughing", "😆", "Smileys"), ("sweat_smile", "😅", "Smileys"),
    ("rofl", "🤣", "Smileys"), ("joy", "😂", "Smileys"), ("slightly_smiling_face", "🙂", "Smileys"),
    ("upside_down_face", "🙃", "Smileys"), ("wink", "😉", "Smileys"), ("blush", "😊", "Smileys"),
    ("innocent", "😇", "Smileys"), ("smiling_face_with_three_hearts", "🥰", "Smileys"),
    ("heart_eyes", "😍", "Smileys"), ("star_struck", "🤩", "Smileys"), ("kissing_heart", "😘", "Smileys"),
    ("yum", "😋", "Smileys"), ("stuck_out_tongue", "😛", "Smileys"),
    ("stuck_out_tongue_winking_eye", "😜", "Smileys"), ("zany_face", "🤪", "Smileys"),
    ("money_mouth_face", "🤑", "Smileys"), ("hugs", "🤗", "Smileys"),
    ("shushing_face", "🤫", "Smileys"), ("thinking", "🤔", "Smileys"),
    ("zipper_mouth_face", "🤐", "Smileys"), ("neutral_face", "😐", "Smileys"),
    ("expressionless", "😑", "Smileys"), ("smirk", "😏", "Smileys"),
    ("unamused", "😒", "Smileys"), ("roll_eyes", "🙄", "Smileys"), ("grimacing", "😬", "Smileys"),
    ("lying_face", "🤥", "Smileys"), ("relieved", "😌", "Smileys"), ("pensive", "😔", "Smileys"),
    ("sleepy", "😪", "Smileys"), ("sleeping", "😴", "Smileys"), ("mask", "😷", "Smileys"),
    ("face_with_thermometer", "🤒", "Smileys"), ("nauseated_face", "🤢", "Smileys"),
    ("vomiting_face", "🤮", "Smileys"), ("sneezing_face", "🤧", "Smileys"),
    ("hot_face", "🥵", "Smileys"), ("cold_face", "🥶", "Smileys"), ("woozy_face", "🥴", "Smileys"),
    ("dizzy_face", "😵", "Smileys"), ("exploding_head", "🤯", "Smileys"),
    ("cowboy_hat_face", "🤠", "Smileys"), ("partying_face", "🥳", "Smileys"),
    ("sunglasses", "😎", "Smileys"), ("nerd_face", "🤓", "Smileys"), ("monocle_face", "🧐", "Smileys"),
    ("confused", "😕", "Smileys"), ("worried", "😟", "Smileys"), ("slightly_frowning_face", "🙁", "Smileys"),
    ("open_mouth", "😮", "Smileys"), ("hushed", "😯", "Smileys"), ("astonished", "😲", "Smileys"),
    ("flushed", "😳", "Smileys"), ("pleading_face", "🥺", "Smileys"), ("frowning", "😦", "Smileys"),
    ("anguished", "😧", "Smileys"), ("fearful", "😨", "Smileys"), ("cold_sweat", "😰", "Smileys"),
    ("disappointed_relieved", "😥", "Smileys"), ("cry", "😢", "Smileys"), ("sob", "😭", "Smileys"),
    ("scream", "😱", "Smileys"), ("confounded", "😖", "Smileys"), ("persevere", "😣", "Smileys"),
    ("disappointed", "😞", "Smileys"), ("sweat", "😓", "Smileys"), ("weary", "😩", "Smileys"),
    ("tired_face", "😫", "Smileys"), ("yawning_face", "🥱", "Smileys"),
    ("triumph", "😤", "Smileys"), ("rage", "😡", "Smileys"), ("angry", "😠", "Smileys"),
    ("cursing_face", "🤬", "Smileys"), ("smiling_imp", "😈", "Smileys"), ("imp", "👿", "Smileys"),
    ("skull", "💀", "Smileys"), ("poop", "💩", "Smileys"), ("clown_face", "🤡", "Smileys"),
    ("ghost", "👻", "Smileys"), ("alien", "👽", "Smileys"), ("robot", "🤖", "Smileys"),
    // ── Gestures & people ───────────────────────────────────────────
    ("wave", "👋", "Gestures"), ("raised_hand", "✋", "Gestures"), ("vulcan_salute", "🖖", "Gestures"),
    ("ok_hand", "👌", "Gestures"), ("pinched_fingers", "🤌", "Gestures"), ("v", "✌️", "Gestures"),
    ("crossed_fingers", "🤞", "Gestures"), ("love_you_gesture", "🤟", "Gestures"),
    ("metal", "🤘", "Gestures"), ("call_me_hand", "🤙", "Gestures"),
    ("point_left", "👈", "Gestures"), ("point_right", "👉", "Gestures"),
    ("point_up_2", "👆", "Gestures"), ("point_down", "👇", "Gestures"),
    ("+1", "👍", "Gestures"), ("thumbsup", "👍", "Gestures"),
    ("-1", "👎", "Gestures"), ("thumbsdown", "👎", "Gestures"),
    ("fist", "✊", "Gestures"), ("facepunch", "👊", "Gestures"), ("clap", "👏", "Gestures"),
    ("raised_hands", "🙌", "Gestures"), ("open_hands", "👐", "Gestures"),
    ("handshake", "🤝", "Gestures"), ("pray", "🙏", "Gestures"), ("muscle", "💪", "Gestures"),
    ("eyes", "👀", "Gestures"), ("eye", "👁️", "Gestures"), ("brain", "🧠", "Gestures"),
    ("tongue", "👅", "Gestures"), ("ear", "👂", "Gestures"),
    ("man_facepalming", "🤦", "Gestures"), ("man_shrugging", "🤷", "Gestures"),
    ("shrug", "🤷", "Gestures"), ("facepalm", "🤦", "Gestures"),
    // ── Hearts ──────────────────────────────────────────────────────
    ("heart", "❤️", "Hearts"), ("orange_heart", "🧡", "Hearts"), ("yellow_heart", "💛", "Hearts"),
    ("green_heart", "💚", "Hearts"), ("blue_heart", "💙", "Hearts"), ("purple_heart", "💜", "Hearts"),
    ("black_heart", "🖤", "Hearts"), ("white_heart", "🤍", "Hearts"), ("brown_heart", "🤎", "Hearts"),
    ("broken_heart", "💔", "Hearts"), ("heavy_heart_exclamation", "❣️", "Hearts"),
    ("two_hearts", "💕", "Hearts"), ("sparkling_heart", "💖", "Hearts"),
    ("heartpulse", "💗", "Hearts"), ("heartbeat", "💓", "Hearts"), ("revolving_hearts", "💞", "Hearts"),
    // ── Animals & nature ────────────────────────────────────────────
    ("dog", "🐶", "Nature"), ("cat", "🐱", "Nature"), ("mouse", "🐭", "Nature"),
    ("rabbit", "🐰", "Nature"), ("fox_face", "🦊", "Nature"), ("bear", "🐻", "Nature"),
    ("panda_face", "🐼", "Nature"), ("koala", "🐨", "Nature"), ("tiger", "🐯", "Nature"),
    ("lion", "🦁", "Nature"), ("cow", "🐮", "Nature"), ("pig", "🐷", "Nature"),
    ("frog", "🐸", "Nature"), ("monkey_face", "🐵", "Nature"), ("chicken", "🐔", "Nature"),
    ("penguin", "🐧", "Nature"), ("bird", "🐦", "Nature"), ("duck", "🦆", "Nature"),
    ("owl", "🦉", "Nature"), ("bee", "🐝", "Nature"), ("bug", "🐛", "Nature"),
    ("butterfly", "🦋", "Nature"), ("snail", "🐌", "Nature"), ("turtle", "🐢", "Nature"),
    ("snake", "🐍", "Nature"), ("octopus", "🐙", "Nature"), ("crab", "🦀", "Nature"),
    ("dolphin", "🐬", "Nature"), ("whale", "🐳", "Nature"), ("shark", "🦈", "Nature"),
    ("unicorn", "🦄", "Nature"), ("dragon", "🐉", "Nature"),
    ("cactus", "🌵", "Nature"), ("evergreen_tree", "🌲", "Nature"), ("palm_tree", "🌴", "Nature"),
    ("four_leaf_clover", "🍀", "Nature"), ("maple_leaf", "🍁", "Nature"),
    ("mushroom", "🍄", "Nature"), ("rose", "🌹", "Nature"), ("sunflower", "🌻", "Nature"),
    ("sun_with_face", "🌞", "Nature"), ("full_moon_with_face", "🌝", "Nature"),
    ("crescent_moon", "🌙", "Nature"), ("star2", "🌟", "Nature"), ("zap", "⚡", "Nature"),
    ("fire", "🔥", "Nature"), ("boom", "💥", "Nature"), ("comet", "☄️", "Nature"),
    ("rainbow", "🌈", "Nature"), ("cloud", "☁️", "Nature"), ("snowflake", "❄️", "Nature"),
    ("snowman", "⛄", "Nature"), ("droplet", "💧", "Nature"), ("ocean", "🌊", "Nature"),
    // ── Food & drink ────────────────────────────────────────────────
    ("apple", "🍎", "Food"), ("banana", "🍌", "Food"), ("watermelon", "🍉", "Food"),
    ("grapes", "🍇", "Food"), ("strawberry", "🍓", "Food"), ("peach", "🍑", "Food"),
    ("pineapple", "🍍", "Food"), ("avocado", "🥑", "Food"), ("eggplant", "🍆", "Food"),
    ("hot_pepper", "🌶️", "Food"), ("corn", "🌽", "Food"), ("bread", "🍞", "Food"),
    ("cheese", "🧀", "Food"), ("bacon", "🥓", "Food"), ("hamburger", "🍔", "Food"),
    ("fries", "🍟", "Food"), ("pizza", "🍕", "Food"), ("hotdog", "🌭", "Food"),
    ("taco", "🌮", "Food"), ("burrito", "🌯", "Food"), ("ramen", "🍜", "Food"),
    ("spaghetti", "🍝", "Food"), ("sushi", "🍣", "Food"), ("bento", "🍱", "Food"),
    ("doughnut", "🍩", "Food"), ("cookie", "🍪", "Food"), ("birthday", "🎂", "Food"),
    ("cake", "🍰", "Food"), ("chocolate_bar", "🍫", "Food"), ("candy", "🍬", "Food"),
    ("popcorn", "🍿", "Food"), ("coffee", "☕", "Food"), ("tea", "🍵", "Food"),
    ("beer", "🍺", "Food"), ("beers", "🍻", "Food"), ("clinking_glasses", "🥂", "Food"),
    ("wine_glass", "🍷", "Food"), ("tumbler_glass", "🥃", "Food"), ("cup_with_straw", "🥤", "Food"),
    // ── Activities & objects ────────────────────────────────────────
    ("soccer", "⚽", "Objects"), ("basketball", "🏀", "Objects"), ("football", "🏈", "Objects"),
    ("baseball", "⚾", "Objects"), ("tennis", "🎾", "Objects"), ("8ball", "🎱", "Objects"),
    ("ping_pong", "🏓", "Objects"), ("dart", "🎯", "Objects"), ("video_game", "🎮", "Objects"),
    ("game_die", "🎲", "Objects"), ("chess_pawn", "♟️", "Objects"), ("jigsaw", "🧩", "Objects"),
    ("trophy", "🏆", "Objects"), ("medal_sports", "🏅", "Objects"), ("1st_place_medal", "🥇", "Objects"),
    ("guitar", "🎸", "Objects"), ("musical_keyboard", "🎹", "Objects"), ("trumpet", "🎺", "Objects"),
    ("drum", "🥁", "Objects"), ("microphone", "🎤", "Objects"), ("headphones", "🎧", "Objects"),
    ("musical_note", "🎵", "Objects"), ("notes", "🎶", "Objects"),
    ("art", "🎨", "Objects"), ("clapper", "🎬", "Objects"), ("movie_camera", "🎥", "Objects"),
    ("camera", "📷", "Objects"), ("computer", "💻", "Objects"), ("desktop_computer", "🖥️", "Objects"),
    ("keyboard", "⌨️", "Objects"), ("computer_mouse", "🖱️", "Objects"), ("iphone", "📱", "Objects"),
    ("phone", "☎️", "Objects"), ("tv", "📺", "Objects"), ("radio", "📻", "Objects"),
    ("battery", "🔋", "Objects"), ("electric_plug", "🔌", "Objects"), ("bulb", "💡", "Objects"),
    ("flashlight", "🔦", "Objects"), ("wrench", "🔧", "Objects"), ("hammer", "🔨", "Objects"),
    ("gear", "⚙️", "Objects"), ("bomb", "💣", "Objects"), ("hocho", "🔪", "Objects"),
    ("shield", "🛡️", "Objects"), ("key", "🔑", "Objects"), ("lock", "🔒", "Objects"),
    ("unlock", "🔓", "Objects"), ("mag", "🔍", "Objects"), ("telescope", "🔭", "Objects"),
    ("microscope", "🔬", "Objects"), ("pill", "💊", "Objects"), ("syringe", "💉", "Objects"),
    ("moneybag", "💰", "Objects"), ("dollar", "💵", "Objects"), ("credit_card", "💳", "Objects"),
    ("gem", "💎", "Objects"), ("gift", "🎁", "Objects"), ("balloon", "🎈", "Objects"),
    ("tada", "🎉", "Objects"), ("confetti_ball", "🎊", "Objects"), ("crown", "👑", "Objects"),
    ("rocket", "🚀", "Objects"), ("airplane", "✈️", "Objects"), ("car", "🚗", "Objects"),
    ("bike", "🚲", "Objects"), ("alarm_clock", "⏰", "Objects"), ("hourglass", "⌛", "Objects"),
    ("email", "✉️", "Objects"), ("package", "📦", "Objects"), ("memo", "📝", "Objects"),
    ("pencil2", "✏️", "Objects"), ("books", "📚", "Objects"), ("book", "📖", "Objects"),
    ("pushpin", "📌", "Objects"), ("paperclip", "📎", "Objects"), ("scissors", "✂️", "Objects"),
    ("wastebasket", "🗑️", "Objects"), ("chart_with_upwards_trend", "📈", "Objects"),
    ("chart_with_downwards_trend", "📉", "Objects"), ("calendar", "📅", "Objects"),
    // ── Symbols ─────────────────────────────────────────────────────
    ("100", "💯", "Symbols"), ("check", "✔️", "Symbols"), ("white_check_mark", "✅", "Symbols"),
    ("x", "❌", "Symbols"), ("question", "❓", "Symbols"), ("exclamation", "❗", "Symbols"),
    ("warning", "⚠️", "Symbols"), ("no_entry", "⛔", "Symbols"), ("no_entry_sign", "🚫", "Symbols"),
    ("star", "⭐", "Symbols"), ("sparkles", "✨", "Symbols"), ("dizzy", "💫", "Symbols"),
    ("zzz", "💤", "Symbols"), ("dash", "💨", "Symbols"), ("sweat_drops", "💦", "Symbols"),
    ("speech_balloon", "💬", "Symbols"), ("thought_balloon", "💭", "Symbols"),
    ("bell", "🔔", "Symbols"), ("no_bell", "🔕", "Symbols"), ("mega", "📣", "Symbols"),
    ("loud_sound", "🔊", "Symbols"), ("mute", "🔇", "Symbols"),
    ("arrow_right", "➡️", "Symbols"), ("arrow_left", "⬅️", "Symbols"),
    ("arrow_up", "⬆️", "Symbols"), ("arrow_down", "⬇️", "Symbols"),
    ("arrows_counterclockwise", "🔄", "Symbols"), ("infinity", "♾️", "Symbols"),
    ("recycle", "♻️", "Symbols"), ("copyright", "©️", "Symbols"), ("tm", "™️", "Symbols"),
];

/// Longest shortcode length we will consider when scanning.
const MAX_SHORTCODE_LEN: usize = 40;

fn shortcode_map() -> &'static HashMap<&'static str, &'static str> {
    static MAP: OnceLock<HashMap<&'static str, &'static str>> = OnceLock::new();
    MAP.get_or_init(|| EMOJI.iter().map(|(name, emoji, _)| (*name, *emoji)).collect())
}

/// Replace `:shortcode:` occurrences with their emoji.
///
/// Unknown codes are left untouched (`:whatever:` stays as typed).
/// Scanning resumes right after a failed opening colon so things like
/// `10::joy:` still convert the inner code.
pub fn replace_shortcodes(input: &str) -> String {
    // Fast path: no colon, no work (the common case for chat traffic).
    if !input.contains(':') {
        return input.to_string();
    }

    let map = shortcode_map();
    let mut out = String::with_capacity(input.len());
    let mut rest = input;

    while let Some(start) = rest.find(':') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];

        let matched = after.find(':').and_then(|end| {
            let name = &after[..end];
            let valid = !name.is_empty()
                && name.len() <= MAX_SHORTCODE_LEN
                && name
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'+' | b'-'));
            if valid {
                map.get(name.to_ascii_lowercase().as_str())
                    .map(|emoji| (*emoji, end))
            } else {
                None
            }
        });

        match matched {
            Some((emoji, end)) => {
                out.push_str(emoji);
                rest = &after[end + 1..];
            }
            None => {
                // Not a shortcode — keep the colon, rescan from the
                // character right after it.
                out.push(':');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

// ── Picker popup ────────────────────────────────────────────────────────

const CELL: i32 = 34; // button cell (32 px button + breathing room)
const COLS: i32 = 9;
const PICKER_W: i32 = CELL * COLS + 16 /* scrollbar */ + 12 /* margins */;
const PICKER_H: i32 = 300;
const HEADER_H: i32 = 22;

/// Build the emoji palette as a borderless popup window at `(x, y)`
/// (screen coordinates of the top-left corner).
///
/// `on_pick` is invoked with the emoji string each time a cell is
/// clicked; the window stays open so several emoji can be inserted in
/// a row. The caller owns the returned window and decides when to hide
/// or delete it (Escape inside the picker hides it too).
/// Returns the picker window and the image wrappers its cells display.
/// The caller must keep the images alive as long as the window lives:
/// `set_image` does not own the underlying Fl image — the last Rust
/// wrapper to drop deletes it, leaving the cell drawing freed memory.
pub fn open_picker<F, D>(
    x: i32,
    y: i32,
    on_pick: F,
    mut on_dismiss: D,
) -> (window::Window, Vec<fltk::image::RgbImage>)
where
    F: FnMut(&str) + 'static,
    D: FnMut() + 'static,
{
    // Popups must never be adopted by whatever group is current.
    group::Group::set_current(None::<&group::Group>);

    let mut win = window::Window::default()
        .with_size(PICKER_W, PICKER_H)
        .with_pos(x, y);
    win.set_border(false);
    win.set_color(theme::BG_SECONDARY);
    win.set_frame(enums::FrameType::BorderBox);

    let mut scroll = group::Scroll::default_fill();
    scroll.set_type(group::ScrollType::Vertical);
    scroll.set_frame(enums::FrameType::FlatBox);
    scroll.set_color(theme::BG_SECONDARY);

    // Manual grid layout inside the scroll.
    let ox = scroll.x() + 6;
    let oy = scroll.y() + 4;
    let mut cur_y = oy;
    let mut col = 0;
    let mut last_section = "";
    let on_pick = std::rc::Rc::new(std::cell::RefCell::new(on_pick));
    let mut images: Vec<fltk::image::RgbImage> = Vec::new();

    for (name, emoji, section) in EMOJI {
        if *section != last_section {
            // Section header on its own line.
            if col != 0 {
                cur_y += CELL;
                col = 0;
            }
            let mut header = fltk::frame::Frame::new(ox, cur_y, CELL * COLS, HEADER_H, None)
                .with_label(section)
                .with_align(enums::Align::Left | enums::Align::Inside);
            header.set_label_color(theme::TEXT_MUTED);
            header.set_label_size(10);
            header.set_label_font(enums::Font::HelveticaBold);
            cur_y += HEADER_H;
            last_section = section;
        }

        let bx = ox + col * CELL;
        let mut btn = fltk::button::Button::new(bx, cur_y, 32, 32, None);
        btn.set_frame(enums::FrameType::FlatBox);
        btn.set_color(theme::BG_SECONDARY);
        btn.set_selection_color(theme::BG_TERTIARY);
        // Twemoji image: identical, full-color glyphs on every platform
        // (font rendering is monochrome on Windows GDI). Text label only
        // as a fallback for anything not bundled.
        match crate::ui::emoji_assets::rgb_image(emoji, 24) {
            Some(img) => {
                // Clone keeps the strong count above 1 across
                // set_image's internal drop; the original goes in the
                // keeper vec the caller pins to the window's lifetime.
                btn.set_image(Some(img.clone()));
                images.push(img);
            }
            None => {
                btn.set_label(emoji);
                btn.set_label_size(18);
            }
        }
        btn.set_tooltip(&format!(":{name}:"));
        let cb = on_pick.clone();
        btn.set_callback(move |_| (cb.borrow_mut())(emoji));

        col += 1;
        if col == COLS {
            col = 0;
            cur_y += CELL;
        }
    }

    scroll.end();
    win.end();

    // One handler for both dismissal paths — Escape and losing focus
    // (click outside, app switch). Installed here, once: a second
    // `win.handle` at the call site would silently REPLACE this one.
    win.handle(move |_, ev| match ev {
        enums::Event::Unfocus => {
            on_dismiss();
            true
        }
        enums::Event::KeyDown if fltk::app::event_key() == enums::Key::Escape => {
            on_dismiss();
            true
        }
        _ => false,
    });

    win.show();
    (win, images)
}

/// Height of the picker popup, for anchoring it above the emoji button.
pub fn picker_height() -> i32 {
    PICKER_H
}

#[cfg(test)]
mod tests {
    use super::replace_shortcodes;

    #[test]
    fn replaces_known_codes() {
        assert_eq!(replace_shortcodes("hi :joy: there"), "hi 😂 there");
        assert_eq!(replace_shortcodes(":+1::fire:"), "👍🔥");
        assert_eq!(replace_shortcodes(":JOY:"), "😂"); // case-insensitive
    }

    #[test]
    fn leaves_unknown_and_plain_colons() {
        assert_eq!(replace_shortcodes("meet at 10:30:45 ok"), "meet at 10:30:45 ok");
        assert_eq!(replace_shortcodes(":notacode:"), ":notacode:");
        assert_eq!(replace_shortcodes("a:b"), "a:b");
        assert_eq!(replace_shortcodes("::"), "::");
        assert_eq!(replace_shortcodes("no colons"), "no colons");
    }

    #[test]
    fn rescans_after_failed_open() {
        assert_eq!(replace_shortcodes("x::joy:"), "x:😂");
        assert_eq!(replace_shortcodes(":bad :joy:"), ":bad 😂");
    }

    #[test]
    fn no_dupes_in_table() {
        // Aliases like +1/thumbsup are fine; a repeated *name* is a bug.
        let mut names: Vec<&str> = super::EMOJI.iter().map(|(n, _, _)| *n).collect();
        let before = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(before, names.len(), "duplicate shortcode name in EMOJI");
    }
}
