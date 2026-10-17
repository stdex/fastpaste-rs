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
    GetForegroundWindow, GetWindowThreadProcessId, SetForegroundWindow,
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

/// The process and thread that own `hwnd`, as one number, or `None` if
/// it names no window.
fn owner_of(hwnd: HWND) -> Option<u64> {
    let mut pid = 0u32;
    // SAFETY: any HWND value is accepted (an invalid one returns 0), and
    // `pid` is a live u32 owned by this frame.
    let tid = unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
    (tid != 0).then(|| (u64::from(pid) << 32) | u64::from(tid))
}

impl ForegroundWindow for WindowsForeground {
    fn capture(&self) -> ForegroundToken {
        // SAFETY: no arguments, no preconditions.
        let hwnd = unsafe { GetForegroundWindow() };
        match owner_of(hwnd) {
            Some(owner) => ForegroundToken::with_owner(hwnd as usize as u64, owner),
            None => ForegroundToken::default(),
        }
    }

    fn restore(&self, token: &ForegroundToken) -> bool {
        let Some(raw) = token.raw() else {
            return false;
        };
        let hwnd = raw as usize as HWND;
        // A closed window's handle can be recycled for an unrelated one;
        // focusing that would paste into the wrong application.
        if !token.still_owned(owner_of(hwnd)) {
            return false;
        }
        // SAFETY: both take any HWND; this one names a live window owned
        // by the thread that owned it at capture.
        unsafe {
            if GetForegroundWindow() == hwnd {
                return true;
            }
            SetForegroundWindow(hwnd) != 0
        }
    }
}
