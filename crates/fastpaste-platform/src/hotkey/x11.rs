//! X11 backend: `XGrabKey` on the XWayland root window.
//!
//! Selected as `SystemHotkeys` on Linux. Note the limitation recorded in
//! the README: under a Wayland compositor a keystroke only reaches
//! XWayland while an X11/XWayland window has focus, so these grabs do
//! not fire over Wayland-native windows.

use std::sync::mpsc::Receiver;

use evdev::KeyCode;

use super::{GlobalHotkey, HotkeyError};
use crate::take_once::TakeOnceChannel;
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

/// Pause after an unexpected `poll(2)` failure, so a persistent error
/// cannot become a busy loop.
const POLL_ERROR_BACKOFF: Duration = Duration::from_millis(100);

/// Reconnect policy after the X connection drops (XWayland crash,
/// compositor restart, session teardown). Bounded: a session that is
/// going away for good must let the reader thread exit rather than
/// retry forever.
const RECONNECT_ATTEMPTS: u32 = 5;
const RECONNECT_DELAY: Duration = Duration::from_millis(500);

/// How long `register` waits for the reader thread's reply. The reader
/// answers as soon as its round trip to the server completes; a bound is
/// still needed because this is called from the UI thread, and an
/// unresponsive X server would otherwise freeze the whole interface.
const REGISTER_TIMEOUT: Duration = Duration::from_secs(3);

use x11rb::connection::Connection as _;
use x11rb::protocol::xproto::{self, ConnectionExt as _, GrabMode, ModMask, Setup};
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
            // Exactly 8 rows per protocol (Shift, Lock, Control,
            // Mod1..Mod5); `take(8)` keeps a malformed reply from
            // shifting past bit 15 and panicking the reader thread.
            for (i, row) in mm.keycodes.chunks_exact(width).take(8).enumerate() {
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
    /// Wind the reader thread down. Sent by `Drop`.
    Shutdown,
}

/// One live registration.
///
/// The `sequence` is kept so grabs can be replayed verbatim after the X
/// connection is rebuilt — `base_mods` is derived from the *old*
/// [`ModifierLayout`], which a new server may not share.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Registration {
    id: u32,
    sequence: String,
    keycode: u8,
    base_mods: u16,
}

/// Bits of an X `KeyButMask` that are actual keyboard modifiers:
/// Shift, Lock, Control and Mod1..Mod5 (bits 0-7).
///
/// Everything above them must be masked off before matching. Bits 8-12
/// carry the pointer button state, and XKB reports the effective keyboard
/// *group* in bits 13-14 — so comparing the raw `state` meant a hotkey
/// silently stopped matching whenever a mouse button was held, and, where
/// the group bits are populated, under any non-primary keyboard layout.
/// The passive grab still fires (the server matches core modifiers only),
/// so the event arrives and is then discarded — which would defeat the
/// layout independence this whole backend exists to provide.
const REAL_MODS: u16 = 0x00FF;

/// Every (keycode, modifiers) pair the given registrations currently
/// hold a grab on.
fn claimed_combos(registrations: &[Registration], lock_combos: &[u16]) -> Vec<(u8, u16)> {
    registrations
        .iter()
        .flat_map(|r| {
            lock_combos
                .iter()
                .map(move |l| (r.keycode, r.base_mods | l))
        })
        .collect()
}

/// Which of `old`'s grabbed combinations to actually release when its id
/// is re-registered onto `new_keycode`/`new_combos`.
///
/// Two exclusions, both load-bearing:
///
///  * combinations still in the new set (the sequence did not change) —
///    releasing them would destroy the grab just taken;
///  * combinations another id currently owns. Without this, swapping the
///    two shortcuts in the options dialog killed one of them: id1 takes
///    id2's old sequence, then re-registering id2 retires "its" old
///    combinations and ungrabs the grab id1 had just established, leaving
///    that shortcut dead for the session with no error and no log line.
fn combos_to_retire(
    old: &Registration,
    new_keycode: u8,
    new_combos: &[u16],
    lock_combos: &[u16],
    others: &[Registration],
) -> Vec<(u8, u16)> {
    let claimed = claimed_combos(others, lock_combos);
    lock_combos
        .iter()
        .map(|l| old.base_mods | l)
        .filter(|mods| {
            let is_new = old.keycode == new_keycode && new_combos.contains(mods);
            let owned_elsewhere = claimed.contains(&(old.keycode, *mods));
            !is_new && !owned_elsewhere
        })
        .map(|mods| (old.keycode, mods))
        .collect()
}

/// Empty a wake pipe, reporting whether the reader should keep going.
///
/// A free function over `Read` so the EOF case — which *is* the fix for
/// the busy loop — can be tested without an X server. Returning `false`
/// on `Ok(0)` is the load-bearing line: without it the read returns
/// `Ok(0)` forever while `POLLHUP` keeps `poll` returning instantly, and
/// the thread spins at 100% CPU for the life of the process.
fn drain_wake_from(rx: &mut impl Read) -> bool {
    let mut buf = [0u8; 64];
    loop {
        match rx.read(&mut buf) {
            Ok(0) => return false, // EOF: every writer is dropped.
            Ok(n) if n < buf.len() => return true,
            Ok(_) => continue, // a full buffer may mean more is waiting
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return true,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => {
                tracing::debug!("x11-hotkey-reader: wake pipe read: {e}");
                return false;
            }
        }
    }
}

/// What the reader should do after `poll(2)` returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PollOutcome {
    /// Business as usual — go round again.
    Continue,
    /// The X connection is gone; try to rebuild it.
    Reconnect,
    /// The owner is gone; wind the thread down.
    Exit,
}

/// Classify a `poll(2)` result from the two fds' `revents`.
///
/// Pure integer logic, extracted so the classification can be tested: the
/// original code never looked at `revents` at all, which is half of why a
/// dead X server became a busy loop rather than a reconnect.
fn poll_outcome(x_revents: libc::c_short, wake_revents: libc::c_short) -> PollOutcome {
    const BAD: libc::c_short = libc::POLLHUP | libc::POLLERR | libc::POLLNVAL;
    // The wake pipe is checked first: if the owner is gone there is
    // nothing left to serve, whatever the X fd says.
    if wake_revents & BAD != 0 {
        return PollOutcome::Exit;
    }
    if x_revents & BAD != 0 {
        return PollOutcome::Reconnect;
    }
    PollOutcome::Continue
}

/// Match a KeyPress against the registration table.
///
/// A free function over plain data so it can be tested without an X
/// server — this is where the modifier masking lives, and it is the part
/// most worth pinning.
fn match_registration(
    registrations: &[Registration],
    keycode: u8,
    state: u16,
    layout: &ModifierLayout,
) -> Option<u32> {
    let effective_mods = state & REAL_MODS & !layout.lock_mods;
    registrations
        .iter()
        .find(|r| r.keycode == keycode && r.base_mods == effective_mods)
        .map(|r| r.id)
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
    /// Live registrations. Searched linearly — they number in single
    /// digits.
    registrations: Vec<Registration>,
    /// Hotkey-fire events out to the consumer.
    events: std::sync::mpsc::Sender<u32>,
}

impl ReaderState {
    /// Blocking event loop: drain commands and buffered events, quiet the
    /// wake pipe, then sleep in `poll(2)` until the X connection or the
    /// wake pipe becomes readable.
    ///
    /// Every exit from the loop is deliberate. The previous version could
    /// not exit at all: it ignored `poll_for_event`'s `Err` and never
    /// looked at `revents`, so once the X server went away both the poll
    /// and the event drain returned instantly and the thread span at 100%
    /// CPU for the life of the process — with hotkeys dead and nothing
    /// logged.
    fn run(mut self) {
        use std::os::fd::AsRawFd;
        loop {
            // 1. Quiet the wake pipe FIRST. Order matters: `command()`
            //    sends on the channel before writing its wake byte, so
            //    any byte drained here belongs to a command that was
            //    already queued — the `try_recv` below is guaranteed to
            //    see it. (Draining after the command drain instead could
            //    consume a byte whose command arrives a moment later,
            //    leaving `poll` asleep with work pending.)
            if !self.drain_wake() {
                tracing::debug!("x11-hotkey-reader: wake pipe closed; exiting");
                return;
            }

            // 2. Commands — a register must land in the table before we
            //    block, and its reply round-trip may read events.
            loop {
                match self.cmd_rx.try_recv() {
                    Ok(HotkeyCommand::Shutdown) => {
                        tracing::debug!("x11-hotkey-reader: shutdown requested");
                        return;
                    }
                    Ok(cmd) => self.handle(cmd),
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        tracing::debug!("x11-hotkey-reader: owner dropped; exiting");
                        return;
                    }
                }
            }

            // 3. Events already buffered in userspace. An error here means
            //    the connection is gone, not that the queue is empty.
            if !self.drain_events() {
                if self.reconnect() {
                    continue;
                }
                tracing::error!("x11-hotkey-reader: X connection lost for good; exiting");
                return;
            }

            // 4. Block until X traffic or the next command's wake byte.
            //    The fds are rebuilt each pass because `reconnect` swaps
            //    the connection (and therefore its socket) underneath us.
            let mut fds = [
                libc::pollfd {
                    fd: self.conn.stream().as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: self.wake_rx.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            // SAFETY: `fds` is a live, correctly-sized array of two
            // initialised `pollfd`s owned by this stack frame, and the
            // count passed matches its length. Both fds are owned by
            // `self` and outlive the call. `poll` writes only `revents`.
            let rc = unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) };
            if rc < 0 {
                let err = std::io::Error::last_os_error();
                if err.raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                tracing::error!("x11-hotkey-reader poll: {err}");
                std::thread::sleep(POLL_ERROR_BACKOFF);
                continue;
            }

            match poll_outcome(fds[0].revents, fds[1].revents) {
                PollOutcome::Continue => {}
                PollOutcome::Exit => {
                    tracing::debug!("x11-hotkey-reader: wake pipe hung up; exiting");
                    return;
                }
                PollOutcome::Reconnect => {
                    if !self.reconnect() {
                        tracing::error!(
                            "x11-hotkey-reader: X connection hung up for good; exiting"
                        );
                        return;
                    }
                }
            }
        }
    }

    /// Empty the wake pipe. Returns false once it has hit EOF, which
    /// means the writing end (owned by `X11GlobalHotkey`) is gone.
    fn drain_wake(&mut self) -> bool {
        drain_wake_from(&mut self.wake_rx)
    }

    /// Dispatch every buffered event. Returns false if the connection
    /// failed (as opposed to simply having no more events).
    fn drain_events(&mut self) -> bool {
        loop {
            match self.conn.poll_for_event() {
                Ok(Some(event)) => self.dispatch(event),
                Ok(None) => return true,
                Err(e) => {
                    tracing::error!("x11-hotkey-reader: poll_for_event: {e}");
                    return false;
                }
            }
        }
    }

    /// Rebuild the X connection and replay every registration.
    ///
    /// Returns false when the server stays unreachable, in which case the
    /// reader exits rather than spinning. Sequences are re-parsed against
    /// the *new* server's modifier layout, since Alt/Super bits are not
    /// guaranteed to land on the same modifier slots.
    fn reconnect(&mut self) -> bool {
        for attempt in 1..=RECONNECT_ATTEMPTS {
            std::thread::sleep(RECONNECT_DELAY);
            let (conn, screen_num) = match x11rb::connect(None) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(
                        "x11-hotkey-reader: reconnect {attempt}/{RECONNECT_ATTEMPTS}: {e}"
                    );
                    continue;
                }
            };
            let root = conn.setup().roots[screen_num].root;
            let layout = ModifierLayout::resolve(&conn, conn.setup());
            self.conn = conn;
            self.screen_root = root;
            self.layout = layout;

            // Replay from the stored sequences, not the cached keycodes.
            let previous = std::mem::take(&mut self.registrations);
            for reg in previous {
                if let Err(e) = self.register(reg.id, &reg.sequence) {
                    tracing::error!(
                        "x11-hotkey-reader: could not restore hotkey id={} seq={:?}: {e}",
                        reg.id,
                        reg.sequence
                    );
                    // Keep it: a dropped entry makes a later `unregister`
                    // silently do nothing.
                    self.registrations.push(reg);
                }
            }
            tracing::info!("x11-hotkey-reader: reconnected to X");
            return true;
        }
        false
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
            // Handled in `run`, which needs to return rather than loop.
            HotkeyCommand::Shutdown => {}
        }
    }

    /// Atomic registration: grab the NEW combinations first and only
    /// retire the old ones on success — a failed re-registration leaves
    /// the previous sequence live instead of a dead hotkey.
    fn register(&mut self, id: u32, sequence: &str) -> Result<(), HotkeyError> {
        let Some((keycode, base_mods)) = parse_sequence(sequence, &self.layout) else {
            return Err(HotkeyError::BadSequence(sequence.to_string()));
        };
        let combos = self.layout.lock_combos();
        let new_combos: Vec<u16> = combos.iter().map(|l| base_mods | l).collect();

        let mut grabbed: Vec<u16> = Vec::with_capacity(new_combos.len());
        for &mods in &new_combos {
            match self.grab(keycode, mods) {
                Ok(()) => grabbed.push(mods),
                Err(err) => {
                    // Roll back through the SAME ownership guard the
                    // success path uses. A same-client GrabKey on an
                    // identical (window, keycode, modifiers) *replaces*
                    // the existing passive grab rather than adding one,
                    // so a combination in `grabbed` may be the only grab
                    // behind another id's registration — or behind this
                    // id's own, when the sequence is unchanged.
                    // Releasing it unconditionally killed a live shortcut
                    // silently, and left `registrations` describing a
                    // grab that no longer existed, so every later
                    // register/unregister computed retirement from a
                    // table that was lying.
                    let claimed = claimed_combos(&self.registrations, &combos);
                    for done in grabbed {
                        if !claimed.contains(&(keycode, done)) {
                            self.ungrab(keycode, done);
                        }
                    }
                    let _ = self.conn.flush();
                    return Err(classify_grab_error(sequence, mods, err));
                }
            }
        }

        // Success: retire the old combinations. See `combos_to_retire`.
        if let Some(old) = self.registrations.iter().find(|r| r.id == id).cloned() {
            let others: Vec<Registration> = self
                .registrations
                .iter()
                .filter(|r| r.id != id)
                .cloned()
                .collect();
            for (kc, mods) in combos_to_retire(&old, keycode, &new_combos, &combos, &others) {
                self.ungrab(kc, mods);
            }
        }
        let _ = self.conn.flush();

        let entry = Registration {
            id,
            sequence: sequence.to_string(),
            keycode,
            base_mods,
        };
        match self.registrations.iter_mut().find(|r| r.id == id) {
            Some(slot) => *slot = entry,
            None => self.registrations.push(entry),
        }
        tracing::info!(
            "registered hotkey id={id} seq={sequence:?} keycode={keycode} mods={base_mods:#x}"
        );
        Ok(())
    }

    fn unregister(&mut self, id: u32) {
        let Some(gone) = self.registrations.iter().find(|r| r.id == id).cloned() else {
            return;
        };
        let combos = self.layout.lock_combos();
        let others: Vec<Registration> = self
            .registrations
            .iter()
            .filter(|r| r.id != id)
            .cloned()
            .collect();
        // No new set: nothing is exempt except what another id owns.
        for (kc, mods) in combos_to_retire(&gone, gone.keycode, &[], &combos, &others) {
            self.ungrab(kc, mods);
        }
        let _ = self.conn.flush();
        self.registrations.retain(|r| r.id != id);
        tracing::info!("unregistered hotkey id={id}");
    }

    fn dispatch(&mut self, event: x11rb::protocol::Event) {
        match event {
            x11rb::protocol::Event::KeyPress(kp) => {
                // `kp.state` is a `KeyButMask` newtype; convert to the raw
                // u16 for matching. See [`REAL_MODS`] for why the raw
                // value cannot be compared directly.
                let state: u16 = u16::from(kp.state);
                match match_registration(&self.registrations, kp.detail, state, &self.layout) {
                    Some(id) => {
                        // Logged on the MATCH too, not only on the miss:
                        // "the wrong action fired" is invisible otherwise,
                        // because a wrong match is still a match. This one
                        // line is what tells a user reporting "the
                        // four-key combination runs the three-key action"
                        // which registration actually claimed the event
                        // and what modifier state arrived with it.
                        tracing::debug!(
                            "hotkey id={id} fired (keycode={}, state={:#x}, lock_mods={:#x})",
                            kp.detail,
                            state,
                            self.layout.lock_mods,
                        );
                        let _ = self.events.send(id);
                    }
                    None => {
                        // No registration matches this keycode+modifier
                        // combination — shouldn't happen for grabs we own,
                        // but ignore defensively rather than dropping the
                        // reader thread.
                        tracing::trace!(
                            "KeyPress keycode={} state={:#x} matched no registration; ignoring",
                            kp.detail,
                            state,
                        );
                    }
                }
            }
            // The modifier map changed (keyboard hotplug, `xmodmap`).
            // `lock_mods`/`alt_mod` are resolved once at connect, so
            // without this the cached geometry goes stale and matching
            // breaks. Layout *switching* is unaffected — the keycodes are
            // physical — so this only fires on genuine remapping.
            x11rb::protocol::Event::MappingNotify(n)
                if n.request == xproto::Mapping::MODIFIER
                    || n.request == xproto::Mapping::KEYBOARD =>
            {
                tracing::info!("x11-hotkey-reader: modifier mapping changed; re-resolving");
                let previous = std::mem::take(&mut self.registrations);

                // Release the old grabs against the OLD layout, BEFORE
                // `self.layout` is replaced. The connection survives a
                // remap (unlike `reconnect`, where the server drops the
                // client's grabs for us), so anything not released here
                // stays in the server's table for the life of the
                // process — and a passive grab consumes its combination
                // process-wide, so the user's previous shortcut would be
                // silently swallowed in every application while firing
                // nothing.
                let old_combos = self.layout.lock_combos();
                for reg in &previous {
                    for l in &old_combos {
                        self.ungrab(reg.keycode, reg.base_mods | l);
                    }
                }
                let _ = self.conn.flush();

                self.layout = ModifierLayout::resolve(&self.conn, self.conn.setup());
                for reg in previous {
                    if let Err(e) = self.register(reg.id, &reg.sequence) {
                        // Keep the entry so the table still describes what
                        // the user asked for: dropping it would make a
                        // later `unregister` a no-op and orphan any grab
                        // that does exist.
                        tracing::error!(
                            "x11-hotkey-reader: could not re-grab id={} seq={:?}: {e}",
                            reg.id,
                            reg.sequence
                        );
                        self.registrations.push(reg);
                    }
                }
            }
            x11rb::protocol::Event::Error(e) => {
                tracing::warn!("x11-hotkey-reader: X protocol error: {e:?}");
            }
            _ => {}
        }
    }

    /// `grab_key` reports a ConnectionError from the call itself and a
    /// ReplyError from `.check()`. `.check()` forces a synchronous round
    /// trip, which is what makes a grab conflict observable at all
    /// (asynchronously it would surface as a stray error event later).
    ///
    /// GrabMode::ASYNC for both pointer and keyboard: a SYNC keyboard grab
    /// freezes keyboard processing until the client calls AllowEvents, so
    /// a bug on that path would lock the user's keyboard entirely.
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
        if let Err(e) = self
            .conn
            .ungrab_key(keycode, self.screen_root, ModMask::from(mods))
        {
            // A leaked grab is otherwise invisible, and it will make the
            // *next* registration of that combination fail confusingly.
            tracing::trace!("ungrab keycode={keycode} mods={mods:#x}: {e}");
        }
    }
}

/// Turn a failed grab into the most specific error available.
///
/// `BadAccess` means another client already holds this key combination —
/// overwhelmingly the common real-world failure (the desktop environment
/// owns Ctrl+U, say). Reporting it distinctly is what lets the options
/// dialog tell the user something actionable instead of silently
/// reverting the field.
fn classify_grab_error(
    sequence: &str,
    mods: u16,
    err: x11rb::errors::ReplyOrIdError,
) -> HotkeyError {
    use x11rb::errors::ReplyOrIdError;
    use x11rb::protocol::ErrorKind;
    if let ReplyOrIdError::X11Error(ref x) = err
        && x.error_kind == ErrorKind::Access
    {
        return HotkeyError::AlreadyGrabbed {
            key: sequence.to_string(),
            mods,
        };
    }
    HotkeyError::GrabFailed {
        key: sequence.to_string(),
        mods,
        detail: err.to_string(),
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

        // Deliberately NOT selecting KEY_PRESS on the root window. A
        // passive grab delivers to the grabbing client regardless, so the
        // subscription bought nothing — while making this process a
        // recipient of every KeyPress delivered to the root window, i.e.
        // XWayland keystrokes whenever focus lands there. No functional
        // gain is worth widening keystroke visibility.
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
        // reader died — surface that instead of hanging. The timeout
        // covers the other case: the reader is alive but stuck in a round
        // trip to an unresponsive server. This is called from the UI
        // thread, so an unbounded wait there freezes the interface with
        // no way out.
        match reply_rx.recv_timeout(REGISTER_TIMEOUT) {
            Ok(result) => result,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Err(HotkeyError::ReaderGone(
                format!("no reply within {REGISTER_TIMEOUT:?}"),
            )),
            Err(e) => Err(HotkeyError::ReaderGone(e.to_string())),
        }
    }

    fn unregister(&self, id: u32) {
        let _ = self.command(HotkeyCommand::Unregister { id });
    }

    fn events(&self) -> Receiver<u32> {
        self.events.take()
    }
}

impl Drop for X11GlobalHotkey {
    /// Wind the reader down explicitly.
    ///
    /// Dropping the command sender and the wake pipe would eventually be
    /// noticed by the reader anyway, but asking it to stop is immediate
    /// and makes the intent legible. Without any of this, a dropped
    /// backend used to leave the thread spinning on a hung-up pipe.
    fn drop(&mut self) {
        let _ = self.cmd_tx.send(HotkeyCommand::Shutdown);
        if let Ok(mut wake) = self.wake_tx.lock() {
            let _ = wake.write(&[1]);
        }
    }
}

/// Named keys accepted in a sequence, mapped to their kernel `KEY_*`
/// name. Single letters and digits are handled separately (`KEY_A`,
/// `KEY_7`), so this table only covers the rest.
///
/// Function keys in particular are the most conventional choice for a
/// global shortcut, and the options dialog takes free text — before this,
/// typing `Ctrl+F5` produced an unexplained `BadSequence` and a field
/// that snapped back.
const NAMED_KEYS: &[(&str, &str)] = &[
    ("f1", "KEY_F1"),
    ("f2", "KEY_F2"),
    ("f3", "KEY_F3"),
    ("f4", "KEY_F4"),
    ("f5", "KEY_F5"),
    ("f6", "KEY_F6"),
    ("f7", "KEY_F7"),
    ("f8", "KEY_F8"),
    ("f9", "KEY_F9"),
    ("f10", "KEY_F10"),
    ("f11", "KEY_F11"),
    ("f12", "KEY_F12"),
    ("space", "KEY_SPACE"),
    ("tab", "KEY_TAB"),
    ("esc", "KEY_ESC"),
    ("escape", "KEY_ESC"),
    ("enter", "KEY_ENTER"),
    ("return", "KEY_ENTER"),
    ("backspace", "KEY_BACKSPACE"),
    ("insert", "KEY_INSERT"),
    ("delete", "KEY_DELETE"),
    ("home", "KEY_HOME"),
    ("end", "KEY_END"),
    ("pageup", "KEY_PAGEUP"),
    ("pagedown", "KEY_PAGEDOWN"),
    ("up", "KEY_UP"),
    ("down", "KEY_DOWN"),
    ("left", "KEY_LEFT"),
    ("right", "KEY_RIGHT"),
    ("minus", "KEY_MINUS"),
    ("-", "KEY_MINUS"),
    ("equal", "KEY_EQUAL"),
    ("=", "KEY_EQUAL"),
    ("comma", "KEY_COMMA"),
    (",", "KEY_COMMA"),
    ("period", "KEY_DOT"),
    (".", "KEY_DOT"),
    ("slash", "KEY_SLASH"),
    ("/", "KEY_SLASH"),
    ("semicolon", "KEY_SEMICOLON"),
    (";", "KEY_SEMICOLON"),
    ("apostrophe", "KEY_APOSTROPHE"),
    ("grave", "KEY_GRAVE"),
    ("`", "KEY_GRAVE"),
    ("backslash", "KEY_BACKSLASH"),
    ("leftbracket", "KEY_LEFTBRACE"),
    ("[", "KEY_LEFTBRACE"),
    ("rightbracket", "KEY_RIGHTBRACE"),
    ("]", "KEY_RIGHTBRACE"),
];

/// Resolve the key half of a sequence to its kernel `KEY_*` name.
fn kernel_key_name(key_str: &str) -> Option<String> {
    let lower = key_str.to_ascii_lowercase();
    if let Some((_, name)) = NAMED_KEYS.iter().find(|(k, _)| *k == lower) {
        return Some((*name).to_string());
    }
    // Exactly one character: a Latin letter or an ASCII digit.
    let mut chars = key_str.chars();
    let c = chars.next()?.to_ascii_uppercase();
    if chars.next().is_some() || !c.is_ascii_alphanumeric() {
        return None;
    }
    Some(format!("KEY_{c}"))
}

/// Parse a sequence like "Ctrl+U" or "Ctrl+Shift+F5" into (X keycode, base
/// mods). Supports Ctrl / Shift / Alt / Super modifiers plus a single Latin
/// letter, an ASCII digit, or one of [`NAMED_KEYS`]. The keycode is the X
/// keycode: the evdev kernel code (the same constants the uinput backend
/// emits) plus the fixed offset 8 the X server layers on top — a PHYSICAL
/// key, so the hotkey works under any active keyboard layout.
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
    // evdev's `KeyCode: FromStr` accepts the kernel KEY_* names, so the
    // physical keycode comes straight from input-event-codes.h — no
    // hand-maintained keycode table (only the name aliases above).
    let keycode = kernel_key_name(key_str)?.parse::<KeyCode>().ok()?;
    Some((u8::try_from(keycode.0).ok()? + 8, base_mods))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hotkey::{NullGlobalHotkey, OPEN_DIALOG_ID, OPEN_MAIN_WINDOW_ID};

    /// The conventional layout: CapsLock=Lock(0x2), NumLock=Mod2(0x10),
    /// Alt=Mod1, Super=Mod4.
    fn test_layout() -> ModifierLayout {
        ModifierLayout {
            lock_mods: u16::from(ModMask::LOCK) | u16::from(ModMask::M2),
            alt_mod: u16::from(ModMask::M1),
            super_mod: u16::from(ModMask::M4),
        }
    }

    fn reg(id: u32, sequence: &str, layout: &ModifierLayout) -> Registration {
        let (keycode, base_mods) = parse_sequence(sequence, layout).unwrap();
        Registration {
            id,
            sequence: sequence.to_string(),
            keycode,
            base_mods,
        }
    }

    #[test]
    fn null_register_is_no_op() {
        let h = NullGlobalHotkey::new();
        h.register(OPEN_DIALOG_ID, "Ctrl+U").unwrap();
        h.unregister(OPEN_DIALOG_ID);
    }

    // ---- the busy-loop guards ------------------------------------------

    /// The line that fixes the 100%-CPU spin: a wake pipe at EOF must
    /// stop the reader, not be read forever.
    #[test]
    fn a_closed_wake_pipe_stops_the_reader() {
        let (tx, mut rx) = std::os::unix::net::UnixStream::pair().unwrap();
        rx.set_nonblocking(true).unwrap();
        drop(tx); // the owner went away
        assert!(!drain_wake_from(&mut rx), "EOF must end the loop");
    }

    #[test]
    fn an_empty_wake_pipe_keeps_going() {
        let (_tx, mut rx) = std::os::unix::net::UnixStream::pair().unwrap();
        rx.set_nonblocking(true).unwrap();
        // Nonblocking with nothing to read == WouldBlock, not EOF.
        assert!(drain_wake_from(&mut rx));
    }

    #[test]
    fn pending_wake_bytes_are_fully_drained() {
        use std::io::Write;
        let (mut tx, mut rx) = std::os::unix::net::UnixStream::pair().unwrap();
        rx.set_nonblocking(true).unwrap();
        tx.write_all(&[1u8; 200]).unwrap();
        assert!(drain_wake_from(&mut rx));
        // Everything is consumed, so a second pass sees an empty pipe
        // rather than re-waking immediately.
        assert!(drain_wake_from(&mut rx));
        let mut buf = [0u8; 1];
        assert!(matches!(
            rx.read(&mut buf).map_err(|e| e.kind()),
            Err(std::io::ErrorKind::WouldBlock)
        ));
    }

    #[test]
    fn poll_outcome_classifies_each_fd() {
        use PollOutcome::*;
        // Ordinary readability.
        assert_eq!(poll_outcome(libc::POLLIN, 0), Continue);
        assert_eq!(poll_outcome(0, libc::POLLIN), Continue);
        assert_eq!(poll_outcome(0, 0), Continue);

        // A dead X fd means reconnect, not spin.
        for bad in [libc::POLLHUP, libc::POLLERR, libc::POLLNVAL] {
            assert_eq!(poll_outcome(bad, 0), Reconnect, "x revents {bad:#x}");
            // Even alongside readable data.
            assert_eq!(poll_outcome(bad | libc::POLLIN, 0), Reconnect);
        }

        // A dead wake pipe means the owner is gone: exit.
        for bad in [libc::POLLHUP, libc::POLLERR, libc::POLLNVAL] {
            assert_eq!(poll_outcome(0, bad), Exit, "wake revents {bad:#x}");
        }

        // Both dead: exiting wins — there is nobody left to serve.
        assert_eq!(poll_outcome(libc::POLLHUP, libc::POLLHUP), Exit);
    }

    // ---- modifier matching ---------------------------------------------

    #[test]
    fn matches_a_plain_registration() {
        let layout = test_layout();
        let regs = vec![reg(OPEN_DIALOG_ID, "Ctrl+U", &layout)];
        let ctrl = u16::from(ModMask::CONTROL);
        assert_eq!(
            match_registration(&regs, regs[0].keycode, ctrl, &layout),
            Some(OPEN_DIALOG_ID)
        );
    }

    #[test]
    fn lock_bits_in_the_event_state_are_ignored() {
        let layout = test_layout();
        let regs = vec![reg(OPEN_DIALOG_ID, "Ctrl+U", &layout)];
        let ctrl = u16::from(ModMask::CONTROL);
        for lock in [
            u16::from(ModMask::LOCK),
            u16::from(ModMask::M2),
            u16::from(ModMask::LOCK) | u16::from(ModMask::M2),
        ] {
            assert_eq!(
                match_registration(&regs, regs[0].keycode, ctrl | lock, &layout),
                Some(OPEN_DIALOG_ID),
                "CapsLock/NumLock state must not break matching"
            );
        }
    }

    /// The regression that made this a free function. `KeyPress.state`
    /// carries the pointer button mask in bits 8-12, so holding a mouse
    /// button used to stop every hotkey from firing.
    #[test]
    fn pointer_button_bits_do_not_break_matching() {
        let layout = test_layout();
        let regs = vec![reg(OPEN_DIALOG_ID, "Ctrl+U", &layout)];
        let ctrl = u16::from(ModMask::CONTROL);
        for button_bit in 8..=12 {
            let state = ctrl | (1 << button_bit);
            assert_eq!(
                match_registration(&regs, regs[0].keycode, state, &layout),
                Some(OPEN_DIALOG_ID),
                "button bit {button_bit} must be masked off"
            );
        }
    }

    /// XKB reports the effective keyboard *group* in bits 13-14 of the
    /// core event state. Left unmasked, every hotkey stops matching under
    /// a second keyboard layout — defeating the layout independence this
    /// whole backend exists to provide.
    #[test]
    fn keyboard_group_bits_do_not_break_matching() {
        let layout = test_layout();
        let regs = vec![reg(OPEN_DIALOG_ID, "Ctrl+U", &layout)];
        let ctrl = u16::from(ModMask::CONTROL);
        for group in 1..=3u16 {
            let state = ctrl | (group << 13);
            assert_eq!(
                match_registration(&regs, regs[0].keycode, state, &layout),
                Some(OPEN_DIALOG_ID),
                "group {group} must be masked off"
            );
        }
    }

    #[test]
    fn a_genuinely_different_modifier_does_not_match() {
        let layout = test_layout();
        let regs = vec![reg(OPEN_DIALOG_ID, "Ctrl+U", &layout)];
        let shift_u = u16::from(ModMask::SHIFT);
        assert_eq!(
            match_registration(&regs, regs[0].keycode, shift_u, &layout),
            None
        );
        // Ctrl+Shift+U must not fire the Ctrl+U registration either.
        let both = u16::from(ModMask::CONTROL) | u16::from(ModMask::SHIFT);
        assert_eq!(
            match_registration(&regs, regs[0].keycode, both, &layout),
            None
        );
    }

    #[test]
    fn the_two_default_shortcuts_stay_distinct() {
        let layout = test_layout();
        let regs = vec![
            reg(OPEN_DIALOG_ID, "Ctrl+U", &layout),
            reg(OPEN_MAIN_WINDOW_ID, "Ctrl+Shift+U", &layout),
        ];
        let ctrl = u16::from(ModMask::CONTROL);
        let ctrl_shift = ctrl | u16::from(ModMask::SHIFT);
        let kc = regs[0].keycode;
        assert_eq!(
            match_registration(&regs, kc, ctrl, &layout),
            Some(OPEN_DIALOG_ID)
        );
        assert_eq!(
            match_registration(&regs, kc, ctrl_shift, &layout),
            Some(OPEN_MAIN_WINDOW_ID)
        );
    }

    // ---- cross-id grab ownership ---------------------------------------

    /// Swapping the two shortcuts must leave both alive. Before the
    /// ownership check, step 2 here ungrabbed the combination step 1 had
    /// just taken, silently killing Ctrl+Shift+U for the session.
    #[test]
    fn swapping_two_shortcuts_never_retires_the_others_grab() {
        let layout = test_layout();
        let combos = layout.lock_combos();

        // Start: id1 = Ctrl+U, id2 = Ctrl+Shift+U.
        let id1_old = reg(OPEN_DIALOG_ID, "Ctrl+U", &layout);
        let id2_old = reg(OPEN_MAIN_WINDOW_ID, "Ctrl+Shift+U", &layout);

        // Step 1: id1 moves onto Ctrl+Shift+U (id2 still holds it).
        let (kc1, mods1) = parse_sequence("Ctrl+Shift+U", &layout).unwrap();
        let new1: Vec<u16> = combos.iter().map(|l| mods1 | l).collect();
        let retire1 = combos_to_retire(
            &id1_old,
            kc1,
            &new1,
            &combos,
            std::slice::from_ref(&id2_old),
        );
        // It gives up its own Ctrl+U ...
        assert!(
            retire1
                .iter()
                .all(|(_, m)| m & u16::from(ModMask::SHIFT) == 0)
        );
        assert_eq!(retire1.len(), combos.len());

        // Step 2: id2 moves onto Ctrl+U, while id1 now owns Ctrl+Shift+U.
        let id1_new = reg(OPEN_DIALOG_ID, "Ctrl+Shift+U", &layout);
        let (kc2, mods2) = parse_sequence("Ctrl+U", &layout).unwrap();
        let new2: Vec<u16> = combos.iter().map(|l| mods2 | l).collect();
        let retire2 = combos_to_retire(&id2_old, kc2, &new2, &combos, &[id1_new]);
        assert!(
            retire2.is_empty(),
            "id2's old combos are all owned by id1 now; retiring them would \
             kill a live shortcut: {retire2:?}"
        );
    }

    /// The rollback path must exempt the same combinations the success
    /// path does. Releasing a combination another id owns kills a live
    /// shortcut; releasing one this id already owned leaves the
    /// registration table describing a grab that no longer exists.
    #[test]
    fn a_rollback_never_releases_a_combination_someone_still_owns() {
        let layout = test_layout();
        let combos = layout.lock_combos();
        let id2 = reg(OPEN_MAIN_WINDOW_ID, "Ctrl+Shift+U", &layout);

        // id1 is being moved onto id2's sequence and the grab fails
        // partway: every combination it managed to take is one id2 holds.
        let claimed = claimed_combos(std::slice::from_ref(&id2), &combos);
        for l in &combos {
            assert!(
                claimed.contains(&(id2.keycode, id2.base_mods | l)),
                "the guard must see every lock-variant id2 owns"
            );
        }
        // And it must not over-claim a combination nobody holds.
        let (kc, other) = parse_sequence("Alt+J", &layout).unwrap();
        assert!(!claimed.contains(&(kc, other)));
    }

    #[test]
    fn re_registering_the_same_sequence_keeps_its_grab() {
        let layout = test_layout();
        let combos = layout.lock_combos();
        let old = reg(OPEN_DIALOG_ID, "Ctrl+U", &layout);
        let (kc, mods) = parse_sequence("Ctrl+U", &layout).unwrap();
        let new: Vec<u16> = combos.iter().map(|l| mods | l).collect();
        assert!(
            combos_to_retire(&old, kc, &new, &combos, &[]).is_empty(),
            "an unchanged sequence must not release its own grab"
        );
    }

    #[test]
    fn a_changed_sequence_releases_every_old_combo() {
        let layout = test_layout();
        let combos = layout.lock_combos();
        let old = reg(OPEN_DIALOG_ID, "Ctrl+U", &layout);
        let (kc, mods) = parse_sequence("Alt+J", &layout).unwrap();
        let new: Vec<u16> = combos.iter().map(|l| mods | l).collect();
        let retire = combos_to_retire(&old, kc, &new, &combos, &[]);
        assert_eq!(retire.len(), combos.len());
        assert!(retire.iter().all(|(k, _)| *k == old.keycode));
    }

    // ---- key parsing ----------------------------------------------------

    #[test]
    fn parse_sequence_accepts_function_keys() {
        let layout = test_layout();
        for (seq, name) in [
            ("Ctrl+F1", "KEY_F1"),
            ("Ctrl+F5", "KEY_F5"),
            ("Ctrl+Shift+F12", "KEY_F12"),
        ] {
            let (keycode, _) =
                parse_sequence(seq, &layout).unwrap_or_else(|| panic!("{seq} must parse"));
            let expected = name.parse::<KeyCode>().unwrap().0 as u8 + 8;
            assert_eq!(keycode, expected, "{seq}");
        }
    }

    #[test]
    fn parse_sequence_accepts_named_and_punctuation_keys() {
        let layout = test_layout();
        for (seq, name) in [
            ("Ctrl+Space", "KEY_SPACE"),
            ("Alt+Tab", "KEY_TAB"),
            ("Ctrl+Esc", "KEY_ESC"),
            ("Ctrl+Escape", "KEY_ESC"),
            ("Ctrl+Insert", "KEY_INSERT"),
            ("Ctrl+.", "KEY_DOT"),
            ("Ctrl+Period", "KEY_DOT"),
            ("Ctrl+[", "KEY_LEFTBRACE"),
        ] {
            let (keycode, _) =
                parse_sequence(seq, &layout).unwrap_or_else(|| panic!("{seq} must parse"));
            let expected = name.parse::<KeyCode>().unwrap().0 as u8 + 8;
            assert_eq!(keycode, expected, "{seq}");
        }
    }

    /// The sequences `HotkeySettings` ships as defaults must actually
    /// parse. Nothing checked this before, so a default the platform
    /// layer rejects would only surface as a warning at startup and a
    /// silently inert shortcut — and the current defaults carry three
    /// modifiers, which no other test exercises.
    ///
    /// Mirrors `fastpaste_app::settings::HotkeySettings`; that crate
    /// depends on this one, so the strings are repeated rather than
    /// imported.
    #[test]
    fn the_shipped_default_sequences_parse() {
        let layout = test_layout();
        let dialog = parse_sequence("Ctrl+Alt+V", &layout)
            .expect("the default selection-dialog hotkey must parse");
        let main = parse_sequence("Ctrl+Alt+M", &layout)
            .expect("the default main-window hotkey must parse");

        // The defaults differ by KEY, not by modifier. A pair separated
        // only by Shift collapses wherever something normalises Shift
        // away, and the two actions become one.
        assert_ne!(dialog.0, main.0, "the defaults must differ by keycode");
        assert_eq!(
            dialog.1, main.1,
            "…and share their modifiers, so neither can shadow the other"
        );

        // Whatever the lock state, neither grab can be mistaken for the
        // other: the keycodes differ, so no combination of lock bits
        // makes them equal.
        for combo in layout.lock_combos() {
            assert_ne!((dialog.0, dialog.1 | combo), (main.0, main.1 | combo));
        }
    }

    #[test]
    fn named_keys_are_case_insensitive() {
        let layout = test_layout();
        assert_eq!(
            parse_sequence("Ctrl+f5", &layout),
            parse_sequence("Ctrl+F5", &layout)
        );
    }

    #[test]
    fn every_named_key_resolves_to_a_real_kernel_code() {
        // The table is hand-written; a typo would be a runtime
        // BadSequence for a key the options dialog advertises.
        for (alias, name) in NAMED_KEYS {
            let key = name
                .parse::<KeyCode>()
                .unwrap_or_else(|_| panic!("{alias} -> {name} is not a kernel key name"));
            assert!(
                u8::try_from(key.0).is_ok(),
                "{name} does not fit an X keycode"
            );
        }
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
