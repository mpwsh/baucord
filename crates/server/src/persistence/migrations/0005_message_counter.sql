-- baucord 1.2.1 schema, migration 5: a message-id counter that outlives
-- the messages, and two indexes account deletion scans without.

-- The highest message id ever assigned. The next id used to come from
-- MAX(msg_id) over the messages still stored, so once retention had
-- pruned every message a restart began again at 1: ids were reused,
-- and every read mark, held as "read up to id N", counted the new
-- messages as already read.
CREATE TABLE counters (
    name  TEXT PRIMARY KEY,
    value INTEGER NOT NULL
) WITHOUT ROWID;
INSERT INTO counters (name, value)
SELECT 'msg_id', MAX(
    COALESCE((SELECT MAX(msg_id) FROM messages), 0),
    COALESCE((SELECT MAX(last_read_msg_id) FROM message_reads), 0),
    COALESCE((SELECT MAX(msg_id) FROM reports), 0)
);

-- "Everything this person wrote" (erase, purge) and "their DM
-- conversations as the second party" each scanned the whole table.
CREATE INDEX messages_user ON messages(user_id);
CREATE INDEX conversations_user_b ON conversations(user_b) WHERE kind = 1;
