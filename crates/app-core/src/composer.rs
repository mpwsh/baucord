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
    let (a, b) = {
        let (c, a) = (snap(text, cursor), snap(text, anchor));
        (c.min(a), c.max(a))
    };
    let mut out = String::with_capacity(text.len() + 1);
    out.push_str(&text[..a]);
    out.push('\n');
    out.push_str(&text[b..]);
    (out, a + 1)
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
