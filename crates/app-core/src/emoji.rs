//! Emoji shortcodes — `:joy:` → 😂.
//!
//! Applied to outgoing messages before sending and to incoming
//! messages before display (so older clients that send raw shortcodes
//! still render nicely). The catalog ([`crate::EMOJI`]) also feeds the
//! picker palette. Ported verbatim from the FLTK `ui/emoji.rs`.

use std::collections::HashMap;
use std::sync::OnceLock;

use crate::EMOJI;

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
