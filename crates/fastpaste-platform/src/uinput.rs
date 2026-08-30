//! `/dev/uinput` virtual keyboard used to emit Ctrl+V on Wayland.

use std::sync::Mutex;
use std::time::Duration;

use evdev::uinput::VirtualDevice;
use evdev::{AttributeSet, EventType, InputEvent, KeyCode};
use thiserror::Error;

const EVENT_SPACING: Duration = Duration::from_millis(15);

/// How long to wait after creating the virtual device before it can be
/// used. See the call site in [`EvdevUinputCtrlV::new`].
const DEVICE_SETTLE: Duration = Duration::from_millis(200);

#[derive(Error, Debug)]
pub enum UinputError {
    #[error("could not open /dev/uinput: {0}")]
    Open(#[source] std::io::Error),
    #[error("could not emit event: {0}")]
    Emit(#[source] std::io::Error),
    #[error("internal device lock poisoned")]
    Poisoned,
}

pub trait UinputCtrlV: Send + Sync {
    /// Whether this is a real device backend rather than the no-op one.
    ///
    /// It says nothing about the device's current *health*: a device that
    /// was created successfully and has since died still reports `true`,
    /// and [`Self::send_ctrl_v`] is what surfaces that. Callers must treat
    /// an `Err` from `send_ctrl_v` as "the keystroke did not happen", the
    /// same way they treat `available() == false`.
    fn available(&self) -> bool;
    fn send_ctrl_v(&self) -> Result<(), UinputError>;
}

/// The key transitions that make up one Ctrl+V, in order.
///
/// Split out from the emit loop purely so it can be asserted in a test —
/// press/release ordering is the kind of thing that silently regresses
/// into a stuck modifier, and `/dev/uinput` is unavailable in CI.
fn ctrl_v_frames() -> [(KeyCode, bool); 4] {
    [
        (KeyCode::KEY_LEFTCTRL, true),
        (KeyCode::KEY_V, true),
        (KeyCode::KEY_V, false),
        (KeyCode::KEY_LEFTCTRL, false),
    ]
}

pub struct EvdevUinputCtrlV {
    // VirtualDevice::emit takes &mut self, so we wrap in a Mutex. Send + Sync
    // hold because VirtualDevice itself is Send + Sync.
    device: Mutex<VirtualDevice>,
}

impl EvdevUinputCtrlV {
    pub fn new() -> Result<Self, UinputError> {
        // Only the keys `send_ctrl_v` ever emits. A narrower capability set
        // keeps the device report small and avoids the huge keymaps some
        // compositors handle poorly.
        let mut keys = AttributeSet::<KeyCode>::new();
        keys.insert(KeyCode::KEY_LEFTCTRL);
        keys.insert(KeyCode::KEY_V);
        let device = VirtualDevice::builder()
            .map_err(UinputError::Open)?
            .name("fastpaste virtual keyboard")
            .with_keys(&keys)
            .map_err(UinputError::Open)?
            .build()
            .map_err(UinputError::Open)?;
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

    fn emit_key(&self, key: KeyCode, press: bool) -> Result<(), UinputError> {
        // `VirtualDevice::emit` appends its own SYN_REPORT after the slice
        // it is given, so this must NOT include one — a manual sync here
        // produced a second, empty frame per key.
        let events = [InputEvent::new(EventType::KEY.0, key.0, i32::from(press))];
        let mut dev = self.device.lock().map_err(|_| UinputError::Poisoned)?;
        dev.emit(&events).map_err(UinputError::Emit)
    }

    fn press_ctrl_v(&self) -> Result<(), UinputError> {
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

impl UinputCtrlV for EvdevUinputCtrlV {
    fn available(&self) -> bool {
        true
    }

    fn send_ctrl_v(&self) -> Result<(), UinputError> {
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

pub struct NullUinputCtrlV;
impl UinputCtrlV for NullUinputCtrlV {
    fn available(&self) -> bool {
        false
    }
    fn send_ctrl_v(&self) -> Result<(), UinputError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_reports_unavailable_and_no_ops() {
        let n = NullUinputCtrlV;
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
