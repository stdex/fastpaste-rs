//! fastpaste app layer: services that orchestrate data + platform.

pub mod clipboard_history;
pub mod context;
pub mod paster;
pub mod paths;
pub mod settings;

pub use clipboard_history::{ClipboardHistory, HistoryEntry};
pub use context::AppContext;
pub use paster::{PasteError, Paster};
pub use settings::{
    ClipboardHistorySettings, GeneralSettings, HotkeySettings, PasteSettings, Settings,
};
