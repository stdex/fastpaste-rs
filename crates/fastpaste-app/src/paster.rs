//! Paster: 4-step paste sequence (snapshot -> set payload -> wait -> Ctrl+V ->
//! restore) on Wayland. The simplest service thanks to the spike findings.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use fastpaste_platform::{Clipboard, PasteKeys};

use thiserror::Error;

#[derive(Error, Debug)]
pub enum PasteError {
    #[error("clipboard: {0}")]
    Clipboard(#[from] fastpaste_platform::ClipboardError),
    #[error("uinput: {0}")]
    Uinput(#[from] fastpaste_platform::PasteKeyError),
}

pub struct Paster {
    clipboard: Arc<dyn Clipboard>,
    uinput: Arc<dyn PasteKeys>,
    /// Interior-mutable so the Options dialog's Apply can retune the paste
    /// sequence at runtime (the Arc<Paster> is shared with UI threads).
    delay_ms: AtomicU64,
    restore_clipboard: AtomicBool,
    /// Guards a whole paste sequence.
    ///
    /// The GUI spawns a fresh worker thread per invocation, so a
    /// double-tapped hotkey used to interleave two sequences: A snapshots
    /// `orig`, A writes `payloadA`, **B snapshots `payloadA`**, A restores
    /// `orig`, B writes `payloadB`, B restores `payloadA`. The user's
    /// original clipboard was gone and a snippet was left in its place.
    ///
    /// Taken with `try_lock`, never blocking: a second request while one
    /// is in flight is *dropped*, not queued. A double-tap means one
    /// paste. Queueing them would be correct but unhelpful — the sequence
    /// sleeps for `delay_ms` (configurable to 5s), so four quick presses
    /// would hold the user's clipboard for twenty seconds and fire
    /// keystrokes into whatever window happened to have focus by then.
    sequence: Mutex<()>,
}

impl Paster {
    pub fn new(
        clipboard: Arc<dyn Clipboard>,
        uinput: Arc<dyn PasteKeys>,
        delay_ms: u64,
        restore_clipboard: bool,
    ) -> Self {
        Self {
            clipboard,
            uinput,
            delay_ms: AtomicU64::new(delay_ms),
            restore_clipboard: AtomicBool::new(restore_clipboard),
            sequence: Mutex::new(()),
        }
    }

    /// Update the tunables (Options-dialog Apply).
    pub fn set_config(&self, delay_ms: u64, restore_clipboard: bool) {
        self.delay_ms.store(delay_ms, Ordering::Release);
        self.restore_clipboard
            .store(restore_clipboard, Ordering::Release);
    }

    /// Paste `payload` into the focused application.
    ///
    /// The sequence is: snapshot the clipboard, write the payload, wait
    /// for the compositor to serve it, emit Ctrl+V, then put the snapshot
    /// back. Every step after the first write has to unwind correctly,
    /// because the thing being borrowed is the user's clipboard:
    ///
    ///  * the restore runs even when the keystroke fails, so a failed
    ///    paste cannot leave the payload sitting in place of whatever the
    ///    user had copied (that loss was unrecoverable);
    ///  * the restore is skipped when no keystroke was *sent* at all, so
    ///    the documented "/dev/uinput unavailable — press Ctrl+V
    ///    yourself" fallback actually leaves something to paste. With the
    ///    default `restore_clipboard = true` that path used to write the
    ///    payload and wipe it ~70 ms later, so nothing happened anywhere;
    ///  * each write is announced to the clipboard backend individually
    ///    and only immediately before it is attempted, so an announcement
    ///    is at most one write-attempt ahead of reality. (Announcing
    ///    *before* the write is the only race-free ordering — the watcher
    ///    can observe the change before `set_text` returns — so a failed
    ///    write does leave one announcement standing. It expires with the
    ///    backend's TTL, and until then can only ever swallow a
    ///    byte-identical copy.)
    pub fn paste_text(&self, payload: &str) -> Result<(), PasteError> {
        // See `Paster::sequence`: drop a concurrent request rather than
        // queueing it. A poisoned lock is recovered — the guard protects
        // `()`, so there is no state to be left inconsistent.
        let _seq = match self.sequence.try_lock() {
            Ok(guard) => guard,
            Err(std::sync::TryLockError::Poisoned(e)) => e.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => {
                tracing::warn!("a paste is already in flight; ignoring this request");
                return Ok(());
            }
        };

        let delay_ms = self.delay_ms.load(Ordering::Acquire);
        let restore = self.restore_clipboard.load(Ordering::Acquire);

        // Snapshot BEFORE overwriting. `Empty` is not a failure — there
        // was simply nothing to put back — but a real read error means we
        // do not know what we are about to destroy, so don't pretend.
        let snapshot = if restore {
            match self.clipboard.text() {
                Ok(text) => Some(text),
                Err(fastpaste_platform::ClipboardError::Empty) => None,
                Err(e) => {
                    tracing::warn!("could not snapshot clipboard before paste: {e}");
                    None
                }
            }
        } else {
            None
        };

        // From here on the clipboard has been (or is about to be)
        // clobbered, so every exit goes through the restore below —
        // including a failed payload write, which can still have cleared
        // the selection before failing.
        self.clipboard.suppress_text(payload);
        let outcome = match self.clipboard.set_text(payload) {
            Ok(()) => self.emit_paste(delay_ms),
            Err(e) => Err(PasteError::from(e)),
        };

        // `emitted` is the question the restore hinges on: was a keystroke
        // actually delivered? If not, the payload has to stay put.
        match (&outcome, snapshot) {
            (Ok(true), Some(snap)) | (Err(_), Some(snap)) => {
                self.clipboard.suppress_text(&snap);
                if let Err(e) = self.clipboard.set_text(&snap) {
                    // Report the original failure in preference to this
                    // one, but never swallow it silently.
                    tracing::error!("could not restore clipboard after paste: {e}");
                }
            }
            (Ok(false), _) => {
                tracing::warn!(
                    "/dev/uinput unavailable; payload left on clipboard, user must Ctrl+V"
                );
            }
            (_, None) => {}
        }

        outcome.map(|_| ())
    }

    /// Wait out the settle delay and emit Ctrl+V. `Ok(false)` means there
    /// was no device to emit through — a clean degrade, not an error.
    fn emit_paste(&self, delay_ms: u64) -> Result<bool, PasteError> {
        // Check for a device BEFORE the settle delay: with the null
        // backend the sleep buys nothing and only holds the sequence
        // guard (for up to 5s at the configured maximum).
        if !self.uinput.available() {
            return Ok(false);
        }
        if delay_ms > 0 {
            std::thread::sleep(Duration::from_millis(delay_ms));
        }
        self.uinput.send_ctrl_v()?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fastpaste_platform::{Clipboard, ClipboardError, ClipboardPayload, NullClipboard};
    use std::sync::Mutex;

    /// A PasteKeys impl that counts `send_ctrl_v` calls, and can be
    /// told to fail the way a revoked or dead device does.
    struct CountingUinput {
        count: Mutex<u32>,
        available: bool,
        fail: bool,
    }
    impl CountingUinput {
        fn new(available: bool) -> Self {
            Self {
                count: Mutex::new(0),
                available,
                fail: false,
            }
        }
        fn failing() -> Self {
            Self {
                count: Mutex::new(0),
                available: true,
                fail: true,
            }
        }
        fn calls(&self) -> u32 {
            *self.count.lock().unwrap()
        }
    }
    impl PasteKeys for CountingUinput {
        fn available(&self) -> bool {
            self.available
        }
        fn send_ctrl_v(&self) -> Result<(), fastpaste_platform::PasteKeyError> {
            *self.count.lock().unwrap() += 1;
            if self.fail {
                return Err(fastpaste_platform::PasteKeyError::Poisoned);
            }
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

    /// A Clipboard that records every `suppress_text` announcement and
    /// every write, so tests can pin the paste sequence's contract with
    /// the watcher.
    struct RecordingClipboard {
        inner: NullClipboard,
        announced: Mutex<Vec<String>>,
        writes: Mutex<Vec<String>>,
        fail_read: bool,
        empty_read: bool,
    }

    impl RecordingClipboard {
        fn new() -> Self {
            Self {
                inner: NullClipboard::new(),
                announced: Mutex::new(Vec::new()),
                writes: Mutex::new(Vec::new()),
                fail_read: false,
                empty_read: false,
            }
        }
        fn with_unreadable_clipboard() -> Self {
            Self {
                fail_read: true,
                ..Self::new()
            }
        }
        fn with_empty_clipboard() -> Self {
            Self {
                empty_read: true,
                ..Self::new()
            }
        }
        fn announced(&self) -> Vec<String> {
            self.announced.lock().unwrap().clone()
        }
        fn writes(&self) -> Vec<String> {
            self.writes.lock().unwrap().clone()
        }
    }

    impl Clipboard for RecordingClipboard {
        fn set_text(&self, text: &str) -> Result<(), ClipboardError> {
            self.writes.lock().unwrap().push(text.to_owned());
            self.inner.set_text(text)
        }
        fn text(&self) -> Result<String, ClipboardError> {
            if self.fail_read {
                return Err(ClipboardError::Access("no owner".into()));
            }
            if self.empty_read {
                return Err(ClipboardError::Empty);
            }
            self.inner.text()
        }
        fn changes(&self) -> std::sync::mpsc::Receiver<ClipboardPayload> {
            self.inner.changes()
        }
        fn suppress_text(&self, text: &str) {
            self.announced.lock().unwrap().push(text.to_owned());
        }
    }

    /// Every write the paster makes must be announced, and it must
    /// announce nothing it does not write — an announcement without a
    /// matching write is what used to swallow the user's next real copy.
    #[test]
    fn every_write_is_announced_and_nothing_else_is() {
        let clip = Arc::new(RecordingClipboard::new());
        let uinput = Arc::new(CountingUinput::new(true));
        clip.set_text("orig").unwrap();
        clip.writes.lock().unwrap().clear();

        let paster = Paster::new(clip.clone(), uinput.clone(), 0, true);
        paster.paste_text("payload").unwrap();

        assert_eq!(clip.writes(), vec!["payload", "orig"]);
        assert_eq!(
            clip.announced(),
            clip.writes(),
            "announcements must correspond exactly to writes"
        );
    }

    #[test]
    fn without_restore_only_the_payload_is_announced() {
        let clip = Arc::new(RecordingClipboard::new());
        let uinput = Arc::new(CountingUinput::new(true));

        let paster = Paster::new(clip.clone(), uinput.clone(), 0, false);
        paster.paste_text("payload").unwrap();

        assert_eq!(clip.writes(), vec!["payload"]);
        assert_eq!(clip.announced(), vec!["payload"]);
    }

    /// An unreadable clipboard used to arm two suppressions while making
    /// only one write, leaving one armed forever.
    #[test]
    fn an_unreadable_clipboard_does_not_over_announce() {
        let clip = Arc::new(RecordingClipboard::with_unreadable_clipboard());
        let uinput = Arc::new(CountingUinput::new(true));

        let paster = Paster::new(clip.clone(), uinput.clone(), 0, /*restore=*/ true);
        paster.paste_text("payload").unwrap();

        assert_eq!(clip.writes(), vec!["payload"], "nothing to restore");
        assert_eq!(
            clip.announced(),
            vec!["payload"],
            "and nothing over-announced"
        );
    }

    #[test]
    fn an_empty_clipboard_is_not_treated_as_a_failure() {
        let clip = Arc::new(RecordingClipboard::with_empty_clipboard());
        let uinput = Arc::new(CountingUinput::new(true));

        let paster = Paster::new(clip.clone(), uinput.clone(), 0, true);
        paster
            .paste_text("payload")
            .expect("an empty clipboard is normal");
        assert_eq!(clip.announced(), vec!["payload"]);
    }

    /// The Critical case: with the DEFAULT restore_clipboard = true and no
    /// uinput device, the payload must survive on the clipboard for the
    /// user to paste by hand. It used to be written and then wiped ~70 ms
    /// later, so the documented fallback did nothing at all.
    #[test]
    fn unavailable_uinput_leaves_the_payload_on_the_clipboard_even_with_restore() {
        let clip = Arc::new(NullClipboard::new());
        let uinput = Arc::new(CountingUinput::new(/*available=*/ false));
        clip.set_text("orig").unwrap();

        let paster = Paster::new(clip.clone(), uinput.clone(), 0, /*restore=*/ true);
        paster.paste_text("payload").unwrap();

        assert_eq!(uinput.calls(), 0);
        assert_eq!(
            clip.text().unwrap(),
            "payload",
            "no keystroke was sent, so the payload must stay for a manual Ctrl+V"
        );
    }

    /// The other Critical case: a failed keystroke must not leave the
    /// user's clipboard permanently clobbered.
    #[test]
    fn a_failed_keystroke_still_restores_the_clipboard() {
        let clip = Arc::new(NullClipboard::new());
        let uinput = Arc::new(CountingUinput::failing());
        clip.set_text("something precious").unwrap();

        let paster = Paster::new(clip.clone(), uinput.clone(), 0, /*restore=*/ true);
        let err = paster.paste_text("payload").unwrap_err();

        assert!(
            matches!(err, PasteError::Uinput(_)),
            "the failure is reported"
        );
        assert_eq!(
            clip.text().unwrap(),
            "something precious",
            "the user's clipboard must survive a failed paste"
        );
    }

    #[test]
    fn a_failed_keystroke_announces_the_restore_too() {
        let clip = Arc::new(RecordingClipboard::new());
        let uinput = Arc::new(CountingUinput::failing());
        clip.set_text("orig").unwrap();
        clip.writes.lock().unwrap().clear();

        let paster = Paster::new(clip.clone(), uinput.clone(), 0, true);
        let _ = paster.paste_text("payload");

        assert_eq!(clip.writes(), vec!["payload", "orig"]);
        assert_eq!(clip.announced(), clip.writes());
    }

    /// A clipboard whose first `set_text` parks until released, so a
    /// second `paste_text` is guaranteed to arrive mid-sequence. Without
    /// this the concurrency tests are timing-dependent and would pass
    /// against the unguarded code whenever the threads happened not to
    /// overlap.
    struct BlockingClipboard {
        inner: NullClipboard,
        entered: Mutex<Option<std::sync::mpsc::Sender<()>>>,
        release: Mutex<Option<std::sync::mpsc::Receiver<()>>>,
        writes: Mutex<Vec<String>>,
    }

    impl BlockingClipboard {
        /// Unarmed: writes pass straight through, so a test can set up its
        /// starting clipboard without tripping the trap.
        fn new() -> Arc<Self> {
            Arc::new(Self {
                inner: NullClipboard::new(),
                entered: Mutex::new(None),
                release: Mutex::new(None),
                writes: Mutex::new(Vec::new()),
            })
        }

        /// Arm the trap: the NEXT `set_text` signals on the returned
        /// receiver and then parks until the returned sender fires.
        fn arm(&self) -> (std::sync::mpsc::Receiver<()>, std::sync::mpsc::Sender<()>) {
            let (entered_tx, entered_rx) = std::sync::mpsc::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            *self.entered.lock().unwrap() = Some(entered_tx);
            *self.release.lock().unwrap() = Some(release_rx);
            (entered_rx, release_tx)
        }

        fn writes(&self) -> Vec<String> {
            self.writes.lock().unwrap().clone()
        }
    }

    impl Clipboard for BlockingClipboard {
        fn set_text(&self, text: &str) -> Result<(), ClipboardError> {
            self.writes.lock().unwrap().push(text.to_owned());
            // Only the first write parks.
            if let Some(tx) = self.entered.lock().unwrap().take() {
                let _ = tx.send(());
                if let Some(rx) = self.release.lock().unwrap().take() {
                    let _ = rx.recv();
                }
            }
            self.inner.set_text(text)
        }
        fn text(&self) -> Result<String, ClipboardError> {
            self.inner.text()
        }
        fn changes(&self) -> std::sync::mpsc::Receiver<ClipboardPayload> {
            self.inner.changes()
        }
    }

    /// A second press while a paste is in flight is DROPPED, not queued.
    /// Queueing was the earlier fix and was worse: the sequence sleeps
    /// for `delay_ms` (up to 5s), so a handful of quick presses would
    /// hold the clipboard for many seconds and then fire keystrokes into
    /// whatever window had focus by then.
    #[test]
    fn a_paste_arriving_mid_sequence_is_dropped_not_queued() {
        let clip = BlockingClipboard::new();
        let uinput = Arc::new(CountingUinput::new(true));
        clip.set_text("orig").unwrap(); // setup, while still unarmed
        clip.writes.lock().unwrap().clear();

        let paster = Arc::new(Paster::new(clip.clone(), uinput.clone(), 0, true));
        let (entered, release) = clip.arm();

        let first = {
            let paster = Arc::clone(&paster);
            std::thread::spawn(move || paster.paste_text("first").unwrap())
        };
        // Deterministic: the first sequence is now parked inside set_text.
        entered.recv().expect("the first paste must reach set_text");

        // This one must return immediately, having written nothing.
        paster
            .paste_text("second")
            .expect("a dropped paste is not an error");
        assert_eq!(
            clip.writes(),
            vec!["first"],
            "the second request must not have touched the clipboard"
        );

        release.send(()).unwrap();
        first.join().unwrap();

        assert_eq!(uinput.calls(), 1, "exactly one keystroke was sent");
        assert_eq!(
            clip.writes(),
            vec!["first", "orig"],
            "and the pair completed"
        );
    }

    /// However many threads pile in, the user's clipboard comes back and
    /// no sequence is left half-applied.
    #[test]
    fn concurrent_pastes_never_leave_the_clipboard_clobbered() {
        let clip = Arc::new(RecordingClipboard::new());
        let uinput = Arc::new(CountingUinput::new(true));
        clip.set_text("orig").unwrap();
        clip.writes.lock().unwrap().clear();

        let paster = Arc::new(Paster::new(clip.clone(), uinput.clone(), 5, true));
        let mut handles = Vec::new();
        for i in 0..4 {
            let paster = Arc::clone(&paster);
            handles.push(std::thread::spawn(move || {
                paster.paste_text(&format!("payload{i}")).unwrap();
            }));
        }
        for h in handles {
            h.join().unwrap();
        }

        let writes = clip.writes();
        let calls = uinput.calls() as usize;
        assert!(
            (1..=4).contains(&calls),
            "some pastes may be dropped: {calls}"
        );
        assert_eq!(
            writes.len(),
            calls * 2,
            "every sequence that ran wrote exactly a payload and a restore: {writes:?}"
        );
        // Writes must come in well-formed [payload, "orig"] pairs — this
        // is what interleaving would break.
        for pair in writes.chunks(2) {
            assert!(pair[0].starts_with("payload"), "{writes:?}");
            assert_eq!(pair[1], "orig", "{writes:?}");
        }
        assert_eq!(clip.text().unwrap(), "orig");
    }

    #[test]
    fn set_config_retunes_the_sequence() {
        let clip = Arc::new(NullClipboard::new());
        let uinput = Arc::new(CountingUinput::new(true));
        clip.set_text("orig").unwrap();

        let paster = Paster::new(clip.clone(), uinput.clone(), 0, /*restore=*/ false);
        paster.paste_text("a").unwrap();
        assert_eq!(clip.text().unwrap(), "a", "no restore yet");

        paster.set_config(0, /*restore=*/ true);
        paster.paste_text("b").unwrap();
        assert_eq!(clip.text().unwrap(), "a", "restore is live now");
    }
}
