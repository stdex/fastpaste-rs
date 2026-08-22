//! Paster: 4-step paste sequence (snapshot -> set payload -> wait -> Ctrl+V ->
//! restore) on Wayland. The simplest service thanks to the spike findings.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use fastpaste_platform::{Clipboard, UinputCtrlV};

use thiserror::Error;

#[derive(Error, Debug)]
pub enum PasteError {
    #[error("clipboard: {0}")]
    Clipboard(#[from] fastpaste_platform::ClipboardError),
    #[error("uinput: {0}")]
    Uinput(#[from] fastpaste_platform::UinputError),
}

pub struct Paster {
    clipboard: Arc<dyn Clipboard>,
    uinput: Arc<dyn UinputCtrlV>,
    /// Interior-mutable so the Options dialog's Apply can retune the paste
    /// sequence at runtime (the Arc<Paster> is shared with UI threads).
    delay_ms: AtomicU64,
    restore_clipboard: AtomicBool,
}

impl Paster {
    pub fn new(
        clipboard: Arc<dyn Clipboard>,
        uinput: Arc<dyn UinputCtrlV>,
        delay_ms: u64,
        restore_clipboard: bool,
    ) -> Self {
        Self {
            clipboard,
            uinput,
            delay_ms: AtomicU64::new(delay_ms),
            restore_clipboard: AtomicBool::new(restore_clipboard),
        }
    }

    /// Update the tunables (Options-dialog Apply).
    pub fn set_config(&self, delay_ms: u64, restore_clipboard: bool) {
        self.delay_ms.store(delay_ms, Ordering::Release);
        self.restore_clipboard
            .store(restore_clipboard, Ordering::Release);
    }

    pub fn paste_text(&self, payload: &str) -> Result<(), PasteError> {
        let delay_ms = self.delay_ms.load(Ordering::Acquire);
        let restore = self.restore_clipboard.load(Ordering::Acquire);
        // Our own writes (the payload, and the restored snapshot below)
        // look like clipboard changes to the watcher. Announce them so
        // the backend records the new state but doesn't report the
        // paste to the history consumer as a user copy. With
        // restore_clipboard there are exactly two writes, without it one.
        self.clipboard
            .suppress_next_changes(if restore { 2 } else { 1 });

        let snapshot = if restore {
            self.clipboard.text().ok()
        } else {
            None
        };
        self.clipboard.set_text(payload)?;
        if delay_ms > 0 {
            std::thread::sleep(Duration::from_millis(delay_ms));
        }
        if self.uinput.available() {
            self.uinput.send_ctrl_v()?;
        } else {
            tracing::warn!("/dev/uinput unavailable; payload left on clipboard, user must Ctrl+V");
        }
        if let Some(snap) = snapshot {
            self.clipboard.set_text(&snap)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fastpaste_platform::{Clipboard, ClipboardError, ClipboardPayload, NullClipboard};
    use std::sync::Mutex;

    /// A UinputCtrlV impl that counts `send_ctrl_v` calls. Used to assert
    /// that paste fires it.
    struct CountingUinput {
        count: Mutex<u32>,
        available: bool,
    }
    impl CountingUinput {
        fn new(available: bool) -> Self {
            Self {
                count: Mutex::new(0),
                available,
            }
        }
        fn calls(&self) -> u32 {
            *self.count.lock().unwrap()
        }
    }
    impl UinputCtrlV for CountingUinput {
        fn available(&self) -> bool {
            self.available
        }
        fn send_ctrl_v(&self) -> Result<(), fastpaste_platform::UinputError> {
            *self.count.lock().unwrap() += 1;
            Ok(())
        }
    }

    #[test]
    fn paste_sets_clipboard_then_emits_ctrl_v() {
        let clip = Arc::new(NullClipboard::new());
        let uinput = Arc::new(CountingUinput::new(true));
        // Pre-set something so we can verify restore.
        clip.set_text("orig").unwrap();

        let paster = Paster::new(
            clip.clone(),
            uinput.clone(),
            /*delay=*/ 0,
            /*restore=*/ true,
        );
        paster.paste_text("payload").unwrap();

        assert_eq!(uinput.calls(), 1, "send_ctrl_v must fire exactly once");
        // After paste with restore=true, the clipboard must hold the snapshot.
        assert_eq!(clip.text().unwrap(), "orig");
    }

    #[test]
    fn paste_without_restore_leaves_payload_on_clipboard() {
        let clip = Arc::new(NullClipboard::new());
        let uinput = Arc::new(CountingUinput::new(true));

        let paster = Paster::new(clip.clone(), uinput.clone(), 0, /*restore=*/ false);
        paster.paste_text("payload").unwrap();

        assert_eq!(clip.text().unwrap(), "payload");
        assert_eq!(uinput.calls(), 1);
    }

    #[test]
    fn paste_with_unavailable_uinput_skips_ctrl_v_and_warns() {
        let clip = Arc::new(NullClipboard::new());
        let uinput = Arc::new(CountingUinput::new(/*available=*/ false));

        let paster = Paster::new(clip.clone(), uinput.clone(), 0, /*restore=*/ false);
        paster.paste_text("payload").unwrap();

        assert_eq!(
            uinput.calls(),
            0,
            "must NOT fire send_ctrl_v when unavailable"
        );
        assert_eq!(
            clip.text().unwrap(),
            "payload",
            "payload must still be on clipboard for manual Ctrl+V"
        );
    }

    /// A Clipboard that records `suppress_next_changes` calls, so tests
    /// can pin the paste sequence's suppression contract.
    struct RecordingClipboard {
        inner: NullClipboard,
        suppress_calls: Mutex<Vec<u32>>,
    }

    impl RecordingClipboard {
        fn new() -> Self {
            Self {
                inner: NullClipboard::new(),
                suppress_calls: Mutex::new(Vec::new()),
            }
        }
        fn suppress_calls(&self) -> Vec<u32> {
            self.suppress_calls.lock().unwrap().clone()
        }
    }

    impl Clipboard for RecordingClipboard {
        fn set_text(&self, text: &str) -> Result<(), ClipboardError> {
            self.inner.set_text(text)
        }
        fn text(&self) -> Result<String, ClipboardError> {
            self.inner.text()
        }
        fn changes(&self) -> std::sync::mpsc::Receiver<ClipboardPayload> {
            self.inner.changes()
        }
        fn suppress_next_changes(&self, n: u32) {
            self.suppress_calls.lock().unwrap().push(n);
        }
    }

    /// The paste sequence must suppress exactly its own writes: two with
    /// restore (payload + snapshot), one without. Otherwise the event-
    /// driven clipboard watcher would record every paste into history as
    /// if the user had copied it.
    #[test]
    fn paste_suppresses_own_clipboard_writes() {
        let clip = Arc::new(RecordingClipboard::new());
        let uinput = Arc::new(CountingUinput::new(true));

        let with_restore = Paster::new(clip.clone(), uinput.clone(), 0, true);
        with_restore.paste_text("payload").unwrap();
        assert_eq!(clip.suppress_calls(), vec![2], "payload + snapshot writes");

        clip.suppress_calls.lock().unwrap().clear();

        let without_restore = Paster::new(clip.clone(), uinput.clone(), 0, false);
        without_restore.paste_text("payload").unwrap();
        assert_eq!(
            clip.suppress_calls(),
            vec![1],
            "payload stays, no restore write"
        );
    }
}
