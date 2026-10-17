//! Wayland/X11 backend: arboard for reads and writes, and
//! `wl-clipboard-watch` for change notifications with an arboard polling
//! fallback.
//!
//! Selected as `SystemClipboard` on Linux.

use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use wl_clipboard_watch::{Config as WatchConfig, Event as WatchEvent, Transfer, Watcher};

use super::{
    Clipboard, ClipboardError, ClipboardPayload, ClipboardSnapshot, ImageSnapshot, Read,
    SuppressList,
};
use crate::take_once::TakeOnceChannel;

/// Polling interval for the legacy fallback loop (no data-control
/// protocol available).
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

    /// One MIME listing decides which single read to make. Without the
    /// data-control protocol (an X11 session) nothing can be listed: files
    /// and text are tried through arboard, and an image is not saved.
    fn snapshot(&self) -> ClipboardSnapshot {
        let mut clip = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let clip = std::cell::RefCell::new(&mut *clip);
        let files = || Read::from_arboard(clip.borrow_mut().get().file_list());
        let text = || Read::from_arboard(clip.borrow_mut().get_text());
        match list_mime_types() {
            Listing::Empty => ClipboardSnapshot::Empty,
            Listing::Offered(mimes) => {
                let offer = offer_from_mimes(mimes.iter().map(String::as_str));
                super::snapshot_in_order(
                    || if offer.files { files() } else { Read::Absent },
                    || if offer.text { text() } else { Read::Absent },
                    || offer.image.as_deref().map_or(Read::Absent, read_image),
                    || Some(offer.any),
                )
            }
            Listing::Unavailable => super::snapshot_in_order(files, text, || Read::Absent, || None),
        }
    }

    fn restore(&self, snapshot: &ClipboardSnapshot) -> Result<(), ClipboardError> {
        let mut clip = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        super::arboard_restore(&mut clip, snapshot, write_image)
    }
}

/// The clipboard's offered MIME types, sorted so a choice among them is
/// deterministic.
enum Listing {
    Offered(Vec<String>),
    Empty,
    /// No data-control protocol: there is no way to tell an empty
    /// clipboard from one holding a format we cannot read.
    Unavailable,
}

fn list_mime_types() -> Listing {
    use wl_clipboard_rs::paste::{ClipboardType, Error, Seat, get_mime_types};
    match get_mime_types(ClipboardType::Regular, Seat::Unspecified) {
        Ok(types) if types.is_empty() => Listing::Empty,
        Ok(types) => {
            let mut types: Vec<String> = types.into_iter().collect();
            types.sort();
            Listing::Offered(types)
        }
        Err(Error::ClipboardEmpty) => Listing::Empty,
        Err(e) => {
            tracing::debug!("could not list clipboard formats: {e}");
            Listing::Unavailable
        }
    }
}

/// What a MIME listing says is on the clipboard.
#[derive(Debug, Default)]
struct Offer {
    files: bool,
    text: bool,
    /// The image type to save: PNG if offered, else the first image type.
    image: Option<String>,
    any: bool,
}

fn offer_from_mimes<'a>(mimes: impl IntoIterator<Item = &'a str>) -> Offer {
    let mut offer = Offer::default();
    let mut images: Vec<&str> = Vec::new();
    for mime in mimes {
        offer.any = true;
        offer.files |= mime == "text/uri-list";
        offer.text |=
            mime.starts_with("text/plain") || matches!(mime, "UTF8_STRING" | "STRING" | "TEXT");
        if mime.starts_with("image/") {
            images.push(mime);
        }
    }
    images.sort_unstable();
    offer.image = images
        .iter()
        .find(|m| **m == "image/png")
        .or(images.first())
        .map(|m| (*m).to_owned());
    offer
}

/// Read one image type's bytes as offered, without decoding them.
fn read_image(mime: &str) -> Read<ImageSnapshot> {
    use std::io::Read as _;
    use wl_clipboard_rs::paste::{ClipboardType, Error, MimeType, Seat, get_contents};
    match get_contents(
        ClipboardType::Regular,
        Seat::Unspecified,
        MimeType::Specific(mime),
    ) {
        Ok((mut pipe, offered)) => {
            let mut bytes = Vec::new();
            match pipe.read_to_end(&mut bytes) {
                Ok(_) => Read::Got(ImageSnapshot {
                    formats: vec![(offered, bytes)],
                }),
                Err(e) => Read::Failed(e.to_string()),
            }
        }
        Err(Error::ClipboardEmpty | Error::NoMimeType) => Read::Absent,
        Err(e) => Read::Failed(e.to_string()),
    }
}

/// Offer the saved bytes again under their own MIME types. Served from a
/// background thread until something else is copied, as arboard does.
fn write_image(image: &ImageSnapshot) -> Result<(), ClipboardError> {
    use wl_clipboard_rs::copy::{MimeSource, MimeType, Options, Source};
    let sources = image
        .formats
        .iter()
        .map(|(mime, bytes)| MimeSource {
            source: Source::Bytes(bytes.clone().into_boxed_slice()),
            mime_type: MimeType::Specific(mime.clone()),
        })
        .collect();
    Options::new()
        .copy_multi(sources)
        .map_err(|e| ClipboardError::Access(Box::new(e)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn offer(mimes: &[&str]) -> Offer {
        offer_from_mimes(mimes.iter().copied())
    }

    /// One MIME listing decides which single read the snapshot makes,
    /// instead of trying text, image and files in turn (a round trip
    /// each) before even asking what is there.
    #[test]
    fn a_file_copy_offers_files_and_text() {
        let o = offer(&[
            "text/uri-list",
            "text/plain;charset=utf-8",
            "x-special/gnome-copied-files",
        ]);
        assert!(o.files && o.text && o.any);
        assert_eq!(o.image, None);
    }

    #[test]
    fn png_is_preferred_among_image_types() {
        let o = offer(&["image/jpeg", "image/png", "image/webp", "text/html"]);
        assert_eq!(o.image.as_deref(), Some("image/png"));
        assert!(!o.text && !o.files);
    }

    #[test]
    fn another_image_type_is_taken_when_there_is_no_png() {
        assert_eq!(offer(&["image/webp"]).image.as_deref(), Some("image/webp"));
        assert_eq!(
            offer(&["image/webp", "image/bmp"]).image.as_deref(),
            Some("image/bmp"),
            "deterministic: the listing is a set, so pick by name"
        );
    }

    #[test]
    fn every_text_spelling_counts_as_text() {
        for m in [
            "text/plain",
            "text/plain;charset=utf-8",
            "UTF8_STRING",
            "STRING",
            "TEXT",
        ] {
            assert!(offer(&[m]).text, "{m}");
        }
    }

    #[test]
    fn unknown_formats_are_something_and_nothing_is_nothing() {
        let o = offer(&["application/x-kde-cutselection"]);
        assert!(o.any && !o.files && !o.text && o.image.is_none());
        assert!(!offer(&[]).any);
    }
}
