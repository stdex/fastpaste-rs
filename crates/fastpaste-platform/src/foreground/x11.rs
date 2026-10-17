//! X11 backend: `GetInputFocus` to capture, `SetInputFocus` to restore.
//!
//! On a Wayland desktop this only sees XWayland clients, which matches
//! the hotkeys: they fire only while an X11 window has focus, so when
//! one fires there is an X window to capture. When a Wayland-native
//! window has focus, XWayland reports the root window and nothing is
//! captured — the paste then goes wherever the compositor puts focus.

use x11rb::connection::Connection as _;
use x11rb::protocol::xproto::{ConnectionExt as _, InputFocus, Window};
use x11rb::rust_connection::RustConnection;

use super::{ForegroundError, ForegroundToken, ForegroundWindow};

/// `GetInputFocus` answers for "no focus" and "focus follows the
/// pointer". Neither is a window.
const FOCUS_NONE: Window = 0;
const FOCUS_POINTER_ROOT: Window = 1;

/// Map a `GetInputFocus` answer to a token. Split out so the sentinel
/// handling is testable without an X server.
fn token_for_focus(focus: Window, root: Window) -> ForegroundToken {
    if focus == FOCUS_NONE || focus == FOCUS_POINTER_ROOT || focus == root {
        ForegroundToken::default()
    } else {
        ForegroundToken::from_raw(u64::from(focus))
    }
}

/// A connection of its own, separate from the hotkey reader's: that one
/// is owned by the reader thread and blocks in `poll(2)`.
pub struct X11Foreground {
    conn: RustConnection,
    root: Window,
}

impl X11Foreground {
    pub fn new() -> Result<Self, ForegroundError> {
        let (conn, screen) = x11rb::connect(None).map_err(|e| ForegroundError(e.to_string()))?;
        let root = conn.setup().roots[screen].root;
        Ok(Self { conn, root })
    }

    fn focus(&self) -> Option<Window> {
        match self.conn.get_input_focus().map(|c| c.reply()) {
            Ok(Ok(reply)) => Some(reply.focus),
            Ok(Err(e)) => {
                tracing::debug!("GetInputFocus failed: {e}");
                None
            }
            Err(e) => {
                tracing::debug!("GetInputFocus failed: {e}");
                None
            }
        }
    }
}

impl ForegroundWindow for X11Foreground {
    fn capture(&self) -> ForegroundToken {
        self.focus()
            .map(|focus| token_for_focus(focus, self.root))
            .unwrap_or_default()
    }

    fn restore(&self, token: &ForegroundToken) -> bool {
        let Some(win) = token.raw().and_then(|raw| Window::try_from(raw).ok()) else {
            return false;
        };
        if self.focus() == Some(win) {
            return true;
        }
        // A window that has since been unmapped or destroyed answers with
        // BadMatch/BadWindow; that is a `false`, not a failure.
        let result = self
            .conn
            .set_input_focus(InputFocus::PARENT, win, x11rb::CURRENT_TIME)
            .map_err(|e| e.to_string())
            .and_then(|cookie| cookie.check().map_err(|e| e.to_string()));
        match result {
            Ok(()) => true,
            Err(e) => {
                tracing::debug!("SetInputFocus({win:#x}) failed: {e}");
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROOT: u32 = 0x1e6;

    /// `GetInputFocus` reports None (0) and PointerRoot (1) as sentinels,
    /// and the root window when no client has focus — on XWayland that is
    /// what a Wayland-native window having focus looks like. None of them
    /// is a window to give focus back to.
    #[test]
    fn sentinels_and_the_root_capture_nothing() {
        for focus in [0, 1, ROOT] {
            assert_eq!(
                token_for_focus(focus, ROOT),
                ForegroundToken::default(),
                "focus {focus:#x}"
            );
        }
    }

    #[test]
    fn a_client_window_is_captured() {
        assert_eq!(token_for_focus(0x2a00007, ROOT).raw(), Some(0x2a00007));
    }
}
