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
///
/// Unlike the Windows builder, nothing here releases modifiers the user
/// is still holding: a uinput device cannot read the state of the real
/// keyboards. In practice the hotkey's modifiers are long released by
/// the time a snippet is picked from the dialog.
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
    pub const LSHIFT: u16 = 0xA0;
    pub const RSHIFT: u16 = 0xA1;
    pub const LCONTROL: u16 = 0xA2;
    pub const RCONTROL: u16 = 0xA3;
    pub const LMENU: u16 = 0xA4;
    pub const RMENU: u16 = 0xA5;
    pub const LWIN: u16 = 0x5B;
    pub const RWIN: u16 = 0x5C;
    /// Unassigned, so tapping it does nothing — except stop the release
    /// of a held Alt or Win from counting as a solo tap.
    pub const MASK: u16 = 0xE8;
}

/// Modifier keys physically held when the chord fires, one bit per side:
/// releasing a key needs to name the side that is actually down.
#[cfg(any(windows, test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct ModMask(u8);

#[cfg(any(windows, test))]
impl ModMask {
    const NONE: Self = Self(0);
    const LCTRL: Self = Self(1 << 0);
    const RCTRL: Self = Self(1 << 1);
    const LSHIFT: Self = Self(1 << 2);
    const RSHIFT: Self = Self(1 << 3);
    const LALT: Self = Self(1 << 4);
    const RALT: Self = Self(1 << 5);
    const LWIN: Self = Self(1 << 6);
    const RWIN: Self = Self(1 << 7);
    const CTRL: Self = Self(Self::LCTRL.0 | Self::RCTRL.0);
    const ALT: Self = Self(Self::LALT.0 | Self::RALT.0);
    const WIN: Self = Self(Self::LWIN.0 | Self::RWIN.0);

    fn intersects(self, other: Self) -> bool {
        self.0 & other.0 != 0
    }

    fn without(self, other: Self) -> Self {
        Self(self.0 & !other.0)
    }
}

#[cfg(any(windows, test))]
impl std::ops::BitOr for ModMask {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

/// Each side's bit and its virtual key, in the order they are released.
#[cfg(any(windows, test))]
const MODIFIER_KEYS: [(ModMask, u16); 8] = [
    (ModMask::LCTRL, vk::LCONTROL),
    (ModMask::RCTRL, vk::RCONTROL),
    (ModMask::LSHIFT, vk::LSHIFT),
    (ModMask::RSHIFT, vk::RSHIFT),
    (ModMask::LALT, vk::LMENU),
    (ModMask::RALT, vk::RMENU),
    (ModMask::LWIN, vk::LWIN),
    (ModMask::RWIN, vk::RWIN),
];

/// Keys whose scan code carries the E0 prefix. `SendInput` takes that as
/// `KEYEVENTF_EXTENDEDKEY`; without it right Ctrl reads as left Ctrl.
#[cfg(any(windows, test))]
fn is_extended(key: u16) -> bool {
    matches!(key, vk::RCONTROL | vk::RMENU | vk::LWIN | vk::RWIN)
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
        vk::CONTROL | vk::LCONTROL | vk::RCONTROL => 0x1D,
        vk::V => 0x2F,
        vk::LSHIFT => 0x2A,
        vk::RSHIFT => 0x36,
        vk::LMENU | vk::RMENU => 0x38,
        vk::LWIN => 0x5B,
        vk::RWIN => 0x5C,
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
///
/// `held` is what the user is still physically holding. Any modifier the
/// chord does not use is released first — a held Alt turns Ctrl+V into
/// Ctrl+Alt+V — and not pressed again, which would risk leaving it stuck
/// if the user lets go meanwhile. A held modifier the chord *does* use is
/// left alone: it already makes the chord, and releasing it at the end
/// would desynchronise a key the user is holding down.
#[cfg(any(windows, test))]
fn windows_paste_inputs(
    method: PasteMethod,
    held: ModMask,
    scan_of: impl Fn(u16) -> u16,
) -> Vec<KeyStroke> {
    let stroke = |key: u16, up: bool| KeyStroke {
        vk: key,
        scan: match scan_of(key) {
            0 => us_scan_code(key),
            scan => scan,
        },
        up,
        extended: is_extended(key),
    };
    let (chord_modifier, modifier, key) = match method {
        PasteMethod::CtrlV => (ModMask::CTRL, vk::CONTROL, vk::V),
    };

    let mut strokes = Vec::new();
    let stray = held.without(chord_modifier);
    if stray.intersects(ModMask::ALT | ModMask::WIN) {
        // Releasing Alt or Win right after pressing nothing else is a tap:
        // it opens the menu bar or the Start menu.
        strokes.push(stroke(vk::MASK, false));
        strokes.push(stroke(vk::MASK, true));
    }
    for (bit, side) in MODIFIER_KEYS {
        if stray.intersects(bit) {
            strokes.push(stroke(side, true));
        }
    }

    let press_modifier = !held.intersects(chord_modifier);
    if press_modifier {
        strokes.push(stroke(modifier, false));
    }
    strokes.push(stroke(key, false));
    strokes.push(stroke(key, true));
    if press_modifier {
        strokes.push(stroke(modifier, true));
    }
    strokes
}

/// Which modifiers the user is physically holding right now.
#[cfg(windows)]
fn held_modifiers() -> ModMask {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
    MODIFIER_KEYS
        .iter()
        .filter(|(_, side)| {
            // SAFETY: any virtual-key code is a valid argument.
            let state = unsafe { GetAsyncKeyState(i32::from(*side)) };
            // The high bit of the SHORT is "down now".
            state < 0
        })
        .fold(ModMask::NONE, |mask, (bit, _)| mask | *bit)
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
        let strokes = windows_paste_inputs(method, held_modifiers(), windows_scan_code);
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
        let strokes = windows_paste_inputs(PasteMethod::CtrlV, ModMask::NONE, fake_layout);
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
        let strokes = windows_paste_inputs(PasteMethod::CtrlV, ModMask::NONE, |_| 0);
        assert!(strokes.iter().all(|k| k.scan != 0), "{strokes:?}");
        assert_eq!(strokes[0].scan, 0x1D, "Ctrl");
        assert_eq!(strokes[1].scan, 0x2F, "V");
    }

    #[test]
    fn windows_strokes_release_every_press() {
        let strokes = windows_paste_inputs(PasteMethod::CtrlV, ModMask::NONE, fake_layout);
        for key in [vk::CONTROL, vk::V] {
            let downs = strokes.iter().filter(|k| k.vk == key && !k.up).count();
            let ups = strokes.iter().filter(|k| k.vk == key && k.up).count();
            assert_eq!(downs, ups, "{key:#x} must not be left held");
        }
    }

    /// Finding 7. The user may still be holding the hotkey's modifiers
    /// when the chord fires; held Alt and Shift turned Ctrl+V into
    /// Ctrl+Alt+Shift+V, which pastes nothing. They are released first and
    /// not pressed again, and the chord itself is untouched.
    #[test]
    fn held_modifiers_are_released_before_the_chord() {
        let strokes = windows_paste_inputs(
            PasteMethod::CtrlV,
            ModMask::LALT | ModMask::LSHIFT,
            fake_layout,
        );
        let first_ctrl_down = strokes
            .iter()
            .position(|k| k.vk == vk::CONTROL && !k.up)
            .expect("the chord still presses Ctrl");
        for side in [vk::LMENU, vk::LSHIFT] {
            let up = strokes
                .iter()
                .position(|k| k.vk == side && k.up)
                .unwrap_or_else(|| panic!("{side:#x} is never released: {strokes:?}"));
            assert!(
                up < first_ctrl_down,
                "{side:#x} is released before the chord"
            );
            assert!(
                !strokes.iter().any(|k| k.vk == side && !k.up),
                "{side:#x} must not be pressed again"
            );
        }
        let core: Vec<(u16, bool)> = strokes[strokes.len() - 4..]
            .iter()
            .map(|k| (k.vk, k.up))
            .collect();
        assert_eq!(
            core,
            vec![
                (vk::CONTROL, false),
                (vk::V, false),
                (vk::V, true),
                (vk::CONTROL, true)
            ]
        );
    }

    /// A Ctrl the user is holding already makes the chord: pressing it
    /// again is pointless and the closing Ctrl-up would leave Windows
    /// thinking it is released while it is physically down.
    #[test]
    fn a_held_ctrl_is_used_not_pressed_or_released() {
        for held in [ModMask::LCTRL, ModMask::RCTRL] {
            let strokes = windows_paste_inputs(PasteMethod::CtrlV, held, fake_layout);
            assert!(
                !strokes
                    .iter()
                    .any(|k| [vk::CONTROL, vk::LCONTROL, vk::RCONTROL].contains(&k.vk)),
                "no Ctrl transitions at all with {held:?}: {strokes:?}"
            );
            let keys: Vec<(u16, bool)> = strokes.iter().map(|k| (k.vk, k.up)).collect();
            assert_eq!(keys, vec![(vk::V, false), (vk::V, true)]);
        }
    }

    /// Releasing Alt or Win on its own is a "tap": it opens the window's
    /// menu bar or the Start menu. An unassigned key is tapped first so
    /// the release is not a solo one.
    #[test]
    fn a_released_alt_or_win_is_masked_first() {
        for held in [ModMask::LALT, ModMask::RALT, ModMask::LWIN, ModMask::RWIN] {
            let strokes = windows_paste_inputs(PasteMethod::CtrlV, held, fake_layout);
            let keys: Vec<(u16, bool)> = strokes[..3].iter().map(|k| (k.vk, k.up)).collect();
            assert_eq!(keys[..2], [(vk::MASK, false), (vk::MASK, true)], "{held:?}");
            assert!(keys[2].1, "then the release: {held:?}");
        }
        let shift_only = windows_paste_inputs(PasteMethod::CtrlV, ModMask::LSHIFT, fake_layout);
        assert!(
            !shift_only.iter().any(|k| k.vk == vk::MASK),
            "Shift has no tap action, so no mask"
        );
    }

    /// Right Ctrl/Alt and both Win keys are extended keys: without the
    /// flag the release would name a different key (the left one, or a
    /// numpad key) and the held one would stay down.
    #[test]
    fn right_side_and_win_releases_carry_the_extended_flag() {
        let strokes = windows_paste_inputs(
            PasteMethod::CtrlV,
            ModMask::RALT | ModMask::LWIN | ModMask::RWIN | ModMask::LSHIFT,
            fake_layout,
        );
        let release = |key: u16| *strokes.iter().find(|k| k.vk == key && k.up).unwrap();
        assert!(release(vk::RMENU).extended);
        assert!(release(vk::LWIN).extended);
        assert!(release(vk::RWIN).extended);
        assert!(!release(vk::LSHIFT).extended);
        assert!(
            strokes
                .iter()
                .filter(|k| k.vk != vk::MASK)
                .all(|k| k.scan != 0)
        );
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
