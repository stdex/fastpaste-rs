//! Which window the paste is meant for.
//!
//! The selection dialog takes focus when it opens, and when it hides the
//! desktop hands focus to whatever it picks — not necessarily the window
//! the user was typing into. So the window that had focus when the hotkey
//! fired is captured then, and given focus back right before the
//! keystroke.
//!
//! The seam follows the crate's pattern: the [`ForegroundWindow`] trait,
//! the opaque [`ForegroundToken`] and the null backend live here, the
//! per-platform backends in the submodules, and the crate root re-exports
//! one of them as [`crate::SystemForeground`].

use thiserror::Error;

#[cfg(target_os = "linux")]
pub mod x11;
#[cfg(target_os = "linux")]
pub use x11::X11Foreground;

#[cfg(windows)]
pub mod windows;
#[cfg(windows)]
pub use windows::WindowsForeground;

#[derive(Error, Debug)]
#[error("foreground window tracking unavailable: {0}")]
pub struct ForegroundError(pub String);

/// A captured window, or nothing. Opaque to everything above this crate:
/// only the backend that produced it knows what the number means.
///
/// A plain integer rather than a native handle, so it is `Send` and can
/// ride from the hotkey thread to the paste worker.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ForegroundToken(Option<u64>);

impl ForegroundToken {
    /// Wrap a backend's window id. Public so that test fakes in other
    /// crates can make one; production tokens come from
    /// [`ForegroundWindow::capture`].
    pub fn from_raw(id: u64) -> Self {
        Self(Some(id))
    }

    /// The captured id, or `None` if nothing was captured.
    pub fn raw(&self) -> Option<u64> {
        self.0
    }
}

pub trait ForegroundWindow: Send + Sync {
    /// The window that has focus right now, or an empty token when there
    /// is none this backend can name.
    fn capture(&self) -> ForegroundToken;

    /// Give focus back to a captured window. Best-effort: `false` means
    /// nothing was captured, the window is gone, or the platform refused,
    /// and the caller pastes into whatever has focus.
    fn restore(&self, token: &ForegroundToken) -> bool;
}

/// No focus tracking: captures nothing, restores nothing.
pub struct NullForegroundWindow;

impl ForegroundWindow for NullForegroundWindow {
    fn capture(&self) -> ForegroundToken {
        ForegroundToken::default()
    }
    fn restore(&self, _token: &ForegroundToken) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// No backend (Wayland-native, or no X display): nothing is captured
    /// and nothing is restored, so the paste goes wherever focus already
    /// is — the behaviour before this seam existed.
    #[test]
    fn null_foreground_is_a_clean_degrade() {
        let null = NullForegroundWindow;
        let token = null.capture();
        assert_eq!(token, ForegroundToken::default());
        assert!(token.raw().is_none());
        assert!(!null.restore(&token));
        assert!(
            !null.restore(&ForegroundToken::from_raw(42)),
            "a token from elsewhere is not restorable without a backend"
        );
    }

    #[test]
    fn a_token_carries_its_window_id() {
        assert_eq!(ForegroundToken::from_raw(0x2a00007).raw(), Some(0x2a00007));
    }
}
