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
/// only the backend that produced it knows what the numbers mean.
///
/// Plain integers rather than a native handle, so it is `Send` and can
/// ride from the hotkey thread to the paste worker.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ForegroundToken(Option<Captured>);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Captured {
    window: u64,
    /// Who owned the window at capture, where the backend can tell. A
    /// window id is recycled once its window closes; the owner is what
    /// tells the original from a newcomer that inherited the id.
    owner: Option<u64>,
}

impl ForegroundToken {
    /// Wrap a backend's window id. Public so that test fakes in other
    /// crates can make one; production tokens come from
    /// [`ForegroundWindow::capture`].
    pub fn from_raw(window: u64) -> Self {
        Self(Some(Captured {
            window,
            owner: None,
        }))
    }

    /// A window id together with its owner at capture time.
    pub fn with_owner(window: u64, owner: u64) -> Self {
        Self(Some(Captured {
            window,
            owner: Some(owner),
        }))
    }

    /// The captured window id, or `None` if nothing was captured.
    pub fn raw(&self) -> Option<u64> {
        self.0.map(|c| c.window)
    }

    /// Whether the window id still names the captured window, given who
    /// owns it now (`None`: no such window any more). A token without an
    /// owner cannot tell and accepts any live window.
    pub fn still_owned(&self, current_owner: Option<u64>) -> bool {
        match self.0 {
            None => false,
            Some(Captured { owner: None, .. }) => true,
            Some(Captured { owner, .. }) => owner == current_owner,
        }
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

    /// A window id can be recycled once its window closes. A token that
    /// knows who owned the window only accepts a window with that owner,
    /// so a paste never lands in an unrelated window that inherited the id.
    #[test]
    fn a_token_with_an_owner_only_matches_that_owner() {
        let token = ForegroundToken::with_owner(0x1001, 7);
        assert_eq!(token.raw(), Some(0x1001));
        assert!(token.still_owned(Some(7)));
        assert!(
            !token.still_owned(Some(8)),
            "the id now belongs to someone else"
        );
        assert!(!token.still_owned(None), "the window is gone");
    }

    /// A backend that cannot name an owner (X11) does not check one; an
    /// empty token never matches.
    #[test]
    fn ownerless_and_empty_tokens() {
        assert!(ForegroundToken::from_raw(0x1001).still_owned(Some(3)));
        assert!(!ForegroundToken::default().still_owned(Some(3)));
    }

    #[test]
    fn a_token_carries_its_window_id() {
        assert_eq!(ForegroundToken::from_raw(0x2a00007).raw(), Some(0x2a00007));
    }
}
