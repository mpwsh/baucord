//! Emoji shortcodes — `:joy:` → 😂.
//!
//! Applied to outgoing messages before sending and to incoming
//! messages before display (so older clients that send raw shortcodes
//! still render nicely). The catalog (`emoji_table::EMOJI`) also feeds the
//! picker palette. Ported verbatim from the FLTK `ui/emoji.rs`.

use std::{collections::HashMap, sync::OnceLock};

use crate::emoji_table::EMOJI;

/// Longest shortcode length we will consider when scanning.
const MAX_SHORTCODE_LEN: usize = 40;

fn shortcode_map() -> &'static HashMap<&'static str, &'static str> {
    static MAP: OnceLock<HashMap<&'static str, &'static str>> = OnceLock::new();
    MAP.get_or_init(|| EMOJI.iter().map(|(name, emoji)| (*name, *emoji)).collect())
}

/// Strip VARIATION SELECTOR-16 (U+FE0F) for display. Slint's Skia
/// raster text path currently fails glyph fallback for BASE+VS16
/// clusters (they render as tofu boxes) while bare code points render
/// — text-presentation-default emoji may come out monochrome without
/// the selector, but visible beats boxed. Remove when the upstream
/// shaping handles VS16 sequences.
#[must_use]
pub(crate) fn strip_variation_selectors(input: &str) -> String {
    if !input.contains('\u{fe0f}') {
        return input.to_string();
    }
    input.chars().filter(|c| *c != '\u{fe0f}').collect()
}

/// [`replace_shortcodes`], then [`strip_variation_selectors`]: a message
/// as it is shown. The send path stripped the selectors too, so they
/// were gone from history for good, though only the display needs it
/// gone (review 2026-10, P3-49).
#[must_use]
pub fn for_display(input: &str) -> String {
    strip_variation_selectors(&replace_shortcodes(input))
}

/// The picker's emoji: `(shortcode, glyph as shown)`, each glyph once
/// — five had two names and showed twice — and without the selector
/// the display strips anyway.
#[must_use]
pub fn palette() -> Vec<(&'static str, String)> {
    let mut seen = std::collections::HashSet::new();
    EMOJI
        .iter()
        .filter(|(_, glyph)| seen.insert(*glyph))
        .map(|(name, glyph)| (*name, strip_variation_selectors(glyph)))
        .collect()
}

/// Replace `:shortcode:` occurrences with their emoji. Variation
/// selectors are kept: what is sent and stored is what was typed.
///
/// Unknown codes are left untouched (`:whatever:` stays as typed).
/// Scanning resumes right after a failed opening colon so things like
/// `10::joy:` still convert the inner code.
#[must_use]
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

        if let Some((emoji, end)) = matched {
            out.push_str(emoji);
            rest = &after[end + 1..];
        } else {
            // Not a shortcode — keep the colon, rescan from the
            // character right after it.
            out.push(':');
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selectors_are_kept_on_the_way_out_and_stripped_for_display() {
        let typed = "fly \u{2708}\u{fe0f} :heart:";
        assert!(replace_shortcodes(typed).contains('\u{fe0f}'));
        assert!(!for_display(typed).contains('\u{fe0f}'));
        let palette = palette();
        let thumbs = palette.iter().filter(|(_, g)| g == "\u{1f44d}").count();
        assert_eq!(thumbs, 1, "one 👍 for two names");
        assert!(palette.iter().all(|(_, g)| !g.contains('\u{fe0f}')));
    }

    #[test]
    fn vs16_is_stripped_for_display() {
        assert_eq!(
            strip_variation_selectors("a\u{2708}\u{fe0f}b"),
            "a\u{2708}b"
        );
        assert_eq!(strip_variation_selectors("plain"), "plain");
        // The display path normalizes; the send path keeps what was
        // typed (P3-49).
        assert_eq!(
            for_display("\u{26a0}\u{fe0f} watch out"),
            "\u{26a0} watch out"
        );
        assert_eq!(for_display(":warning: here"), "\u{26a0} here");
    }

    #[test]
    fn replaces_known_shortcode() {
        assert_eq!(replace_shortcodes("hi :joy:"), "hi 😂");
    }

    #[test]
    fn leaves_unknown_shortcode_as_typed() {
        assert_eq!(replace_shortcodes(":not_a_thing:"), ":not_a_thing:");
    }

    #[test]
    fn recovers_after_stray_colon() {
        assert_eq!(replace_shortcodes("10::joy:"), "10:😂");
    }

    #[test]
    fn no_colon_is_untouched() {
        assert_eq!(replace_shortcodes("plain"), "plain");
    }
}
