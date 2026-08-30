//! SQLite-backed storage with refinery migrations.

use std::path::Path;

use rusqlite::Connection;

use crate::error::DataError;
use crate::item::{Item, ItemKind};

refinery::embed_migrations!("./migrations");

/// Schema version this build expects, derived from the embedded migrations
/// so that adding `V002__*.sql` cannot desync it from the runner. A
/// hand-maintained constant that someone forgets to bump would reject every
/// user's database as [`DataError::CorruptSchema`] on the next launch.
fn expected_schema_version() -> u32 {
    migrations::runner()
        .get_migrations()
        .iter()
        .map(|m| u32::try_from(m.version()).unwrap_or(0))
        .max()
        .unwrap_or(0)
}

#[derive(Debug)]
pub struct Database {
    conn: Connection,
}

impl Database {
    /// Open (and create if necessary) the database at `path`. When `read_only`
    /// is true and the file doesn't exist, returns `DataError::NotFound`.
    ///
    /// Note this touches the filesystem (`create_dir_all`) despite the
    /// crate's "no I/O beyond SQLite" rule. It is a deliberate convenience
    /// so a caller cannot get a confusing SQLite "unable to open" for a
    /// missing directory; the app layer creates the same directory first.
    pub fn open(path: &Path, read_only: bool) -> Result<Self, DataError> {
        if read_only && !path.exists() {
            return Err(DataError::NotFound(path.to_path_buf()));
        }
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }

        let flags = if read_only {
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
        } else {
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_CREATE
        };
        let mut conn = Connection::open_with_flags(path, flags)?;

        // Guard *before* running migrations. A database written by a newer
        // build must be refused with `CorruptSchema`; handing it to the
        // runner instead produces refinery's `MissingVersion`, whose
        // message ("migration V002__x is missing from the filesystem")
        // describes the cause backwards from the user's point of view.
        let expected = expected_schema_version();
        Self::check_version(Self::read_schema_version(&conn)?, expected)?;

        // Apply migrations (no-op on reopen). Skipped in read-only mode
        // because the migration table would need to be writable.
        if !read_only {
            migrations::runner().run(&mut conn).map_err(|e| {
                // Refinery raises `MissingVersion` for two opposite
                // situations, and they must not report the same way:
                //
                //  * an APPLIED migration whose file this build does not
                //    have — a database from a newer version. Belt and
                //    braces behind the pre-check above, and genuinely a
                //    corrupt-schema condition from our side.
                //  * a migration ON DISK, at or below the recorded
                //    version, that was never applied — someone
                //    back-ported a `V00x`. That is a packaging mistake,
                //    not a future database, and reporting it as
                //    "corrupt schema (found None, expected 1)" sends the
                //    reader looking in exactly the wrong place.
                match e.kind() {
                    refinery::error::Kind::MissingVersion(m)
                        if u32::try_from(m.version()).unwrap_or(u32::MAX) > expected =>
                    {
                        // `found` now carries the offending version
                        // instead of `None`, so the message names the
                        // thing that is wrong.
                        DataError::CorruptSchema {
                            found: u32::try_from(m.version()).ok(),
                            expected,
                        }
                    }
                    _ => DataError::from(e),
                }
            })?;
        }

        let db = Self { conn };
        // Deliberately re-checked after the runner: on the write path the
        // migrations just ran, so this reads the post-migration version
        // rather than the one the pre-check saw. On the read-only path it
        // is redundant, and harmlessly so.
        db.ensure_schema_current()?;
        Ok(db)
    }

    /// Refuse to use a database whose refinery version is newer than this
    /// build — an older binary reading a newer schema would silently
    /// misinterpret rows.
    fn ensure_schema_current(&self) -> Result<(), DataError> {
        Self::check_version(self.schema_version()?, expected_schema_version())
    }

    fn check_version(found: Option<u32>, expected: u32) -> Result<(), DataError> {
        if let Some(found) = found
            && found > expected
        {
            return Err(DataError::CorruptSchema {
                found: Some(found),
                expected,
            });
        }
        Ok(())
    }

    /// The schema version recorded by refinery, or `None` when the
    /// migration table is absent (a fresh file, or a read-only open that
    /// skipped the runner) or present but empty.
    pub fn schema_version(&self) -> Result<Option<u32>, DataError> {
        Self::read_schema_version(&self.conn)
    }

    fn read_schema_version(conn: &Connection) -> Result<Option<u32>, DataError> {
        use rusqlite::OptionalExtension;
        // A missing table is a `SqliteFailure`, not `QueryReturnedNoRows`,
        // so `.optional()` on the MAX() query alone would propagate it
        // rather than reporting `None`. Check existence explicitly.
        let has_table = conn
            .query_row(
                "SELECT 1 FROM sqlite_master \
                 WHERE type = 'table' AND name = 'refinery_schema_history'",
                [],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if !has_table {
            return Ok(None);
        }

        // `MAX()` over an empty table returns one row holding NULL, which a
        // direct `get::<_, i64>` would reject as InvalidColumnType. Read
        // through `Option` so "table present but empty" reports `None`.
        let row: Option<i64> = conn.query_row(
            "SELECT MAX(version) FROM refinery_schema_history",
            [],
            |r| r.get(0),
        )?;
        // A version too large for u32 is by definition ahead of this build;
        // saturate rather than truncating (`4294967297 as u32 == 1` would
        // sail straight past the guard).
        Ok(row.map(|v| u32::try_from(v).unwrap_or(u32::MAX)))
    }

    /// Insert a new item. On success, fills in `item.id`, `item.order_index`
    /// (if it was -1, it is auto-assigned to the next slot among siblings),
    /// and refreshes `item.created_at` / `item.updated_at` to match what was
    /// stored.
    pub fn insert(&self, item: &mut Item) -> Result<(), DataError> {
        // Validate the parent here as well as in `update`. Without it the
        // invariant was owned on two of the three write paths, and the
        // gap was reachable from an ordinary UI race: the GUI takes the
        // parent id from an asynchronously-refreshed tree model, so a
        // folder deleted between the refresh and the click yields a row
        // whose parent no longer exists — invisible (the flattener
        // descends from the root and never reaches it), un-deletable
        // (`remove_subtree` on any live ancestor cannot reach it either),
        // and, now that `update` validates, un-editable too. A stale
        // click must not be able to mint a permanent orphan.
        //
        // A row that does not exist yet has no descendants, so only the
        // sentinel and existence checks apply.
        self.validate_new_parent(item.parent_id)?;
        // Item doc comment: "order_index == -1 means append on insert."
        // The append slot is computed inside the INSERT via a scalar
        // subquery, so read and write are one atomic statement — no separate
        // SELECT round trip and no window between MAX() and INSERT. The
        // subquery's minimum is 0 (COALESCE(NULL, -1) + 1).
        const SQL: &str = "\
            INSERT INTO items \
                (parent_id, type, title, body_plain, body_rtf, comment, order_index, created_at, updated_at) \
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, \
                CASE WHEN ?7 < 0 THEN \
                    (SELECT COALESCE(MAX(order_index), -1) + 1 FROM items WHERE parent_id = ?1) \
                ELSE ?7 END, \
                ?8, ?9) \
            RETURNING id, order_index, created_at, updated_at";

        // rusqlite's `chrono` feature turns `DateTime<Utc>` into a TEXT
        // parameter (and back) natively, so timestamps cross the SQL
        // boundary as typed values end to end.
        let now = chrono::Utc::now();
        let rtf: &str = item.body_rtf.as_deref().unwrap_or("");

        let (id, order_index, created_at, updated_at): (
            i64,
            i32,
            chrono::DateTime<chrono::Utc>,
            chrono::DateTime<chrono::Utc>,
        ) = self.conn.query_row(
            SQL,
            rusqlite::params![
                item.parent_id,
                item.kind.as_i64(),
                item.title,
                item.body_plain,
                rtf,
                item.comment,
                item.order_index,
                now,
                now,
            ],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;

        item.id = Some(id);
        item.order_index = order_index;
        item.created_at = created_at;
        item.updated_at = updated_at;
        Ok(())
    }

    const LOAD_ALL_SQL: &'static str = "\
        SELECT id, parent_id, type, title, body_plain, body_rtf, comment, order_index, created_at, updated_at \
        FROM items \
        ORDER BY parent_id, order_index, id";

    /// Load every item, ordered by parent_id then order_index (stable).
    ///
    /// Only real persisted rows are returned — the virtual Clipboard
    /// History folder is added at the app layer, so no negative ids appear
    /// here.
    ///
    /// Strict: a single unreadable row fails the whole load. Callers that
    /// render the result to a user should prefer [`Self::load_all_lenient`],
    /// so one corrupt row cannot present as "all your snippets are gone".
    pub fn load_all(&self) -> Result<Vec<Item>, DataError> {
        let mut stmt = self.conn.prepare(Self::LOAD_ALL_SQL)?;
        let rows = stmt.query_map([], row_to_item)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Like [`Self::load_all`], but skips rows that cannot be decoded
    /// instead of failing the load, returning `(items, skipped)`.
    ///
    /// An unknown `type` discriminant or an unparseable timestamp affects
    /// one row; dropping the user's entire library because of it is the
    /// worse failure. The caller is expected to surface a non-zero
    /// `skipped` rather than ignore it.
    pub fn load_all_lenient(&self) -> Result<(Vec<Item>, usize), DataError> {
        let mut stmt = self.conn.prepare(Self::LOAD_ALL_SQL)?;
        let rows = stmt.query_map([], row_to_item)?;
        let mut out = Vec::new();
        let mut skipped = 0usize;
        for r in rows {
            match r {
                Ok(item) => out.push(item),
                Err(_) => skipped += 1,
            }
        }
        Ok((out, skipped))
    }

    /// Update an existing item's mutable fields. `id` must be `Some`.
    ///
    /// `parent_id` is validated: this is the only layer that can enforce
    /// the tree invariant, and a cycle here would make `remove_subtree`
    /// and the GUI's flattener recurse without end.
    pub fn update(&self, item: &Item) -> Result<(), DataError> {
        let id = item.id.ok_or(DataError::MissingId)?;
        self.validate_parent(id, item.parent_id)?;
        const SQL: &str = "\
            UPDATE items SET \
                parent_id = ?2, type = ?3, title = ?4, body_plain = ?5, \
                body_rtf = ?6, comment = ?7, order_index = ?8, updated_at = ?9 \
            WHERE id = ?1";
        let updated_at = chrono::Utc::now();
        let rtf: &str = item.body_rtf.as_deref().unwrap_or("");
        let changed = self.conn.execute(
            SQL,
            rusqlite::params![
                id,
                item.parent_id,
                item.kind.as_i64(),
                item.title,
                item.body_plain,
                rtf,
                item.comment,
                item.order_index,
                updated_at,
            ],
        )?;
        // Distinguish "wrote it" from "nothing matched" — editing an item
        // another path already deleted must not report success.
        if changed == 0 {
            return Err(DataError::ItemNotFound(id));
        }
        Ok(())
    }

    /// Reparent `id` under `new_parent` (`0` = top level), validating the
    /// tree invariant. The item keeps its `order_index`, so the caller
    /// should treat the destination order as unspecified.
    pub fn move_to_parent(&self, id: i64, new_parent: i64) -> Result<(), DataError> {
        self.validate_parent(id, new_parent)?;
        let changed = self.conn.execute(
            "UPDATE items SET parent_id = ?2, updated_at = ?3 WHERE id = ?1",
            rusqlite::params![id, new_parent, chrono::Utc::now()],
        )?;
        if changed == 0 {
            return Err(DataError::ItemNotFound(id));
        }
        Ok(())
    }

    /// Reject a `parent_id` for a row that does not exist yet: a negative
    /// sentinel, or a row that is not there. `0` is the invisible root and
    /// is always valid.
    ///
    /// Deliberately does NOT run the descendant check. `0` is the root
    /// sentinel *and* the `parent_id` every top-level row carries, so
    /// asking "is this parent a descendant of 0?" is true for the entire
    /// tree — passing a placeholder id into the full check would reject
    /// every legitimate insert.
    fn validate_new_parent(&self, parent_id: i64) -> Result<(), DataError> {
        if parent_id == 0 {
            return Ok(());
        }
        let invalid = DataError::InvalidParent { id: 0, parent_id };
        if parent_id < 0 {
            return Err(invalid);
        }
        use rusqlite::OptionalExtension;
        let exists = self
            .conn
            .query_row("SELECT 1 FROM items WHERE id = ?1", [parent_id], |_| Ok(()))
            .optional()?
            .is_some();
        if exists { Ok(()) } else { Err(invalid) }
    }

    /// Reject a `parent_id` that would break the tree invariant for an
    /// existing row: the item itself, one of its own descendants, a
    /// negative sentinel, or a row that does not exist. `0` is the
    /// invisible root and always valid.
    fn validate_parent(&self, id: i64, parent_id: i64) -> Result<(), DataError> {
        if parent_id == 0 {
            return Ok(());
        }
        if parent_id == id {
            return Err(DataError::InvalidParent { id, parent_id });
        }
        self.validate_new_parent(parent_id)
            .map_err(|_| DataError::InvalidParent { id, parent_id })?;
        if self.is_descendant_of(parent_id, id)? {
            return Err(DataError::InvalidParent { id, parent_id });
        }
        Ok(())
    }

    /// Is `candidate` inside the subtree rooted at `ancestor`?
    ///
    /// Walks parent links upward from `candidate`. `UNION` (not `UNION
    /// ALL`) so a cycle already present in the stored data terminates
    /// instead of looping forever.
    fn is_descendant_of(&self, candidate: i64, ancestor: i64) -> Result<bool, DataError> {
        use rusqlite::OptionalExtension;
        const SQL: &str = "\
            WITH RECURSIVE ancestors(id) AS (\
                SELECT parent_id FROM items WHERE id = ?1 \
                UNION \
                SELECT i.parent_id FROM items i JOIN ancestors a ON i.id = a.id \
            ) \
            SELECT 1 FROM ancestors WHERE id = ?2 LIMIT 1";
        Ok(self
            .conn
            .query_row(SQL, rusqlite::params![candidate, ancestor], |_| Ok(()))
            .optional()?
            .is_some())
    }

    /// Delete `id` and all its descendants recursively. Uses a recursive CTE
    /// to collect all descendant ids, then deletes them in one statement.
    pub fn remove_subtree(&self, id: i64) -> Result<(), DataError> {
        // Recursive CTE: start from `id`, traverse children via parent_id.
        // SQLite supports WITH RECURSIVE since 3.8.3 (2014); bundled rusqlite
        // ships a much newer version.
        //
        // `UNION`, never `UNION ALL`: UNION ALL does not deduplicate, so a
        // `parent_id` cycle makes the recursion non-terminating — this runs
        // on the UI thread holding the connection mutex, so the symptom
        // would be a permanently frozen app with memory climbing. `update`
        // now refuses to create such a cycle, but a hand-edited database
        // must not be able to hang the process either.
        const SQL: &str = "\
            WITH RECURSIVE descendants(id) AS (\
                SELECT id FROM items WHERE id = ?1 \
                UNION \
                SELECT i.id FROM items i JOIN descendants d ON i.parent_id = d.id \
            ) \
            DELETE FROM items WHERE id IN descendants";
        self.conn.execute(SQL, [id])?;
        Ok(())
    }

    /// Fetch a single item by id. Returns None if not found.
    pub fn get(&self, id: i64) -> Result<Option<Item>, DataError> {
        use rusqlite::OptionalExtension;
        const SQL: &str = "\
            SELECT id, parent_id, type, title, body_plain, body_rtf, comment, order_index, created_at, updated_at \
            FROM items WHERE id = ?1";
        let item = self.conn.query_row(SQL, [id], row_to_item).optional()?;
        Ok(item)
    }

    /// Swap this item's order_index with the sibling immediately above it
    /// (lower order_index, same parent). No-op if already first.
    pub fn move_up(&self, id: i64) -> Result<(), DataError> {
        self.swap_with_adjacent(id, /*up=*/ true)
    }

    /// Swap this item's order_index with the sibling immediately below it
    /// (higher order_index, same parent). No-op if already last.
    pub fn move_down(&self, id: i64) -> Result<(), DataError> {
        self.swap_with_adjacent(id, /*up=*/ false)
    }

    fn swap_with_adjacent(&self, id: i64, up: bool) -> Result<(), DataError> {
        use rusqlite::OptionalExtension;
        // unchecked_transaction (vs `transaction`) avoids requiring `&mut
        // self`, matching the `&self` signatures of `move_up`/`move_down`.
        // `tx` is the only handle on the connection within this function and
        // is consumed by `commit()` before we return, so no aliasing occurs.
        //
        // Do NOT call this (or anything reaching it) from inside another
        // transaction: `unchecked_transaction` cannot detect the nesting and
        // the inner `commit()` would end the outer one.
        let tx = self.conn.unchecked_transaction()?;

        // Load the item we're moving. A row that vanished (the tree refresh
        // is asynchronous, so a click can land on a stale row) is a no-op,
        // not a `QueryReturnedNoRows` surfaced as a database fault.
        let Some((parent_id, _)): Option<(i64, i32)> = tx
            .query_row(
                "SELECT parent_id, order_index FROM items WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
        else {
            return Ok(());
        };

        // Positions are only unique if every writer maintained them, and
        // `insert`/`update` both accept an explicit `order_index`. Two
        // siblings sharing a position are never each other's strict
        // neighbour, so a move would jump over one and the pair could never
        // be separated. Renumber this parent's children to a contiguous
        // 0..n-1 in exactly the order `load_all` presents them first.
        Self::normalize_siblings(&tx, parent_id)?;
        let order_index: i32 =
            tx.query_row("SELECT order_index FROM items WHERE id = ?1", [id], |r| {
                r.get(0)
            })?;

        // Find the adjacent sibling.
        let adjacent: Option<(i64, i32)> = if up {
            tx.query_row(
                "SELECT id, order_index FROM items \
                 WHERE parent_id = ?1 AND order_index < ?2 \
                 ORDER BY order_index DESC LIMIT 1",
                rusqlite::params![parent_id, order_index],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
        } else {
            tx.query_row(
                "SELECT id, order_index FROM items \
                 WHERE parent_id = ?1 AND order_index > ?2 \
                 ORDER BY order_index ASC LIMIT 1",
                rusqlite::params![parent_id, order_index],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
        };

        let Some((adj_id, adj_order)) = adjacent else {
            return Ok(()); // No adjacent sibling — no-op.
        };

        // Swap order_index values.
        tx.execute(
            "UPDATE items SET order_index = ?1 WHERE id = ?2",
            rusqlite::params![adj_order, id],
        )?;
        tx.execute(
            "UPDATE items SET order_index = ?1 WHERE id = ?2",
            rusqlite::params![order_index, adj_id],
        )?;

        tx.commit()?;
        Ok(())
    }

    /// Renumber one parent's children to contiguous `0..n-1`, preserving
    /// the order `load_all` reports (`order_index`, then `id` as the
    /// tie-break). Idempotent.
    fn normalize_siblings(tx: &rusqlite::Transaction<'_>, parent_id: i64) -> Result<(), DataError> {
        let ids: Vec<i64> = {
            let mut stmt =
                tx.prepare("SELECT id FROM items WHERE parent_id = ?1 ORDER BY order_index, id")?;
            let rows = stmt.query_map([parent_id], |r| r.get(0))?;
            rows.collect::<Result<_, _>>()?
        };
        for (pos, row_id) in ids.iter().enumerate() {
            tx.execute(
                "UPDATE items SET order_index = ?1 WHERE id = ?2 AND order_index <> ?1",
                rusqlite::params![pos as i32, row_id],
            )?;
        }
        Ok(())
    }
}

/// Map a rusqlite row (columns in `load_all` order) into an `Item`.
fn row_to_item(row: &rusqlite::Row<'_>) -> rusqlite::Result<Item> {
    let id: i64 = row.get(0)?;
    let parent_id: i64 = row.get(1)?;
    let type_int: i64 = row.get(2)?;
    let title: String = row.get(3)?;
    let body_plain: String = row.get(4)?;
    let body_rtf: String = row.get(5)?;
    let comment: String = row.get(6)?;
    let order_index: i32 = row.get(7)?;
    // Typed read: rusqlite's `chrono` FromSql accepts both the 'T'-separated
    // RFC3339 strings written by older builds and the space-separated form
    // it writes itself, so pre-existing databases keep working. An
    // unparseable timestamp now surfaces as an error instead of being
    // silently replaced with "now" (which masked corruption).
    let created_at: chrono::DateTime<chrono::Utc> = row.get(8)?;
    let updated_at: chrono::DateTime<chrono::Utc> = row.get(9)?;

    let kind = ItemKind::from_i64(type_int).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            2,
            rusqlite::types::Type::Integer,
            Box::<dyn std::error::Error + Send + Sync>::from(format!(
                "unknown ItemKind discriminant: {type_int}"
            )),
        )
    })?;

    Ok(Item {
        id: Some(id),
        parent_id,
        kind,
        title,
        body_plain,
        body_rtf: if body_rtf.is_empty() {
            None
        } else {
            Some(body_rtf)
        },
        comment,
        order_index,
        created_at,
        updated_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn open_creates_db_file_if_missing() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.sqlite");
        assert!(!path.exists());

        let db = Database::open(&path, false).expect("open must succeed");
        assert!(path.exists(), "DB file must be created on first open");
        assert_eq!(db.schema_version().unwrap(), Some(1));
    }

    #[test]
    fn open_read_only_fails_when_file_missing() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("missing.sqlite");
        let err = Database::open(&path, true).unwrap_err();
        assert!(
            matches!(err, DataError::NotFound(_)),
            "expected NotFound, got {err:?}"
        );
    }

    #[test]
    fn reopening_existing_db_does_not_reapply_migrations() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("persist.sqlite");

        {
            let db = Database::open(&path, false).unwrap();
            assert_eq!(db.schema_version().unwrap(), Some(1));
        } // db dropped, file closed

        let db = Database::open(&path, false).unwrap();
        assert_eq!(db.schema_version().unwrap(), Some(1));
    }

    #[test]
    fn insert_assigns_id_and_load_all_round_trips() {
        let dir = TempDir::new().unwrap();
        let db = Database::open(&dir.path().join("t.sqlite"), false).unwrap();

        let mut folder = Item::new_folder(0, "My folder");
        db.insert(&mut folder).unwrap();
        assert!(folder.id.is_some(), "insert must fill in the id");

        let mut item = Item::new_plain(folder.id.unwrap(), "Greeting", "Hello, world!");
        db.insert(&mut item).unwrap();
        assert!(item.id.is_some());

        let loaded = db.load_all().unwrap();
        assert_eq!(loaded.len(), 2);
        // load_all returns in stable id order; folder inserted first.
        assert_eq!(loaded[0].title, "My folder");
        assert_eq!(loaded[1].title, "Greeting");
        assert_eq!(loaded[1].body_plain, "Hello, world!");
        // Timestamps cross the SQL boundary as typed values — the
        // RETURNING read-back and the load_all re-read must agree.
        assert_eq!(loaded[1].created_at, item.created_at);
        assert_eq!(loaded[1].updated_at, item.updated_at);
    }

    /// Rows written by pre-`chrono`-feature builds stored `to_rfc3339()`
    /// strings ('T' separator, e.g. "2025-12-31T23:59:59.123+00:00").
    /// rusqlite's `DateTime<Utc>` FromSql accepts that form alongside its
    /// own space-separated one — this test pins the backward compatibility
    /// so existing user databases keep loading.
    #[test]
    fn reads_legacy_t_separated_timestamps() {
        let dir = TempDir::new().unwrap();
        let db = Database::open(&dir.path().join("legacy.sqlite"), false).unwrap();

        let ts = "2025-12-31T23:59:59.123456+00:00";
        db.conn
            .execute(
                "INSERT INTO items \
                    (parent_id, type, title, body_plain, body_rtf, comment, \
                     order_index, created_at, updated_at) \
                 VALUES (0, 0, 'legacy', '', '', '', 0, ?1, ?1)",
                rusqlite::params![ts],
            )
            .unwrap();

        let loaded = db.load_all().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(
            loaded[0].created_at.to_rfc3339(),
            "2025-12-31T23:59:59.123456+00:00",
            "legacy RFC3339 timestamp must parse back exactly"
        );
    }

    #[test]
    fn insert_preserves_unicode() {
        let dir = TempDir::new().unwrap();
        let db = Database::open(&dir.path().join("u.sqlite"), false).unwrap();

        let mut item = Item::new_plain(0, "привет", "Héllo — Wörld 🌍");
        db.insert(&mut item).unwrap();

        let loaded = db.load_all().unwrap();
        assert_eq!(loaded[0].title, "привет");
        assert_eq!(loaded[0].body_plain, "Héllo — Wörld 🌍");
    }

    #[test]
    fn update_changes_title_and_body() {
        let dir = TempDir::new().unwrap();
        let db = Database::open(&dir.path().join("upd.sqlite"), false).unwrap();

        let mut item = Item::new_plain(0, "old", "old body");
        db.insert(&mut item).unwrap();
        let id = item.id.unwrap();

        let mut edited = item.clone();
        edited.title = "new title".into();
        edited.body_plain = "new body".into();
        db.update(&edited).unwrap();

        let loaded = db.load_all().unwrap();
        let row = loaded.iter().find(|i| i.id == Some(id)).unwrap();
        assert_eq!(row.title, "new title");
        assert_eq!(row.body_plain, "new body");
    }

    #[test]
    fn insert_with_negative_order_index_appends_at_end() {
        let dir = TempDir::new().unwrap();
        let db = Database::open(&dir.path().join("ord.sqlite"), false).unwrap();

        let mut a = Item::new_plain(0, "a", "");
        let mut b = Item::new_plain(0, "b", "");
        let mut c = Item::new_plain(0, "c", "");
        db.insert(&mut a).unwrap();
        db.insert(&mut b).unwrap();
        db.insert(&mut c).unwrap();

        let loaded = db.load_all().unwrap();
        // order_index must be contiguous 0..n after auto-assign.
        let orders: Vec<i32> = loaded.iter().map(|i| i.order_index).collect();
        assert_eq!(orders, vec![0, 1, 2]);
    }

    #[test]
    fn read_only_db_rejects_insert() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("ro.sqlite");
        // Create first via RW
        {
            let db = Database::open(&path, false).unwrap();
            let mut item = Item::new_plain(0, "x", "y");
            db.insert(&mut item).unwrap();
        }
        // Reopen RO
        let db = Database::open(&path, true).unwrap();
        let mut item = Item::new_plain(0, "z", "w");
        let err = db.insert(&mut item).unwrap_err();
        assert!(
            matches!(err, DataError::Sqlite(_)),
            "expected Sqlite error on read-only insert, got {err:?}"
        );
    }

    #[test]
    fn remove_subtree_deletes_item_and_all_descendants() {
        let dir = TempDir::new().unwrap();
        let db = Database::open(&dir.path().join("rs.sqlite"), false).unwrap();

        // Build: root → folder1 → child_a, child_b; root → folder2
        let mut folder1 = Item::new_folder(0, "folder1");
        db.insert(&mut folder1).unwrap();
        let f1_id = folder1.id.unwrap();

        let mut folder2 = Item::new_folder(0, "folder2");
        db.insert(&mut folder2).unwrap();

        let mut child_a = Item::new_plain(f1_id, "child_a", "body_a");
        db.insert(&mut child_a).unwrap();
        let ca_id = child_a.id.unwrap();

        let mut child_b = Item::new_plain(f1_id, "child_b", "body_b");
        db.insert(&mut child_b).unwrap();

        // Delete folder1 → should remove folder1 + child_a + child_b
        db.remove_subtree(f1_id).unwrap();

        let remaining = db.load_all().unwrap();
        assert_eq!(remaining.len(), 1, "only folder2 should remain");
        assert_eq!(remaining[0].title, "folder2");
        assert!(db.get(ca_id).unwrap().is_none());
        assert!(db.get(f1_id).unwrap().is_none());
    }

    #[test]
    fn remove_subtree_on_leaf_deletes_only_that_item() {
        let dir = TempDir::new().unwrap();
        let db = Database::open(&dir.path().join("rl.sqlite"), false).unwrap();

        let mut a = Item::new_plain(0, "a", "");
        let mut b = Item::new_plain(0, "b", "");
        db.insert(&mut a).unwrap();
        db.insert(&mut b).unwrap();

        db.remove_subtree(a.id.unwrap()).unwrap();
        let remaining = db.load_all().unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].title, "b");
    }

    #[test]
    fn get_returns_item_by_id() {
        let dir = TempDir::new().unwrap();
        let db = Database::open(&dir.path().join("get.sqlite"), false).unwrap();

        let mut item = Item::new_plain(0, "test", "body");
        db.insert(&mut item).unwrap();
        let id = item.id.unwrap();

        let found = db.get(id).unwrap();
        assert!(found.is_some());
        assert_eq!(found.unwrap().title, "test");

        let missing = db.get(99999).unwrap();
        assert!(missing.is_none());
    }

    #[test]
    fn move_up_swaps_with_previous_sibling() {
        let dir = TempDir::new().unwrap();
        let db = Database::open(&dir.path().join("mu.sqlite"), false).unwrap();

        let mut a = Item::new_plain(0, "a", "");
        let mut b = Item::new_plain(0, "b", "");
        let mut c = Item::new_plain(0, "c", "");
        db.insert(&mut a).unwrap();
        db.insert(&mut b).unwrap();
        db.insert(&mut c).unwrap();
        // order: a=0, b=1, c=2

        db.move_up(b.id.unwrap()).unwrap();
        // order: b=0, a=1, c=2 (or b<1, a→1 via swap)

        let items = db.load_all().unwrap();
        assert_eq!(items[0].title, "b");
        assert_eq!(items[1].title, "a");
        assert_eq!(items[2].title, "c");
    }

    #[test]
    fn move_up_on_first_item_is_noop() {
        let dir = TempDir::new().unwrap();
        let db = Database::open(&dir.path().join("muf.sqlite"), false).unwrap();

        let mut a = Item::new_plain(0, "a", "");
        let mut b = Item::new_plain(0, "b", "");
        db.insert(&mut a).unwrap();
        db.insert(&mut b).unwrap();

        db.move_up(a.id.unwrap()).unwrap(); // a is already first
        let items = db.load_all().unwrap();
        assert_eq!(items[0].title, "a");
        assert_eq!(items[1].title, "b");
    }

    #[test]
    fn move_down_swaps_with_next_sibling() {
        let dir = TempDir::new().unwrap();
        let db = Database::open(&dir.path().join("md.sqlite"), false).unwrap();

        let mut a = Item::new_plain(0, "a", "");
        let mut b = Item::new_plain(0, "b", "");
        let mut c = Item::new_plain(0, "c", "");
        db.insert(&mut a).unwrap();
        db.insert(&mut b).unwrap();
        db.insert(&mut c).unwrap();

        db.move_down(a.id.unwrap()).unwrap();
        // a and b swap
        let items = db.load_all().unwrap();
        assert_eq!(items[0].title, "b");
        assert_eq!(items[1].title, "a");
        assert_eq!(items[2].title, "c");
    }

    #[test]
    fn move_down_on_last_item_is_noop() {
        let dir = TempDir::new().unwrap();
        let db = Database::open(&dir.path().join("mdl.sqlite"), false).unwrap();

        let mut a = Item::new_plain(0, "a", "");
        let mut b = Item::new_plain(0, "b", "");
        db.insert(&mut a).unwrap();
        db.insert(&mut b).unwrap();

        db.move_down(b.id.unwrap()).unwrap(); // b is already last
        let items = db.load_all().unwrap();
        assert_eq!(items[0].title, "a");
        assert_eq!(items[1].title, "b");
    }

    #[test]
    fn update_without_id_is_missing_id_error() {
        let dir = TempDir::new().unwrap();
        let db = Database::open(&dir.path().join("mid.sqlite"), false).unwrap();

        let mut item = Item::new_plain(0, "x", "");
        db.insert(&mut item).unwrap();
        let mut no_id = item.clone();
        no_id.id = None;

        let err = db.update(&no_id).unwrap_err();
        assert!(
            matches!(err, DataError::MissingId),
            "expected MissingId, got {err:?}"
        );
    }

    /// A DB written by a newer build (higher refinery version) must be
    /// rejected, not silently misread. We simulate the future version by
    /// editing the history table directly — refinery's runner would refuse
    /// the opposite direction only after our guard has long run.
    #[test]
    fn schema_ahead_of_build_is_rejected() {
        let dir = TempDir::new().unwrap();
        let db = Database::open(&dir.path().join("ahead.sqlite"), false).unwrap();
        db.ensure_schema_current()
            .expect("a freshly migrated DB must pass the version guard");

        db.conn
            .execute("UPDATE refinery_schema_history SET version = 99", [])
            .unwrap();
        let err = db.ensure_schema_current().unwrap_err();
        assert!(
            matches!(err, DataError::CorruptSchema { expected: 1, .. }),
            "expected CorruptSchema, got {err:?}"
        );
    }

    /// The production path: a DB carrying a future version must be refused
    /// by `open` itself, with `CorruptSchema` rather than refinery's
    /// "migration is missing from the filesystem".
    #[test]
    fn open_rejects_database_from_a_newer_build() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("future.sqlite");
        {
            let db = Database::open(&path, false).unwrap();
            db.conn
                .execute(
                    "INSERT INTO refinery_schema_history (version, name, applied_on, checksum) \
                     VALUES (99, 'future', '2030-01-01T00:00:00.000000000+00:00', '0')",
                    [],
                )
                .unwrap();
        }
        let err = Database::open(&path, false).unwrap_err();
        assert!(
            matches!(
                err,
                DataError::CorruptSchema {
                    found: Some(99),
                    expected: 1
                }
            ),
            "expected CorruptSchema{{found:99}}, got {err:?}"
        );
    }

    #[test]
    fn expected_schema_version_tracks_the_embedded_migrations() {
        // Guards against the constant this replaced: adding V002 without
        // bumping a hand-written number rejected every user's database.
        let highest = migrations::runner()
            .get_migrations()
            .iter()
            .map(|m| m.version())
            .max()
            .unwrap();
        assert_eq!(expected_schema_version(), highest as u32);
        // Pin the value independently of the derivation, so the test is
        // not merely a restatement of the function's own body.
        assert_eq!(expected_schema_version(), 1);
    }

    // ---- tree invariant -------------------------------------------------

    #[test]
    fn remove_subtree_deletes_three_levels_deep() {
        // The one-level test never exercises the CTE's recursive step past
        // a single hop.
        let dir = TempDir::new().unwrap();
        let db = Database::open(&dir.path().join("deep.sqlite"), false).unwrap();

        let mut l1 = Item::new_folder(0, "l1");
        db.insert(&mut l1).unwrap();
        let mut l2 = Item::new_folder(l1.id.unwrap(), "l2");
        db.insert(&mut l2).unwrap();
        let mut l3 = Item::new_folder(l2.id.unwrap(), "l3");
        db.insert(&mut l3).unwrap();
        let mut leaf = Item::new_plain(l3.id.unwrap(), "leaf", "body");
        db.insert(&mut leaf).unwrap();
        let mut sibling = Item::new_plain(0, "keep", "");
        db.insert(&mut sibling).unwrap();

        db.remove_subtree(l1.id.unwrap()).unwrap();

        let remaining = db.load_all().unwrap();
        assert_eq!(remaining.len(), 1, "only the untouched sibling remains");
        assert_eq!(remaining[0].title, "keep");
        assert!(db.get(leaf.id.unwrap()).unwrap().is_none());
    }

    #[test]
    fn update_rejects_self_parent() {
        let dir = TempDir::new().unwrap();
        let db = Database::open(&dir.path().join("selfp.sqlite"), false).unwrap();

        let mut f = Item::new_folder(0, "f");
        db.insert(&mut f).unwrap();
        f.parent_id = f.id.unwrap();

        let err = db.update(&f).unwrap_err();
        assert!(
            matches!(err, DataError::InvalidParent { .. }),
            "expected InvalidParent, got {err:?}"
        );
    }

    #[test]
    fn update_rejects_moving_a_folder_into_its_own_descendant() {
        let dir = TempDir::new().unwrap();
        let db = Database::open(&dir.path().join("cyc.sqlite"), false).unwrap();

        let mut parent = Item::new_folder(0, "parent");
        db.insert(&mut parent).unwrap();
        let mut child = Item::new_folder(parent.id.unwrap(), "child");
        db.insert(&mut child).unwrap();
        let mut grandchild = Item::new_folder(child.id.unwrap(), "grandchild");
        db.insert(&mut grandchild).unwrap();

        parent.parent_id = grandchild.id.unwrap();
        let err = db.update(&parent).unwrap_err();
        assert!(
            matches!(err, DataError::InvalidParent { .. }),
            "expected InvalidParent, got {err:?}"
        );
    }

    #[test]
    fn update_rejects_nonexistent_and_sentinel_parents() {
        let dir = TempDir::new().unwrap();
        let db = Database::open(&dir.path().join("orph.sqlite"), false).unwrap();

        let mut item = Item::new_plain(0, "x", "");
        db.insert(&mut item).unwrap();

        for bad in [9999_i64, crate::item::HISTORY_FOLDER_ID, -1] {
            let mut candidate = item.clone();
            candidate.parent_id = bad;
            let err = db.update(&candidate).unwrap_err();
            assert!(
                matches!(err, DataError::InvalidParent { .. }),
                "parent {bad} must be rejected, got {err:?}"
            );
        }
    }

    #[test]
    fn insert_rejects_a_nonexistent_parent() {
        // The UI race this guards: the tree model is refreshed
        // asynchronously, so the selected folder can be gone by the time
        // the click lands. Minting the row anyway makes a permanent,
        // invisible, un-deletable orphan.
        let dir = TempDir::new().unwrap();
        let db = Database::open(&dir.path().join("ins.sqlite"), false).unwrap();

        let mut orphan = Item::new_plain(4242, "orphan", "");
        let err = db.insert(&mut orphan).unwrap_err();
        assert!(
            matches!(
                err,
                DataError::InvalidParent {
                    parent_id: 4242,
                    ..
                }
            ),
            "expected InvalidParent, got {err:?}"
        );
        assert!(orphan.id.is_none(), "nothing must have been written");
        assert!(db.load_all().unwrap().is_empty());
    }

    #[test]
    fn insert_rejects_the_history_sentinel_as_a_parent() {
        let dir = TempDir::new().unwrap();
        let db = Database::open(&dir.path().join("insh.sqlite"), false).unwrap();
        let mut item = Item::new_plain(crate::item::HISTORY_FOLDER_ID, "x", "");
        assert!(matches!(
            db.insert(&mut item).unwrap_err(),
            DataError::InvalidParent { .. }
        ));
    }

    #[test]
    fn insert_still_accepts_the_root_and_a_real_folder() {
        let dir = TempDir::new().unwrap();
        let db = Database::open(&dir.path().join("insok.sqlite"), false).unwrap();

        let mut root_level = Item::new_plain(0, "root level", "");
        db.insert(&mut root_level).expect("parent 0 is the root");

        let mut folder = Item::new_folder(0, "folder");
        db.insert(&mut folder).unwrap();
        let mut child = Item::new_plain(folder.id.unwrap(), "child", "");
        db.insert(&mut child)
            .expect("a real folder is a valid parent");

        assert_eq!(db.load_all().unwrap().len(), 3);
    }

    #[test]
    fn move_down_separates_siblings_that_share_an_order_index() {
        // `move_up` has its own test; the two directions are separate SQL.
        let dir = TempDir::new().unwrap();
        let db = Database::open(&dir.path().join("dupd.sqlite"), false).unwrap();

        let mut a = Item::new_plain(0, "a", "");
        a.order_index = 0;
        let mut b = Item::new_plain(0, "b", "");
        b.order_index = 0;
        let mut c = Item::new_plain(0, "c", "");
        c.order_index = 0;
        db.insert(&mut a).unwrap();
        db.insert(&mut b).unwrap();
        db.insert(&mut c).unwrap();

        db.move_down(a.id.unwrap()).unwrap();
        let titles: Vec<String> = db
            .load_all()
            .unwrap()
            .into_iter()
            .map(|i| i.title)
            .collect();
        assert_eq!(titles, ["b", "a", "c"], "a must actually move past b");
    }

    #[test]
    fn move_to_parent_on_a_nonexistent_id_reports_not_found() {
        let dir = TempDir::new().unwrap();
        let db = Database::open(&dir.path().join("mtpx.sqlite"), false).unwrap();
        let mut folder = Item::new_folder(0, "f");
        db.insert(&mut folder).unwrap();

        let err = db.move_to_parent(4242, folder.id.unwrap()).unwrap_err();
        assert!(
            matches!(err, DataError::ItemNotFound(4242)),
            "expected ItemNotFound(4242), got {err:?}"
        );
    }

    /// A cycle that does NOT contain the deletion root — the harder case
    /// for the CTE than a cycle starting at the row being deleted.
    #[test]
    fn remove_subtree_terminates_on_a_cycle_below_the_root() {
        let dir = TempDir::new().unwrap();
        let db = Database::open(&dir.path().join("cyc2.sqlite"), false).unwrap();

        // root -> a -> b, then b <-> c hand-edited into a loop.
        let mut root = Item::new_folder(0, "root");
        db.insert(&mut root).unwrap();
        let mut a = Item::new_folder(root.id.unwrap(), "a");
        db.insert(&mut a).unwrap();
        let mut b = Item::new_folder(a.id.unwrap(), "b");
        db.insert(&mut b).unwrap();
        let mut c = Item::new_folder(b.id.unwrap(), "c");
        db.insert(&mut c).unwrap();
        db.conn
            .execute(
                "UPDATE items SET parent_id = ?1 WHERE id = ?2",
                rusqlite::params![c.id.unwrap(), b.id.unwrap()],
            )
            .unwrap();

        // The point is that this TERMINATES. Re-pointing b at c detaches
        // the b/c pair from the root, so they are correctly left behind —
        // orphaned by the hand edit, not by the delete.
        db.remove_subtree(root.id.unwrap()).unwrap();
        let left: Vec<String> = db
            .load_all()
            .unwrap()
            .into_iter()
            .map(|i| i.title)
            .collect();
        assert!(!left.contains(&"root".to_string()));
        assert!(!left.contains(&"a".to_string()));
        assert_eq!(left.len(), 2, "the detached cycle survives: {left:?}");

        // And deleting into the cycle itself also terminates.
        db.remove_subtree(b.id.unwrap()).unwrap();
        assert!(db.load_all().unwrap().is_empty());
    }

    #[test]
    fn update_accepts_a_legitimate_reparent() {
        let dir = TempDir::new().unwrap();
        let db = Database::open(&dir.path().join("repar.sqlite"), false).unwrap();

        let mut a = Item::new_folder(0, "a");
        let mut b = Item::new_folder(0, "b");
        db.insert(&mut a).unwrap();
        db.insert(&mut b).unwrap();
        let mut leaf = Item::new_plain(a.id.unwrap(), "leaf", "");
        db.insert(&mut leaf).unwrap();

        leaf.parent_id = b.id.unwrap();
        db.update(&leaf).unwrap();
        assert_eq!(
            db.get(leaf.id.unwrap()).unwrap().unwrap().parent_id,
            b.id.unwrap()
        );

        // And back to the root.
        db.move_to_parent(leaf.id.unwrap(), 0).unwrap();
        assert_eq!(db.get(leaf.id.unwrap()).unwrap().unwrap().parent_id, 0);
    }

    #[test]
    fn move_to_parent_refuses_to_build_a_cycle() {
        let dir = TempDir::new().unwrap();
        let db = Database::open(&dir.path().join("mtp.sqlite"), false).unwrap();

        let mut outer = Item::new_folder(0, "outer");
        db.insert(&mut outer).unwrap();
        let mut inner = Item::new_folder(outer.id.unwrap(), "inner");
        db.insert(&mut inner).unwrap();

        let err = db
            .move_to_parent(outer.id.unwrap(), inner.id.unwrap())
            .unwrap_err();
        assert!(matches!(err, DataError::InvalidParent { .. }));
    }

    /// A cycle can still reach the DB by hand-editing. `remove_subtree`
    /// must terminate rather than spin forever holding the connection.
    #[test]
    fn remove_subtree_terminates_on_a_hand_written_cycle() {
        let dir = TempDir::new().unwrap();
        let db = Database::open(&dir.path().join("hcyc.sqlite"), false).unwrap();

        let mut a = Item::new_folder(0, "a");
        db.insert(&mut a).unwrap();
        let mut b = Item::new_folder(a.id.unwrap(), "b");
        db.insert(&mut b).unwrap();
        // Bypass `update`'s validation the way a hand edit would.
        db.conn
            .execute(
                "UPDATE items SET parent_id = ?1 WHERE id = ?2",
                rusqlite::params![b.id.unwrap(), a.id.unwrap()],
            )
            .unwrap();

        db.remove_subtree(a.id.unwrap()).unwrap();
        assert!(
            db.load_all().unwrap().is_empty(),
            "the cycle is deleted whole"
        );
    }

    // ---- update semantics ----------------------------------------------

    #[test]
    fn update_of_a_deleted_item_reports_not_found() {
        let dir = TempDir::new().unwrap();
        let db = Database::open(&dir.path().join("gone.sqlite"), false).unwrap();

        let mut item = Item::new_plain(0, "x", "");
        db.insert(&mut item).unwrap();
        let id = item.id.unwrap();
        db.remove_subtree(id).unwrap();

        let err = db.update(&item).unwrap_err();
        assert!(
            matches!(err, DataError::ItemNotFound(n) if n == id),
            "expected ItemNotFound({id}), got {err:?}"
        );
    }

    // ---- ordering -------------------------------------------------------

    #[test]
    fn move_up_separates_siblings_that_share_an_order_index() {
        // Duplicate positions are reachable through an explicit
        // `order_index`; a strict-comparison neighbour search would jump
        // over the twin and never separate the pair.
        let dir = TempDir::new().unwrap();
        let db = Database::open(&dir.path().join("dup.sqlite"), false).unwrap();

        let mut a = Item::new_plain(0, "a", "");
        a.order_index = 0;
        let mut b = Item::new_plain(0, "b", "");
        b.order_index = 0;
        let mut c = Item::new_plain(0, "c", "");
        c.order_index = 0;
        db.insert(&mut a).unwrap();
        db.insert(&mut b).unwrap();
        db.insert(&mut c).unwrap();

        // load_all's tie-break is id, so the starting order is a, b, c.
        let titles: Vec<String> = db
            .load_all()
            .unwrap()
            .into_iter()
            .map(|i| i.title)
            .collect();
        assert_eq!(titles, ["a", "b", "c"]);

        db.move_up(c.id.unwrap()).unwrap();
        let titles: Vec<String> = db
            .load_all()
            .unwrap()
            .into_iter()
            .map(|i| i.title)
            .collect();
        assert_eq!(titles, ["a", "c", "b"], "c must actually move past b");

        // Positions are contiguous again after the normalising pass.
        let orders: Vec<i32> = db
            .load_all()
            .unwrap()
            .iter()
            .map(|i| i.order_index)
            .collect();
        assert_eq!(orders, [0, 1, 2]);
    }

    #[test]
    fn move_on_a_nonexistent_id_is_a_noop() {
        let dir = TempDir::new().unwrap();
        let db = Database::open(&dir.path().join("mnx.sqlite"), false).unwrap();
        let mut a = Item::new_plain(0, "a", "");
        db.insert(&mut a).unwrap();

        db.move_up(4242)
            .expect("a stale row id must not be an error");
        db.move_down(4242)
            .expect("a stale row id must not be an error");
        assert_eq!(db.load_all().unwrap().len(), 1);
    }

    #[test]
    fn insert_with_explicit_order_index_is_honoured() {
        let dir = TempDir::new().unwrap();
        let db = Database::open(&dir.path().join("exp.sqlite"), false).unwrap();

        let mut a = Item::new_plain(0, "a", "");
        a.order_index = 5;
        db.insert(&mut a).unwrap();
        assert_eq!(a.order_index, 5);

        // A subsequent append lands after it, not on top of it.
        let mut b = Item::new_plain(0, "b", "");
        db.insert(&mut b).unwrap();
        assert_eq!(b.order_index, 6);
    }

    // ---- corrupt rows ---------------------------------------------------

    fn insert_raw_row(db: &Database, type_int: i64, created: &str) {
        db.conn
            .execute(
                "INSERT INTO items \
                    (parent_id, type, title, body_plain, body_rtf, comment, \
                     order_index, created_at, updated_at) \
                 VALUES (0, ?1, 'bad', '', '', '', 0, ?2, ?2)",
                rusqlite::params![type_int, created],
            )
            .unwrap();
    }

    #[test]
    fn unknown_item_kind_discriminant_is_an_error_not_a_default() {
        let dir = TempDir::new().unwrap();
        let db = Database::open(&dir.path().join("kind.sqlite"), false).unwrap();
        insert_raw_row(&db, 99, "2025-01-01 00:00:00.000000+00:00");

        let err = db.load_all().unwrap_err();
        assert!(
            matches!(err, DataError::Sqlite(_)),
            "corruption must surface, got {err:?}"
        );
    }

    #[test]
    fn malformed_timestamp_is_an_error() {
        let dir = TempDir::new().unwrap();
        let db = Database::open(&dir.path().join("ts.sqlite"), false).unwrap();
        insert_raw_row(&db, 1, "not a timestamp");
        assert!(db.load_all().is_err());
    }

    #[test]
    fn load_all_lenient_skips_bad_rows_and_keeps_the_good_ones() {
        // The whole point: one unreadable row must not present to the user
        // as "every snippet is gone".
        let dir = TempDir::new().unwrap();
        let db = Database::open(&dir.path().join("len.sqlite"), false).unwrap();

        let mut good = Item::new_plain(0, "good", "keep me");
        db.insert(&mut good).unwrap();
        insert_raw_row(&db, 99, "2025-01-01 00:00:00.000000+00:00");
        insert_raw_row(&db, 1, "not a timestamp");
        let mut good2 = Item::new_plain(0, "good2", "");
        db.insert(&mut good2).unwrap();

        assert!(db.load_all().is_err(), "the strict load still fails");

        let (items, skipped) = db.load_all_lenient().unwrap();
        assert_eq!(skipped, 2);
        let titles: Vec<String> = items.into_iter().map(|i| i.title).collect();
        assert_eq!(titles, ["good", "good2"]);
    }

    #[test]
    fn load_all_lenient_reports_zero_skips_on_clean_data() {
        let dir = TempDir::new().unwrap();
        let db = Database::open(&dir.path().join("clean.sqlite"), false).unwrap();
        let mut a = Item::new_plain(0, "a", "");
        db.insert(&mut a).unwrap();

        let (items, skipped) = db.load_all_lenient().unwrap();
        assert_eq!(skipped, 0);
        assert_eq!(items.len(), 1);
    }

    // ---- payloads -------------------------------------------------------

    #[test]
    fn body_rtf_empty_string_round_trips_as_none() {
        // Pins the documented asymmetry so a future change to the column
        // has to update the doc comment too.
        let dir = TempDir::new().unwrap();
        let db = Database::open(&dir.path().join("rtf.sqlite"), false).unwrap();

        let mut item = Item::new_plain(0, "x", "");
        item.body_rtf = Some(String::new());
        db.insert(&mut item).unwrap();
        assert_eq!(db.get(item.id.unwrap()).unwrap().unwrap().body_rtf, None);

        let mut with_rtf = Item::new_plain(0, "y", "");
        with_rtf.body_rtf = Some("<b>hi</b>".into());
        db.insert(&mut with_rtf).unwrap();
        assert_eq!(
            db.get(with_rtf.id.unwrap()).unwrap().unwrap().body_rtf,
            Some("<b>hi</b>".to_string())
        );
    }

    #[test]
    fn large_payload_round_trips() {
        // The realistic clipboard case: a multi-megabyte capture.
        let dir = TempDir::new().unwrap();
        let db = Database::open(&dir.path().join("big.sqlite"), false).unwrap();

        let body = "x".repeat(4 * 1024 * 1024);
        let mut item = Item::new_plain(0, "big", body.clone());
        db.insert(&mut item).unwrap();

        let loaded = db.get(item.id.unwrap()).unwrap().unwrap();
        assert_eq!(loaded.body_plain.len(), body.len());
        assert_eq!(loaded.body_plain, body);
    }
}
