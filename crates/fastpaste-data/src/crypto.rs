//! SQLCipher key application and whole-file conversion.
//!
//! Everything that knows a passphrase exists lives here. `database.rs`
//! calls [`apply_key`] and otherwise stays ignorant of encryption.

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

#[cfg(test)]
mod tests {
    // `use super::*` matters: Task 5 adds tests here that call
    // `encrypt_database` and friends unqualified. Nothing in this module
    // uses it yet, hence the allow below — Task 5 removes it once it does.
    #[allow(unused_imports)]
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
}
