//! SQLCipher key application and whole-file conversion.
//!
//! Everything that knows a passphrase exists lives here. `database.rs`
//! calls [`apply_key`] and otherwise stays ignorant of encryption.

use std::path::Path;

use rusqlite::{Connection, OptionalExtension};

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
}
