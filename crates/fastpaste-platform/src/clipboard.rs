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
//! module degrades watching to the legacy polling loop — one condition
//! (data-control availability) decides both transports, so the watcher
//! never goes native while writes take the XWayland bridge.
//!
//! Own writes: `set_text` from this process shows up as a selection
//! change just like an external copy. The paste sequence announces
//! itself via [`Clipboard::suppress_next_changes`] so its writes
//! (payload + restored snapshot) are recorded in `last_seen` but not
//! emitted to the history consumer.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use thiserror::Error;
use wl_clipboard_watch::{Config as WatchConfig, Event as WatchEvent, Transfer, Watcher};

use crate::take_once::TakeOnceChannel;

#[derive(Error, Debug)]
pub enum ClipboardError {
    #[error("clipboard access failed: {0}")]
    Access(#[source] Box<dyn std::error::Error + Send + Sync>),
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

    /// Declare that the next `n` clipboard changes will originate from
    /// this process's own writes (e.g. the paste sequence writes the
    /// payload and then restores the snapshot) and must not be reported
    /// through `changes()` as user copies. Backends without change
    /// introspection ignore this.
    fn suppress_next_changes(&self, _n: u32) {}
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
    /// Remaining own-writes to swallow; see [`Clipboard::
    /// suppress_next_changes`].
    suppress: Arc<AtomicU32>,
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
        let suppress = Arc::new(AtomicU32::new(0));
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
                *last_seen.lock().expect("last_seen poisoned") = inner
                    .lock()
                    .expect("arboard mutex poisoned")
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
        suppress: Arc<AtomicU32>,
    ) {
        let mut first = true;
        loop {
            let event = match watcher.next_event() {
                Ok(e) => e,
                Err(e) => {
                    tracing::error!("clipboard-watcher: {e}; reconnecting");
                    thread::sleep(WATCH_RECONNECT_DELAY);
                    match Watcher::connect_with(watch_config()) {
                        Ok(w) => watcher = w,
                        Err(e) => {
                            tracing::error!("clipboard-watcher: reconnect failed: {e}");
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
                        *last_seen.lock().expect("last_seen poisoned") = text;
                        continue;
                    }
                    let Some(text) = text else { continue };

                    // Own write (paste sequence)? Record the new state so
                    // the diff stays correct, but don't emit. Resetting
                    // last_seen on the way out means the next external
                    // copy always reports, even if its text equals the
                    // restored snapshot.
                    if take_suppression(&suppress) {
                        *last_seen.lock().expect("last_seen poisoned") = None;
                        tracing::debug!(
                            "clipboard-watcher: swallowed own write ({})",
                            text.chars().count()
                        );
                        continue;
                    }

                    let mut last = last_seen.lock().expect("last_seen poisoned");
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
                    *last_seen.lock().expect("last_seen poisoned") = None;
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
        suppress: Arc<AtomicU32>,
    ) {
        loop {
            thread::sleep(POLL_INTERVAL);
            let current = {
                // Hold the lock only for the read; release before sending
                // so a slow consumer (or a `set_text` racing in from the
                // main thread) isn't blocked.
                let mut clip = match inner.lock() {
                    Ok(g) => g,
                    Err(_) => {
                        // Mutex poisoned — unrecoverable for this thread.
                        tracing::error!("clipboard-poller: arboard mutex poisoned; exiting");
                        return;
                    }
                };
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

            if take_suppression(&suppress) {
                *last_seen.lock().expect("last_seen poisoned") = None;
                continue;
            }

            let changed = {
                let mut last = last_seen.lock().expect("last_seen poisoned");
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
/// its own event), or the data isn't valid UTF-8 (lossy in that case;
/// clipboard text is UTF-8 in practice on Wayland).
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

/// Decrement the suppression counter if it's positive; returns whether a
/// suppression was consumed.
fn take_suppression(suppress: &AtomicU32) -> bool {
    suppress
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1))
        .is_ok()
}

impl Clipboard for ArboardClipboard {
    fn set_text(&self, text: &str) -> Result<(), ClipboardError> {
        let mut clip = self.inner.lock().expect("arboard mutex poisoned");
        clip.set_text(text.to_owned())
            .map_err(|e| ClipboardError::Access(Box::new(e)))?;
        Ok(())
    }

    fn text(&self) -> Result<String, ClipboardError> {
        let mut clip = self.inner.lock().expect("arboard mutex poisoned");
        clip.get_text()
            .map_err(|e| ClipboardError::Access(Box::new(e)))
    }

    fn changes(&self) -> Receiver<ClipboardPayload> {
        // Take-once via the shared helper; first caller gets the real
        // receiver (the watcher / polling thread's sender stays alive via
        // its clone).
        self.changes.take()
    }

    fn suppress_next_changes(&self, n: u32) {
        if n > 0 {
            self.suppress.fetch_add(n, Ordering::AcqRel);
        }
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
        self.last_set.lock().unwrap().clone()
    }
}

impl Default for NullClipboard {
    fn default() -> Self {
        Self::new()
    }
}

impl Clipboard for NullClipboard {
    fn set_text(&self, text: &str) -> Result<(), ClipboardError> {
        *self.last_set.lock().unwrap() = Some(text.to_owned());
        Ok(())
    }
    fn text(&self) -> Result<String, ClipboardError> {
        Ok(self.last_set.lock().unwrap().clone().unwrap_or_default())
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

    /// The default `suppress_next_changes` on NullClipboard is a no-op —
    /// pinned so the trait default stays optional for backends without
    /// change introspection.
    #[test]
    fn null_clipboard_suppress_is_no_op() {
        let clip = NullClipboard::new();
        clip.suppress_next_changes(2);
        // No state to observe; just must not panic and must not affect
        // the other operations.
        clip.set_text("x").unwrap();
        assert_eq!(clip.text().unwrap(), "x");
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
