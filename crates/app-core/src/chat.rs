//! The chat history model and link detection.
//!
//! [`ChatHistory`] is the source of truth the UI model mirrors: rows of
//! [`ChatItem`], capped at [`MAX_ROWS`] (oldest dropped). ```-fenced
//! code becomes its own [`ChatItem::Code`] row; links stay inline in
//! the message text (the markdown translator linkifies them for
//! StyledText).

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
    /// A ```-fenced code block extracted from a message.
    Code {
        ts: String,
        user: String,
        code: String,
    },
    Image {
        ts: String,
        user: String,
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

    /// Append a user message. ```-fenced blocks become their own
    /// [`ChatItem::Code`] rows (monospace panels in the UI); links
    /// render inline inside the message text (the markdown translator
    /// linkifies them for StyledText).
    pub fn push_message(&mut self, ts: &str, user: &str, body: &str) -> HistoryDelta {
        let mut appended = 0;
        for segment in crate::markdown::split_fences(body) {
            match segment {
                crate::markdown::Segment::Text(text) => {
                    self.rows.push(ChatItem::Message {
                        ts: ts.into(),
                        user: user.into(),
                        body: text,
                    });
                    appended += 1;
                }
                crate::markdown::Segment::Code(code) => {
                    self.rows.push(ChatItem::Code {
                        ts: ts.into(),
                        user: user.into(),
                        code,
                    });
                    appended += 1;
                }
            }
        }
        self.cap(appended)
    }

    pub fn push_system(&mut self, body: &str) -> HistoryDelta {
        self.rows.push(ChatItem::System { body: body.into() });
        self.cap(1)
    }

    /// Note: the compressed bytes deliberately do **not** live here.
    /// The shell keeps exactly one copy (its viewer/save map, keyed by
    /// row id and evicted with the row); history only needs the
    /// metadata to lay the row out. Storing bytes here too used to
    /// double every image's footprint for nothing — no code ever read
    /// them back.
    pub fn push_image(
        &mut self,
        ts: &str,
        user: &str,
        width: u32,
        height: u32,
        is_gif: bool,
    ) -> HistoryDelta {
        self.rows.push(ChatItem::Image {
            ts: ts.into(),
            user: user.into(),
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

/// Strip control characters (e.g. the literal ^V Windows synthesizes
/// around Ctrl+V) — the reactive backstop the input had in FLTK.
#[must_use]
pub fn scrub_control_chars(s: &str) -> String {
    // Newlines are legitimate since the composer went multi-line
    // (Shift+Enter); every other control char still gets dropped —
    // including \r, so CRLF pastes normalize to \n.
    s.chars().filter(|c| *c == '\n' || !c.is_control()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn code_fences_become_rows() {
        let mut h = ChatHistory::new();
        let d = h.push_message("12:00", "a", "look:\n```\nlet x = 1;\n```\nand https://x.test");
        // Links stay inline in the message text (linkified at render
        // time) — no extra rows.
        assert_eq!(d.appended, 3);
        assert!(matches!(&h.rows()[0], ChatItem::Message { body, .. } if body == "look:"));
        assert!(matches!(&h.rows()[1], ChatItem::Code { code, .. } if code == "let x = 1;"));
        assert!(matches!(&h.rows()[2], ChatItem::Message { body, .. } if body.contains("https")));
    }

    #[test]
    fn links_inside_code_stay_code() {
        let mut h = ChatHistory::new();
        let d = h.push_message("12:00", "a", "```\nhttps://not-a-link.test\n```");
        assert_eq!(d.appended, 1);
        assert!(matches!(&h.rows()[0], ChatItem::Code { .. }));
    }

    #[test]
    fn message_links_stay_inline() {
        let mut h = ChatHistory::new();
        let d = h.push_message("12:00", "a", "see https://x.test/a and www.y.test");
        // One row: links render inline via the markdown translator.
        assert_eq!(d.appended, 1);
        assert!(matches!(&h.rows()[0], ChatItem::Message { body, .. } if body.contains("www.y.test")));
    }

    #[test]
    fn scrub_removes_control_chars() {
        assert_eq!(scrub_control_chars("a\u{16}b\u{7}c"), "abc");
        assert_eq!(scrub_control_chars("one\r\ntwo\nthree"), "one\ntwo\nthree");
    }
}
