-- baucord 1.1 schema, migration 1 (spec §2).
--
-- Ids: `user_id` and `key_hash` are 32-byte blobs (SHA-256 of the raw
-- Ed25519 public key; a user's id is the hash of the key that created
-- it, permanent). Timestamps are milliseconds since the Unix epoch.
-- Enumerations are small integers; the Rust side owns their meaning.

CREATE TABLE users (
    user_id            BLOB PRIMARY KEY NOT NULL,
    name               TEXT NOT NULL,
    created_at         INTEGER NOT NULL,
    last_seen          INTEGER NOT NULL,
    invited_with       TEXT,
    name_color         INTEGER,
    name_style         INTEGER NOT NULL DEFAULT 0,   -- 0 normal, 1 bold, 2 italic
    avatar_blob        BLOB,
    profile_locked     INTEGER NOT NULL DEFAULT 0,
    profile_updated_at INTEGER
);

CREATE TABLE user_keys (
    key_hash   BLOB PRIMARY KEY NOT NULL,
    user_id    BLOB NOT NULL REFERENCES users(user_id) ON DELETE CASCADE,
    label      TEXT NOT NULL DEFAULT '',
    added_at   INTEGER NOT NULL,
    last_seen  INTEGER NOT NULL,
    retired_at INTEGER
);
CREATE INDEX user_keys_user ON user_keys(user_id);

CREATE TABLE roles (
    role_id     INTEGER PRIMARY KEY,
    name        TEXT NOT NULL UNIQUE,
    permissions INTEGER NOT NULL,
    rank        INTEGER NOT NULL,
    builtin     INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE user_roles (
    user_id    BLOB NOT NULL REFERENCES users(user_id) ON DELETE CASCADE,
    role_id    INTEGER NOT NULL REFERENCES roles(role_id) ON DELETE CASCADE,
    granted_by BLOB,
    granted_at INTEGER NOT NULL,
    PRIMARY KEY (user_id, role_id)
);

CREATE TABLE restrictions (
    id         INTEGER PRIMARY KEY,
    user_id    BLOB REFERENCES users(user_id) ON DELETE CASCADE,
    key_hash   BLOB,
    ip         TEXT,
    kind       INTEGER NOT NULL,   -- 0 server, 1 voice, 2 general_text, 3 dm, 4 upload, 5 share, 6 profile
    until      INTEGER,
    reason     TEXT NOT NULL DEFAULT '',
    by         BLOB,
    created_at INTEGER NOT NULL
);
CREATE INDEX restrictions_user ON restrictions(user_id);
CREATE INDEX restrictions_key ON restrictions(key_hash);

CREATE TABLE invites (
    code        TEXT PRIMARY KEY NOT NULL,
    created_by  BLOB REFERENCES users(user_id),   -- NULL: the bootstrap code or the CLI
    created_at  INTEGER NOT NULL,
    expires_at  INTEGER,
    max_uses    INTEGER,
    uses        INTEGER NOT NULL DEFAULT 0,
    grants_role INTEGER NOT NULL REFERENCES roles(role_id),
    revoked_at  INTEGER
);

CREATE TABLE channels (
    channel_id    INTEGER PRIMARY KEY,
    name          TEXT NOT NULL UNIQUE,
    password      TEXT,
    user_limit    INTEGER,
    min_role      INTEGER REFERENCES roles(role_id),
    permanent     INTEGER NOT NULL DEFAULT 0,
    afk_after_min INTEGER,
    is_afk        INTEGER NOT NULL DEFAULT 0,
    source        INTEGER NOT NULL,   -- 0 config (re-synced from server.yaml), 1 user
    created_by    BLOB,
    created_at    INTEGER NOT NULL,
    position      INTEGER NOT NULL DEFAULT 0,
    empty_since   INTEGER
);

CREATE TABLE conversations (
    conv_id INTEGER PRIMARY KEY,
    kind    INTEGER NOT NULL,   -- 0 general, 1 dm
    user_a  BLOB,
    user_b  BLOB                -- dm: user_a < user_b
);
CREATE UNIQUE INDEX conversations_dm ON conversations(user_a, user_b) WHERE kind = 1;
INSERT INTO conversations (conv_id, kind) VALUES (1, 0);

CREATE TABLE messages (
    msg_id     INTEGER PRIMARY KEY,   -- server-assigned from the in-memory counter, never AUTOINCREMENT
    conv_id    INTEGER NOT NULL REFERENCES conversations(conv_id),
    ts         INTEGER NOT NULL,
    user_id    BLOB NOT NULL,
    body       TEXT NOT NULL,
    blob_id    BLOB,
    deleted_at INTEGER
);
CREATE INDEX messages_conv ON messages(conv_id, msg_id);
CREATE INDEX messages_ts ON messages(ts);

CREATE TABLE message_reads (
    conv_id          INTEGER NOT NULL REFERENCES conversations(conv_id) ON DELETE CASCADE,
    user_id          BLOB NOT NULL,
    last_read_msg_id INTEGER NOT NULL,
    PRIMARY KEY (conv_id, user_id)
);

CREATE TABLE blobs (
    blob_id       BLOB PRIMARY KEY NOT NULL,   -- sha256 of the bytes on disk
    mime          TEXT NOT NULL,               -- what the server sniffed
    declared_mime TEXT,                        -- what the uploader claimed
    size          INTEGER NOT NULL,
    name          TEXT NOT NULL,
    created_by    BLOB,
    created_at    INTEGER NOT NULL,
    refs          INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX blobs_refs ON blobs(refs);

CREATE TABLE audit (
    id            INTEGER PRIMARY KEY,
    ts            INTEGER NOT NULL,
    actor_user_id BLOB,
    action        TEXT NOT NULL,
    target        TEXT,
    detail        TEXT
);

CREATE TABLE user_progress (
    user_id        BLOB PRIMARY KEY NOT NULL REFERENCES users(user_id) ON DELETE CASCADE,
    xp             INTEGER NOT NULL DEFAULT 0,
    talk_seconds   INTEGER NOT NULL DEFAULT 0,
    share_seconds  INTEGER NOT NULL DEFAULT 0,
    watch_seconds  INTEGER NOT NULL DEFAULT 0,
    online_seconds INTEGER NOT NULL DEFAULT 0,
    messages       INTEGER NOT NULL DEFAULT 0,
    updated_at     INTEGER NOT NULL
);

CREATE TABLE telemetry_samples (
    ts            INTEGER NOT NULL,
    user_id       BLOB NOT NULL,
    key_hash      BLOB NOT NULL,
    rtt_ms        INTEGER NOT NULL,
    path_loss     REAL NOT NULL,
    voice_loss    REAL,
    groups_behind INTEGER,
    rung          INTEGER,
    input_device  TEXT,
    output_device TEXT,
    sample        BLOB NOT NULL   -- the whole sample, protocol codec
);
CREATE INDEX telemetry_samples_user ON telemetry_samples(user_id, ts);

CREATE TABLE telemetry_events (
    ts       INTEGER NOT NULL,
    user_id  BLOB NOT NULL,
    key_hash BLOB NOT NULL,
    kind     TEXT NOT NULL,
    event    BLOB NOT NULL         -- the whole event, protocol codec
);
CREATE INDEX telemetry_events_user ON telemetry_events(user_id, ts);
