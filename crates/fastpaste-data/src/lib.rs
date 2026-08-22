//! fastpaste data layer: SQLite-backed snippet storage + Item value type.
//! No GUI, no platform, no I/O beyond SQLite.

pub mod database;
pub mod error;
pub mod item;

pub use database::Database;
pub use error::DataError;
pub use item::{HISTORY_FOLDER_ID, HistoryPosition, Item, ItemKind};
