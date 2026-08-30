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
    -- Written by rusqlite's chrono ToSql as "%F %T%.f%:z": a
    -- SPACE-separated RFC 3339 timestamp whose fractional part is
    -- VARIABLE width — chrono's %.f emits 0, 3, 6 or 9 digits, whichever
    -- the value needs, and nothing at all when the fraction is exactly
    -- zero. So both "2025-12-31 23:59:59.123456789+00:00" and
    -- "2025-12-31 23:59:59+00:00" are valid stored forms; external
    -- tooling must not assume a fixed width. Its FromSql also accepts
    -- the 'T'-separated form, so rows from other writers still read back.
    created_at  TEXT    NOT NULL,
    updated_at  TEXT    NOT NULL
) STRICT;

CREATE INDEX idx_items_parent ON items(parent_id);
CREATE INDEX idx_items_order  ON items(parent_id, order_index);
