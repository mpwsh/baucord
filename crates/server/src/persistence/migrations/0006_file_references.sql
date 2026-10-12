-- baucord 1.2.1 schema, migration 6: which messages point at a file.

-- A file's reference count is recounted at every start from the rows
-- that point at it; without this each file scanned every message.
CREATE INDEX messages_blob ON messages(blob_id) WHERE blob_id IS NOT NULL;
