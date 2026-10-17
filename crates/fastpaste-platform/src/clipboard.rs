//! Clipboard access.
//!
//! This module holds the seam — the [`Clipboard`] trait, the payload and
//! error types, the own-write suppression list, and the null backend.
//! Per-platform implementations live in the submodules and reach the
//! rest of the app through the neutral [`crate::SystemClipboard`] alias.
//!
//! Own writes: `set_text` from this process shows up as a selection
//! change just like an external copy. The paste sequence announces each
//! write via [`Clipboard::suppress_text`] so its own writes (payload +
//! restored snapshot) are recorded in `last_seen` but not emitted to the
//! history consumer.

use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use thiserror::Error;

#[cfg(target_os = "linux")]
pub mod wayland;
#[cfg(target_os = "linux")]
pub use wayland::ArboardClipboard;

#[cfg(windows)]
pub mod windows;
#[cfg(windows)]
pub use windows::WindowsClipboard;

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
    fn unrestorable() -> Self {
        Self::Access("this clipboard backend cannot write that content back".into())
    }

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

/// Pixels taken off the clipboard: RGBA, row-major, 8 bits per channel —
/// arboard's own representation, kept in a type of ours so callers do not
/// depend on the backend crate.
#[derive(Clone, PartialEq, Eq)]
pub struct ImageSnapshot {
    pub width: usize,
    pub height: usize,
    pub rgba: Vec<u8>,
}

impl std::fmt::Debug for ImageSnapshot {
    /// Dimensions only: a screenshot is megabytes of pixels.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "ImageSnapshot({}x{}, {} bytes)",
            self.width,
            self.height,
            self.rgba.len()
        )
    }
}

/// The clipboard's contents, saved so they can be put back after a paste.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipboardSnapshot {
    Text(String),
    Image(ImageSnapshot),
    /// Files copied in a file manager.
    Files(Vec<PathBuf>),
    /// Nothing on the clipboard: nothing to lose, nothing to put back.
    Empty,
    /// Something is there that this backend cannot read back faithfully
    /// (or could not read at all). Overwriting it would lose it for good.
    Unsupported,
}

/// What a clipboard is when neither text, an image nor files could be
/// read from it. `has_other_formats` is whether it offers anything at all,
/// or `None` where the backend cannot list formats.
fn unrepresentable(has_other_formats: Option<bool>) -> ClipboardSnapshot {
    match has_other_formats {
        Some(true) => ClipboardSnapshot::Unsupported,
        Some(false) => ClipboardSnapshot::Empty,
        // Cannot tell. Calling it empty is what happened before snapshots
        // existed; calling it unsupported would refuse every paste here.
        None => ClipboardSnapshot::Empty,
    }
}

/// Snapshot through arboard: text first (the common case, and the one
/// history deals in), then an image, then a file list, and only then ask
/// the backend whether anything else is on offer.
#[cfg(any(target_os = "linux", windows))]
fn arboard_snapshot(
    clip: &mut arboard::Clipboard,
    has_other_formats: impl FnOnce() -> Option<bool>,
) -> ClipboardSnapshot {
    // A real read error means something is there that we failed to save.
    fn failed(what: &str, e: arboard::Error) -> ClipboardSnapshot {
        tracing::warn!("could not read the clipboard's {what}: {e}");
        ClipboardSnapshot::Unsupported
    }

    match clip.get_text() {
        Ok(text) => return ClipboardSnapshot::Text(text),
        Err(arboard::Error::ContentNotAvailable) => {}
        Err(e) => return failed("text", e),
    }
    match clip.get_image() {
        Ok(image) => {
            return ClipboardSnapshot::Image(ImageSnapshot {
                width: image.width,
                height: image.height,
                rgba: image.bytes.into_owned(),
            });
        }
        Err(arboard::Error::ContentNotAvailable) => {}
        Err(e) => return failed("image", e),
    }
    match clip.get().file_list() {
        Ok(files) if !files.is_empty() => return ClipboardSnapshot::Files(files),
        Ok(_) | Err(arboard::Error::ContentNotAvailable) => {}
        Err(e) => return failed("file list", e),
    }
    unrepresentable(has_other_formats())
}

#[cfg(any(target_os = "linux", windows))]
fn arboard_restore(
    clip: &mut arboard::Clipboard,
    snapshot: &ClipboardSnapshot,
) -> Result<(), ClipboardError> {
    let result = match snapshot {
        ClipboardSnapshot::Text(text) => clip.set_text(text.clone()),
        ClipboardSnapshot::Image(image) => clip.set_image(arboard::ImageData {
            width: image.width,
            height: image.height,
            bytes: std::borrow::Cow::Borrowed(&image.rgba),
        }),
        // arboard's text and image setters empty the clipboard first; its
        // Windows file-list setter does not, so whatever was there — our
        // payload, after a paste — would stay alongside the files.
        ClipboardSnapshot::Files(files) => clip.clear().and_then(|()| clip.set().file_list(files)),
        ClipboardSnapshot::Empty => Ok(()),
        ClipboardSnapshot::Unsupported => return Err(ClipboardError::unrestorable()),
    };
    result.map_err(ClipboardError::from_arboard)
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

    /// A counter the OS bumps on every clipboard change, whoever makes
    /// it, or `None` where there is no such thing.
    ///
    /// The paste sequence reads it right after writing its payload and
    /// again before putting the user's snapshot back: if it moved, someone
    /// copied in between, and that newer copy must not be overwritten.
    /// Without a sequence the caller has to compare contents instead.
    fn sequence(&self) -> Option<u64> {
        None
    }

    /// Save whatever is on the clipboard so [`Self::restore`] can put it
    /// back. The default handles text only; a read failure is
    /// [`ClipboardSnapshot::Unsupported`], since something unreadable is
    /// still something the user would lose.
    fn snapshot(&self) -> ClipboardSnapshot {
        match self.text() {
            Ok(text) => ClipboardSnapshot::Text(text),
            Err(ClipboardError::Empty) => ClipboardSnapshot::Empty,
            Err(e) => {
                tracing::warn!("could not read the clipboard: {e}");
                ClipboardSnapshot::Unsupported
            }
        }
    }

    /// Put a [`Self::snapshot`] back. `Empty` is a no-op; anything this
    /// backend cannot write is an error rather than a silent loss.
    fn restore(&self, snapshot: &ClipboardSnapshot) -> Result<(), ClipboardError> {
        match snapshot {
            ClipboardSnapshot::Text(text) => self.set_text(text),
            ClipboardSnapshot::Empty => Ok(()),
            _ => Err(ClipboardError::unrestorable()),
        }
    }
}

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
    use std::sync::Arc;
    use std::thread;

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

    // ---- snapshot / restore ---------------------------------------------

    /// A backend with nothing but `text`/`set_text`, to pin the trait's
    /// default snapshot and restore.
    struct TextOnly {
        read: fn() -> Result<String, ClipboardError>,
        written: Mutex<Vec<String>>,
    }

    impl Clipboard for TextOnly {
        fn set_text(&self, text: &str) -> Result<(), ClipboardError> {
            self.written.lock().unwrap().push(text.to_owned());
            Ok(())
        }
        fn text(&self) -> Result<String, ClipboardError> {
            (self.read)()
        }
        fn changes(&self) -> Receiver<ClipboardPayload> {
            std::sync::mpsc::channel().1
        }
    }

    fn text_only(read: fn() -> Result<String, ClipboardError>) -> TextOnly {
        TextOnly {
            read,
            written: Mutex::new(Vec::new()),
        }
    }

    #[test]
    fn default_snapshot_maps_text_empty_and_unreadable() {
        assert_eq!(
            text_only(|| Ok("t".into())).snapshot(),
            ClipboardSnapshot::Text("t".into())
        );
        assert_eq!(
            text_only(|| Err(ClipboardError::Empty)).snapshot(),
            ClipboardSnapshot::Empty
        );
        // Unreadable is not "empty": something is there that we cannot
        // save, so it must not be treated as safe to overwrite.
        assert_eq!(
            text_only(|| Err(ClipboardError::Access("locked".into()))).snapshot(),
            ClipboardSnapshot::Unsupported
        );
    }

    #[test]
    fn default_restore_writes_text_and_refuses_what_it_cannot_write() {
        let clip = text_only(|| Ok(String::new()));
        clip.restore(&ClipboardSnapshot::Text("back".into()))
            .unwrap();
        clip.restore(&ClipboardSnapshot::Empty).unwrap();
        assert_eq!(*clip.written.lock().unwrap(), vec!["back"]);

        let image = ClipboardSnapshot::Image(ImageSnapshot {
            width: 1,
            height: 1,
            rgba: vec![0, 0, 0, 255],
        });
        assert!(clip.restore(&image).is_err());
        assert!(clip.restore(&ClipboardSnapshot::Unsupported).is_err());
        assert_eq!(
            clip.written.lock().unwrap().len(),
            1,
            "nothing else written"
        );
    }

    /// Neither text, an image nor files could be read. Whether that is
    /// an empty clipboard or one holding a format we cannot represent
    /// decides between "nothing to lose" and "do not touch it".
    #[test]
    fn unreadable_formats_are_unsupported_and_none_is_empty() {
        assert_eq!(unrepresentable(Some(true)), ClipboardSnapshot::Unsupported);
        assert_eq!(unrepresentable(Some(false)), ClipboardSnapshot::Empty);
        // Cannot tell (no way to list formats here): as before this
        // existed, treat it as empty.
        assert_eq!(unrepresentable(None), ClipboardSnapshot::Empty);
    }

    #[test]
    fn image_debug_does_not_dump_the_pixels() {
        let image = ImageSnapshot {
            width: 2,
            height: 1,
            rgba: vec![7; 8],
        };
        let shown = format!("{image:?}");
        assert!(shown.contains("2x1"), "{shown}");
        assert!(!shown.contains("7, 7"), "{shown}");
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
