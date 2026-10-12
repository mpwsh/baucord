//! Composer text surgery the UI can't do itself.
//!
//! `.slint` has no substring operations, so cursor-relative edits
//! round-trip through the host: the composer's key handler forwards
//! the byte offsets Slint exposes (`cursor-position-byte-offset`),
//! the shell computes the new text/caret here, writes the draft back,
//! and re-places the caret. All offsets are UTF-8 byte offsets, the
//! same unit `TextInput.set-selection-offsets` consumes.

/// Clamp `pos` into `text` and snap it back onto a char boundary
/// (defensive: Slint's offsets are already boundary-aligned).
fn snap(text: &str, pos: usize) -> usize {
    let mut p = pos.min(text.len());
    while p > 0 && !text.is_char_boundary(p) {
        p -= 1;
    }
    p
}

/// Insert a newline at the caret, replacing any active selection —
/// what Shift+Enter should do. Returns the new text and caret offset.
#[must_use]
pub fn insert_newline(text: &str, cursor: usize, anchor: usize) -> (String, usize) {
    let (a, b) = span(text, cursor, anchor);
    let mut out = String::with_capacity(text.len() + 1);
    out.push_str(&text[..a]);
    out.push('\n');
    out.push_str(&text[b..]);
    (out, a + 1)
}

/// The selection's two ends in order, on char boundaries.
fn span(text: &str, cursor: usize, anchor: usize) -> (usize, usize) {
    let (c, a) = (snap(text, cursor), snap(text, anchor));
    (c.min(a), c.max(a))
}

/// What is selected between the caret and the anchor — what Copy takes.
/// Empty when they are in the same place.
#[must_use]
pub fn selected(text: &str, cursor: usize, anchor: usize) -> &str {
    let (a, b) = span(text, cursor, anchor);
    &text[a..b]
}

/// The text without its selection, and where the caret then is — what
/// Cut leaves.
#[must_use]
pub fn cut(text: &str, cursor: usize, anchor: usize) -> (String, usize) {
    let (a, b) = span(text, cursor, anchor);
    (format!("{}{}", &text[..a], &text[b..]), a)
}

/// Byte offset of the start (`to_end == false`) or end of the line the
/// caret is on — Ctrl+A / Ctrl+E on macOS.
#[must_use]
pub fn line_edge(text: &str, cursor: usize, to_end: bool) -> usize {
    let c = snap(text, cursor);
    if to_end {
        text[c..].find('\n').map_or(text.len(), |i| c + i)
    } else {
        text[..c].rfind('\n').map_or(0, |i| i + 1)
    }
}

/// What follows the `/` of a typed line, as a command and its
/// arguments: `kick alice spam` → (`kick`, `alice spam`). `None` when it
/// is not a command (a bare `/`, a space right after it, a word with
/// anything but ASCII letters, digits and `-`): the line is then sent as
/// written.
///
/// The word ends at the first whitespace of any kind. The split used to
/// take the first word's length and slice the line by it from the
/// start: after a leading non-breaking or full-width space that offset
/// fell inside a character and the app aborted, and longer words were
/// cut in the wrong place (review P2-15).
#[must_use]
pub fn split_command(rest: &str) -> Option<(&str, &str)> {
    let (command, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    if command.is_empty()
        || !command
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-')
    {
        return None;
    }
    Some((command, args.trim_start()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy_and_cut_take_the_selection_whichever_way_it_was_dragged() {
        assert_eq!(selected("hello world", 0, 5), "hello");
        assert_eq!(selected("hello world", 5, 0), "hello");
        assert_eq!(cut("hello world", 5, 0), (" world".into(), 0));
        assert_eq!(cut("hello world", 6, 11), ("hello ".into(), 6));
    }

    #[test]
    fn nothing_selected_is_nothing_taken() {
        assert_eq!(selected("abc", 2, 2), "");
        assert_eq!(cut("abc", 2, 2), ("abc".into(), 2));
    }

    #[test]
    fn stale_offsets_stay_inside_the_text_and_on_characters() {
        // The draft shrank since the offsets were read, and one of them
        // now falls inside "é".
        assert_eq!(selected("né", 2, 40), "é");
        assert_eq!(cut("né", 2, 40), ("n".into(), 1));
    }

    #[test]
    fn newline_at_caret() {
        assert_eq!(insert_newline("abcd", 2, 2), ("ab\ncd".into(), 3));
    }

    #[test]
    fn newline_replaces_selection() {
        // Selection can be made in either direction.
        assert_eq!(insert_newline("abcdef", 4, 1), ("a\nef".into(), 2));
        assert_eq!(insert_newline("abcdef", 1, 4), ("a\nef".into(), 2));
    }

    #[test]
    fn newline_at_ends() {
        assert_eq!(insert_newline("ab", 0, 0), ("\nab".into(), 1));
        assert_eq!(insert_newline("ab", 2, 2), ("ab\n".into(), 3));
        assert_eq!(insert_newline("", 0, 0), ("\n".into(), 1));
    }

    #[test]
    fn newline_survives_out_of_range_offsets() {
        assert_eq!(insert_newline("ab", 99, 99), ("ab\n".into(), 3));
    }

    #[test]
    fn newline_snaps_to_char_boundary() {
        // 'é' is two bytes; offset 2 splits it.
        let (out, pos) = insert_newline("aéb", 2, 2);
        assert_eq!(out, "a\néb");
        assert_eq!(pos, 2);
    }

    #[test]
    fn line_edges_single_line() {
        assert_eq!(line_edge("hello", 3, false), 0);
        assert_eq!(line_edge("hello", 3, true), 5);
    }

    #[test]
    fn line_edges_multiline() {
        let t = "one\ntwo\nthree";
        // caret inside "two" (offset 5)
        assert_eq!(line_edge(t, 5, false), 4);
        assert_eq!(line_edge(t, 5, true), 7);
        // caret exactly on the newline after "two" belongs to "two"
        assert_eq!(line_edge(t, 7, false), 4);
        // caret at start of "three"
        assert_eq!(line_edge(t, 8, true), 13);
    }

    #[test]
    fn a_command_is_the_word_right_after_the_slash() {
        assert_eq!(
            split_command("kick alice spam"),
            Some(("kick", "alice spam"))
        );
        assert_eq!(split_command("help"), Some(("help", "")));
        assert_eq!(split_command("role-list   x"), Some(("role-list", "x")));
        assert_eq!(split_command("me\tsays hi"), Some(("me", "says hi")));
        assert_eq!(split_command(""), None);
        assert_eq!(split_command(" kick alice"), None);
        assert_eq!(split_command("ki$ck alice"), None);
    }

    #[test]
    fn odd_spaces_split_instead_of_aborting() {
        // A non-breaking space, then a full-width one, before a short
        // word: each used to put the slice inside a character.
        assert_eq!(split_command("\u{a0}me"), None);
        assert_eq!(split_command("\u{3000}ok"), None);
        // And as the separator after the command.
        assert_eq!(split_command("me\u{a0}waves"), Some(("me", "waves")));
        assert_eq!(split_command("w\u{3000}é"), Some(("w", "é")));
    }
}
