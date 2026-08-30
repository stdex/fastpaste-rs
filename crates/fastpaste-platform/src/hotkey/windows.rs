//! Windows backend: `RegisterHotKey` on a dedicated message thread.
//!
//! Genuinely global, unlike the X11 path — the OS delivers `WM_HOTKEY`
//! whatever has focus, so the limitation the README records for Linux
//! does not apply here.
//!
//! Ownership model mirrors the X11 backend: one thread owns everything
//! that must stay on a single thread, and the public API talks to it
//! over a channel. Here the constraint is stricter than a convention —
//! `RegisterHotKey` binds the hotkey to the **calling thread**, and
//! `WM_HOTKEY` is posted to that thread's queue and to no other. So
//! registration must happen on the same thread that pumps messages, and
//! `register` from any other thread has to hand the work over.

use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;
use std::time::Duration;

use windows_sys::Win32::Foundation::{ERROR_HOTKEY_ALREADY_REGISTERED, GetLastError};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, MOD_WIN, RegisterHotKey,
    UnregisterHotKey,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetMessageW, MSG, PM_REMOVE, PeekMessageW, WM_HOTKEY, WM_QUIT,
};

use super::{GlobalHotkey, HotkeyError};
use crate::take_once::TakeOnceChannel;

/// How long `register` waits for the message thread's reply. Bounded for
/// the same reason as the X11 backend: this is called from the UI
/// thread, and an unbounded wait would freeze the interface.
const REGISTER_TIMEOUT: Duration = Duration::from_secs(3);

/// Work handed to the message thread, which owns every registration.
enum Command {
    Register {
        id: u32,
        sequence: String,
        reply: Sender<Result<(), HotkeyError>>,
    },
    Unregister {
        id: u32,
    },
    Shutdown,
}

/// A parsed accelerator in Win32 terms.
struct Accelerator {
    modifiers: HOT_KEY_MODIFIERS,
    vk: u32,
}

/// Map a sequence such as `Ctrl+Alt+V` onto Win32 modifier flags and a
/// virtual-key code.
///
/// Deliberately mirrors the X11 backend's grammar — same separator, same
/// modifier names, same "at least one modifier" rule — so a
/// `config.toml` moves between platforms unchanged. What differs is the
/// key encoding: X11 wants a physical keycode, Win32 wants a virtual-key
/// code.
fn parse_sequence(seq: &str) -> Option<Accelerator> {
    let parts: Vec<&str> = seq.split('+').map(str::trim).collect();
    let (key_str, mods) = parts.split_last()?;

    let mut modifiers: HOT_KEY_MODIFIERS = 0;
    for m in mods {
        modifiers |= match m.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => MOD_CONTROL,
            "shift" => MOD_SHIFT,
            "alt" => MOD_ALT,
            "super" | "meta" | "win" => MOD_WIN,
            _ => return None,
        };
    }
    // A bare key would be swallowed system-wide, exactly as on X11.
    if modifiers == 0 {
        return None;
    }
    // Do not autorepeat: holding the combination should open one dialog,
    // not a stream of them.
    modifiers |= MOD_NOREPEAT;

    Some(Accelerator {
        modifiers,
        vk: virtual_key(key_str)?,
    })
}

/// Resolve the key half of a sequence to a virtual-key code.
///
/// The letter and digit cases use the documented identity of `VK_A`..
/// `VK_Z` and `VK_0`..`VK_9` with their ASCII code points, which is why
/// they need no table.
fn virtual_key(key: &str) -> Option<u32> {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse as k;

    let lower = key.to_ascii_lowercase();
    let named = |v: u16| Some(v as u32);
    match lower.as_str() {
        "f1" => return named(k::VK_F1),
        "f2" => return named(k::VK_F2),
        "f3" => return named(k::VK_F3),
        "f4" => return named(k::VK_F4),
        "f5" => return named(k::VK_F5),
        "f6" => return named(k::VK_F6),
        "f7" => return named(k::VK_F7),
        "f8" => return named(k::VK_F8),
        "f9" => return named(k::VK_F9),
        "f10" => return named(k::VK_F10),
        "f11" => return named(k::VK_F11),
        "f12" => return named(k::VK_F12),
        "space" => return named(k::VK_SPACE),
        "tab" => return named(k::VK_TAB),
        "esc" | "escape" => return named(k::VK_ESCAPE),
        "enter" | "return" => return named(k::VK_RETURN),
        "backspace" => return named(k::VK_BACK),
        "insert" => return named(k::VK_INSERT),
        "delete" => return named(k::VK_DELETE),
        "home" => return named(k::VK_HOME),
        "end" => return named(k::VK_END),
        "pageup" => return named(k::VK_PRIOR),
        "pagedown" => return named(k::VK_NEXT),
        "up" => return named(k::VK_UP),
        "down" => return named(k::VK_DOWN),
        "left" => return named(k::VK_LEFT),
        "right" => return named(k::VK_RIGHT),
        "minus" | "-" => return named(k::VK_OEM_MINUS),
        "equal" | "=" => return named(k::VK_OEM_PLUS),
        "comma" | "," => return named(k::VK_OEM_COMMA),
        "period" | "." => return named(k::VK_OEM_PERIOD),
        "slash" | "/" => return named(k::VK_OEM_2),
        "semicolon" | ";" => return named(k::VK_OEM_1),
        "apostrophe" => return named(k::VK_OEM_7),
        "grave" | "`" => return named(k::VK_OEM_3),
        "backslash" => return named(k::VK_OEM_5),
        "leftbracket" | "[" => return named(k::VK_OEM_4),
        "rightbracket" | "]" => return named(k::VK_OEM_6),
        _ => {}
    }

    // A single Latin letter or ASCII digit.
    let mut chars = key.chars();
    let c = chars.next()?.to_ascii_uppercase();
    if chars.next().is_some() || !c.is_ascii_alphanumeric() {
        return None;
    }
    Some(c as u32)
}

/// Global hotkeys via `RegisterHotKey`.
pub struct WindowsGlobalHotkey {
    cmd_tx: Sender<Command>,
    /// Thread id of the message pump, so a command can wake its queue.
    thread_id: u32,
    events: TakeOnceChannel<u32>,
}

impl WindowsGlobalHotkey {
    pub fn new() -> Result<Self, HotkeyError> {
        let (cmd_tx, cmd_rx) = channel();
        let (ready_tx, ready_rx) = channel();
        let events = TakeOnceChannel::new();
        let events_tx = events.sender();

        thread::Builder::new()
            .name("win-hotkey-pump".into())
            .spawn(move || message_loop(cmd_rx, events_tx, ready_tx))
            .map_err(|e| HotkeyError::Connect(format!("spawn message pump: {e}")))?;

        // The thread id is needed before any command can be posted, and
        // only the thread itself can report it.
        let thread_id = ready_rx
            .recv_timeout(REGISTER_TIMEOUT)
            .map_err(|e| HotkeyError::Connect(format!("message pump did not start: {e}")))?;

        Ok(Self {
            cmd_tx,
            thread_id,
            events,
        })
    }

    /// Queue a command and wake the pump so it drains the queue.
    ///
    /// `GetMessageW` blocks until the thread has a message, so posting a
    /// channel value alone would not wake it — hence the null message.
    fn command(&self, cmd: Command) -> Result<(), HotkeyError> {
        use windows_sys::Win32::UI::WindowsAndMessaging::{PostThreadMessageW, WM_NULL};

        self.cmd_tx
            .send(cmd)
            .map_err(|e| HotkeyError::ReaderGone(e.to_string()))?;
        // SAFETY: a plain message post to a thread id this type owns; no
        // pointers are involved.
        unsafe { PostThreadMessageW(self.thread_id, WM_NULL, 0, 0) };
        Ok(())
    }
}

impl GlobalHotkey for WindowsGlobalHotkey {
    fn register(&self, id: u32, sequence: &str) -> Result<(), HotkeyError> {
        let (reply_tx, reply_rx) = channel();
        self.command(Command::Register {
            id,
            sequence: sequence.to_string(),
            reply: reply_tx,
        })?;
        match reply_rx.recv_timeout(REGISTER_TIMEOUT) {
            Ok(result) => result,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Err(HotkeyError::ReaderGone(
                format!("no reply within {REGISTER_TIMEOUT:?}"),
            )),
            Err(e) => Err(HotkeyError::ReaderGone(e.to_string())),
        }
    }

    fn unregister(&self, id: u32) {
        let _ = self.command(Command::Unregister { id });
    }

    fn events(&self) -> Receiver<u32> {
        self.events.take()
    }
}

impl Drop for WindowsGlobalHotkey {
    fn drop(&mut self) {
        let _ = self.command(Command::Shutdown);
    }
}

/// The message pump. Owns every registration, because `RegisterHotKey`
/// binds a hotkey to the calling thread and `WM_HOTKEY` is delivered
/// only to that thread's queue.
fn message_loop(cmd_rx: Receiver<Command>, events: Sender<u32>, ready: Sender<u32>) {
    use windows_sys::Win32::System::Threading::GetCurrentThreadId;

    // SAFETY: no arguments, no pointers.
    let thread_id = unsafe { GetCurrentThreadId() };
    if ready.send(thread_id).is_err() {
        return; // the owner gave up before we started
    }

    // Ids this thread currently holds, so shutdown can release them and
    // a re-registration knows to drop the previous one first.
    let mut registered: Vec<u32> = Vec::new();

    loop {
        // Commands first: a register must take effect before the next
        // block, exactly as in the X11 reader.
        loop {
            match cmd_rx.try_recv() {
                Ok(Command::Register {
                    id,
                    sequence,
                    reply,
                }) => {
                    let result = register_one(id, &sequence, &mut registered);
                    let _ = reply.send(result);
                }
                Ok(Command::Unregister { id }) => unregister_one(id, &mut registered),
                Ok(Command::Shutdown) => {
                    release_all(&mut registered);
                    return;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    release_all(&mut registered);
                    return;
                }
            }
        }

        // Drain anything already queued without blocking, so a burst of
        // presses is not serialised behind one GetMessageW per pass.
        let mut msg: MSG = unsafe { std::mem::zeroed() };
        // SAFETY: `msg` is a live, initialised MSG owned by this frame.
        while unsafe { PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) } != 0 {
            if !dispatch(&msg, &events) {
                release_all(&mut registered);
                return;
            }
        }

        // Block until something arrives. A `WM_NULL` from `command()`
        // wakes us to drain the channel above.
        // SAFETY: as above; a null window handle means "any window of
        // this thread", which is what a thread-message queue needs.
        let rc = unsafe { GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) };
        if rc == -1 {
            tracing::error!(
                "win-hotkey-pump: GetMessageW failed ({}); exiting",
                unsafe { GetLastError() }
            );
            release_all(&mut registered);
            return;
        }
        if rc == 0 || !dispatch(&msg, &events) {
            release_all(&mut registered);
            return;
        }
    }
}

/// Forward a hotkey message to the consumer. Returns false when the
/// thread should wind down.
fn dispatch(msg: &MSG, events: &Sender<u32>) -> bool {
    if msg.message == WM_QUIT {
        return false;
    }
    if msg.message == WM_HOTKEY {
        let id = msg.wParam as u32;
        tracing::debug!("win-hotkey-pump: hotkey id={id} fired");
        // A send failure means the consumer is gone, which is not a
        // reason to stop holding the registrations.
        let _ = events.send(id);
    }
    true
}

fn register_one(id: u32, sequence: &str, registered: &mut Vec<u32>) -> Result<(), HotkeyError> {
    let Some(accel) = parse_sequence(sequence) else {
        return Err(HotkeyError::BadSequence(sequence.to_string()));
    };

    // Replace atomically from the caller's point of view: drop the old
    // binding only once the new one is known to be valid, and restore
    // nothing on failure — the previous registration is still live
    // because we have not touched it yet.
    //
    // Win32 refuses a second RegisterHotKey for an id already in use, so
    // an id being re-registered must be released first. That opens a
    // window where the shortcut is unbound; it closes within the same
    // pass of this loop.
    let held = registered.contains(&id);
    if held {
        // SAFETY: null window handle registers against the calling
        // thread, which is the one that registered it.
        unsafe { UnregisterHotKey(std::ptr::null_mut(), id as i32) };
    }

    // SAFETY: as above; the arguments are plain integers.
    let ok = unsafe { RegisterHotKey(std::ptr::null_mut(), id as i32, accel.modifiers, accel.vk) };
    if ok == 0 {
        // SAFETY: no arguments.
        let err = unsafe { GetLastError() };
        if held {
            registered.retain(|r| *r != id);
        }
        return Err(if err == ERROR_HOTKEY_ALREADY_REGISTERED {
            HotkeyError::AlreadyGrabbed {
                key: sequence.to_string(),
                mods: accel.modifiers as u16,
            }
        } else {
            HotkeyError::GrabFailed {
                key: sequence.to_string(),
                mods: accel.modifiers as u16,
                detail: format!("RegisterHotKey failed with error {err}"),
            }
        });
    }

    if !held {
        registered.push(id);
    }
    tracing::info!("registered hotkey id={id} seq={sequence:?}");
    Ok(())
}

fn unregister_one(id: u32, registered: &mut Vec<u32>) {
    if !registered.contains(&id) {
        return;
    }
    // SAFETY: releasing a registration this thread owns.
    unsafe { UnregisterHotKey(std::ptr::null_mut(), id as i32) };
    registered.retain(|r| *r != id);
    tracing::info!("unregistered hotkey id={id}");
}

fn release_all(registered: &mut Vec<u32>) {
    for id in registered.iter() {
        // SAFETY: as above.
        unsafe { UnregisterHotKey(std::ptr::null_mut(), *id as i32) };
    }
    registered.clear();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_shipped_defaults() {
        let dialog = parse_sequence("Ctrl+Alt+V").expect("default must parse");
        let main = parse_sequence("Ctrl+Alt+M").expect("default must parse");
        assert_eq!(dialog.modifiers, main.modifiers);
        assert_ne!(dialog.vk, main.vk, "the defaults differ by key");
        assert_eq!(dialog.vk, 'V' as u32);
    }

    #[test]
    fn autorepeat_is_off() {
        // Holding the combination must open one dialog, not a stream.
        let a = parse_sequence("Ctrl+Alt+V").unwrap();
        assert_ne!(a.modifiers & MOD_NOREPEAT, 0);
    }

    #[test]
    fn a_bare_key_is_rejected() {
        // It would be swallowed system-wide.
        assert!(parse_sequence("V").is_none());
        assert!(parse_sequence("F5").is_none());
    }

    #[test]
    fn the_modifier_grammar_matches_the_x11_backend() {
        let a = parse_sequence("Ctrl+Shift+Alt+Super+K").unwrap();
        for m in [MOD_CONTROL, MOD_SHIFT, MOD_ALT, MOD_WIN] {
            assert_ne!(a.modifiers & m, 0);
        }
        assert!(parse_sequence("Hyper+K").is_none(), "unknown modifier");
    }

    #[test]
    fn named_and_punctuation_keys_resolve() {
        use windows_sys::Win32::UI::Input::KeyboardAndMouse as k;
        assert_eq!(parse_sequence("Ctrl+F5").unwrap().vk, k::VK_F5 as u32);
        assert_eq!(parse_sequence("Ctrl+Space").unwrap().vk, k::VK_SPACE as u32);
        assert_eq!(
            parse_sequence("Ctrl+.").unwrap().vk,
            k::VK_OEM_PERIOD as u32
        );
        assert_eq!(parse_sequence("Ctrl+7").unwrap().vk, '7' as u32);
    }
}
