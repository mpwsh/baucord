-- Columns nothing reads (review 2026-10, the dead-code list; decided
-- 2026-10-16): a profile lock that went to policy, a key label never
-- shown, two channel columns the channel list never used, the MIME an
-- uploader claimed (the sniffed one is what is served), and the whole
-- telemetry sample and event kept as blobs beside the columns the
-- reports read. A server older than this schema refuses the database:
-- going back means restoring a backup taken before.
ALTER TABLE users DROP COLUMN profile_locked;
ALTER TABLE user_keys DROP COLUMN label;
ALTER TABLE channels DROP COLUMN is_afk;
ALTER TABLE channels DROP COLUMN position;
ALTER TABLE blobs DROP COLUMN declared_mime;
ALTER TABLE telemetry_samples DROP COLUMN sample;
ALTER TABLE telemetry_events DROP COLUMN event;
