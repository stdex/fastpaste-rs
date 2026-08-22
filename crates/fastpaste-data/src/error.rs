//! Error type for the data layer.

use std::path::PathBuf;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum DataError {
    #[error("database not found: {0}")]
    NotFound(PathBuf),

    #[error("cannot update an item that has no id")]
    MissingId,

    #[error("corrupt schema (found {found:?}, expected {expected})")]
    CorruptSchema { found: Option<u32>, expected: u32 },

    #[error("rusqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("migration: {0}")]
    Migration(#[from] refinery::Error),
}
