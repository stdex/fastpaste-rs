//! Wayland clipboard access.
//!
//! Change detection is event-driven: `wl-clipboard-watch` binds the
//! compositor's `ext-data-control-v1` (falling back to
//! `wlr-data-control-v1`) and blocks in `next_event()`, so every
//! selection change is delivered exactly once with no polling latency
//! and no missed intermediate copies (the old 500ms poll lost a copy
//! that was replaced within one interval). The watcher also reads the
//! content itself (`Watcher::receive` is bound to the selection that
//! fired, and reports `Stale` if a newer selection superseded it).
//! (wl-clipboard-watch was kept for watching after verifying that
//! wl-clipboard-rs — which arboard's wayland-data-control feature pulls
//! in — exposes no watch API in any published version.)
//!
//! Reads and writes go through arboard with its `wayland-data-control`
//! feature enabled: on a Wayland session they use the same native
//! data-control protocol as the watcher (each `set_text` serves the
//! selection from a short-lived background thread until replaced).
//! When the compositor offers no data-control protocol (X11/XWayland,
//! exotic setups), arboard falls back to its X11 backend and this
//! module degrades watching to the legacy polling loop. In practice the
//! same condition — data-control availability — drives both, but arboard
//! and `wl-clipboard-watch` probe independently, so nothing here
//! *enforces* that they agree; the suppression mechanism below is
//! designed to be correct either way.
//!
//! Own writes: `set_text` from this process shows up as a selection
//! change just like an external copy. The paste sequence announces each
//! write via [`Clipboard::suppress_text`] so its own writes (payload +
//! restored snapshot) are recorded in `last_seen` but not emitted to the
//! history consumer.

use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use thiserror::Error;
use wl_clipboard_watch::{Config as WatchConfig, Event as WatchEvent, Transfer, Watcher};

use crate::take_once::TakeOnceChannel;

#[derive(Error, Debug)]
pub enum ClipboardError {
    /// Nothing readable is on the clipboard: it is empty, or the owner
    /// offers no text format. Distinct from [`Self::Access`] because a
    /// caller snapshotting the clipboard before overwriting it must treat
    /// "there was nothing to save" differently from "the read broke".
    #[error("clipboard is empty or holds no text")]
    Empty,

    #[error("clipboard access failed: {0}")]
    Access(#[source] Box<dyn std::error::Error + Send + Sync>),
}

impl ClipboardError {
    fn from_arboard(e: arboard::Error) -> Self {
        match e {
            arboard::Error::ContentNotAvailable => Self::Empty,
            other => Self::Access(Box::new(other)),
        }
    }
}

/// One observed clipboard change. `source_process` is empty when the
/// owning process can't be determined (always empty here — the
/// data-control protocols don't expose the source client's PID).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardPayload {
    pub text: String,
    /// Empty string when unknown; kept in the trait surface so a future
    /// backend that can introspect the source can fill it in.
    pub source_process: String,
}

/// Clipboard read/write plus a `changes()` channel for history.
pub trait Clipboard: Send + Sync {
    fn set_text(&self, text: &str) -> Result<(), ClipboardError>;
    fn text(&self) -> Result<String, ClipboardError>;
    /// Channel that receives a [`ClipboardPayload`] whenever the system
    /// clipboard contents change. Receivers can't be cloned, so the
    /// implementation returns its single receiver on the **first** call
    /// and a fresh empty channel thereafter (mirrors
    /// `X11GlobalHotkey::events`).
    fn changes(&self) -> Receiver<ClipboardPayload>;

    /// Announce that `text` is about to be written to the clipboard by
    /// this process, so the resulting change is not reported through
    /// [`Self::changes`] as a user copy.
    ///
    /// Content-addressed, and expiring, on purpose. The previous design
    /// was a plain counter, which desynchronised permanently in two
    /// reachable ways: a write that was announced but never happened (an
    /// error path, or a snapshot that turned out to be empty) left the
    /// count armed forever, and the polling fallback can miss a
    /// set-then-restore round trip entirely — it observes no net change,
    /// consumes nothing, and leaves *both* arms standing. Either way the
    /// leftovers silently swallowed the user's next real copies, with a
    /// symptom ("history sometimes misses a copy") that is close to
    /// undiagnosable. Matching on the text means a miss can only ever
    /// swallow an identical copy, and the TTL means it self-corrects.
    ///
    /// Backends without change introspection ignore this.
    fn suppress_text(&self, _text: &str) {}
}

/// Polling interval for the legacy fallback loop (no data-control
/// protocol available). 500ms matches the C++ reference's polling
/// cadence.
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Upper bound on a single clipboard text received by the watcher.
/// Histories and snippets don't need more; the cap keeps a pathological
/// copy from stalling the watcher thread or allocating unbounded memory.
const MAX_TEXT_BYTES: usize = 4 * 1024 * 1024;

/// How long the watcher waits for the source client to transfer the
/// offered data before giving up on that selection.
const TRANSFER_TIMEOUT: Duration = Duration::from_secs(5);

/// Delay before reconnecting the watcher after a connection error (the
/// compositor may be restarting).
const WATCH_RECONNECT_DELAY: Duration = Duration::from_secs(1);

/// How long an announced own-write stays armed. Comfortably longer than
/// the paste sequence (set → delay → Ctrl+V → restore) and short enough
/// that an announcement the backend never observed expires long before
/// the user copies something it could wrongly match.
const SUPPRESS_TTL: Duration = Duration::from_secs(2);

/// Texts this process is about to write, each with its expiry.
///
/// Small by construction: at most two entries live at a time (a paste's
/// payload and the restored snapshot).
#[derive(Debug, Default)]
struct SuppressList {
    entries: Mutex<Vec<(String, Instant)>>,
}

impl SuppressList {
    fn arm(&self, text: &str) {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        entries.retain(|(_, expiry)| *expiry > now);
        entries.push((text.to_owned(), now + SUPPRESS_TTL));
    }

    /// Consume an armed entry matching `text`, reporting whether one was
    /// found. Expired entries are dropped on the way past.
    fn take(&self, text: &str) -> bool {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        entries.retain(|(_, expiry)| *expiry > now);
        match entries.iter().position(|(t, _)| t == text) {
            Some(i) => {
                entries.remove(i);
                true
            }
            None => false,
        }
    }

    #[cfg(test)]
    fn armed_count(&self) -> usize {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        entries.retain(|(_, expiry)| *expiry > now);
        entries.len()
    }
}

/// MIME types we accept as "text", most preferred first. KWin and
/// friends usually offer all of them for a plain copy.
const TEXT_MIMES: &[&str] = &[
    "text/plain;charset=utf-8",
    "text/plain;charset=UTF-8",
    "text/plain",
    "UTF8_STRING",
];

/// arboard::Clipboard is not Sync by itself (it owns a connection to the
/// compositor). We wrap in an `Arc<Mutex<...>>` so the polling fallback
/// can share it with `set_text`/`text` callers.
pub struct ArboardClipboard {
    inner: Arc<Mutex<arboard::Clipboard>>,
    /// Change-event source; take-once receiver handout (see [`Clipboard::
    /// changes`]). The watcher/polling thread holds a sender clone.
    changes: TakeOnceChannel<ClipboardPayload>,
    /// Own-writes to swallow; see [`Clipboard::suppress_text`].
    suppress: Arc<SuppressList>,
}

/// Build the watcher config from the constants above. `Config::new` only
/// fails on zero limits, which our constants preclude.
fn watch_config() -> WatchConfig {
    WatchConfig::new(MAX_TEXT_BYTES, TRANSFER_TIMEOUT).expect("limits are non-zero constants")
}

impl ArboardClipboard {
    pub fn new() -> Result<Self, ClipboardError> {
        let clip = arboard::Clipboard::new().map_err(|e| ClipboardError::Access(Box::new(e)))?;
        let inner = Arc::new(Mutex::new(clip));

        let last_seen = Arc::new(Mutex::new(None::<String>));
        let suppress = Arc::new(SuppressList::default());
        let changes = TakeOnceChannel::new();

        match Watcher::connect_with(watch_config()) {
            Ok(watcher) => {
                let thread_tx = changes.sender();
                let thread_last = Arc::clone(&last_seen);
                let thread_suppress = Arc::clone(&suppress);
                thread::Builder::new()
                    .name("clipboard-watcher".into())
                    .spawn(move || {
                        Self::watch_loop(watcher, thread_last, thread_tx, thread_suppress)
                    })
                    .map_err(|e| ClipboardError::Access(Box::new(e)))?;
            }
            Err(e) => {
                // No data-control protocol (X11/XWayland or a compositor
                // without it) — degrade to the legacy poll loop. Same
                // `changes()` contract, but with up to POLL_INTERVAL
                // latency and the possibility of missing a copy that is
                // replaced within one interval.
                tracing::warn!(
                    "wl-clipboard-watch unavailable ({e}); \
                     falling back to {POLL_INTERVAL:?} polling"
                );
                // Seed last-seen so we don't immediately re-emit whatever
                // is already on the clipboard at startup as a "change".
                // If the clipboard is currently unreadable (rare; e.g. no
                // owner), treat it as empty — first real content still
                // fires.
                *last_seen.lock().unwrap_or_else(|e| e.into_inner()) = inner
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get_text()
                    .ok();
                let thread_inner = Arc::clone(&inner);
                let thread_last = Arc::clone(&last_seen);
                let thread_suppress = Arc::clone(&suppress);
                let thread_tx = changes.sender();
                thread::Builder::new()
                    .name("clipboard-poller".into())
                    .spawn(move || {
                        Self::poll_loop(thread_inner, thread_last, thread_tx, thread_suppress)
                    })
                    .map_err(|e| ClipboardError::Access(Box::new(e)))?;
            }
        }

        Ok(Self {
            inner,
            changes,
            suppress,
        })
    }

    /// Event-driven loop: one iteration per clipboard selection change.
    ///
    /// The first selection event seeds `last_seen` without emitting (the
    /// data-control device reports the clipboard's current state on
    /// bind — the equivalent of the poll loop's startup seed), so an
    /// app start doesn't record whatever was already copied. A `Cleared`
    /// event resets `last_seen`, meaning re-copying previously copied
    /// text after a clear is reported again (a fresh user action).
    ///
    /// On a fatal connection error the loop logs, waits, and reconnects
    /// — a restarted compositor shouldn't kill history capture forever.
    fn watch_loop(
        mut watcher: Watcher,
        last_seen: Arc<Mutex<Option<String>>>,
        tx: std::sync::mpsc::Sender<ClipboardPayload>,
        suppress: Arc<SuppressList>,
    ) {
        let mut first = true;
        loop {
            let event = match watcher.next_event() {
                Ok(e) => e,
                Err(e) => {
                    tracing::error!("clipboard-watcher: {e}; reconnecting");
                    thread::sleep(WATCH_RECONNECT_DELAY);
                    match Watcher::connect_with(watch_config()) {
                        Ok(w) => {
                            watcher = w;
                            // A fresh connection replays the current
                            // selection on bind. Treat that as a seed
                            // rather than a user copy, exactly as at
                            // startup — otherwise a compositor restart
                            // records whatever happened to be copied.
                            first = true;
                        }
                        Err(e) => {
                            tracing::error!("clipboard-watcher: reconnect failed: {e}");
                            // Keep the old watcher rather than calling
                            // next_event() on a connection we know is
                            // dead; the next iteration retries after the
                            // same delay.
                        }
                    }
                    continue;
                }
            };
            match event {
                WatchEvent::Selection(selection) => {
                    let text = read_text(&mut watcher, &selection);
                    if first {
                        first = false;
                        *last_seen.lock().unwrap_or_else(|e| e.into_inner()) = text;
                        continue;
                    }
                    let Some(text) = text else { continue };

                    // Own write (paste sequence)? Record the new state so
                    // the diff stays correct, but don't emit. Resetting
                    // last_seen on the way out means the next external
                    // copy always reports, even if its text equals the
                    // restored snapshot.
                    if suppress.take(&text) {
                        *last_seen.lock().unwrap_or_else(|e| e.into_inner()) = None;
                        tracing::debug!(
                            "clipboard-watcher: swallowed own write ({} chars)",
                            text.chars().count()
                        );
                        continue;
                    }

                    let mut last = last_seen.lock().unwrap_or_else(|e| e.into_inner());
                    if last.as_deref() != Some(text.as_str()) {
                        *last = Some(text.clone());
                        drop(last);
                        // Send is non-blocking: fails only if the
                        // receiver was dropped (consumer gone). Ignore.
                        let _ = tx.send(ClipboardPayload {
                            text,
                            source_process: String::new(),
                        });
                    }
                }
                WatchEvent::Cleared => {
                    first = false;
                    *last_seen.lock().unwrap_or_else(|e| e.into_inner()) = None;
                }
            }
        }
    }

    /// Legacy polling loop (no data-control protocol). Reads `get_text()`
    /// every [`POLL_INTERVAL`]; on a change from `last_seen`, sends the
    /// new payload and updates `last_seen`. Transient read errors are
    /// logged and treated as "unchanged".
    fn poll_loop(
        inner: Arc<Mutex<arboard::Clipboard>>,
        last_seen: Arc<Mutex<Option<String>>>,
        tx: std::sync::mpsc::Sender<ClipboardPayload>,
        suppress: Arc<SuppressList>,
    ) {
        loop {
            thread::sleep(POLL_INTERVAL);
            let current = {
                // Hold the lock only for the read; release before sending
                // so a slow consumer (or a `set_text` racing in from the
                // main thread) isn't blocked.
                //
                // A poisoned lock means some other thread panicked while
                // holding it. arboard's own state is not left half-written
                // by that (the guard wraps whole calls), and killing
                // history capture for the rest of the session is the worse
                // outcome, so recover the guard and carry on.
                let mut clip = inner.lock().unwrap_or_else(|e| e.into_inner());
                match clip.get_text() {
                    Ok(t) => Some(t),
                    Err(e) => {
                        tracing::trace!("clipboard-poller: get_text: {e}");
                        // Don't update last_seen; treat as no change.
                        None
                    }
                }
            };
            let Some(text) = current else { continue };

            if suppress.take(&text) {
                // Record the text, do NOT reset to None. The watcher can
                // reset (it is edge-triggered, so its next event is by
                // definition a real change), but this loop re-reads the
                // same clipboard every POLL_INTERVAL: clearing `last_seen`
                // meant the very next tick saw the identical text against
                // `None` and reported it as a fresh user copy. With
                // restore_clipboard on, that inserted the user's previous
                // clipboard into history on every single paste — exactly
                // the leak suppression exists to prevent.
                *last_seen.lock().unwrap_or_else(|e| e.into_inner()) = Some(text);
                continue;
            }

            let changed = {
                let mut last = last_seen.lock().unwrap_or_else(|e| e.into_inner());
                if last.as_deref() != Some(text.as_str()) {
                    *last = Some(text.clone());
                    true
                } else {
                    false
                }
            };
            if changed {
                let _ = tx.send(ClipboardPayload {
                    text,
                    source_process: String::new(),
                });
            }
        }
    }
}

/// Read the text of a selection, preferring UTF-8 MIME types. Returns
/// `None` when the selection offers no known text MIME, the transfer
/// went stale (a newer selection superseded it — that one will produce
/// its own event), or the transfer itself failed.
///
/// A completed transfer always yields `Some`: invalid UTF-8 is decoded
/// lossily rather than rejected (clipboard text is UTF-8 in practice on
/// Wayland, and dropping a mostly-readable copy is worse than a
/// replacement character).
fn read_text(watcher: &mut Watcher, selection: &wl_clipboard_watch::Selection) -> Option<String> {
    let mime = TEXT_MIMES.iter().find(|m| selection.offers(m))?;
    let bytes = match watcher.receive(selection, mime) {
        Ok(Transfer::Complete(bytes)) => bytes,
        Ok(Transfer::Stale) => {
            tracing::trace!("clipboard-watcher: transfer stale; skipping");
            return None;
        }
        Err(e) => {
            tracing::debug!("clipboard-watcher: receive {mime}: {e}");
            return None;
        }
    };
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

impl Clipboard for ArboardClipboard {
    fn set_text(&self, text: &str) -> Result<(), ClipboardError> {
        // Recover rather than panic: these run on the UI thread, and a
        // panic inside a Slint callback unwinds through the event loop and
        // takes the process down. The poller is the thread most likely to
        // poison this lock, which makes an `expect` here a crash triggered
        // by an unrelated background failure.
        let mut clip = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        clip.set_text(text.to_owned())
            .map_err(ClipboardError::from_arboard)?;
        Ok(())
    }

    fn text(&self) -> Result<String, ClipboardError> {
        let mut clip = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        clip.get_text().map_err(ClipboardError::from_arboard)
    }

    fn changes(&self) -> Receiver<ClipboardPayload> {
        // Take-once via the shared helper; first caller gets the real
        // receiver (the watcher / polling thread's sender stays alive via
        // its clone).
        self.changes.take()
    }

    fn suppress_text(&self, text: &str) {
        self.suppress.arm(text);
    }
}

/// Null impl for tests.
///
/// `changes()` returns a fresh empty channel (forever). Tests that need
/// to drive a [`ClipboardHistory`] through the `changes()` flow should
/// construct their own channel and feed the history directly rather
/// than going through this Null.
pub struct NullClipboard {
    pub last_set: Mutex<Option<String>>,
}

impl NullClipboard {
    pub fn new() -> Self {
        Self {
            last_set: Mutex::new(None),
        }
    }
    pub fn last_set_text(&self) -> Option<String> {
        self.last_set
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

impl Default for NullClipboard {
    fn default() -> Self {
        Self::new()
    }
}

impl Clipboard for NullClipboard {
    fn set_text(&self, text: &str) -> Result<(), ClipboardError> {
        *self.last_set.lock().unwrap_or_else(|e| e.into_inner()) = Some(text.to_owned());
        Ok(())
    }
    fn text(&self) -> Result<String, ClipboardError> {
        Ok(self
            .last_set
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .unwrap_or_default())
    }
    fn changes(&self) -> Receiver<ClipboardPayload> {
        let (_tx, rx) = std::sync::mpsc::channel();
        rx
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_clipboard_round_trips() {
        let clip = NullClipboard::new();
        clip.set_text("hello").unwrap();
        assert_eq!(clip.last_set_text(), Some("hello".to_string()));
        assert_eq!(clip.text().unwrap(), "hello");
    }

    #[test]
    fn null_clipboard_default_empty() {
        let clip = NullClipboard::new();
        assert_eq!(clip.text().unwrap(), "");
    }

    /// `changes()` on NullClipboard must never deliver (it's a fresh
    /// empty channel). We poll with a tiny timeout to keep the test fast.
    #[test]
    fn null_clipboard_changes_is_silent() {
        let clip = NullClipboard::new();
        let rx = clip.changes();
        if let Ok(p) = rx.recv_timeout(Duration::from_millis(50)) {
            panic!("NullClipboard changes() unexpectedly delivered: {p:?}");
        }
    }

    /// The default `suppress_text` on NullClipboard is a no-op — pinned
    /// so the trait default stays optional for backends without change
    /// introspection.
    #[test]
    fn null_clipboard_suppress_is_no_op() {
        let clip = NullClipboard::new();
        clip.suppress_text("payload");
        // No state to observe; just must not panic and must not affect
        // the other operations.
        clip.set_text("x").unwrap();
        assert_eq!(clip.text().unwrap(), "x");
    }

    // ---- SuppressList ---------------------------------------------------

    #[test]
    fn suppression_matches_by_content() {
        let s = SuppressList::default();
        s.arm("payload");
        assert!(
            !s.take("something the user copied"),
            "a foreign copy is reported"
        );
        assert!(s.take("payload"), "our own write is swallowed");
        assert!(!s.take("payload"), "and only once");
    }

    #[test]
    fn suppression_handles_the_full_paste_pair() {
        // The real sequence: announce the payload, then the snapshot.
        let s = SuppressList::default();
        s.arm("snippet text");
        s.arm("what the user had before");
        assert!(s.take("snippet text"));
        assert!(s.take("what the user had before"));
        assert_eq!(s.armed_count(), 0);
    }

    /// The regression this design exists to prevent: an announcement the
    /// backend never observes must not swallow an unrelated later copy.
    /// Under the old counter it did — permanently.
    #[test]
    fn an_unobserved_announcement_does_not_eat_a_later_copy() {
        let s = SuppressList::default();
        s.arm("payload");
        s.arm("snapshot");
        // The polling fallback misses the round trip entirely: neither
        // announced write is ever seen.
        assert!(!s.take("a completely different thing the user copied"));
        assert!(!s.take("and another"));
    }

    #[test]
    fn identical_writes_need_one_arm_each() {
        let s = SuppressList::default();
        s.arm("same");
        s.arm("same");
        assert!(s.take("same"));
        assert!(s.take("same"));
        assert!(!s.take("same"));
    }

    #[test]
    fn expired_announcements_are_dropped() {
        let s = SuppressList::default();
        {
            let mut entries = s.entries.lock().unwrap();
            // Arm one that expired a moment ago.
            entries.push((
                "stale".to_string(),
                Instant::now() - Duration::from_millis(1),
            ));
        }
        assert_eq!(s.armed_count(), 0, "expiry is enforced on inspection");
        assert!(!s.take("stale"), "and an expired entry never suppresses");
    }

    #[test]
    fn suppression_is_shared_across_threads() {
        let s = Arc::new(SuppressList::default());
        s.arm("from the main thread");
        let s2 = Arc::clone(&s);
        let consumed = thread::spawn(move || s2.take("from the main thread"))
            .join()
            .unwrap();
        assert!(consumed, "the watcher thread sees what the paste armed");
    }

    /// Payload equality is structural — important for tests that compare
    /// captured payloads against expectations.
    #[test]
    fn payload_equality_is_structural() {
        let a = ClipboardPayload {
            text: "x".to_string(),
            source_process: String::new(),
        };
        let b = ClipboardPayload {
            text: "x".to_string(),
            source_process: String::new(),
        };
        let c = ClipboardPayload {
            text: "y".to_string(),
            source_process: String::new(),
        };
        assert_eq!(a, b);
        assert_ne!(a, c);
    }
}
