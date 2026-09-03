//! SQLCipher key application and whole-file conversion.
//!
//! Everything that knows a passphrase exists lives here. `database.rs`
//! calls [`open_keyed`] and otherwise stays ignorant of encryption.

use std::path::Path;

use rusqlite::{Connection, OptionalExtension};
use secrecy::{ExposeSecret, SecretString};

use crate::error::DataError;

/// Is this the "file header did not decrypt" error? SQLCipher reports a
/// wrong key exactly the same way it reports a file that was never a
/// database at all, because from its side those are the same observation.
pub(crate) fn is_not_a_database(e: &rusqlite::Error) -> bool {
    matches!(
        e,
        rusqlite::Error::SqliteFailure(inner, _)
            if inner.code == rusqlite::ErrorCode::NotADatabase
    )
}

/// Does this build link SQLCipher? A plain SQLite build answers
/// `PRAGMA cipher_version` with no row at all.
pub(crate) fn has_sqlcipher(conn: &Connection) -> bool {
    conn.query_row("PRAGMA cipher_version", [], |r| r.get::<_, String>(0))
        .optional()
        .ok()
        .flatten()
        .is_some_and(|v| !v.is_empty())
}

/// Touch the schema. This is the cheapest operation that forces SQLCipher
/// to decrypt page 1, which is what turns a wrong key into an error at
/// open time rather than midway through some unrelated query later.
pub(crate) fn probe_readable(conn: &Connection) -> Result<(), rusqlite::Error> {
    conn.query_row("SELECT count(*) FROM sqlite_schema", [], |r| {
        r.get::<_, i64>(0)
    })
    .map(|_| ())
}

/// Whether the database at `path` is encrypted, plaintext, or not there.
pub(crate) fn encryption_state(path: &Path) -> Result<EncryptionState, DataError> {
    if !path.exists() {
        return Ok(EncryptionState::Absent);
    }
    let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    match probe_readable(&conn) {
        Ok(()) => Ok(EncryptionState::Plaintext),
        Err(e) if is_not_a_database(&e) => {
            if has_sqlcipher(&conn) {
                Ok(EncryptionState::Encrypted)
            } else {
                Err(DataError::EncryptedButUnsupported)
            }
        }
        Err(e) => Err(DataError::from(e)),
    }
}

/// Whether a database file is readable as-is, needs a key, or is absent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncryptionState {
    /// No file yet — a new install.
    Absent,
    /// Readable with no key.
    Plaintext,
    /// Needs a passphrase.
    Encrypted,
}

/// Key `conn`, then prove the key is right.
///
/// `PRAGMA key` has to be the first statement on the connection —
/// SQLCipher will not accept it once anything else has touched the file.
/// It also succeeds unconditionally: a wrong passphrase is only revealed
/// by trying to read a page, which is what [`probe_readable`] is for.
/// Without that probe a bad passphrase surfaces much later as an opaque
/// `NotADatabase` from inside some unrelated query.
///
/// `pragma_update` binds the value rather than formatting it, so a
/// passphrase containing a quote cannot break out of the pragma.
pub(crate) fn apply_key(conn: &Connection, key: Option<&SecretString>) -> Result<(), DataError> {
    if let Some(key) = key {
        conn.pragma_update(None, "key", key.expose_secret())?;
    }
    match probe_readable(conn) {
        Ok(()) => Ok(()),
        Err(e) if is_not_a_database(&e) => {
            if has_sqlcipher(conn) {
                Err(DataError::WrongPassphrase)
            } else {
                Err(DataError::EncryptedButUnsupported)
            }
        }
        Err(e) => Err(DataError::from(e)),
    }
}

/// Open a connection at `path` and key it. Shared by [`crate::Database`]
/// and by the conversion helpers.
pub(crate) fn open_keyed(
    path: &Path,
    read_only: bool,
    key: Option<&SecretString>,
) -> Result<Connection, DataError> {
    let flags = if read_only {
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
    } else {
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_CREATE
    };
    let conn = Connection::open_with_flags(path, flags)?;
    apply_key(&conn, key)?;
    Ok(conn)
}

/// Where a conversion writes before it commits by rename. Beside the
/// original, so the rename stays on one filesystem and is therefore
/// atomic.
fn conversion_temp_path(path: &Path) -> std::path::PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".new");
    std::path::PathBuf::from(name)
}

fn item_count(conn: &Connection) -> Result<i64, DataError> {
    Ok(conn.query_row("SELECT count(*) FROM items", [], |r| r.get(0))?)
}

/// Copy every page of `conn`'s main schema into a fresh database at
/// `dest`, keyed with `key` (`None` writes plaintext).
///
/// `sqlcipher_export` is the only supported way to change a database's
/// encryption: SQLCipher cannot rewrite a file in place, because the
/// header itself is part of what changes. It copies the whole schema,
/// `refinery_schema_history` included, so migration state survives.
fn export_to(conn: &Connection, dest: &Path, key: Option<&SecretString>) -> Result<(), DataError> {
    // `to_string_lossy` would silently ATTACH at a *different*, mangled
    // path on a non-UTF-8 `dest` while `convert`'s verify-open and rename
    // still use the exact `Path` — the conversion then fails safe (the
    // rename target never got written), but leaves the lossy-named file
    // behind as an orphan `clean_orphaned_conversion` cannot find, because
    // it only ever looks for the exact `Path` with `.new` appended. For a
    // decrypt, that orphan is a complete plaintext copy of the library.
    // Fail before ATTACH ever runs instead.
    let dest_str = dest.to_str().ok_or_else(|| {
        DataError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("destination path is not valid UTF-8: {}", dest.display()),
        ))
    })?;
    // An empty key means "plaintext" to ATTACH — that is the documented
    // way back out, not an oversight.
    let key_str = key
        .map(|k| k.expose_secret().to_string())
        .unwrap_or_default();

    conn.execute(
        "ATTACH DATABASE ?1 AS target KEY ?2",
        rusqlite::params![dest_str, key_str],
    )?;
    let exported = conn.query_row("SELECT sqlcipher_export('target')", [], |_| Ok(()));
    // Detach whatever happened, so a failed export cannot leave the
    // connection holding the partial file open.
    let detached = conn.execute("DETACH DATABASE target", []);
    exported?;
    detached?;
    Ok(())
}

/// Best-effort durability before the rename that commits a conversion.
///
/// Opened for writing, not read-only: `sync_all` calls into Win32's
/// `FlushFileBuffers` on Windows, which requires a handle opened with
/// `GENERIC_WRITE`. A read-only handle fails there before ever reaching
/// the rename.
fn fsync_file(path: &Path) -> Result<(), DataError> {
    std::fs::OpenOptions::new()
        .write(true)
        .open(path)?
        .sync_all()?;
    Ok(())
}

/// Rewrite the database at `path` from key `from` to key `to`.
///
/// Never touches the original until the replacement has been opened under
/// its new key and confirmed to hold the same number of items. A crash at
/// any point before the rename leaves the original intact and an orphaned
/// `.new` beside it, which [`clean_orphaned_conversion`] removes. Every
/// failure from the point `tmp` starts existing *up to the rename* is
/// funnelled through one cleanup, so none of them can leave it behind
/// either — except a failed `std::fs::rename` itself, which sits outside
/// that closure by necessity (it is what commits the conversion) and so
/// is not covered by it; a `.new` left behind by that case is exactly
/// what `clean_orphaned_conversion` exists to pick up on the next launch.
///
/// `after_export`, if given, is called with `tmp`'s path immediately
/// after the export succeeds and before that file is reopened to verify
/// it. It exists only so a test can reach that one gap — everything else
/// in this function happens inside a single synchronous call with no
/// other place to interject — and every real caller below passes `None`.
///
/// Requires exclusive access to `path`: the caller must hold no open
/// connection to it. `path` must already exist — this converts a file
/// in place, it does not create one.
fn convert(
    path: &Path,
    from: Option<&SecretString>,
    to: Option<&SecretString>,
    after_export: Option<&dyn Fn(&Path)>,
) -> Result<(), DataError> {
    if !path.exists() {
        return Err(DataError::NotFound(path.to_path_buf()));
    }

    let tmp = conversion_temp_path(path);
    // A previous crash may have left one. It is not a backup of anything.
    let _ = std::fs::remove_file(&tmp);

    // One closure, one cleanup site: every fallible step below can `?`
    // out of it without duplicating the "remove tmp" step at each call
    // site, and none of them can therefore forget to.
    let outcome = (|| -> Result<(), DataError> {
        let expected = {
            // Not read-only: a connection opened read-only inherits that
            // mode for whatever it ATTACHes too, and ATTACH must create
            // `tmp`. Nothing here writes to `path` itself.
            let src = open_keyed(path, false, from)?;
            // `sqlcipher_export` rebuilds every index on `tmp`, and
            // SQLite's sorter can spill an in-progress sort to a file in
            // the system temp directory when the working set is large.
            // During an ENCRYPT that would be plaintext index data
            // written outside the data directory — precisely the
            // "plaintext lands in a temp file" leak the design doc cites
            // when it rejects the encrypted-container-decrypted-on-unlock
            // alternative. `temp_store` is not a cipher pragma (only
            // `key`, `rekey`, `cipher_version` are), so setting it here
            // does not violate the stock-SQLCipher-parameters rule.
            // 2 = MEMORY (0 = DEFAULT, 1 = FILE); an integer avoids any
            // ambiguity in how the keyword would otherwise be quoted.
            src.pragma_update(None, "temp_store", 2i64)?;
            let n = item_count(&src)?;
            export_to(&src, &tmp, to)?;
            if let Some(f) = after_export {
                f(&tmp);
            }
            n
        };

        let got = {
            let dst = open_keyed(&tmp, true, to)?;
            item_count(&dst)?
        };
        if got != expected {
            return Err(DataError::ConversionMismatch { expected, got });
        }

        fsync_file(&tmp)
    })();

    if let Err(e) = outcome {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }

    // Atomic: same directory, therefore same filesystem. This both
    // installs the new file and destroys the old one — deliberately. A
    // `.bak` here would preserve exactly the readable copy that
    // encrypting was meant to remove.
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Encrypt a plaintext database in place.
///
/// Requires exclusive access to `path`: the caller must hold no open
/// connection to it.
pub fn encrypt_database(path: &Path, passphrase: &SecretString) -> Result<(), DataError> {
    convert(path, None, Some(passphrase), None)
}

/// Decrypt an encrypted database back to plaintext in place.
///
/// Requires exclusive access to `path`: the caller must hold no open
/// connection to it.
pub fn decrypt_database(path: &Path, passphrase: &SecretString) -> Result<(), DataError> {
    convert(path, Some(passphrase), None, None)
}

/// Change the passphrase of an already-encrypted database.
///
/// Unlike encrypting and decrypting, this is genuinely in place:
/// `PRAGMA rekey` rewrites each page under the new key without an export.
///
/// Requires exclusive access to `path`: the caller must hold no open
/// connection to it. `path` must already exist — `open_keyed`'s
/// `SQLITE_OPEN_CREATE` flag would otherwise fabricate an empty database
/// and "rekey" that instead of reporting the passphrase can't be checked
/// against anything.
pub fn change_passphrase(
    path: &Path,
    current: &SecretString,
    new: &SecretString,
) -> Result<(), DataError> {
    if !path.exists() {
        return Err(DataError::NotFound(path.to_path_buf()));
    }
    let conn = open_keyed(path, false, Some(current))?;
    conn.pragma_update(None, "rekey", new.expose_secret())?;
    Ok(())
}

/// Delete the temporary file a crashed conversion may have left behind.
/// Safe to call at every launch; a missing file is not an error.
///
/// Requires exclusive access to `path`: the caller must hold no open
/// connection to it.
pub fn clean_orphaned_conversion(path: &Path) -> Result<(), DataError> {
    let tmp = conversion_temp_path(path);
    match std::fs::remove_file(&tmp) {
        Ok(()) => {
            tracing::warn!("removed an orphaned conversion file: {}", tmp.display());
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(DataError::from(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Database, EncryptionState};
    use tempfile::TempDir;

    #[test]
    fn encryption_state_reports_absent_and_plaintext() {
        let dir = TempDir::new().unwrap();

        let missing = dir.path().join("nope.sqlite");
        assert_eq!(
            Database::encryption_state(&missing).unwrap(),
            EncryptionState::Absent,
            "a file that is not there is a new install, not an error"
        );

        let plain = dir.path().join("plain.sqlite");
        drop(Database::open(&plain, false).unwrap());
        assert_eq!(
            Database::encryption_state(&plain).unwrap(),
            EncryptionState::Plaintext
        );
    }

    use crate::{DataError, Item};
    use secrecy::SecretString;

    fn pass(s: &str) -> SecretString {
        SecretString::from(s.to_string())
    }

    #[test]
    fn a_keyed_database_round_trips_with_the_same_passphrase() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("enc.sqlite");
        let p = pass("correct horse battery staple");

        {
            let db = Database::open_with_key(&path, false, Some(&p)).unwrap();
            let mut item = Item::new_plain(0, "Greeting", "Hello, world!");
            db.insert(&mut item).unwrap();
        }

        let db = Database::open_with_key(&path, false, Some(&p)).unwrap();
        let loaded = db.load_all().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].body_plain, "Hello, world!");
    }

    #[test]
    fn the_wrong_passphrase_is_reported_as_such() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("enc.sqlite");

        {
            let db = Database::open_with_key(&path, false, Some(&pass("right"))).unwrap();
            let mut item = Item::new_plain(0, "T", "B");
            db.insert(&mut item).unwrap();
        }

        let err = Database::open_with_key(&path, false, Some(&pass("wrong"))).unwrap_err();
        assert!(
            matches!(err, DataError::WrongPassphrase),
            "expected WrongPassphrase, got {err:?}"
        );
    }

    #[test]
    fn an_encrypted_database_will_not_open_unkeyed() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("enc.sqlite");

        {
            let db = Database::open_with_key(&path, false, Some(&pass("s3cret"))).unwrap();
            let mut item = Item::new_plain(0, "T", "B");
            db.insert(&mut item).unwrap();
        }

        let err = Database::open(&path, false).unwrap_err();
        assert!(
            matches!(err, DataError::WrongPassphrase),
            "expected WrongPassphrase, got {err:?}"
        );
    }

    #[test]
    fn encryption_state_detects_an_encrypted_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("enc.sqlite");
        {
            let db = Database::open_with_key(&path, false, Some(&pass("s3cret"))).unwrap();
            let mut item = Item::new_plain(0, "T", "B");
            db.insert(&mut item).unwrap();
        }
        assert_eq!(
            Database::encryption_state(&path).unwrap(),
            EncryptionState::Encrypted
        );
    }

    #[test]
    fn migrations_run_under_a_key() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("enc.sqlite");
        let db = Database::open_with_key(&path, false, Some(&pass("s3cret"))).unwrap();
        assert_eq!(db.schema_version().unwrap(), Some(1));
    }

    /// Seed a plaintext database with a folder and two snippets, and
    /// return the path. Used by the conversion tests.
    fn seeded(dir: &TempDir, name: &str) -> std::path::PathBuf {
        let path = dir.path().join(name);
        let db = Database::open(&path, false).unwrap();
        let mut folder = Item::new_folder(0, "Work");
        db.insert(&mut folder).unwrap();
        let mut a = Item::new_plain(folder.id.unwrap(), "Token", "abc123");
        db.insert(&mut a).unwrap();
        let mut b = Item::new_plain(0, "Address", "1 High St");
        db.insert(&mut b).unwrap();
        path
    }

    #[test]
    fn encrypting_preserves_every_item_and_the_migration_history() {
        let dir = TempDir::new().unwrap();
        let path = seeded(&dir, "lib.sqlite");
        let p = pass("s3cret");

        encrypt_database(&path, &p).unwrap();

        assert_eq!(
            Database::encryption_state(&path).unwrap(),
            EncryptionState::Encrypted
        );
        let db = Database::open_with_key(&path, false, Some(&p)).unwrap();
        let loaded = db.load_all().unwrap();
        assert_eq!(loaded.len(), 3);
        // `load_all` orders by (parent_id, order_index, id): the two
        // root items (Work, Address) sort before Token, which lives under
        // the Work folder's non-zero parent_id. Check every body rather
        // than one index, so a corrupted "Work" or "Address" with an
        // intact "Token" would still be caught.
        let bodies: std::collections::BTreeSet<_> =
            loaded.iter().map(|i| i.body_plain.as_str()).collect();
        assert_eq!(
            bodies,
            std::collections::BTreeSet::from(["", "abc123", "1 High St"])
        );
        assert_eq!(
            db.schema_version().unwrap(),
            Some(1),
            "sqlcipher_export must carry refinery_schema_history across"
        );
    }

    #[test]
    fn decrypting_restores_a_plaintext_file() {
        let dir = TempDir::new().unwrap();
        let path = seeded(&dir, "lib.sqlite");
        let p = pass("s3cret");

        encrypt_database(&path, &p).unwrap();
        decrypt_database(&path, &p).unwrap();

        assert_eq!(
            Database::encryption_state(&path).unwrap(),
            EncryptionState::Plaintext
        );
        let db = Database::open(&path, false).unwrap();
        assert_eq!(db.load_all().unwrap().len(), 3);
    }

    #[test]
    fn changing_the_passphrase_invalidates_the_old_one() {
        let dir = TempDir::new().unwrap();
        let path = seeded(&dir, "lib.sqlite");
        let old = pass("old one");
        let new = pass("new one");

        encrypt_database(&path, &old).unwrap();
        change_passphrase(&path, &old, &new).unwrap();

        let err = Database::open_with_key(&path, false, Some(&old)).unwrap_err();
        assert!(matches!(err, DataError::WrongPassphrase));

        let db = Database::open_with_key(&path, false, Some(&new)).unwrap();
        assert_eq!(db.load_all().unwrap().len(), 3);
    }

    #[test]
    fn changing_the_passphrase_needs_the_current_one() {
        let dir = TempDir::new().unwrap();
        let path = seeded(&dir, "lib.sqlite");
        encrypt_database(&path, &pass("right")).unwrap();

        let err = change_passphrase(&path, &pass("guess"), &pass("new")).unwrap_err();
        assert!(matches!(err, DataError::WrongPassphrase));
    }

    /// A passphrase containing a quote must not be able to break out of
    /// the pragma. `rusqlite::pragma_update` binds the value and doubles
    /// embedded quotes rather than interpolating it, so this pins that
    /// property against the whole encrypt/decrypt round trip rather than
    /// just trusting a reading of `rusqlite`'s source.
    #[test]
    fn a_passphrase_containing_a_quote_round_trips() {
        let dir = TempDir::new().unwrap();
        let path = seeded(&dir, "lib.sqlite");
        let p = pass("o'brien's \"secret\"");

        encrypt_database(&path, &p).unwrap();

        let db = Database::open_with_key(&path, false, Some(&p)).unwrap();
        assert_eq!(db.load_all().unwrap().len(), 3);

        let new = pass("d'artagnan");
        change_passphrase(&path, &p, &new).unwrap();

        let err = Database::open_with_key(&path, false, Some(&p)).unwrap_err();
        assert!(matches!(err, DataError::WrongPassphrase));
        let db = Database::open_with_key(&path, false, Some(&new)).unwrap();
        assert_eq!(db.load_all().unwrap().len(), 3);
    }

    /// A crash between the export and the rename leaves a `.new` file. It
    /// must never be mistaken for the real database, and the next launch
    /// clears it.
    #[test]
    fn an_orphaned_conversion_file_is_cleaned_up() {
        let dir = TempDir::new().unwrap();
        let path = seeded(&dir, "lib.sqlite");
        let orphan = path.with_extension("sqlite.new");
        std::fs::write(&orphan, b"junk").unwrap();

        clean_orphaned_conversion(&path).unwrap();

        assert!(!orphan.exists(), "the orphan must be gone");
        assert!(path.exists(), "the real database must be untouched");
        assert_eq!(
            Database::open(&path, false)
                .unwrap()
                .load_all()
                .unwrap()
                .len(),
            3
        );
    }

    /// The plaintext file this feature exists to eliminate must not
    /// survive as a backup.
    #[test]
    fn encrypting_leaves_no_plaintext_copy_behind() {
        let dir = TempDir::new().unwrap();
        let path = seeded(&dir, "lib.sqlite");
        encrypt_database(&path, &pass("s3cret")).unwrap();

        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .filter(|n| n != "lib.sqlite")
            .collect();
        assert!(
            leftovers.is_empty(),
            "no copy of the database may remain: {leftovers:?}"
        );
    }

    /// `convert()` (behind `encrypt_database`/`decrypt_database`) must
    /// refuse a path that doesn't exist rather than fabricate one:
    /// `open_keyed`'s writable-open flags include `SQLITE_OPEN_CREATE`,
    /// which would otherwise silently produce and "convert" an empty
    /// database, and `database.rs` already reserves this exact situation
    /// for `DataError::NotFound`.
    #[test]
    fn encrypting_a_missing_database_returns_not_found_and_creates_nothing() {
        let dir = TempDir::new().unwrap();
        let missing = dir.path().join("nope.sqlite");

        let err = encrypt_database(&missing, &pass("s3cret")).unwrap_err();
        assert!(
            matches!(err, DataError::NotFound(_)),
            "expected NotFound, got {err:?}"
        );
        assert!(!missing.exists(), "must not fabricate a database file");
        assert!(
            !conversion_temp_path(&missing).exists(),
            "must not leave a temp file behind either"
        );
    }

    #[test]
    fn decrypting_a_missing_database_returns_not_found() {
        let dir = TempDir::new().unwrap();
        let missing = dir.path().join("nope.sqlite");

        let err = decrypt_database(&missing, &pass("s3cret")).unwrap_err();
        assert!(
            matches!(err, DataError::NotFound(_)),
            "expected NotFound, got {err:?}"
        );
        assert!(!missing.exists());
    }

    /// `export_to` must reject a destination that cannot round-trip through
    /// UTF-8 rather than silently `ATTACH`ing at a `to_string_lossy`-mangled
    /// path — see the comment on `export_to` for why that orphan matters
    /// (a complete plaintext copy, on a decrypt, that the cleanup on the
    /// next launch cannot find because it looks for the exact path).
    #[cfg(unix)]
    #[test]
    fn export_to_rejects_a_non_utf8_destination() {
        use std::os::unix::ffi::OsStrExt;

        let dir = TempDir::new().unwrap();
        let path = seeded(&dir, "lib.sqlite");
        let conn = open_keyed(&path, false, None).unwrap();

        let bad_name = std::ffi::OsStr::from_bytes(b"bad-\xff-name.sqlite");
        let dest = dir.path().join(bad_name);

        let err = export_to(&conn, &dest, None).unwrap_err();
        assert!(
            matches!(err, DataError::Io(_)),
            "expected an Io error rejecting the non-UTF-8 path, got {err:?}"
        );
        assert!(
            !dest.exists(),
            "must fail before ATTACH ever creates anything"
        );
    }

    #[test]
    fn changing_the_passphrase_of_a_missing_database_returns_not_found() {
        let dir = TempDir::new().unwrap();
        let missing = dir.path().join("nope.sqlite");

        let err = change_passphrase(&missing, &pass("old"), &pass("new")).unwrap_err();
        assert!(
            matches!(err, DataError::NotFound(_)),
            "expected NotFound, got {err:?}"
        );
        assert!(!missing.exists(), "must not fabricate a database file");
    }

    /// The riskiest branch in `convert()`: something goes wrong after the
    /// export but before the temp file is trusted. This forces exactly
    /// that, via `convert`'s `after_export` test seam, and checks all
    /// three safety properties at once: the right error comes back, the
    /// temp file is gone, and the original is completely unharmed.
    #[test]
    fn a_corrupted_export_rolls_back_and_leaves_the_original_intact() {
        let dir = TempDir::new().unwrap();
        let path = seeded(&dir, "lib.sqlite");
        let p = pass("s3cret");

        let corrupt: &dyn Fn(&Path) = &|tmp: &Path| {
            std::fs::write(tmp, b"not a database").unwrap();
        };
        let result = convert(&path, None, Some(&p), Some(corrupt));

        let err = result.unwrap_err();
        assert!(
            matches!(err, DataError::WrongPassphrase),
            "corruption surfaces as this build's stand-in for \"not a \
             database\" when the verify-open fails; got {err:?}"
        );

        assert!(
            !conversion_temp_path(&path).exists(),
            "the corrupted temp file must not linger"
        );
        assert_eq!(
            Database::encryption_state(&path).unwrap(),
            EncryptionState::Plaintext,
            "the original must be untouched by a failed conversion"
        );
        let db = Database::open(&path, false).unwrap();
        assert_eq!(db.load_all().unwrap().len(), 3);
    }
}
