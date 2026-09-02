//! Error type for the data layer.

use std::path::PathBuf;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum DataError {
    #[error("database not found: {0}")]
    NotFound(PathBuf),

    #[error("cannot update an item that has no id")]
    MissingId,

    #[error("no item with id {0}")]
    ItemNotFound(i64),

    /// A `parent_id` that would break the tree invariant: the item itself,
    /// one of its own descendants (a cycle), or a row that does not exist.
    #[error("item {id} cannot have parent {parent_id}")]
    InvalidParent { id: i64, parent_id: i64 },

    #[error("corrupt schema (found {found:?}, expected {expected})")]
    CorruptSchema { found: Option<u32>, expected: u32 },

    #[error("rusqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("migration: {0}")]
    Migration(#[from] refinery::Error),

    /// The file is encrypted and this build cannot open it. Only reachable
    /// from a build that dropped the SQLCipher feature; reported
    /// separately so it does not masquerade as corruption.
    #[error("database is encrypted, but this build has no SQLCipher support")]
    EncryptedButUnsupported,
}
