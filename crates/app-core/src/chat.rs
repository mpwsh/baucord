//! The chat history model and link detection.
//!
//! [`ChatHistory`] is the source of truth the UI model mirrors: rows of
//! [`ChatItem`], capped at [`MAX_ROWS`] (oldest dropped). Link detection
//! is ported verbatim from the FLTK `chat_view.rs` (with its tests);
//! detected links become their own [`ChatItem::Link`] rows right after
//! the message that carried them — Slint renders them as dedicated
//! tappable rows.

/// History cap — oldest rows are dropped past this.
pub const MAX_ROWS: usize = 500;

/// Character cap for an outgoing message (matches the wire check).
pub const MAX_MESSAGE_CHARS: usize = 400;

/// One row of chat history.
#[derive(Debug, Clone, PartialEq)]
pub enum ChatItem {
    Message {
        ts: String,
        user: String,
        body: String,
    },
    System {
        body: String,
    },
    /// A link detected inside the preceding message.
    Link {
        url: String,
    },
    Image {
        ts: String,
        user: String,
        /// Original PNG/GIF bytes — kept for save/viewer/GIF replay.
        bytes: Vec<u8>,
        width: u32,
        height: u32,
        is_gif: bool,
    },
}

/// Rows dropped from the front + rows appended, for incremental model
/// sync (the UI keeps a `VecModel` mirror).
pub struct HistoryDelta {
    pub dropped_front: usize,
    pub appended: usize,
}

#[derive(Default)]
pub struct ChatHistory {
    rows: Vec<ChatItem>,
    next_row_id: u64,
}

impl ChatHistory {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn rows(&self) -> &[ChatItem] {
        &self.rows
    }

    pub fn clear(&mut self) {
        self.rows.clear();
    }

    /// Monotonic id for image rows (GIF driver / viewer targeting).
    pub fn next_id(&mut self) -> u64 {
        self.next_row_id += 1;
        self.next_row_id
    }

    /// Append a user message; any links it contains become their own
    /// rows right after it.
    pub fn push_message(&mut self, ts: &str, user: &str, body: &str) -> HistoryDelta {
        let links = detect_links(body);
        let mut appended = 1;
        self.rows.push(ChatItem::Message {
            ts: ts.into(),
            user: user.into(),
            body: body.into(),
        });
        for url in links {
            self.rows.push(ChatItem::Link { url });
            appended += 1;
        }
        self.cap(appended)
    }

    pub fn push_system(&mut self, body: &str) -> HistoryDelta {
        self.rows.push(ChatItem::System { body: body.into() });
        self.cap(1)
    }

    pub fn push_image(
        &mut self,
        ts: &str,
        user: &str,
        bytes: Vec<u8>,
        width: u32,
        height: u32,
        is_gif: bool,
    ) -> HistoryDelta {
        self.rows.push(ChatItem::Image {
            ts: ts.into(),
            user: user.into(),
            bytes,
            width,
            height,
            is_gif,
        });
        self.cap(1)
    }

    fn cap(&mut self, appended: usize) -> HistoryDelta {
        let dropped = self.rows.len().saturating_sub(MAX_ROWS);
        if dropped > 0 {
            self.rows.drain(..dropped);
        }
        HistoryDelta {
            dropped_front: dropped,
            appended,
        }
    }
}

/// Detect http(s):// and www. links in a message.
///
/// Behavior carried over from the FLTK build: trailing punctuation is
/// trimmed out of the link, bare `http://` isn't a link, and `www.`
/// links get an `https://` prefix.
#[must_use]
pub fn detect_links(text: &str) -> Vec<String> {
    let mut links = Vec::new();
    for piece in split_keep_whitespace(text) {
        if piece.chars().next().is_some_and(char::is_whitespace) {
            continue;
        }
        let lower = piece.to_ascii_lowercase();
        let is_url = lower.starts_with("http://") || lower.starts_with("https://");
        let is_www = !is_url && lower.starts_with("www.") && piece.len() > 4;
        if !(is_url || is_www) {
            continue;
        }
        let trimmed = piece.trim_end_matches(['.', ',', ';', ':', '!', '?', ')', ']', '"', '\'']);
        if trimmed.len() < 8 && !is_www {
            // "http://" with nothing behind it — not a real link.
            continue;
        }
        links.push(if is_www {
            format!("https://{trimmed}")
        } else {
            trimmed.to_string()
        });
    }
    links
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

/// Strip control characters (e.g. the literal ^V Windows synthesizes
/// around Ctrl+V) — the reactive backstop the input had in FLTK.
#[must_use]
pub fn scrub_control_chars(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_http_links() {
        assert_eq!(
            detect_links("see https://example.com/x for details"),
            vec!["https://example.com/x"]
        );
    }

    #[test]
    fn trims_trailing_punctuation() {
        assert_eq!(
            detect_links("go to https://example.com/x."),
            vec!["https://example.com/x"]
        );
    }

    #[test]
    fn www_gets_https_prefix() {
        assert_eq!(detect_links("www.example.com rocks"), vec!["https://www.example.com"]);
    }

    #[test]
    fn plain_text_has_no_links() {
        assert!(detect_links("no links here, just http talk").is_empty());
    }

    #[test]
    fn whitespace_splitter_roundtrips() {
        let s = "a  b\tc \n d";
        assert_eq!(split_keep_whitespace(s).concat(), s);
    }

    #[test]
    fn history_caps_at_max_rows() {
        let mut h = ChatHistory::new();
        for i in 0..(MAX_ROWS + 20) {
            h.push_system(&format!("row {i}"));
        }
        assert_eq!(h.rows().len(), MAX_ROWS);
        assert_eq!(
            h.rows()[0],
            ChatItem::System { body: "row 20".into() }
        );
    }

    #[test]
    fn message_links_become_rows() {
        let mut h = ChatHistory::new();
        let d = h.push_message("12:00", "a", "see https://x.test/a and www.y.test");
        assert_eq!(d.appended, 3);
        assert!(matches!(h.rows()[1], ChatItem::Link { .. }));
        assert!(matches!(h.rows()[2], ChatItem::Link { .. }));
    }

    #[test]
    fn scrub_removes_control_chars() {
        assert_eq!(scrub_control_chars("a\u{16}b\u{7}c"), "abc");
    }
}
