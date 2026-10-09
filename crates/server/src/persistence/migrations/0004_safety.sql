-- baucord 1.2 schema, migration 4: blocking and reporting
-- (protocol::safety, PERSISTENCE-SPEC §12 S1).

-- Who blocked whom. A block is the blocker's, on every device; it
-- goes with either account.
CREATE TABLE blocks (
    blocker    BLOB NOT NULL REFERENCES users(user_id) ON DELETE CASCADE,
    blocked    BLOB NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (blocker, blocked)
);
CREATE INDEX blocks_blocked ON blocks(blocked);

-- Reports to the moderators. The reported text is copied in, as it
-- was: an edit or a delete afterwards does not change what was
-- reported. A report about someone goes when their account does; a
-- report by someone stays, without the reporter, when theirs does.
-- Resolved reports are pruned with the message history window.
CREATE TABLE reports (
    id          INTEGER PRIMARY KEY,
    ts          INTEGER NOT NULL,
    reporter    BLOB,
    target_user BLOB,
    target_name TEXT NOT NULL,
    msg_id      INTEGER,
    in_dm       INTEGER NOT NULL DEFAULT 0,
    excerpt     TEXT NOT NULL DEFAULT '',
    reason      INTEGER NOT NULL,   -- protocol::ReportReason
    note        TEXT NOT NULL DEFAULT '',
    resolved_at INTEGER,
    resolved_by BLOB
);
CREATE INDEX reports_open ON reports(resolved_at, id);
CREATE INDEX reports_target ON reports(target_user);
