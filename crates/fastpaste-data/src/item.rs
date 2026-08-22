//! Data model: snippets and folders.
//! The central value type that crosses all layers. Plain copyable struct.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ItemKind {
    Folder = 0,
    Plain = 1,
    RichText = 2,
    Clip = 3,
}

impl ItemKind {
    /// Convert to the integer stored in the SQLite `items.type` column.
    pub fn as_i64(self) -> i64 {
        self as i64
    }

    /// Parse from the integer in the SQLite `items.type` column.
    pub fn from_i64(v: i64) -> Option<Self> {
        match v {
            0 => Some(Self::Folder),
            1 => Some(Self::Plain),
            2 => Some(Self::RichText),
            3 => Some(Self::Clip),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Item {
    /// `None` = not yet persisted. `Some(rowid)` after `Database::insert`.
    pub id: Option<i64>,
    /// `0` = top-level child of the invisible root.
    pub parent_id: i64,
    pub kind: ItemKind,
    pub title: String,
    pub body_plain: String,
    /// Always `None` in v1 (RTF is out of scope). Kept for forward-compat
    /// so adding RTF later doesn't break the schema or the type.
    pub body_rtf: Option<String>,
    pub comment: String,
    /// `-1` means "append on insert"; ≥0 after insert.
    pub order_index: i32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Item {
    /// Convenience: a brand-new Folder with sensible defaults for insert.
    pub fn new_folder(parent_id: i64, title: impl Into<String>) -> Self {
        Self {
            id: None,
            parent_id,
            kind: ItemKind::Folder,
            title: title.into(),
            body_plain: String::new(),
            body_rtf: None,
            comment: String::new(),
            order_index: -1,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    /// Convenience: a brand-new Plain text item.
    pub fn new_plain(parent_id: i64, title: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            id: None,
            parent_id,
            kind: ItemKind::Plain,
            title: title.into(),
            body_plain: body.into(),
            body_rtf: None,
            comment: String::new(),
            order_index: -1,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    pub fn is_folder(&self) -> bool {
        self.kind == ItemKind::Folder
    }
}

/// Sentinel for the virtual "Clipboard History" folder. DB rowids are always
/// ≥1, so collisions are impossible. Mirror of C++ `kHistoryFolderId`.
pub const HISTORY_FOLDER_ID: i64 = -1;

/// Where the virtual Clipboard History folder sits in the tree.
///
/// Serde form is the lowercase word (`"top"` / `"bottom"`) — matches the
/// on-disk TOML the pre-enum stringly settings wrote. Unknown values
/// deserialize to [`HistoryPosition::Bottom`], preserving the old
/// "anything but top is bottom" leniency.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HistoryPosition {
    Top,
    #[serde(other)]
    Bottom,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn item_kind_round_trip() {
        for kind in [
            ItemKind::Folder,
            ItemKind::Plain,
            ItemKind::RichText,
            ItemKind::Clip,
        ] {
            assert_eq!(ItemKind::from_i64(kind.as_i64()), Some(kind));
        }
        assert_eq!(ItemKind::from_i64(99), None);
    }

    #[test]
    fn new_folder_has_correct_defaults() {
        let f = Item::new_folder(0, "root");
        assert_eq!(f.parent_id, 0);
        assert_eq!(f.kind, ItemKind::Folder);
        assert_eq!(f.order_index, -1);
        assert!(f.body_plain.is_empty());
        assert!(f.body_rtf.is_none());
        assert!(f.is_folder());
    }

    #[test]
    fn history_folder_id_is_negative() {
        // Sanity: must not collide with DB rowids (which are ≥1).
        const { assert!(HISTORY_FOLDER_ID < 0) }
    }

    #[test]
    fn history_position_serde_round_trips_lowercase() {
        assert_eq!(
            serde_json::to_string(&HistoryPosition::Top).unwrap(),
            r#""top""#
        );
        assert_eq!(
            serde_json::to_string(&HistoryPosition::Bottom).unwrap(),
            r#""bottom""#
        );
        assert_eq!(
            serde_json::from_str::<HistoryPosition>(r#""top""#).unwrap(),
            HistoryPosition::Top
        );
        assert_eq!(
            serde_json::from_str::<HistoryPosition>(r#""bottom""#).unwrap(),
            HistoryPosition::Bottom
        );
    }

    #[test]
    fn history_position_unknown_value_falls_back_to_bottom() {
        // Pin the lenient parse: a config from a future version (or a
        // hand-edited "middle") must not make the whole settings file fail.
        assert_eq!(
            serde_json::from_str::<HistoryPosition>(r#""middle""#).unwrap(),
            HistoryPosition::Bottom
        );
    }
}
