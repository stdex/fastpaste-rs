//! Windows backend: `GetForegroundWindow` to capture,
//! `SetForegroundWindow` to restore.
//!
//! `SetForegroundWindow` is refused to a process that is not in the
//! foreground and did not receive the last input event. The paste runs
//! straight after the user pressed Enter in our own dialog, which
//! satisfies that; if it is refused anyway the restore reports `false`
//! and the paste goes wherever focus already is.

use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, IsWindow, SetForegroundWindow,
};

use super::{ForegroundError, ForegroundToken, ForegroundWindow};

pub struct WindowsForeground;

impl WindowsForeground {
    /// Cannot fail; returns a `Result` so the composition root treats
    /// every platform alike.
    pub fn new() -> Result<Self, ForegroundError> {
        Ok(Self)
    }
}

impl ForegroundWindow for WindowsForeground {
    fn capture(&self) -> ForegroundToken {
        // SAFETY: no arguments, no preconditions.
        let hwnd = unsafe { GetForegroundWindow() };
        if hwnd.is_null() {
            ForegroundToken::default()
        } else {
            ForegroundToken::from_raw(hwnd as usize as u64)
        }
    }

    fn restore(&self, token: &ForegroundToken) -> bool {
        let Some(raw) = token.raw() else {
            return false;
        };
        let hwnd = raw as usize as HWND;
        // SAFETY: IsWindow accepts any value and only reports whether it
        // names a live window; the other two are only reached with one.
        unsafe {
            if IsWindow(hwnd) == 0 {
                return false;
            }
            if GetForegroundWindow() == hwnd {
                return true;
            }
            SetForegroundWindow(hwnd) != 0
        }
    }
}
