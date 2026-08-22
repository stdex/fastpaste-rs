//! Global hotkey abstraction. v1 ships only X11GlobalHotkey
//! (XGrabKey on XWayland root window) — validated working under KDE
//! Plasma 6 KWin Wayland by the xgrabkey-spike, which also ruled out
//! ashpd/portal and KGlobalAccel.

use std::sync::mpsc::Receiver;

use evdev::KeyCode;
use thiserror::Error;

use crate::take_once::TakeOnceChannel;

#[derive(Error, Debug)]
pub enum HotkeyError {
    #[error("X11 connection failed: {0}")]
    Connect(String),

    #[error("XGrabKey failed for {key:?} (modifiers={mods:#x}): {source}")]
    GrabFailed {
        key: String,
        mods: u16,
        #[source]
        source: x11rb::errors::ReplyOrIdError,
    },

    #[error("key sequence not parseable: {0}")]
    BadSequence(String),

    #[error("hotkey reader thread unavailable: {0}")]
    ReaderGone(String),
}

/// One registered shortcut. The GUI's `hotkey-events` thread matches
/// incoming ids against the actions it knows.
pub const OPEN_DIALOG_ID: u32 = 1;

/// Second hotkey: Ctrl+Shift+U → open the persistent Main Window
/// (CRUD editor). The Selection Dialog (id=1) is for quick paste; this one
/// is for editing the snippet library.
pub const OPEN_MAIN_WINDOW_ID: u32 = 2;

/// The contract: register a key sequence globally and emit `id` when it fires.
///
/// Implementations are responsible for the lock-modifier combinatorics
/// (NumLock/CapsLock/ScrollLock) so a toggled lock key doesn't break the grab.
pub trait GlobalHotkey: Send + Sync {
    /// Register `id` to fire on `sequence` (e.g. "Ctrl+U"). Re-registering
    /// an id replaces its previous sequence atomically — on failure the
    /// previous registration stays live. Returns Err if the sequence
    /// can't be parsed or the grab conflicts with another X client.
    fn register(&self, id: u32, sequence: &str) -> Result<(), HotkeyError>;

    /// Unregister a previously-registered id. No-op if not registered.
    fn unregister(&self, id: u32);

    /// Channel that receives `id` for each fire. Implementations spawn
    /// a background thread that drains the X event queue.
    fn events(&self) -> Receiver<u32>;
}

/// Null impl for tests and platforms without X. Uses the shared
/// take-once helper, so a test can `fire()` an id and read it back via
/// `events()` exactly like the real backend.
pub struct NullGlobalHotkey {
    events: TakeOnceChannel<u32>,
}

impl NullGlobalHotkey {
    pub fn new() -> Self {
        Self {
            events: TakeOnceChannel::new(),
        }
    }

    /// Test helper: simulate a hotkey fire.
    pub fn fire(&self, id: u32) {
        let _ = self.events.sender().send(id);
    }
}

impl Default for NullGlobalHotkey {
    fn default() -> Self {
        Self::new()
    }
}

impl GlobalHotkey for NullGlobalHotkey {
    fn register(&self, _id: u32, _sequence: &str) -> Result<(), HotkeyError> {
        Ok(())
    }
    fn unregister(&self, _id: u32) {}
    fn events(&self) -> Receiver<u32> {
        self.events.take()
    }
}

// ---------------------------------------------------------------------------
// X11GlobalHotkey — real backend (XGrabKey on XWayland root window).
//
// Ownership model: the reader thread OWNS the `RustConnection` exclusively.
// `register`/`unregister` (any thread) send [`HotkeyCommand`]s over a
// channel; a byte written to a wake pipe releases the reader's `poll(2)`
// (which also watches the connection fd), so the reader can block with
// zero CPU without any thread holding the connection across a blocking
// read.
//
// This backend resolves keys to PHYSICAL keycodes (the evdev kernel
// code + 8), not to keysyms — a hotkey keeps working under any active
// keyboard layout. That is deliberate: the `global-hotkey` crate (Tauri)
// was tried here and rejected because its x11 backend resolves keysyms
// through the core GetKeyboardMapping, which under a Cyrillic layout has
// no Latin 'u' keysym at all — registration failed and the app exited.
//
// Modifier bits (Alt, NumLock, …) are likewise resolved from the server at
// connect time via GetModifierMapping — see [`ModifierLayout`].
// ---------------------------------------------------------------------------

use std::io::{Read, Write};
use std::thread;
use std::time::Duration;

use x11rb::connection::Connection as _;
use x11rb::protocol::xproto::{self, ConnectionExt as _, EventMask, GrabMode, ModMask, Setup};
use x11rb::rust_connection::RustConnection;

/// X keysyms for the modifiers we classify (`keysymdef.h` values; x11rb
/// 0.13 ships no constants module).
mod keysyms {
    pub const CAPS_LOCK: u32 = 0xffe5;
    pub const NUM_LOCK: u32 = 0xff7f;
    pub const SCROLL_LOCK: u32 = 0xff14;
    pub const ALT_L: u32 = 0xffe9;
    pub const ALT_R: u32 = 0xffea;
    pub const META_L: u32 = 0xffe7;
    pub const META_R: u32 = 0xffe8;
    pub const SUPER_L: u32 = 0xffeb;
    pub const SUPER_R: u32 = 0xffec;
    pub const HYPER_L: u32 = 0xffed;
    pub const HYPER_R: u32 = 0xffee;
}

/// Real modifier geometry, resolved from the server at connect.
///
/// Replaces the old hardcoded masks, which were wrong twice over: NumLock
/// was assumed to be Mod1 (0x8) — the bit that actually carries Alt on
/// mainstream setups — so the reader stripped Alt from every KeyPress and
/// Alt hotkeys could never match, while Ctrl+Alt+X mis-dispatched as
/// Ctrl+X. The true NumLock bit (usually Mod2) was never covered at all.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ModifierLayout {
    /// Union of modifier bits whose keys are Caps/Num/Scroll-Lock. These
    /// are the bits the grab covers for every on/off combination and the
    /// bits stripped from KeyPress state before matching.
    lock_mods: u16,
    /// Bit carrying Alt (or Meta). Fallback: Mod1.
    alt_mod: u16,
    /// Bit carrying Super (or Hyper). Fallback: Mod4.
    super_mod: u16,
}

impl ModifierLayout {
    /// Legacy-safe default for servers where the mapping query fails:
    /// CapsLock is Lock (0x2), NumLock is conventionally Mod2 (0x10) —
    /// NOT Mod1, which carries Alt.
    fn fallback() -> Self {
        Self {
            lock_mods: u16::from(ModMask::LOCK) | u16::from(ModMask::M2),
            alt_mod: u16::from(ModMask::M1),
            super_mod: u16::from(ModMask::M4),
        }
    }

    /// Resolve the layout from the server; falls back (with a warning) on
    /// any query failure.
    fn resolve(conn: &RustConnection, setup: &Setup) -> Self {
        let resolved = (|| -> Option<Self> {
            let mm = conn.get_modifier_mapping().ok()?.reply().ok()?;
            let width = mm.keycodes_per_modifier() as usize;
            if width == 0 {
                return None;
            }

            // keycode → keysyms table from the core keyboard mapping, so
            // each modifier-slot keycode can be classified by keysym.
            let min_kc = setup.min_keycode as usize;
            let max_kc = setup.max_keycode as usize;
            let span = max_kc.checked_sub(min_kc)?.checked_add(1)?;
            let km = conn
                .get_keyboard_mapping(
                    setup.min_keycode,
                    u8::try_from(span.min(usize::from(u8::MAX))).ok()?,
                )
                .ok()?
                .reply()
                .ok()?;
            let kpkc = km.keysyms_per_keycode as usize;
            if kpkc == 0 {
                return None;
            }
            let keysyms_for = |kc: u8| -> &[u32] {
                let idx = usize::from(kc);
                if idx < min_kc || idx > max_kc {
                    return &[];
                }
                km.keysyms
                    .chunks_exact(kpkc)
                    .nth(idx - min_kc)
                    .unwrap_or(&[])
            };

            // Row order is Shift, Lock, Control, Mod1..Mod5 — row i ↔ bit
            // 1<<i, which matches ModMask's raw values.
            let mut lock_mods: u16 = 0;
            let mut alt_mod: Option<u16> = None;
            let mut super_mod: Option<u16> = None;
            for (i, row) in mm.keycodes.chunks_exact(width).enumerate() {
                let bit = 1u16 << i;
                for &kc in row {
                    if kc == 0 {
                        continue;
                    }
                    let syms = keysyms_for(kc);
                    let any = |set: &[u32]| syms.iter().any(|s| set.contains(s));
                    if any(&[keysyms::CAPS_LOCK, keysyms::NUM_LOCK, keysyms::SCROLL_LOCK]) {
                        lock_mods |= bit;
                    }
                    if any(&[
                        keysyms::ALT_L,
                        keysyms::ALT_R,
                        keysyms::META_L,
                        keysyms::META_R,
                    ]) {
                        alt_mod = alt_mod.or(Some(bit));
                    }
                    if any(&[
                        keysyms::SUPER_L,
                        keysyms::SUPER_R,
                        keysyms::HYPER_L,
                        keysyms::HYPER_R,
                    ]) {
                        super_mod = super_mod.or(Some(bit));
                    }
                }
            }

            Some(
                Self {
                    lock_mods,
                    alt_mod: alt_mod.unwrap_or(u16::from(ModMask::M1)),
                    super_mod: super_mod.unwrap_or(u16::from(ModMask::M4)),
                }
                .sanitized(),
            )
        })();

        resolved.unwrap_or_else(|| {
            tracing::warn!("modifier mapping unavailable; using default layout");
            Self::fallback()
        })
    }

    /// Drop lock bits that collide with alt/super — the modifier function
    /// wins (a lock key mapped into Alt's slot must not make the reader
    /// strip Alt from KeyPress state). Pathological servers only.
    fn sanitized(mut self) -> Self {
        let conflict = self.lock_mods & (self.alt_mod | self.super_mod);
        if conflict != 0 {
            tracing::warn!(
                "lock bits {conflict:#x} collide with alt/super; excluding them \
                 from lock handling (toggling that lock may break matching)"
            );
            self.lock_mods &= !(self.alt_mod | self.super_mod);
        }
        self
    }

    /// Powerset of the lock bits — every on/off combination the grab
    /// covers so a toggled lock key doesn't break it. Two mapped lock
    /// modifiers → 4 combos (the old hardcoded behavior); three → 8.
    /// More than three never occurs; if a server manages it, cap at the
    /// first three and warn.
    fn lock_combos(&self) -> Vec<u16> {
        let mut bits: Vec<u16> = (0..u16::BITS)
            .map(|i| self.lock_mods & (1 << i))
            .filter(|&m| m != 0)
            .collect();
        if bits.len() > 3 {
            tracing::warn!(
                "{} lock modifiers mapped; capping grab combos at 3",
                bits.len()
            );
            bits.truncate(3);
        }
        let mut combos = vec![0u16];
        for bit in bits {
            let with_bit: Vec<u16> = combos.iter().map(|&c| c | bit).collect();
            combos.extend_from_slice(&with_bit);
        }
        combos
    }
}

/// Command from the public API (any thread) to the reader thread, the
/// sole owner of the X connection.
enum HotkeyCommand {
    /// Register/replace `id`. The reader replies with the grab result;
    /// `register` blocks on the reply.
    Register {
        id: u32,
        sequence: String,
        reply: std::sync::mpsc::Sender<Result<(), HotkeyError>>,
    },
    /// Fire-and-forget removal (the trait method returns `()`); the
    /// reader logs any errors.
    Unregister { id: u32 },
}

/// Everything the reader thread owns. No locks: the registrations table
/// and the connection live only on this thread.
struct ReaderState {
    conn: RustConnection,
    /// Read end of the wake pipe (nonblocking). One byte per pending
    /// command releases the `poll(2)` block.
    wake_rx: std::os::unix::net::UnixStream,
    cmd_rx: Receiver<HotkeyCommand>,
    layout: ModifierLayout,
    screen_root: u32,
    /// id → (X keycode, base modifier mask without lock bits). Searched
    /// linearly — registrations number in single digits.
    registrations: Vec<(u32, u8, u16)>,
    /// Hotkey-fire events out to the consumer.
    events: std::sync::mpsc::Sender<u32>,
}

impl ReaderState {
    /// Blocking event loop: drain commands and buffered events, quiet the
    /// wake pipe, then sleep in `poll(2)` until the X connection or the
    /// wake pipe becomes readable. Process-lifetime thread — no shutdown
    /// path (it dies with the process).
    fn run(mut self) {
        use std::os::fd::AsRawFd;
        let xfd = self.conn.stream().as_raw_fd();
        let wfd = self.wake_rx.as_raw_fd();
        let mut fds = [
            libc::pollfd {
                fd: xfd,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: wfd,
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        loop {
            // 1. Quiet the wake pipe FIRST. Order matters: `command()`
            //    sends on the channel before writing its wake byte, so
            //    any byte drained here belongs to a command that was
            //    already queued — the `try_recv` below is guaranteed to
            //    see it. (Draining after the command drain instead could
            //    consume a byte whose command arrives a moment later,
            //    leaving `poll` asleep with work pending.)
            let mut buf = [0u8; 64];
            while let Ok(n) = self.wake_rx.read(&mut buf) {
                if n < buf.len() {
                    break;
                }
            }
            // 2. Commands — a register must land in the table before we
            //    block, and its reply round-trip may read events.
            while let Ok(cmd) = self.cmd_rx.try_recv() {
                self.handle(cmd);
            }
            // 3. Events already buffered in userspace.
            while let Ok(Some(event)) = self.conn.poll_for_event() {
                self.dispatch(event);
            }
            // 4. Block until X traffic or the next command's wake byte.
            let rc = unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) };
            if rc < 0 {
                let err = std::io::Error::last_os_error();
                if err.raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                tracing::error!("x11-hotkey-reader poll: {err}");
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }

    fn handle(&mut self, cmd: HotkeyCommand) {
        match cmd {
            HotkeyCommand::Register {
                id,
                sequence,
                reply,
            } => {
                let _ = reply.send(self.register(id, &sequence));
            }
            HotkeyCommand::Unregister { id } => self.unregister(id),
        }
    }

    /// Atomic registration: grab the NEW combinations first and only
    /// retire the old ones on success — a failed re-registration leaves
    /// the previous sequence live instead of a dead hotkey.
    fn register(&mut self, id: u32, sequence: &str) -> Result<(), HotkeyError> {
        let Some((keycode, base_mods)) = parse_sequence(sequence, &self.layout) else {
            return Err(HotkeyError::BadSequence(sequence.to_string()));
        };
        let new_combos: Vec<u16> = self
            .layout
            .lock_combos()
            .into_iter()
            .map(|l| base_mods | l)
            .collect();

        let mut grabbed: Vec<u16> = Vec::with_capacity(new_combos.len());
        for &mods in &new_combos {
            match self.grab(keycode, mods) {
                Ok(()) => grabbed.push(mods),
                Err(source) => {
                    for done in grabbed {
                        self.ungrab(keycode, done);
                    }
                    let _ = self.conn.flush();
                    return Err(HotkeyError::GrabFailed {
                        key: sequence.to_string(),
                        mods,
                        source,
                    });
                }
            }
        }

        // Success: retire the old combos that are not part of the new
        // set. The set difference matters when the sequence is unchanged —
        // ungrabbing everything blindly would destroy the fresh grab.
        let old = self
            .registrations
            .iter()
            .copied()
            .find(|(rid, _, _)| *rid == id);
        if let Some((_, old_keycode, old_base)) = old {
            let old_combos: Vec<u16> = self
                .layout
                .lock_combos()
                .into_iter()
                .map(|l| old_base | l)
                .collect();
            for mods in old_combos {
                if old_keycode != keycode || !new_combos.contains(&mods) {
                    self.ungrab(old_keycode, mods);
                }
            }
        }
        let _ = self.conn.flush();

        match self.registrations.iter_mut().find(|(rid, _, _)| *rid == id) {
            Some(slot) => *slot = (id, keycode, base_mods),
            None => self.registrations.push((id, keycode, base_mods)),
        }
        tracing::info!(
            "registered hotkey id={id} seq={sequence:?} keycode={keycode} mods={base_mods:#x}"
        );
        Ok(())
    }

    fn unregister(&mut self, id: u32) {
        let Some((_, keycode, base_mods)) = self
            .registrations
            .iter()
            .copied()
            .find(|(rid, _, _)| *rid == id)
        else {
            return;
        };
        for extra in self.layout.lock_combos() {
            self.ungrab(keycode, base_mods | extra);
        }
        let _ = self.conn.flush();
        self.registrations.retain(|(rid, _, _)| *rid != id);
        tracing::info!("unregistered hotkey id={id}");
    }

    fn dispatch(&mut self, event: x11rb::protocol::Event) {
        let x11rb::protocol::Event::KeyPress(kp) = event else {
            return; // ignore non-KeyPress events
        };
        // `kp.state` is a `KeyButMask` newtype; convert to the raw u16 so
        // the lock bits (per the resolved layout) can be masked off.
        let state: u16 = u16::from(kp.state);
        let effective_mods = state & !self.layout.lock_mods;
        let id = self
            .registrations
            .iter()
            .find(|(_, keycode, base_mods)| *keycode == kp.detail && *base_mods == effective_mods)
            .map(|(id, _, _)| *id);
        match id {
            Some(id) => {
                let _ = self.events.send(id);
            }
            None => {
                // No registration matches this keycode+modifier
                // combination — shouldn't happen for grabs we own,
                // but ignore defensively rather than dropping the
                // reader thread.
                tracing::trace!(
                    "KeyPress keycode={} state={:#x} (effective={:#x}) matched no registration; ignoring",
                    kp.detail,
                    state,
                    effective_mods
                );
            }
        }
    }

    /// grab_key returns ConnectionError from the call and ReplyError from
    /// `.check()`; normalize both to ReplyOrIdError for the error field.
    fn grab(&self, keycode: u8, mods: u16) -> Result<(), x11rb::errors::ReplyOrIdError> {
        self.conn
            .grab_key(
                true,
                self.screen_root,
                ModMask::from(mods),
                keycode,
                GrabMode::ASYNC,
                GrabMode::ASYNC,
            )
            .map_err(x11rb::errors::ReplyOrIdError::from)
            .and_then(|cookie| cookie.check().map_err(x11rb::errors::ReplyOrIdError::from))
    }

    fn ungrab(&self, keycode: u8, mods: u16) {
        let _ = self
            .conn
            .ungrab_key(keycode, self.screen_root, ModMask::from(mods));
    }
}

/// Real impl: XGrabKey on XWayland root window, fronting the reader
/// thread that owns the connection (see the section comment above).
pub struct X11GlobalHotkey {
    cmd_tx: std::sync::mpsc::Sender<HotkeyCommand>,
    /// Write side of the wake pipe; one byte per command releases the
    /// reader's `poll(2)`. Mutex because `Write` needs `&mut` and the
    /// trait methods take `&self`. Nonblocking — a dropped byte under
    /// `WouldBlock` is safe because the pipe necessarily still holds an
    /// unread byte, so the level-triggered poll fires anyway.
    wake_tx: std::sync::Mutex<std::os::unix::net::UnixStream>,
    events: TakeOnceChannel<u32>,
}

impl X11GlobalHotkey {
    pub fn new() -> Result<Self, HotkeyError> {
        let (conn, screen_num) =
            x11rb::connect(None).map_err(|e| HotkeyError::Connect(e.to_string()))?;
        let root = conn.setup().roots[screen_num].root;
        let layout = ModifierLayout::resolve(&conn, conn.setup());

        // Subscribe to KeyPress on root (belt-and-braces from the spike;
        // passive grabs deliver to the grabbing client regardless).
        conn.change_window_attributes(
            root,
            &xproto::ChangeWindowAttributesAux::new().event_mask(Some(EventMask::KEY_PRESS)),
        )
        .map_err(|e| HotkeyError::Connect(e.to_string()))?;
        conn.flush()
            .map_err(|e| HotkeyError::Connect(e.to_string()))?;

        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel();
        let events = TakeOnceChannel::new();
        let (wake_tx, wake_rx) = std::os::unix::net::UnixStream::pair()
            .map_err(|e| HotkeyError::Connect(format!("wake pipe: {e}")))?;
        wake_tx
            .set_nonblocking(true)
            .map_err(|e| HotkeyError::Connect(format!("wake pipe: {e}")))?;
        wake_rx
            .set_nonblocking(true)
            .map_err(|e| HotkeyError::Connect(format!("wake pipe: {e}")))?;

        let state = ReaderState {
            conn,
            wake_rx,
            cmd_rx,
            layout,
            screen_root: root,
            registrations: Vec::new(),
            events: events.sender(),
        };
        thread::Builder::new()
            .name("x11-hotkey-reader".into())
            .spawn(move || state.run())
            .map_err(|e| HotkeyError::Connect(format!("spawn reader: {e}")))?;

        Ok(Self {
            cmd_tx,
            wake_tx: std::sync::Mutex::new(wake_tx),
            events,
        })
    }

    fn command(&self, cmd: HotkeyCommand) -> Result<(), HotkeyError> {
        self.cmd_tx
            .send(cmd)
            .map_err(|e| HotkeyError::ReaderGone(e.to_string()))?;
        if let Ok(mut wake) = self.wake_tx.lock() {
            let _ = wake.write(&[1]);
        }
        Ok(())
    }
}

impl GlobalHotkey for X11GlobalHotkey {
    fn register(&self, id: u32, sequence: &str) -> Result<(), HotkeyError> {
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        self.command(HotkeyCommand::Register {
            id,
            sequence: sequence.to_string(),
            reply: reply_tx,
        })?;
        // The reader always replies; a dropped reply sender means the
        // reader died — surface that instead of hanging.
        reply_rx
            .recv()
            .map_err(|e| HotkeyError::ReaderGone(e.to_string()))?
    }

    fn unregister(&self, id: u32) {
        let _ = self.command(HotkeyCommand::Unregister { id });
    }

    fn events(&self) -> Receiver<u32> {
        self.events.take()
    }
}

/// Parse a sequence like "Ctrl+U" or "Ctrl+Shift+U" into (X keycode, base mods).
/// Supports Ctrl / Shift / Alt / Super modifiers and a single Latin letter or
/// ASCII digit as the key. The keycode is the X keycode: the evdev kernel
/// code (the same constants the uinput backend emits) plus the fixed offset 8
/// the X server layers on top — a PHYSICAL key, so the hotkey works under any
/// active keyboard layout.
fn parse_sequence(seq: &str, layout: &ModifierLayout) -> Option<(u8, u16)> {
    let parts: Vec<&str> = seq.split('+').map(str::trim).collect();
    // split_last returns (last, rest) — last is the key, rest are the modifiers.
    let (key_str, mods) = parts.split_last()?;
    let mut base_mods: u16 = 0;
    for m in mods {
        match m.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => base_mods |= u16::from(ModMask::CONTROL),
            "shift" => base_mods |= u16::from(ModMask::SHIFT),
            // Alt's bit comes from the server's modifier map. Hardcoding
            // Mod1 made Alt hotkeys unmatchable — the reader stripped
            // Mod1 from every KeyPress as a "lock" bit.
            "alt" => base_mods |= layout.alt_mod,
            "super" | "meta" => base_mods |= layout.super_mod,
            _ => return None,
        }
    }
    // A bare-key grab would swallow that key inside every application, so at
    // least one modifier is required.
    if base_mods == 0 {
        return None;
    }
    // Exactly one character: a Latin letter or an ASCII digit.
    let mut chars = key_str.chars();
    let c = chars.next()?.to_ascii_uppercase();
    if chars.next().is_some() || !c.is_ascii_alphanumeric() {
        return None;
    }
    // evdev's `KeyCode: FromStr` accepts the kernel KEY_* names, so the
    // physical keycode comes straight from input-event-codes.h — no
    // hand-maintained table.
    let keycode = format!("KEY_{c}").parse::<KeyCode>().ok()?;
    Some((keycode.0 as u8 + 8, base_mods))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The conventional layout: CapsLock=Lock(0x2), NumLock=Mod2(0x10),
    /// Alt=Mod1, Super=Mod4.
    fn test_layout() -> ModifierLayout {
        ModifierLayout {
            lock_mods: u16::from(ModMask::LOCK) | u16::from(ModMask::M2),
            alt_mod: u16::from(ModMask::M1),
            super_mod: u16::from(ModMask::M4),
        }
    }

    #[test]
    fn null_register_is_no_op() {
        let h = NullGlobalHotkey::new();
        h.register(OPEN_DIALOG_ID, "Ctrl+U").unwrap();
        h.unregister(OPEN_DIALOG_ID);
    }

    /// The Null backend's `fire()` must be observable through `events()` —
    /// tests drive the dispatch flow through exactly this pair.
    #[test]
    fn null_fire_is_observable_via_events() {
        let h = NullGlobalHotkey::new();
        let rx = h.events();
        h.fire(OPEN_DIALOG_ID);
        assert_eq!(
            rx.recv_timeout(std::time::Duration::from_millis(100)),
            Ok(OPEN_DIALOG_ID)
        );
    }

    /// Letters must map to their real X keycodes (evdev kernel code + 8),
    /// pinned against the kernel input-event-codes table — this now also
    /// covers the evdev `FromStr` path that replaced the hand-rolled table.
    #[test]
    fn parse_sequence_maps_letters_to_real_x_keycodes() {
        let cases = [('a', 38), ('i', 31), ('n', 57), ('u', 30), ('z', 52)];
        for (letter, x_keycode) in cases {
            let seq = format!("Ctrl+{letter}");
            let (keycode, _) =
                parse_sequence(&seq, &test_layout()).unwrap_or_else(|| panic!("{seq} must parse"));
            assert_eq!(keycode, x_keycode, "wrong keycode for {seq}");
        }
    }

    #[test]
    fn parse_sequence_combines_modifiers() {
        let l = test_layout();
        let (_, mods) = parse_sequence("Ctrl+Shift+U", &l).unwrap();
        assert_eq!(
            mods,
            u16::from(ModMask::CONTROL) | u16::from(ModMask::SHIFT)
        );
        let (_, mods) = parse_sequence("alt + m", &l).unwrap();
        assert_eq!(mods, u16::from(ModMask::M1));
        // Super maps to Mod4 (the Options dialog hint documents it).
        let (_, mods) = parse_sequence("Super+U", &l).unwrap();
        assert_eq!(mods, u16::from(ModMask::M4));
    }

    /// Alt must map through the resolved layout, not a hardcoded bit —
    /// the regression that made every Alt hotkey dead.
    #[test]
    fn parse_maps_alt_via_layout() {
        let odd_layout = ModifierLayout {
            lock_mods: u16::from(ModMask::LOCK),
            alt_mod: u16::from(ModMask::M3),
            super_mod: u16::from(ModMask::M4),
        };
        let (_, mods) = parse_sequence("Alt+M", &odd_layout).unwrap();
        assert_eq!(mods, u16::from(ModMask::M3));
    }

    /// The fallback layout (unmappable server) keeps the conventional
    /// bits: Lock + Mod2 locks, Mod1 alt, Mod4 super.
    #[test]
    fn fallback_layout_is_conventional() {
        let f = ModifierLayout::fallback();
        assert_eq!(
            f.lock_mods,
            u16::from(ModMask::LOCK) | u16::from(ModMask::M2)
        );
        assert_eq!(f.alt_mod, u16::from(ModMask::M1));
        assert_eq!(f.super_mod, u16::from(ModMask::M4));
        let (_, mods) = parse_sequence("Alt+M", &f).unwrap();
        assert_eq!(mods, u16::from(ModMask::M1));
    }

    /// A lock bit colliding with Alt must be dropped from the lock set —
    /// the modifier function wins.
    #[test]
    fn sanitized_drops_lock_bits_colliding_with_alt() {
        let pathological = ModifierLayout {
            lock_mods: u16::from(ModMask::LOCK) | u16::from(ModMask::M1),
            alt_mod: u16::from(ModMask::M1),
            super_mod: u16::from(ModMask::M4),
        }
        .sanitized();
        assert_eq!(pathological.lock_mods, u16::from(ModMask::LOCK));
        assert_eq!(pathological.alt_mod, u16::from(ModMask::M1));
    }

    /// Two lock bits → the 4-combo powerset the grab covers.
    #[test]
    fn lock_combos_is_the_powerset() {
        let l = ModifierLayout {
            lock_mods: u16::from(ModMask::LOCK) | u16::from(ModMask::M2),
            alt_mod: u16::from(ModMask::M1),
            super_mod: u16::from(ModMask::M4),
        };
        assert_eq!(
            l.lock_combos(),
            vec![
                0,
                u16::from(ModMask::LOCK),
                u16::from(ModMask::M2),
                u16::from(ModMask::LOCK) | u16::from(ModMask::M2),
            ]
        );

        let none = ModifierLayout { lock_mods: 0, ..l };
        assert_eq!(none.lock_combos(), vec![0]);
    }

    /// Digits must map to their real X keycodes (kernel codes 1..9,0 are
    /// 2..11, plus the X offset 8).
    #[test]
    fn parse_sequence_maps_digits_to_real_x_keycodes() {
        let cases = [('1', 10), ('9', 18), ('0', 19)];
        for (digit, x_keycode) in cases {
            let seq = format!("Ctrl+{digit}");
            let (keycode, _) =
                parse_sequence(&seq, &test_layout()).unwrap_or_else(|| panic!("{seq} must parse"));
            assert_eq!(keycode, x_keycode, "wrong keycode for {seq}");
        }
    }

    #[test]
    fn parse_sequence_rejects_unsupported_sequences() {
        let l = test_layout();
        assert!(parse_sequence("Ctrl+Ф", &l).is_none()); // non-Latin key
        assert!(parse_sequence("Ctrl+ab", &l).is_none()); // multi-letter key
        assert!(parse_sequence("U", &l).is_none()); // no modifier
        assert!(parse_sequence("Ctrl", &l).is_none()); // key without modifier
        assert!(parse_sequence("", &l).is_none()); // empty
    }
}
