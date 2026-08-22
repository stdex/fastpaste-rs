//! `/dev/uinput` virtual keyboard used to emit Ctrl+V on Wayland.

use std::sync::Mutex;
use std::time::Duration;

use evdev::uinput::VirtualDevice;
use evdev::{AttributeSet, EventType, InputEvent, KeyCode};
use thiserror::Error;

const EVENT_SPACING: Duration = Duration::from_millis(15);

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
    fn available(&self) -> bool;
    fn send_ctrl_v(&self) -> Result<(), UinputError>;
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
        std::thread::sleep(Duration::from_millis(200));
        Ok(Self {
            device: Mutex::new(device),
        })
    }

    fn emit_key(&self, key: KeyCode, press: bool) -> Result<(), UinputError> {
        let events = [
            InputEvent::new(EventType::KEY.0, key.0, i32::from(press)),
            InputEvent::new(EventType::SYNCHRONIZATION.0, 0, 0),
        ];
        let mut dev = self.device.lock().map_err(|_| UinputError::Poisoned)?;
        dev.emit(&events).map_err(UinputError::Emit)
    }

    fn press_ctrl_v(&self) -> Result<(), UinputError> {
        self.emit_key(KeyCode::KEY_LEFTCTRL, true)?;
        std::thread::sleep(EVENT_SPACING);
        self.emit_key(KeyCode::KEY_V, true)?;
        std::thread::sleep(EVENT_SPACING);
        self.emit_key(KeyCode::KEY_V, false)?;
        std::thread::sleep(EVENT_SPACING);
        self.emit_key(KeyCode::KEY_LEFTCTRL, false)?;
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
}
