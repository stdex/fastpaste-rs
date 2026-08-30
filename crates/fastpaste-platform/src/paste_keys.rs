//! Synthesising the paste keystroke.
//!
//! The trait is the seam; the backend is per-platform. Linux drives a
//! `/dev/uinput` virtual keyboard (Wayland offers no way to inject a key
//! into another client); Windows uses `SendInput`, which needs no device
//! and no permissions.

use thiserror::Error;

#[cfg(target_os = "linux")]
use std::sync::Mutex;
#[cfg(target_os = "linux")]
use std::time::Duration;

#[cfg(target_os = "linux")]
use evdev::uinput::VirtualDevice;
#[cfg(target_os = "linux")]
use evdev::{AttributeSet, EventType, InputEvent, KeyCode};

#[cfg(target_os = "linux")]
const EVENT_SPACING: Duration = Duration::from_millis(15);

/// How long to wait after creating the virtual device before it can be
/// used. See the call site in [`EvdevPasteKeys::new`].
#[cfg(target_os = "linux")]
const DEVICE_SETTLE: Duration = Duration::from_millis(200);

#[derive(Error, Debug)]
pub enum PasteKeyError {
    /// The backend could not be set up: `/dev/uinput` is missing or not
    /// writable on Linux; on Windows this does not occur, since
    /// `SendInput` needs no handle.
    #[error("could not open the input device: {0}")]
    Open(#[source] std::io::Error),
    #[error("could not emit event: {0}")]
    Emit(#[source] std::io::Error),
    #[error("internal device lock poisoned")]
    Poisoned,
    /// The OS accepted fewer events than were submitted — on Windows,
    /// typically because a higher-integrity window has the foreground
    /// and is blocking synthetic input (UIPI).
    #[error("the system rejected the synthetic keystroke ({sent} of {expected} events accepted)")]
    Rejected { sent: u32, expected: u32 },
}

pub trait PasteKeys: Send + Sync {
    /// Whether this is a real device backend rather than the no-op one.
    ///
    /// It says nothing about the device's current *health*: a device that
    /// was created successfully and has since died still reports `true`,
    /// and [`Self::send_ctrl_v`] is what surfaces that. Callers must treat
    /// an `Err` from `send_ctrl_v` as "the keystroke did not happen", the
    /// same way they treat `available() == false`.
    fn available(&self) -> bool;
    fn send_ctrl_v(&self) -> Result<(), PasteKeyError>;
}

/// The key transitions that make up one Ctrl+V, in order.
///
/// Split out from the emit loop purely so it can be asserted in a test —
/// press/release ordering is the kind of thing that silently regresses
/// into a stuck modifier, and `/dev/uinput` is unavailable in CI.
#[cfg(target_os = "linux")]
fn ctrl_v_frames() -> [(KeyCode, bool); 4] {
    [
        (KeyCode::KEY_LEFTCTRL, true),
        (KeyCode::KEY_V, true),
        (KeyCode::KEY_V, false),
        (KeyCode::KEY_LEFTCTRL, false),
    ]
}

#[cfg(target_os = "linux")]
pub struct EvdevPasteKeys {
    // VirtualDevice::emit takes &mut self, so we wrap in a Mutex. Send + Sync
    // hold because VirtualDevice itself is Send + Sync.
    device: Mutex<VirtualDevice>,
}

#[cfg(target_os = "linux")]
impl EvdevPasteKeys {
    pub fn new() -> Result<Self, PasteKeyError> {
        // Only the keys `send_ctrl_v` ever emits. A narrower capability set
        // keeps the device report small and avoids the huge keymaps some
        // compositors handle poorly.
        let mut keys = AttributeSet::<KeyCode>::new();
        keys.insert(KeyCode::KEY_LEFTCTRL);
        keys.insert(KeyCode::KEY_V);
        let device = VirtualDevice::builder()
            .map_err(PasteKeyError::Open)?
            .name("fastpaste virtual keyboard")
            .with_keys(&keys)
            .map_err(PasteKeyError::Open)?
            .build()
            .map_err(PasteKeyError::Open)?;
        // Give the compositor/libinput time to notice the new device
        // before anyone emits through it. A keystroke sent into a device
        // the compositor has not finished enumerating is simply dropped,
        // which would make the very first paste of a session silently do
        // nothing.
        std::thread::sleep(DEVICE_SETTLE);
        Ok(Self {
            device: Mutex::new(device),
        })
    }

    fn emit_key(&self, key: KeyCode, press: bool) -> Result<(), PasteKeyError> {
        // `VirtualDevice::emit` appends its own SYN_REPORT after the slice
        // it is given, so this must NOT include one — a manual sync here
        // produced a second, empty frame per key.
        let events = [InputEvent::new(EventType::KEY.0, key.0, i32::from(press))];
        let mut dev = self.device.lock().map_err(|_| PasteKeyError::Poisoned)?;
        dev.emit(&events).map_err(PasteKeyError::Emit)
    }

    fn press_ctrl_v(&self) -> Result<(), PasteKeyError> {
        let frames = ctrl_v_frames();
        for (i, (key, press)) in frames.iter().enumerate() {
            self.emit_key(*key, *press)?;
            if i + 1 < frames.len() {
                std::thread::sleep(EVENT_SPACING);
            }
        }
        Ok(())
    }
}

#[cfg(target_os = "linux")]
impl PasteKeys for EvdevPasteKeys {
    fn available(&self) -> bool {
        true
    }

    fn send_ctrl_v(&self) -> Result<(), PasteKeyError> {
        let result = self.press_ctrl_v();
        if result.is_err() {
            // Never leave keys logically held on the virtual device — a
            // failed emit mid-sequence would keep Ctrl stuck down for every
            // later interaction with the compositor.
            let _ = self.emit_key(KeyCode::KEY_V, false);
            let _ = self.emit_key(KeyCode::KEY_LEFTCTRL, false);
        }
        result
    }
}

// ---------------------------------------------------------------------------
// Windows — SendInput
// ---------------------------------------------------------------------------

/// Synthesises Ctrl+V with `SendInput`.
///
/// Simpler than the Linux path in every way that matters: no device to
/// create, no permissions to arrange, and no settle delay — the events
/// are queued straight into the same input stream a real keyboard feeds,
/// so they arrive in order and the compositor question does not exist.
///
/// `new()` cannot fail, but keeps the `Result` so the composition root
/// treats both platforms identically.
#[cfg(windows)]
pub struct WindowsPasteKeys;

#[cfg(windows)]
impl WindowsPasteKeys {
    pub fn new() -> Result<Self, PasteKeyError> {
        Ok(Self)
    }
}

#[cfg(windows)]
impl Default for WindowsPasteKeys {
    fn default() -> Self {
        Self
    }
}

#[cfg(windows)]
impl PasteKeys for WindowsPasteKeys {
    fn available(&self) -> bool {
        true
    }

    fn send_ctrl_v(&self) -> Result<(), PasteKeyError> {
        use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
            INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, SendInput, VIRTUAL_KEY,
            VK_CONTROL, VK_V,
        };

        fn key(vk: VIRTUAL_KEY, up: bool) -> INPUT {
            INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: vk,
                        wScan: 0,
                        dwFlags: if up { KEYEVENTF_KEYUP } else { 0 },
                        time: 0,
                        dwExtraInfo: 0,
                    },
                },
            }
        }

        // One call, so the four events cannot be interleaved with real
        // input from the user's keyboard. Ctrl brackets V, and every
        // press has its release — a stuck Ctrl is the nastiest failure
        // this module can produce.
        let inputs = [
            key(VK_CONTROL, false),
            key(VK_V, false),
            key(VK_V, true),
            key(VK_CONTROL, true),
        ];

        // SAFETY: `inputs` is a live, correctly-sized array of
        // initialised `INPUT` values owned by this stack frame, and the
        // size argument matches `INPUT`'s layout as the API requires.
        let sent = unsafe {
            SendInput(
                inputs.len() as u32,
                inputs.as_ptr(),
                size_of::<INPUT>() as i32,
            )
        };

        if sent as usize != inputs.len() {
            // Blocked partway — most often UIPI: a window running at a
            // higher integrity level has the foreground and the OS
            // refuses synthetic input to it. Release the modifier so the
            // user is not left with a stuck Ctrl.
            let release = [key(VK_CONTROL, true)];
            // SAFETY: as above.
            unsafe { SendInput(1, release.as_ptr(), size_of::<INPUT>() as i32) };
            return Err(PasteKeyError::Rejected {
                sent,
                expected: inputs.len() as u32,
            });
        }
        Ok(())
    }
}

pub struct NullPasteKeys;
impl PasteKeys for NullPasteKeys {
    fn available(&self) -> bool {
        false
    }
    fn send_ctrl_v(&self) -> Result<(), PasteKeyError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_reports_unavailable_and_no_ops() {
        let n = NullPasteKeys;
        assert!(!n.available());
        n.send_ctrl_v().unwrap(); // does nothing, but doesn't error
    }

    /// A stuck Ctrl is the nastiest failure this module can produce, so
    /// pin the exact transition order: every press is matched by a
    /// release, and Ctrl brackets V.
    #[test]
    fn ctrl_v_frames_press_and_release_in_order() {
        let frames = ctrl_v_frames();
        assert_eq!(
            frames,
            [
                (KeyCode::KEY_LEFTCTRL, true),
                (KeyCode::KEY_V, true),
                (KeyCode::KEY_V, false),
                (KeyCode::KEY_LEFTCTRL, false),
            ]
        );

        // Ctrl is held for the whole of V's press and release.
        let pos = |needle: (KeyCode, bool)| frames.iter().position(|f| *f == needle).unwrap();
        assert!(pos((KeyCode::KEY_LEFTCTRL, true)) < pos((KeyCode::KEY_V, true)));
        assert!(pos((KeyCode::KEY_V, false)) < pos((KeyCode::KEY_LEFTCTRL, false)));
    }

    #[test]
    fn every_pressed_key_is_released() {
        let frames = ctrl_v_frames();
        for key in [KeyCode::KEY_LEFTCTRL, KeyCode::KEY_V] {
            let presses = frames.iter().filter(|(k, p)| *k == key && *p).count();
            let releases = frames.iter().filter(|(k, p)| *k == key && !*p).count();
            assert_eq!(presses, releases, "{key:?} must not be left held");
        }
    }

    /// The device only declares the keys it emits; a frame naming a key
    /// outside that set would be silently dropped by the kernel.
    #[test]
    fn frames_only_use_declared_keys() {
        for (key, _) in ctrl_v_frames() {
            assert!(
                key == KeyCode::KEY_LEFTCTRL || key == KeyCode::KEY_V,
                "{key:?} is not in the device's declared capability set"
            );
        }
    }
}
