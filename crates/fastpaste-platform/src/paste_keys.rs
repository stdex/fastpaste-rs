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

impl PasteKeyError {
    /// Whether the OS refused the keystroke, as opposed to the device
    /// failing. A refusal means the target is there but will not take
    /// synthetic input, so the caller should leave the payload on the
    /// clipboard for the user to paste by hand.
    pub fn is_rejected(&self) -> bool {
        matches!(self, Self::Rejected { .. })
    }
}

/// Which chord pastes. Ctrl+V is what nearly every application takes;
/// the choice exists for the ones that do not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PasteMethod {
    #[default]
    CtrlV,
}

pub trait PasteKeys: Send + Sync {
    /// Whether this is a real device backend rather than the no-op one.
    ///
    /// It says nothing about the device's current *health*: a device that
    /// was created successfully and has since died still reports `true`,
    /// and [`Self::send_paste`] is what surfaces that. Callers must treat
    /// an `Err` from `send_paste` as "the keystroke did not happen", the
    /// same way they treat `available() == false`.
    fn available(&self) -> bool;
    /// Press and release the chord for `method`.
    fn send_paste(&self, method: PasteMethod) -> Result<(), PasteKeyError>;
}

// ---------------------------------------------------------------------------
// Linux — /dev/uinput
// ---------------------------------------------------------------------------

/// Every key [`paste_frames`] can emit. The virtual device declares
/// exactly these: a narrower capability set keeps the device report small
/// and avoids the huge keymaps some compositors handle poorly, and a key
/// outside it would be silently dropped by the kernel.
#[cfg(target_os = "linux")]
const DEVICE_KEYS: [KeyCode; 2] = [KeyCode::KEY_LEFTCTRL, KeyCode::KEY_V];

/// The key transitions that make up one paste chord, in order.
///
/// Split out from the emit loop purely so it can be asserted in a test —
/// press/release ordering is the kind of thing that silently regresses
/// into a stuck modifier, and `/dev/uinput` is unavailable in CI.
///
/// These are *physical* keys: the compositor maps them through the
/// user's layout, so on a layout that moves V the chord moves with it.
#[cfg(target_os = "linux")]
fn paste_frames(method: PasteMethod) -> Vec<(KeyCode, bool)> {
    let (modifier, key) = match method {
        PasteMethod::CtrlV => (KeyCode::KEY_LEFTCTRL, KeyCode::KEY_V),
    };
    vec![
        (modifier, true),
        (key, true),
        (key, false),
        (modifier, false),
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
        let mut keys = AttributeSet::<KeyCode>::new();
        for key in DEVICE_KEYS {
            keys.insert(key);
        }
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

    fn press(&self, frames: &[(KeyCode, bool)]) -> Result<(), PasteKeyError> {
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

    fn send_paste(&self, method: PasteMethod) -> Result<(), PasteKeyError> {
        let frames = paste_frames(method);
        let result = self.press(&frames);
        if result.is_err() {
            // Never leave keys logically held on the virtual device — a
            // failed emit mid-sequence would keep a modifier stuck down
            // for every later interaction with the compositor. Release
            // in reverse press order, so the modifier goes last.
            for (key, _) in frames.iter().filter(|(_, press)| *press).rev() {
                let _ = self.emit_key(*key, false);
            }
        }
        result
    }
}

// ---------------------------------------------------------------------------
// Windows — SendInput
// ---------------------------------------------------------------------------

/// Windows virtual-key codes, as plain numbers so the stroke builder
/// compiles — and is tested — on every platform.
#[cfg(any(windows, test))]
mod vk {
    pub const CONTROL: u16 = 0x11;
    pub const V: u16 = 0x56;
}

/// One key transition for `SendInput`, before it becomes an `INPUT`.
#[cfg(any(windows, test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct KeyStroke {
    vk: u16,
    scan: u16,
    up: bool,
    extended: bool,
}

/// The US set-1 scan code for a virtual key, for when the active layout
/// gives none: a zero scan code is what RDP and raw-input readers drop.
#[cfg(any(windows, test))]
fn us_scan_code(key: u16) -> u16 {
    match key {
        vk::CONTROL => 0x1D,
        vk::V => 0x2F,
        _ => 0,
    }
}

/// The strokes for one paste chord, in order, each carrying the scan code
/// `scan_of` (the layout's VK -> scan lookup) gives it.
///
/// Both the virtual key and the scan code are sent, as the reference
/// implementation does: applications keep matching on the virtual key,
/// while RDP, VMs and raw-input readers get the scan code they need
/// instead of zero.
#[cfg(any(windows, test))]
fn windows_paste_inputs(method: PasteMethod, scan_of: impl Fn(u16) -> u16) -> Vec<KeyStroke> {
    let stroke = |key: u16, up: bool| KeyStroke {
        vk: key,
        scan: match scan_of(key) {
            0 => us_scan_code(key),
            scan => scan,
        },
        up,
        extended: false,
    };
    let (modifier, key) = match method {
        PasteMethod::CtrlV => (vk::CONTROL, vk::V),
    };
    vec![
        stroke(modifier, false),
        stroke(key, false),
        stroke(key, true),
        stroke(modifier, true),
    ]
}

/// The active layout's scan code for a virtual key, or 0 if it has none.
#[cfg(windows)]
fn windows_scan_code(key: u16) -> u16 {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{MAPVK_VK_TO_VSC, MapVirtualKeyW};
    // SAFETY: a pure table lookup; any input is valid.
    unsafe { MapVirtualKeyW(u32::from(key), MAPVK_VK_TO_VSC) as u16 }
}

/// Synthesises the paste chord with `SendInput`.
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

    fn send_paste(&self, method: PasteMethod) -> Result<(), PasteKeyError> {
        use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
            INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP,
            SendInput,
        };

        fn input(k: &KeyStroke) -> INPUT {
            let mut flags = 0;
            if k.up {
                flags |= KEYEVENTF_KEYUP;
            }
            if k.extended {
                flags |= KEYEVENTF_EXTENDEDKEY;
            }
            INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: k.vk,
                        wScan: k.scan,
                        dwFlags: flags,
                        time: 0,
                        dwExtraInfo: 0,
                    },
                },
            }
        }

        fn send(inputs: &[INPUT]) -> u32 {
            // SAFETY: `inputs` is a live, correctly-sized slice of
            // initialised `INPUT` values, and the size argument matches
            // `INPUT`'s layout as the API requires.
            unsafe {
                SendInput(
                    inputs.len() as u32,
                    inputs.as_ptr(),
                    size_of::<INPUT>() as i32,
                )
            }
        }

        // One call, so the events cannot be interleaved with real input
        // from the user's keyboard. The modifier brackets the key, and
        // every press has its release — a stuck modifier is the nastiest
        // failure this module can produce.
        let strokes = windows_paste_inputs(method, windows_scan_code);
        let inputs: Vec<INPUT> = strokes.iter().map(input).collect();
        let sent = send(&inputs);

        if sent as usize != inputs.len() {
            // Blocked partway — most often UIPI: a window running at a
            // higher integrity level has the foreground and the OS
            // refuses synthetic input to it. Release what was pressed so
            // the user is not left with a stuck modifier.
            let releases: Vec<INPUT> = strokes
                .iter()
                .filter(|k| !k.up)
                .rev()
                .map(|k| input(&KeyStroke { up: true, ..*k }))
                .collect();
            send(&releases);
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
    fn send_paste(&self, _method: PasteMethod) -> Result<(), PasteKeyError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only an OS refusal (UIPI) counts as "rejected": the target exists
    /// and refused us, so the payload should stay for a manual paste. A
    /// broken device is a different failure.
    #[test]
    fn only_rejected_is_rejected() {
        assert!(
            PasteKeyError::Rejected {
                sent: 1,
                expected: 4
            }
            .is_rejected()
        );
        assert!(!PasteKeyError::Poisoned.is_rejected());
        assert!(!PasteKeyError::Emit(std::io::Error::other("gone")).is_rejected());
        assert!(!PasteKeyError::Open(std::io::Error::other("no device")).is_rejected());
    }

    #[test]
    fn null_reports_unavailable_and_no_ops() {
        let n = NullPasteKeys;
        assert!(!n.available());
        n.send_paste(PasteMethod::CtrlV).unwrap(); // does nothing, but doesn't error
    }

    // ---- Windows builder (pure, so it is tested on every platform) -----

    /// A layout's VK -> scan table, standing in for `MapVirtualKeyW`.
    /// Deliberately not the US set-1 values, so a test can tell a looked-up
    /// scan code from the fallback.
    fn fake_layout(vk: u16) -> u16 {
        match vk {
            vk::CONTROL => 0x61,
            vk::V => 0x62,
            _ => 0,
        }
    }

    /// Finding 6. `wScan = 0` is ignored by RDP, VMs and raw-input
    /// readers, which then see no key at all. Every stroke carries the
    /// scan code the current layout gives its virtual key.
    #[test]
    fn ctrl_v_inputs_carry_scancodes() {
        let strokes = windows_paste_inputs(PasteMethod::CtrlV, fake_layout);
        assert_eq!(
            strokes,
            vec![
                KeyStroke {
                    vk: vk::CONTROL,
                    scan: 0x61,
                    up: false,
                    extended: false
                },
                KeyStroke {
                    vk: vk::V,
                    scan: 0x62,
                    up: false,
                    extended: false
                },
                KeyStroke {
                    vk: vk::V,
                    scan: 0x62,
                    up: true,
                    extended: false
                },
                KeyStroke {
                    vk: vk::CONTROL,
                    scan: 0x61,
                    up: true,
                    extended: false
                },
            ]
        );
    }

    /// A layout with no mapping for a key must not bring `wScan = 0`
    /// back: fall back to the US set-1 code for the same key.
    #[test]
    fn scan_codes_fall_back_when_the_layout_has_no_mapping() {
        let strokes = windows_paste_inputs(PasteMethod::CtrlV, |_| 0);
        assert!(strokes.iter().all(|k| k.scan != 0), "{strokes:?}");
        assert_eq!(strokes[0].scan, 0x1D, "Ctrl");
        assert_eq!(strokes[1].scan, 0x2F, "V");
    }

    #[test]
    fn windows_strokes_release_every_press() {
        let strokes = windows_paste_inputs(PasteMethod::CtrlV, fake_layout);
        for key in [vk::CONTROL, vk::V] {
            let downs = strokes.iter().filter(|k| k.vk == key && !k.up).count();
            let ups = strokes.iter().filter(|k| k.vk == key && k.up).count();
            assert_eq!(downs, ups, "{key:#x} must not be left held");
        }
    }

    /// The real lookup, where it exists.
    #[cfg(windows)]
    #[test]
    fn map_vk_gives_v_a_scan_code() {
        assert_ne!(windows_scan_code(vk::V), 0);
        assert_ne!(windows_scan_code(vk::CONTROL), 0);
    }

    // ---- Linux frames ---------------------------------------------------

    /// A stuck Ctrl is the nastiest failure this module can produce, so
    /// pin the exact transition order: every press is matched by a
    /// release, and Ctrl brackets V.
    #[cfg(target_os = "linux")]
    #[test]
    fn ctrl_v_frames_press_and_release_in_order() {
        let frames = paste_frames(PasteMethod::CtrlV);
        assert_eq!(
            frames,
            vec![
                (KeyCode::KEY_LEFTCTRL, true),
                (KeyCode::KEY_V, true),
                (KeyCode::KEY_V, false),
                (KeyCode::KEY_LEFTCTRL, false),
            ]
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn every_pressed_key_is_released() {
        let frames = paste_frames(PasteMethod::CtrlV);
        for (key, _) in &frames {
            let presses = frames.iter().filter(|(k, p)| k == key && *p).count();
            let releases = frames.iter().filter(|(k, p)| k == key && !*p).count();
            assert_eq!(presses, releases, "{key:?} must not be left held");
        }
    }

    /// The device only declares the keys it emits; a frame naming a key
    /// outside that set would be silently dropped by the kernel.
    #[cfg(target_os = "linux")]
    #[test]
    fn frames_only_use_declared_keys() {
        for (key, _) in paste_frames(PasteMethod::CtrlV) {
            assert!(
                DEVICE_KEYS.contains(&key),
                "{key:?} is not in the device's declared capability set"
            );
        }
    }
}
