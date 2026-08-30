//! Global hotkey abstraction.
//!
//! This module holds the seam — the [`GlobalHotkey`] trait, the action
//! ids, the error type and the null backend. The real implementations
//! live in the per-platform submodules and are re-exported by the crate
//! root under the neutral [`crate::SystemHotkeys`] alias, so nothing
//! above this crate needs a `cfg`.

use std::sync::mpsc::Receiver;

use thiserror::Error;

use crate::take_once::TakeOnceChannel;

#[cfg(target_os = "linux")]
pub mod x11;
#[cfg(target_os = "linux")]
pub use x11::X11GlobalHotkey;

#[cfg(windows)]
pub mod windows;
#[cfg(windows)]
pub use windows::WindowsGlobalHotkey;

#[derive(Error, Debug)]
pub enum HotkeyError {
    #[error("X11 connection failed: {0}")]
    Connect(String),

    /// Another X client already holds this shortcut. Distinct from
    /// [`Self::GrabFailed`] because it is by far the most likely
    /// real-world failure (the desktop environment owns the combination)
    /// and it is the one the user can actually act on.
    #[error("{key} is already claimed by another application")]
    AlreadyGrabbed { key: String, mods: u16 },

    /// Any other grab failure. The source is a string rather than an
    /// `x11rb` type: this enum is the backend-agnostic [`GlobalHotkey`]
    /// error surface, and an x11rb type here forces every consumer of the
    /// trait to depend on x11rb and makes the variant unconstructible for
    /// any other backend.
    #[error("XGrabKey failed for {key:?} (modifiers={mods:#x}): {detail}")]
    GrabFailed {
        key: String,
        mods: u16,
        detail: String,
    },

    #[error("key sequence not parseable: {0}")]
    BadSequence(String),

    #[error("hotkey reader thread unavailable: {0}")]
    ReaderGone(String),
}

/// One registered shortcut. The GUI's `hotkey-events` thread matches
/// incoming ids against the actions it knows.
pub const OPEN_DIALOG_ID: u32 = 1;

/// Second hotkey: open the persistent Main Window (CRUD editor). The
/// Selection Dialog (id=1) is for quick paste; this one is for editing
/// the snippet library. The sequences themselves come from `Settings`.
pub const OPEN_MAIN_WINDOW_ID: u32 = 2;

/// The contract: register a key sequence globally and emit `id` when it fires.
///
/// Implementations are responsible for the lock-modifier combinatorics
/// (NumLock/CapsLock/ScrollLock) so a toggled lock key doesn't break the grab.
pub trait GlobalHotkey: Send + Sync {
    /// Register `id` to fire on `sequence` (e.g. "Ctrl+U"). Re-registering
    /// an id replaces its previous sequence atomically — on failure the
    /// previous registration stays live. Returns Err if the sequence
    /// can't be parsed or the grab conflicts with another X client.
    ///
    /// Registering one id never disturbs another id's live shortcut, even
    /// when the two swap sequences (which the options dialog does in a
    /// single apply: it re-registers both whenever either changed).
    fn register(&self, id: u32, sequence: &str) -> Result<(), HotkeyError>;

    /// Unregister a previously-registered id. No-op if not registered.
    fn unregister(&self, id: u32);

    /// Channel that receives `id` for each fire. Implementations spawn
    /// a background thread that drains the X event queue.
    fn events(&self) -> Receiver<u32>;
}

/// Null impl for tests and platforms without X. Uses the shared
/// take-once helper, so a test can `fire()` an id and read it back via
/// `events()` exactly like the real backend.
pub struct NullGlobalHotkey {
    events: TakeOnceChannel<u32>,
}

impl NullGlobalHotkey {
    pub fn new() -> Self {
        Self {
            events: TakeOnceChannel::new(),
        }
    }

    /// Test helper: simulate a hotkey fire.
    pub fn fire(&self, id: u32) {
        let _ = self.events.sender().send(id);
    }
}

impl Default for NullGlobalHotkey {
    fn default() -> Self {
        Self::new()
    }
}

impl GlobalHotkey for NullGlobalHotkey {
    fn register(&self, _id: u32, _sequence: &str) -> Result<(), HotkeyError> {
        Ok(())
    }
    fn unregister(&self, _id: u32) {}
    fn events(&self) -> Receiver<u32> {
        self.events.take()
    }
}
