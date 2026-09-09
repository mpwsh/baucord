//! The chat history model and link detection.
//!
//! [`ChatHistory`] is the source of truth the UI model mirrors: rows of
//! [`ChatItem`], capped at [`MAX_ROWS`] (oldest dropped). ```-fenced
//! code becomes its own [`ChatItem::Code`] row; links stay inline in
//! the message text (the markdown translator linkifies them for
//! StyledText).
//!
//! Every user-authored row carries the server's message id, so an
//! edit or a deletion that arrives later can find its rows
//! ([`ChatHistory::edit_message`], [`ChatHistory::delete_message`]) and
//! the UI can offer edit/delete on the rows the session owns.

/// History cap — oldest rows are dropped past this.
pub const MAX_ROWS: usize = 500;

/// Character cap for an outgoing message (matches the wire check).
pub const MAX_MESSAGE_CHARS: usize = 400;

/// Messages from one author closer than this are one run: the first
/// carries the picture and the name, the rest only their body.
pub const GROUP_WINDOW_MS: u64 = 5 * 60 * 1000;

/// Who sent a message and when, as the rows need it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Meta {
    /// The server's message id. Rows of one message share it.
    pub id: u64,
    /// Local `HH:MM`, already formatted.
    pub ts: String,
    /// The server wall clock, ms since the Unix epoch.
    pub ts_ms: u64,
    /// The local calendar day of `ts_ms` (days since the epoch, in the
    /// viewer's zone): where the day dividers go.
    pub day: i64,
    pub user: String,
    /// Authored by this session's user: the edit/delete menu appears.
    pub own: bool,
    /// Sent, not yet acknowledged (a DM waiting for the store): drawn
    /// muted; `id` is a placeholder until then.
    pub pending: bool,
}

/// One row of chat history.
#[derive(Debug, Clone, PartialEq)]
pub enum ChatItem {
    Message {
        meta: Meta,
        body: String,
        /// The server marked it edited (a later edit, or replayed as such).
        edited: bool,
        /// Deleted for everyone: the header stays, the body is a
        /// muted placeholder, the menu is gone.
        deleted: bool,
    },
    System {
        body: String,
    },
    /// A ```-fenced code block extracted from a message.
    Code {
        meta: Meta,
        code: String,
    },
    Image {
        meta: Meta,
        width: u32,
        height: u32,
        is_gif: bool,
    },
    /// "Thursday, August 27th": the day changed between the rows
    /// around it.
    DayMarker {
        label: String,
        day: i64,
    },
    /// A file posted into the conversation (spec §7): a card, fetched
    /// on demand.
    File {
        meta: Meta,
        file: FileMeta,
    },
}

/// What a file card shows and needs (spec §7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileMeta {
    pub blob: [u8; 32],
    pub name: String,
    /// What the server sniffed.
    pub mime: String,
    pub size: u32,
}

impl FileMeta {
    /// `1.2 MB`, `640 KB`, `12 B`.
    #[must_use]
    pub fn size_text(&self) -> String {
        human_size(u64::from(self.size))
    }

    /// A short type word for the card: the subtype in caps (`PDF`,
    /// `ZIP`), or the name's extension when the type is generic.
    #[must_use]
    pub fn kind_text(&self) -> String {
        let ext = std::path::Path::new(&self.name)
            .extension()
            .and_then(|e| e.to_str())
            .filter(|e| !e.is_empty() && e.len() <= 8);
        let generic = self.mime == "application/octet-stream" || self.mime.is_empty();
        match (ext, generic) {
            (Some(ext), true) => ext.to_ascii_uppercase(),
            (_, true) => "FILE".to_string(),
            (_, false) => self
                .mime
                .rsplit('/')
                .next()
                .unwrap_or("file")
                .trim_start_matches("x-")
                .trim_start_matches("vnd.")
                .to_ascii_uppercase(),
        }
    }
}

/// `1.2 MB`, `640 KB`, `12 B` — the client's twin of the server's.
#[must_use]
pub fn human_size(bytes: u64) -> String {
    #[allow(clippy::cast_precision_loss)]
    let b = bytes as f64;
    if bytes >= 1 << 30 {
        format!("{:.1} GB", b / f64::from(1u32 << 30))
    } else if bytes >= 1 << 20 {
        format!("{:.1} MB", b / f64::from(1u32 << 20))
    } else if bytes >= 1 << 10 {
        format!("{:.0} KB", b / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

impl ChatItem {
    /// The author's metadata of a user-authored row.
    #[must_use]
    pub fn meta(&self) -> Option<&Meta> {
        match self {
            ChatItem::Message { meta, .. }
            | ChatItem::Code { meta, .. }
            | ChatItem::Image { meta, .. }
            | ChatItem::File { meta, .. } => Some(meta),
            ChatItem::System { .. } | ChatItem::DayMarker { .. } => None,
        }
    }

    /// The message id of a user-authored row; `None` otherwise.
    #[must_use]
    pub fn message_id(&self) -> Option<u64> {
        self.meta().map(|m| m.id)
    }

    /// The calendar day a row belongs to; `None` for system rows.
    #[must_use]
    pub fn day(&self) -> Option<i64> {
        match self {
            ChatItem::DayMarker { day, .. } => Some(*day),
            other => other.meta().map(|m| m.day),
        }
    }
}

/// Rows dropped from the front + rows appended, for incremental model
/// sync (the UI keeps a `VecModel` mirror).
pub struct HistoryDelta {
    pub dropped_front: usize,
    pub appended: usize,
}

/// A splice in the middle of the history: `removed` rows at `start`
/// were replaced by `inserted` rows at the same position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RangeDelta {
    pub start: usize,
    pub removed: usize,
    pub inserted: usize,
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
    pub fn push_message(&mut self, meta: &Meta, body: &str, edited: bool) -> HistoryDelta {
        let rows = Self::segments(meta, body, edited);
        let appended = rows.len();
        self.rows.extend(rows);
        self.cap(appended)
    }

    pub fn push_system(&mut self, body: &str) -> HistoryDelta {
        self.rows.push(ChatItem::System { body: body.into() });
        self.cap(1)
    }

    /// A day divider. Callers add one before a message whose day
    /// differs from [`ChatHistory::last_day`].
    pub fn push_day_marker(&mut self, label: &str, day: i64) -> HistoryDelta {
        self.rows.push(ChatItem::DayMarker {
            label: label.into(),
            day,
        });
        self.cap(1)
    }

    /// The day of the last row that has one (a message or a divider).
    #[must_use]
    pub fn last_day(&self) -> Option<i64> {
        self.rows.iter().rev().find_map(ChatItem::day)
    }

    /// Rows for one message, fences split out — for callers that
    /// assemble a page with dividers before prepending it.
    #[must_use]
    pub fn rows_for(meta: &Meta, body: &str, edited: bool) -> Vec<ChatItem> {
        Self::segments(meta, body, edited)
    }

    /// Put assembled rows (oldest first) in front of what is shown
    /// ("load earlier"). Returns how many went in. Not capped here:
    /// the reader asked for these; the next append trims from the
    /// front as usual.
    pub fn prepend_rows(&mut self, rows: Vec<ChatItem>) -> usize {
        let inserted = rows.len();
        self.rows.splice(0..0, rows);
        inserted
    }

    /// Drop one row (a divider made redundant by a prepended page).
    pub fn remove_row(&mut self, index: usize) -> Option<RangeDelta> {
        if index >= self.rows.len() {
            return None;
        }
        self.rows.remove(index);
        Some(RangeDelta {
            start: index,
            removed: 1,
            inserted: 0,
        })
    }

    /// Whether row `index` continues the run of the row before it:
    /// same author, the previous row a message (a system line or a
    /// divider breaks a run), within [`GROUP_WINDOW_MS`]. A run's first
    /// row carries the picture and the name; the rest only the body.
    #[must_use]
    pub fn is_continuation(&self, index: usize) -> bool {
        let Some(prev) = index.checked_sub(1).and_then(|i| self.rows.get(i)) else {
            return false;
        };
        let (Some(this), Some(prev)) = (self.rows.get(index).and_then(ChatItem::meta), prev.meta())
        else {
            return false;
        };
        this.user == prev.user && this.ts_ms.saturating_sub(prev.ts_ms) <= GROUP_WINDOW_MS
    }

    /// Note: the compressed bytes deliberately do **not** live here.
    /// The shell keeps exactly one copy (its viewer/save map, keyed by
    /// row id and evicted with the row); history only needs the
    /// metadata to lay the row out. Storing bytes here too used to
    /// double every image's footprint for nothing — no code ever read
    /// them back.
    pub fn push_image(
        &mut self,
        meta: &Meta,
        width: u32,
        height: u32,
        is_gif: bool,
    ) -> HistoryDelta {
        self.rows.push(ChatItem::Image {
            meta: meta.clone(),
            width,
            height,
            is_gif,
        });
        self.cap(1)
    }

    /// A file card (spec §7).
    pub fn push_file(&mut self, meta: &Meta, file: FileMeta) -> HistoryDelta {
        self.rows.push(ChatItem::File {
            meta: meta.clone(),
            file,
        });
        self.cap(1)
    }

    /// A replayed page named this message's file after the fact (the
    /// row arrived as an empty message): it becomes the card. The
    /// index changed, or `None` when the message is not in the window.
    pub fn attach_file(&mut self, id: u64, file: FileMeta) -> Option<usize> {
        let (start, end) = self.span_of(id)?;
        let ChatItem::Message { meta, .. } = &self.rows[start] else {
            return None;
        };
        let card = ChatItem::File {
            meta: meta.clone(),
            file,
        };
        self.rows.splice(start..end, [card]);
        Some(start)
    }

    /// Whether a message with this id is in the window.
    #[must_use]
    pub fn has(&self, id: u64) -> bool {
        self.rows.iter().any(|r| r.message_id() == Some(id))
    }

    /// A card whose file is a picture arrived as bytes: the row becomes
    /// an image (spec §7: pictures in DMs ride the blob path). The
    /// index changed, or `None` when the message is not in the window.
    pub fn attach_image(
        &mut self,
        id: u64,
        width: u32,
        height: u32,
        is_gif: bool,
    ) -> Option<usize> {
        let (start, end) = self.span_of(id)?;
        let meta = self.rows[start].meta()?.clone();
        let image = ChatItem::Image {
            meta,
            width,
            height,
            is_gif,
        };
        self.rows.splice(start..end, [image]);
        Some(start)
    }

    /// Replace a card's file (an upload learned its blob).
    pub fn set_file(&mut self, index: usize, file: FileMeta) {
        if let Some(ChatItem::File { file: slot, .. }) = self.rows.get_mut(index) {
            *slot = file;
        }
    }

    /// The file of row `index`, if it is a card.
    #[must_use]
    pub fn file_at(&self, index: usize) -> Option<&FileMeta> {
        match self.rows.get(index)? {
            ChatItem::File { file, .. } => Some(file),
            _ => None,
        }
    }

    /// A message was retexted for everyone: its rows are replaced by
    /// the new text's rows (re-split on fences) at the same place,
    /// flagged edited. `None` when the message is not in the window
    /// (older than [`MAX_ROWS`], or from before this session).
    pub fn edit_message(&mut self, id: u64, body: &str) -> Option<RangeDelta> {
        let (start, end) = self.span_of(id)?;
        let (ChatItem::Message { meta, .. } | ChatItem::Code { meta, .. }) = &self.rows[start]
        else {
            // Images cannot be edited; the server never sends this.
            return None;
        };
        let rows = Self::segments(meta, body, true);
        let inserted = rows.len();
        self.rows.splice(start..end, rows);
        Some(RangeDelta {
            start,
            removed: end - start,
            inserted,
        })
    }

    /// Someone changed their nickname: every row they authored under
    /// `old` now says `new`. Returns the indices that changed so the
    /// mirror can refresh just those rows.
    pub fn rename_author(&mut self, old: &str, new: &str) -> Vec<usize> {
        let mut changed = Vec::new();
        for (i, row) in self.rows.iter_mut().enumerate() {
            let meta = match row {
                ChatItem::Message { meta, .. }
                | ChatItem::Code { meta, .. }
                | ChatItem::Image { meta, .. }
                | ChatItem::File { meta, .. } => meta,
                ChatItem::System { .. } | ChatItem::DayMarker { .. } => continue,
            };
            if meta.user == old {
                new.clone_into(&mut meta.user);
                changed.push(i);
            }
        }
        changed
    }

    /// The server acknowledged a pending message: its rows take the
    /// real id and lose the muted look.
    pub fn confirm_message(&mut self, placeholder: u64, id: u64) -> Option<RangeDelta> {
        let (start, end) = self.span_of(placeholder)?;
        for row in &mut self.rows[start..end] {
            if let ChatItem::Message { meta, .. }
            | ChatItem::Code { meta, .. }
            | ChatItem::Image { meta, .. }
            | ChatItem::File { meta, .. } = row
            {
                meta.id = id;
                meta.pending = false;
            }
        }
        // Replaced in place: the mirror re-renders exactly these rows.
        Some(RangeDelta {
            start,
            removed: end - start,
            inserted: end - start,
        })
    }

    /// Drop every row of a message the server refused.
    pub fn discard_message(&mut self, id: u64) -> Option<RangeDelta> {
        let (start, end) = self.span_of(id)?;
        self.rows.drain(start..end);
        Some(RangeDelta {
            start,
            removed: end - start,
            inserted: 0,
        })
    }

    /// A message was removed for everyone: its rows collapse into one
    /// placeholder row that keeps the header (who, when) and says the
    /// body is gone.
    pub fn delete_message(&mut self, id: u64) -> Option<RangeDelta> {
        let (start, end) = self.span_of(id)?;
        let (ChatItem::Message { meta, .. }
        | ChatItem::Code { meta, .. }
        | ChatItem::Image { meta, .. }
        | ChatItem::File { meta, .. }) = &self.rows[start]
        else {
            return None;
        };
        let placeholder = ChatItem::Message {
            meta: meta.clone(),
            body: String::new(),
            edited: false,
            deleted: true,
        };
        self.rows.splice(start..end, [placeholder]);
        Some(RangeDelta {
            start,
            removed: end - start,
            inserted: 1,
        })
    }

    /// The contiguous run of rows that belong to message `id`.
    fn span_of(&self, id: u64) -> Option<(usize, usize)> {
        let start = self.rows.iter().position(|r| r.message_id() == Some(id))?;
        let end = self.rows[start..]
            .iter()
            .position(|r| r.message_id() != Some(id))
            .map_or(self.rows.len(), |n| start + n);
        Some((start, end))
    }

    /// One message → its rows, fences split out.
    fn segments(meta: &Meta, body: &str, edited: bool) -> Vec<ChatItem> {
        crate::markdown::split_fences(body)
            .into_iter()
            .map(|segment| match segment {
                crate::markdown::Segment::Text(text) => ChatItem::Message {
                    meta: meta.clone(),
                    body: text,
                    edited,
                    deleted: false,
                },
                crate::markdown::Segment::Code(code) => ChatItem::Code {
                    meta: meta.clone(),
                    code,
                },
            })
            .collect()
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
    s.chars()
        .filter(|c| *c == '\n' || !c.is_control())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(id: u64) -> Meta {
        Meta {
            id,
            ts: "12:00".into(),
            // Placeholder ids sit near `u64::MAX`; a plain multiply
            // overflows there under debug assertions.
            ts_ms: id.saturating_mul(1_000),
            day: 20_000,
            user: "a".into(),
            own: false,
            pending: false,
        }
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
            ChatItem::System {
                body: "row 20".into()
            }
        );
    }

    #[test]
    fn code_fences_become_rows() {
        let mut h = ChatHistory::new();
        let d = h.push_message(
            &meta(1),
            "look:\n```\nlet x = 1;\n```\nand https://x.test",
            false,
        );
        // Links stay inline in the message text (linkified at render
        // time) — no extra rows.
        assert_eq!(d.appended, 3);
        assert!(matches!(&h.rows()[0], ChatItem::Message { body, .. } if body == "look:"));
        assert!(matches!(&h.rows()[1], ChatItem::Code { code, .. } if code == "let x = 1;"));
        assert!(matches!(&h.rows()[2], ChatItem::Message { body, .. } if body.contains("https")));
        assert!(h.rows().iter().all(|r| r.message_id() == Some(1)));
    }

    #[test]
    fn links_inside_code_stay_code() {
        let mut h = ChatHistory::new();
        let d = h.push_message(&meta(1), "```\nhttps://not-a-link.test\n```", false);
        assert_eq!(d.appended, 1);
        assert!(matches!(&h.rows()[0], ChatItem::Code { .. }));
    }

    #[test]
    fn message_links_stay_inline() {
        let mut h = ChatHistory::new();
        let d = h.push_message(&meta(1), "see https://x.test/a and www.y.test", false);
        // One row: links render inline via the markdown translator.
        assert_eq!(d.appended, 1);
        assert!(
            matches!(&h.rows()[0], ChatItem::Message { body, .. } if body.contains("www.y.test"))
        );
    }

    #[test]
    fn edit_replaces_every_row_of_the_message_in_place() {
        let mut h = ChatHistory::new();
        h.push_system("before");
        h.push_message(&meta(7), "one\n```\ncode\n```", false);
        h.push_message(&meta(8), "after", false);
        let d = h
            .edit_message(7, "fixed")
            .expect("message is in the window");
        assert_eq!(
            d,
            RangeDelta {
                start: 1,
                removed: 2,
                inserted: 1
            }
        );
        assert_eq!(h.rows().len(), 3);
        assert!(matches!(
            &h.rows()[1],
            ChatItem::Message { body, edited: true, .. } if body == "fixed"
        ));
        assert!(matches!(&h.rows()[2], ChatItem::Message { body, .. } if body == "after"));
    }

    #[test]
    fn delete_leaves_one_placeholder_with_the_header() {
        let mut h = ChatHistory::new();
        h.push_message(&meta(7), "one\n```\ncode\n```", false);
        let d = h.delete_message(7).expect("message is in the window");
        assert_eq!(
            d,
            RangeDelta {
                start: 0,
                removed: 2,
                inserted: 1
            }
        );
        assert_eq!(h.rows().len(), 1);
        assert!(matches!(
            &h.rows()[0],
            ChatItem::Message { meta, deleted: true, .. } if meta.id == 7
        ));
    }

    #[test]
    fn prepended_pages_go_in_front_in_order() {
        let mut h = ChatHistory::new();
        h.push_message(&meta(10), "newest", false);
        let mut page = ChatHistory::rows_for(&meta(3), "three", false);
        page.extend(ChatHistory::rows_for(&meta(4), "four\n```\nx\n```", false));
        let n = h.prepend_rows(page);
        assert_eq!(n, 3);
        assert_eq!(h.rows().len(), 4);
        assert!(matches!(&h.rows()[0], ChatItem::Message { body, .. } if body == "three"));
        assert!(matches!(&h.rows()[2], ChatItem::Code { .. }));
        assert!(matches!(&h.rows()[3], ChatItem::Message { body, .. } if body == "newest"));
        let d = h.remove_row(2).expect("in range");
        assert_eq!(
            d,
            RangeDelta {
                start: 2,
                removed: 1,
                inserted: 0
            }
        );
        assert_eq!(h.rows().len(), 3);
    }

    #[test]
    fn runs_group_by_author_and_time_and_days_are_tracked() {
        let mut h = ChatHistory::new();
        assert_eq!(h.last_day(), None);
        h.push_day_marker("Monday", 20_000);
        h.push_message(&meta(1), "hi", false);
        h.push_message(&meta(2), "again", false);
        let mut other = meta(3);
        other.user = "b".into();
        h.push_message(&other, "hello", false);
        h.push_system("notice");
        h.push_message(&meta(5), "back", false);
        let mut late = meta(6);
        late.ts_ms = meta(5).ts_ms + GROUP_WINDOW_MS + 1;
        h.push_message(&late, "much later", false);
        let flags: Vec<bool> = (0..h.rows().len()).map(|i| h.is_continuation(i)).collect();
        //           marker hi     again  b:hello system back   later
        assert_eq!(flags, [false, false, true, false, false, false, false]);
        assert_eq!(h.last_day(), Some(20_000));
    }

    #[test]
    fn a_replayed_file_becomes_a_card_and_sizes_read_well() {
        let mut h = ChatHistory::new();
        h.push_message(&meta(1), "", false);
        h.push_message(&meta(2), "text", false);
        let file = FileMeta {
            blob: [9u8; 32],
            name: "notes.pdf".into(),
            mime: "application/pdf".into(),
            size: 1_258_291,
        };
        assert_eq!(h.attach_file(1, file.clone()), Some(0));
        assert_eq!(h.file_at(0), Some(&file));
        assert_eq!(h.attach_file(5, file.clone()), None);
        assert_eq!(file.size_text(), "1.2 MB");
        assert_eq!(file.kind_text(), "PDF");
        let raw = FileMeta {
            name: "dump.bin".into(),
            mime: "application/octet-stream".into(),
            ..file
        };
        assert_eq!(raw.kind_text(), "BIN");
        assert_eq!(human_size(640 * 1024), "640 KB");
        // A deleted card keeps its header.
        assert!(h.delete_message(1).is_some());
        assert!(matches!(
            h.rows()[0],
            ChatItem::Message { deleted: true, .. }
        ));
    }

    #[test]
    fn a_rename_follows_the_author_through_the_history() {
        let mut h = ChatHistory::new();
        h.push_message(&meta(1), "hi", false);
        h.push_system("bob joined");
        let mut other = meta(2);
        other.user = "carol".into();
        h.push_message(&other, "hey", false);
        h.push_message(&meta(3), "again", false);
        let changed = h.rename_author("a", "alicia");
        assert_eq!(changed, vec![0, 3]);
        assert!(h
            .rows()
            .iter()
            .filter_map(ChatItem::meta)
            .all(|m| m.user != "a"));
        assert_eq!(h.rows()[2].meta().map(|m| m.user.as_str()), Some("carol"));
        assert!(h.rename_author("nobody", "x").is_empty());
    }

    #[test]
    fn pending_messages_are_confirmed_or_discarded() {
        let mut h = ChatHistory::new();
        let mut m = meta(u64::MAX - 1);
        m.pending = true;
        h.push_message(&m, "sending", false);
        let d = h
            .confirm_message(u64::MAX - 1, 42)
            .expect("known placeholder");
        assert_eq!(
            d,
            RangeDelta {
                start: 0,
                removed: 1,
                inserted: 1
            }
        );
        assert!(matches!(
            &h.rows()[0],
            ChatItem::Message { meta, .. } if meta.id == 42 && !meta.pending
        ));
        assert!(h.discard_message(42).is_some());
        assert!(h.rows().is_empty());
    }

    #[test]
    fn edits_of_unknown_messages_are_none() {
        let mut h = ChatHistory::new();
        h.push_message(&meta(1), "x", false);
        assert!(h.edit_message(2, "y").is_none());
        assert!(h.delete_message(2).is_none());
    }

    #[test]
    fn scrub_removes_control_chars() {
        assert_eq!(scrub_control_chars("a\u{16}b\u{7}c"), "abc");
        assert_eq!(scrub_control_chars("one\r\ntwo\nthree"), "one\ntwo\nthree");
    }
}
