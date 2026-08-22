-- Initial fastpaste schema. Mirrors the C++ schema shape but starts fresh
-- (no compatibility with existing C++ DBs — fresh DB on first launch).

CREATE TABLE items (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    parent_id   INTEGER NOT NULL DEFAULT 0,
    type        INTEGER NOT NULL,             -- 0=Folder, 1=Plain, 2=RichText, 3=Clip
    title       TEXT    NOT NULL DEFAULT '',
    body_plain  TEXT    NOT NULL DEFAULT '',
    body_rtf    TEXT    NOT NULL DEFAULT '',  -- HTML for RichText; empty otherwise
    comment     TEXT    NOT NULL DEFAULT '',
    order_index INTEGER NOT NULL DEFAULT -1,
    created_at  TEXT    NOT NULL,             -- ISO-8601 UTC with millis
    updated_at  TEXT    NOT NULL
) STRICT;

CREATE INDEX idx_items_parent ON items(parent_id);
CREATE INDEX idx_items_order  ON items(parent_id, order_index);
