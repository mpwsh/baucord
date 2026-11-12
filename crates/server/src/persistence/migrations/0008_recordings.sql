-- baucord 1.5 schema, migration 8: voice channel recordings.
--
-- A recording is made by the server and belongs to the people who were
-- in the channel while it ran: `recording_access` is who may fetch its
-- file, and what each of them sees in their "Server" conversation. A
-- row is written for each person as they are recorded, so a crash
-- loses nobody; an unfinished recording (`blob_id` NULL) is listed to
-- no one until it is finished or repaired.

CREATE TABLE recordings (
    rec_id      INTEGER PRIMARY KEY,
    channel     TEXT NOT NULL,              -- the channel's name when it started
    started_at  INTEGER NOT NULL,           -- ms since the Unix epoch
    duration_ms INTEGER NOT NULL DEFAULT 0,
    started_by  BLOB,                       -- user_id
    starter     TEXT NOT NULL DEFAULT '',   -- their name at the time
    stopped_by  BLOB,                       -- user_id; NULL when the server stopped it
    stop_reason INTEGER,                    -- protocol::recording::RecordingStopReason; NULL while recording
    sample_rate INTEGER NOT NULL,
    size        INTEGER NOT NULL DEFAULT 0,
    blob_id     BLOB                        -- NULL until finished
);
CREATE INDEX recordings_blob ON recordings(blob_id) WHERE blob_id IS NOT NULL;

CREATE TABLE recording_access (
    rec_id  INTEGER NOT NULL REFERENCES recordings(rec_id) ON DELETE CASCADE,
    user_id BLOB NOT NULL REFERENCES users(user_id) ON DELETE CASCADE,
    seen    INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (rec_id, user_id)
);
CREATE INDEX recording_access_user ON recording_access(user_id);

-- The role that grants `record` (permission bit 24) on top of anyone's
-- other roles. A custom role: an admin may rename, restyle or delete
-- it. Rank 5 is what `derived_rank` gives that permission alone.
INSERT INTO roles (name, permissions, rank, builtin, icon, color)
SELECT 'recorder', 16777216, 5, 0, 'mic', 0
WHERE NOT EXISTS (SELECT 1 FROM roles WHERE name = 'recorder');
