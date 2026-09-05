-- 1.1.0 step D: edits are kept in place; the flag tells clients.
ALTER TABLE messages ADD COLUMN edited_at INTEGER;
