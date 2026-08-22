//! fastpaste platform layer: Wayland integration.

pub mod clipboard;
pub mod hotkey;
pub mod take_once;
pub mod uinput;

pub use clipboard::{ArboardClipboard, Clipboard, ClipboardError, ClipboardPayload, NullClipboard};
pub use hotkey::{
    GlobalHotkey, HotkeyError, NullGlobalHotkey, OPEN_DIALOG_ID, OPEN_MAIN_WINDOW_ID,
    X11GlobalHotkey,
};
pub use take_once::TakeOnceChannel;
pub use uinput::{EvdevUinputCtrlV, NullUinputCtrlV, UinputCtrlV, UinputError};
