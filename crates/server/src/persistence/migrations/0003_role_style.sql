-- 1.1.3: a role wears a glyph and a color (spec §4). Built-ins keep
-- theirs from the client (guest ring, organizer wrench, admin shield).
ALTER TABLE roles ADD COLUMN icon TEXT NOT NULL DEFAULT '';
ALTER TABLE roles ADD COLUMN color INTEGER NOT NULL DEFAULT 0;
