//! fastpaste platform layer: everything that touches the OS or the
//! desktop environment.
//!
//! One codebase, several platforms. The three OS concerns — global
//! hotkeys, the clipboard, and synthesising the paste keystroke — each
//! sit behind a trait, and this module re-exports **one** implementation
//! of each under a platform-neutral alias:
//!
//! | Alias | Linux | Windows |
//! |---|---|---|
//! | [`SystemHotkeys`] | `XGrabKey` on XWayland | `RegisterHotKey` |
//! | [`SystemClipboard`] | arboard + `wl-clipboard-watch` | arboard + `WM_CLIPBOARDUPDATE` |
//! | [`SystemPasteKeys`] | `/dev/uinput` | `SendInput` |
//! | [`SystemSecretStore`] | Secret Service | Credential Manager |
//!
//! Layers above pick the alias, never the concrete type, so nothing
//! outside this crate needs a `cfg`. Adding a platform means adding an
//! implementation here and one row to that table — not a fork.

pub mod clipboard;
pub mod hotkey;
pub mod paste_keys;
pub mod secret_store;
pub mod take_once;

pub use clipboard::{Clipboard, ClipboardError, ClipboardPayload, NullClipboard};
pub use hotkey::{
    GlobalHotkey, HotkeyError, NullGlobalHotkey, OPEN_DIALOG_ID, OPEN_MAIN_WINDOW_ID,
};
pub use paste_keys::{NullPasteKeys, PasteKeyError, PasteKeys};
pub use secret_store::{NullSecretStore, SecretStore, SecretStoreError, UnavailableSecretStore};
pub use take_once::TakeOnceChannel;

// ---- Platform-neutral aliases -------------------------------------------

#[cfg(target_os = "linux")]
pub use clipboard::ArboardClipboard as SystemClipboard;
#[cfg(target_os = "linux")]
pub use hotkey::X11GlobalHotkey as SystemHotkeys;
#[cfg(target_os = "linux")]
pub use paste_keys::EvdevPasteKeys as SystemPasteKeys;

#[cfg(windows)]
pub use clipboard::WindowsClipboard as SystemClipboard;
#[cfg(windows)]
pub use hotkey::WindowsGlobalHotkey as SystemHotkeys;
#[cfg(windows)]
pub use paste_keys::WindowsPasteKeys as SystemPasteKeys;

// No cfg needed here — the keyring crate covers both platforms itself.
pub use secret_store::KeyringSecretStore as SystemSecretStore;
