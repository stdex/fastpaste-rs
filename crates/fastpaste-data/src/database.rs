//! SQLite-backed storage with refinery migrations.

use std::path::Path;

use rusqlite::Connection;

use crate::error::DataError;
use crate::item::{Item, ItemKind};

refinery::embed_migrations!("./migrations");

/// Current schema version this build expects. Bumped with each new migration.
const EXPECTED_SCHEMA_VERSION: u32 = 1;

#[derive(Debug)]
pub struct Database {
    conn: Connection,
}

impl Database {
    /// Open (and create if necessary) the database at `path`. When `read_only`
    /// is true and the file doesn't exist, returns `DataError::NotFound`.
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

        // Apply migrations (no-op on reopen). Skipped in read-only mode
        // because the migration table would need to be writable.
        if !read_only {
            migrations::runner().run(&mut conn)?;
        }

        let db = Self { conn };
        db.ensure_schema_current()?;
        Ok(db)
    }

    /// Refuse to use a database whose refinery version is newer than this
    /// build — an older binary reading a newer schema would silently
    /// misinterpret rows. A version older than expected can't occur right
    /// after `open` (the migration runner brings the DB exactly to
    /// [`EXPECTED_SCHEMA_VERSION`]); the guard exists for the future-newer
    /// case and keeps `CorruptSchema` honest.
    fn ensure_schema_current(&self) -> Result<(), DataError> {
        if let Some(found) = self.schema_version()?
            && found > EXPECTED_SCHEMA_VERSION
        {
            return Err(DataError::CorruptSchema {
                found: Some(found),
                expected: EXPECTED_SCHEMA_VERSION,
            });
        }
        Ok(())
    }

    /// Returns the schema_version stored by refinery, or None if the
    /// migration table is missing (shouldn't happen post-open).
    pub fn schema_version(&self) -> Result<Option<u32>, DataError> {
        use rusqlite::OptionalExtension;
        let row: Option<i64> = self
            .conn
            .query_row(
                "SELECT MAX(version) FROM refinery_schema_history",
                [],
                |r| r.get(0),
            )
            .optional()?;
        Ok(row.map(|v| v as u32))
    }

    /// Insert a new item. On success, fills in `item.id`, `item.order_index`
    /// (if it was -1, it is auto-assigned to the next slot among siblings),
    /// and refreshes `item.created_at` / `item.updated_at` to match what was
    /// stored.
    pub fn insert(&self, item: &mut Item) -> Result<(), DataError> {
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

    /// Load every item, ordered by parent_id then order_index (stable).
    /// Includes virtual rows with negative ids is NOT a thing — only real
    /// persisted rows are returned. The virtual Clipboard History folder
    /// is added at the app layer.
    pub fn load_all(&self) -> Result<Vec<Item>, DataError> {
        const SQL: &str = "\
            SELECT id, parent_id, type, title, body_plain, body_rtf, comment, order_index, created_at, updated_at \
            FROM items \
            ORDER BY parent_id, order_index, id";
        let mut stmt = self.conn.prepare(SQL)?;
        let rows = stmt.query_map([], row_to_item)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Update an existing item's mutable fields. `id` must be `Some`.
    pub fn update(&self, item: &Item) -> Result<(), DataError> {
        let id = item.id.ok_or(DataError::MissingId)?;
        const SQL: &str = "\
            UPDATE items SET \
                parent_id = ?2, type = ?3, title = ?4, body_plain = ?5, \
                body_rtf = ?6, comment = ?7, order_index = ?8, updated_at = ?9 \
            WHERE id = ?1";
        let updated_at = chrono::Utc::now();
        let rtf: &str = item.body_rtf.as_deref().unwrap_or("");
        self.conn.execute(
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
        Ok(())
    }

    /// Delete `id` and all its descendants recursively. Uses a recursive CTE
    /// to collect all descendant ids, then deletes them in one statement.
    pub fn remove_subtree(&self, id: i64) -> Result<(), DataError> {
        // Recursive CTE: start from `id`, traverse children via parent_id.
        // SQLite supports WITH RECURSIVE since 3.8.3 (2014); bundled rusqlite
        // ships a much newer version.
        const SQL: &str = "\
            WITH RECURSIVE descendants(id) AS (\
                SELECT id FROM items WHERE id = ?1 \
                UNION ALL \
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
        let tx = self.conn.unchecked_transaction()?;

        // Load the item we're moving.
        let (parent_id, order_index): (i64, i32) = tx.query_row(
            "SELECT parent_id, order_index FROM items WHERE id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;

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
        assert_eq!(db.ensure_schema_current().unwrap(), ());

        db.conn
            .execute("UPDATE refinery_schema_history SET version = 99", [])
            .unwrap();
        let err = db.ensure_schema_current().unwrap_err();
        assert!(
            matches!(err, DataError::CorruptSchema { expected: 1, .. }),
            "expected CorruptSchema, got {err:?}"
        );
    }
}
