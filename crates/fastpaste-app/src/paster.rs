//! Paster: the paste sequence (snapshot -> set payload -> settle -> Ctrl+V ->
//! settle -> restore), over the platform's clipboard and keystroke seams.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use fastpaste_platform::{Clipboard, PasteKeys, PasteMethod};

use thiserror::Error;

#[derive(Error, Debug)]
pub enum PasteError {
    #[error("clipboard: {0}")]
    Clipboard(#[from] fastpaste_platform::ClipboardError),
    #[error("uinput: {0}")]
    Uinput(#[from] fastpaste_platform::PasteKeyError),
}

/// The paste sequence's tunables, as one value so a new one does not
/// reorder every constructor call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PasteConfig {
    /// Wait between writing the payload and the keystroke, so the target
    /// has focus back and the clipboard owner is ready to serve.
    pub delay_ms: u64,
    pub restore_clipboard: bool,
    /// Wait between the keystroke and putting the user's clipboard back.
    /// The keystroke is only *queued*; the target reads the clipboard
    /// whenever it gets round to processing it.
    pub restore_delay_ms: u64,
    pub method: PasteMethod,
}

pub struct Paster {
    clipboard: Arc<dyn Clipboard>,
    uinput: Arc<dyn PasteKeys>,
    /// Interior-mutable so the Options dialog's Apply can retune the paste
    /// sequence at runtime (the Arc<Paster> is shared with UI threads).
    config: Mutex<PasteConfig>,
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
        cfg: PasteConfig,
    ) -> Self {
        Self {
            clipboard,
            uinput,
            config: Mutex::new(cfg),
            sequence: Mutex::new(()),
        }
    }

    /// Update the tunables (Options-dialog Apply).
    pub fn set_config(&self, cfg: PasteConfig) {
        *self.config.lock().unwrap_or_else(|e| e.into_inner()) = cfg;
    }

    /// Paste `payload` into the focused application.
    ///
    /// The sequence is: snapshot the clipboard, write the payload, wait
    /// `delay_ms` for focus and the clipboard owner to settle, emit Ctrl+V,
    /// wait `restore_delay_ms` for the target to read the payload, then put
    /// the snapshot back. Every step after the first write has to unwind
    /// correctly, because the thing being borrowed is the user's clipboard:
    ///
    ///  * the wait after the keystroke is not optional: the keystroke is
    ///    only queued, and restoring straight after it made the target
    ///    paste the user's *old* clipboard;
    ///  * the restore is skipped when someone else wrote the clipboard
    ///    after the payload went on — a copy the user made mid-sequence is
    ///    newer than the snapshot and wins;
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

        let cfg = *self.config.lock().unwrap_or_else(|e| e.into_inner());
        let delay_ms = cfg.delay_ms;
        let restore = cfg.restore_clipboard;

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
        let written = self.clipboard.set_text(payload);
        let payload_written = written.is_ok();
        // Read straight after our own write, so the only thing that can
        // move it from here is somebody else's.
        let seq_after_payload = self.clipboard.sequence();
        let outcome = match written {
            Ok(()) => self.emit_paste(delay_ms),
            Err(e) => Err(PasteError::from(e)),
        };

        // `emitted` is the question the restore hinges on: was a keystroke
        // actually delivered? If not, the payload has to stay put.
        match (&outcome, snapshot) {
            (Ok(false), _) => {
                tracing::warn!(
                    "/dev/uinput unavailable; payload left on clipboard, user must Ctrl+V"
                );
            }
            (_, None) => {}
            // The payload write itself failed: no keystroke, no wait, so
            // nobody else can have written since. Put the snapshot back
            // unconditionally — a write that cleared the clipboard before
            // failing must not read as "the user copied something new".
            (_, Some(snap)) if !payload_written => self.restore(&snap),
            (Ok(true), Some(snap)) => {
                // The keystroke is only queued: the target reads the
                // clipboard when it gets round to it, and restoring before
                // then pastes the user's old clipboard instead of the
                // snippet.
                if cfg.restore_delay_ms > 0 {
                    std::thread::sleep(Duration::from_millis(cfg.restore_delay_ms));
                }
                self.restore_unless_replaced(&snap, payload, seq_after_payload);
            }
            (Err(_), Some(snap)) => {
                self.restore_unless_replaced(&snap, payload, seq_after_payload);
            }
        }

        outcome.map(|_| ())
    }

    /// Put `snapshot` back, unless the clipboard has stopped holding our
    /// payload. If the user copied something while the sequence was
    /// waiting, that copy is newer than the snapshot and overwriting it
    /// would lose it.
    fn restore_unless_replaced(
        &self,
        snapshot: &str,
        payload: &str,
        seq_after_payload: Option<u64>,
    ) {
        if self.replaced_since_payload(payload, seq_after_payload) {
            tracing::info!("clipboard changed during the paste; keeping it instead of restoring");
            return;
        }
        self.restore(snapshot);
    }

    /// Whether anyone else has written the clipboard since our payload.
    /// The OS sequence number answers that exactly where there is one;
    /// otherwise the contents no longer being the payload is the sign.
    fn replaced_since_payload(&self, payload: &str, seq_after_payload: Option<u64>) -> bool {
        if let (Some(before), Some(now)) = (seq_after_payload, self.clipboard.sequence()) {
            return before != now;
        }
        match self.clipboard.text() {
            Ok(current) => current != payload,
            // Something that is not text replaced it.
            Err(fastpaste_platform::ClipboardError::Empty) => true,
            // Cannot tell. Restoring is what the sequence did before this
            // check existed, and leaves the user's own clipboard in place.
            Err(e) => {
                tracing::warn!("could not re-read clipboard before restoring: {e}");
                false
            }
        }
    }

    fn restore(&self, snapshot: &str) {
        self.clipboard.suppress_text(snapshot);
        if let Err(e) = self.clipboard.set_text(snapshot) {
            // Report the original failure in preference to this one, but
            // never swallow it silently.
            tracing::error!("could not restore clipboard after paste: {e}");
        }
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
    use fastpaste_platform::{
        Clipboard, ClipboardError, ClipboardPayload, NullClipboard, PasteMethod,
    };
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::time::Instant;

    /// A config with no settle after the keystroke, so the existing
    /// sequence tests stay instant; the tests that are about that settle
    /// set `restore_delay_ms` themselves.
    fn cfg(delay_ms: u64, restore_clipboard: bool) -> PasteConfig {
        PasteConfig {
            delay_ms,
            restore_clipboard,
            restore_delay_ms: 0,
            method: PasteMethod::CtrlV,
        }
    }

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

        let paster = Paster::new(clip.clone(), uinput.clone(), cfg(0, /*restore=*/ true));
        paster.paste_text("payload").unwrap();

        assert_eq!(uinput.calls(), 1, "send_ctrl_v must fire exactly once");
        // After paste with restore=true, the clipboard must hold the snapshot.
        assert_eq!(clip.text().unwrap(), "orig");
    }

    #[test]
    fn paste_without_restore_leaves_payload_on_clipboard() {
        let clip = Arc::new(NullClipboard::new());
        let uinput = Arc::new(CountingUinput::new(true));

        let paster = Paster::new(clip.clone(), uinput.clone(), cfg(0, /*restore=*/ false));
        paster.paste_text("payload").unwrap();

        assert_eq!(clip.text().unwrap(), "payload");
        assert_eq!(uinput.calls(), 1);
    }

    #[test]
    fn paste_with_unavailable_uinput_skips_ctrl_v_and_warns() {
        let clip = Arc::new(NullClipboard::new());
        let uinput = Arc::new(CountingUinput::new(/*available=*/ false));

        let paster = Paster::new(clip.clone(), uinput.clone(), cfg(0, /*restore=*/ false));
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

        let paster = Paster::new(clip.clone(), uinput.clone(), cfg(0, /*restore=*/ true));
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

        let paster = Paster::new(clip.clone(), uinput.clone(), cfg(0, /*restore=*/ false));
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

        let paster = Paster::new(clip.clone(), uinput.clone(), cfg(0, /*restore=*/ true));
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

        let paster = Paster::new(clip.clone(), uinput.clone(), cfg(0, /*restore=*/ true));
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

        let paster = Paster::new(clip.clone(), uinput.clone(), cfg(0, /*restore=*/ true));
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

        let paster = Paster::new(clip.clone(), uinput.clone(), cfg(0, /*restore=*/ true));
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

        let paster = Paster::new(clip.clone(), uinput.clone(), cfg(0, /*restore=*/ true));
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

        let paster = Arc::new(Paster::new(
            clip.clone(),
            uinput.clone(),
            cfg(0, /*restore=*/ true),
        ));
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

        let paster = Arc::new(Paster::new(
            clip.clone(),
            uinput.clone(),
            cfg(5, /*restore=*/ true),
        ));
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

        let paster = Paster::new(clip.clone(), uinput.clone(), cfg(0, /*restore=*/ false));
        paster.paste_text("a").unwrap();
        assert_eq!(clip.text().unwrap(), "a", "no restore yet");

        paster.set_config(cfg(0, /*restore=*/ true));
        paster.paste_text("b").unwrap();
        assert_eq!(clip.text().unwrap(), "a", "restore is live now");
    }

    // ---- settle after the keystroke, and the mid-paste copy guard -------

    /// A clipboard modelled on Windows' `GetClipboardSequenceNumber`:
    /// every change bumps a counter, whoever makes it. `external_copy`
    /// stands in for the user copying something in another window — it
    /// changes the contents without going through `set_text`, so it never
    /// appears in `writes`.
    ///
    /// Built with `sequenced = false` it reports no sequence, like the
    /// Wayland backend, and the paster has to compare contents instead.
    struct SequencedClipboard {
        contents: Mutex<Option<String>>,
        seq: AtomicU64,
        sequenced: bool,
        /// The next `set_text` clears the clipboard and then fails, the
        /// way a write that loses the selection halfway can.
        fail_next_write: AtomicBool,
        writes: Mutex<Vec<(String, Instant)>>,
    }

    impl SequencedClipboard {
        fn new(sequenced: bool) -> Arc<Self> {
            Arc::new(Self {
                contents: Mutex::new(None),
                seq: AtomicU64::new(0),
                sequenced,
                fail_next_write: AtomicBool::new(false),
                writes: Mutex::new(Vec::new()),
            })
        }
        fn external_copy(&self, text: &str) {
            *self.contents.lock().unwrap() = Some(text.to_owned());
            self.seq.fetch_add(1, Ordering::SeqCst);
        }
        fn clear_writes(&self) {
            self.writes.lock().unwrap().clear();
        }
        fn writes(&self) -> Vec<String> {
            self.writes
                .lock()
                .unwrap()
                .iter()
                .map(|(t, _)| t.clone())
                .collect()
        }
        fn written_at(&self, text: &str) -> Instant {
            self.writes
                .lock()
                .unwrap()
                .iter()
                .find(|(t, _)| t == text)
                .map(|(_, at)| *at)
                .unwrap_or_else(|| panic!("{text:?} was never written"))
        }
    }

    impl Clipboard for SequencedClipboard {
        fn set_text(&self, text: &str) -> Result<(), ClipboardError> {
            self.writes
                .lock()
                .unwrap()
                .push((text.to_owned(), Instant::now()));
            self.seq.fetch_add(1, Ordering::SeqCst);
            if self.fail_next_write.swap(false, Ordering::SeqCst) {
                *self.contents.lock().unwrap() = None;
                return Err(ClipboardError::Access("selection lost".into()));
            }
            *self.contents.lock().unwrap() = Some(text.to_owned());
            Ok(())
        }
        fn text(&self) -> Result<String, ClipboardError> {
            self.contents
                .lock()
                .unwrap()
                .clone()
                .ok_or(ClipboardError::Empty)
        }
        fn changes(&self) -> std::sync::mpsc::Receiver<ClipboardPayload> {
            std::sync::mpsc::channel().1
        }
        fn sequence(&self) -> Option<u64> {
            self.sequenced.then(|| self.seq.load(Ordering::SeqCst))
        }
    }

    /// PasteKeys that timestamps each keystroke and runs `on_send` inside
    /// it — standing in for whatever happens between the keystroke and the
    /// restore, such as the user copying something else.
    struct HookUinput {
        sent_at: Mutex<Vec<Instant>>,
        on_send: Box<dyn Fn() + Send + Sync>,
    }

    impl HookUinput {
        fn new(on_send: impl Fn() + Send + Sync + 'static) -> Arc<Self> {
            Arc::new(Self {
                sent_at: Mutex::new(Vec::new()),
                on_send: Box::new(on_send),
            })
        }
        fn sent_at(&self) -> Instant {
            let sent = self.sent_at.lock().unwrap();
            assert_eq!(sent.len(), 1, "exactly one keystroke");
            sent[0]
        }
    }

    impl PasteKeys for HookUinput {
        fn available(&self) -> bool {
            true
        }
        fn send_ctrl_v(&self) -> Result<(), fastpaste_platform::PasteKeyError> {
            self.sent_at.lock().unwrap().push(Instant::now());
            (self.on_send)();
            Ok(())
        }
    }

    /// Finding 1. `SendInput` and uinput only *queue* the keystroke; the
    /// target reads the clipboard some time later. Restoring straight
    /// after `send` let it read the user's OLD clipboard — the Wine replay
    /// pasted the snippet 0 times out of 12. The restore has to wait out
    /// `restore_delay_ms` after the keystroke, while the focus-settle
    /// `delay_ms` still comes before it.
    #[test]
    fn restore_happens_after_the_keystroke_not_before() {
        const SETTLE: Duration = Duration::from_millis(30);
        const AFTER: Duration = Duration::from_millis(60);
        let clip = SequencedClipboard::new(true);
        clip.set_text("orig").unwrap();
        clip.clear_writes();
        let uinput = HookUinput::new(|| {});

        let paster = Paster::new(
            clip.clone(),
            uinput.clone(),
            PasteConfig {
                delay_ms: SETTLE.as_millis() as u64,
                restore_clipboard: true,
                restore_delay_ms: AFTER.as_millis() as u64,
                method: PasteMethod::CtrlV,
            },
        );
        paster.paste_text("payload").unwrap();

        assert_eq!(clip.writes(), vec!["payload", "orig"]);
        let sent = uinput.sent_at();
        assert!(
            sent.duration_since(clip.written_at("payload")) >= SETTLE,
            "the focus settle belongs before the keystroke"
        );
        let waited = clip.written_at("orig").duration_since(sent);
        assert!(
            waited >= AFTER,
            "the restore came {waited:?} after the keystroke; the target \
             never had a chance to read the payload"
        );
    }

    /// The user copies something while the sequence is still waiting to
    /// restore. That copy is newer than the snapshot and must win; writing
    /// the stale snapshot over it would silently lose it.
    #[test]
    fn restore_is_skipped_when_clipboard_changed_mid_paste() {
        let clip = SequencedClipboard::new(true);
        clip.set_text("orig").unwrap();
        clip.clear_writes();
        let watched = Arc::clone(&clip);
        let uinput = HookUinput::new(move || watched.external_copy("copied mid-paste"));

        let paster = Paster::new(clip.clone(), uinput, cfg(0, /*restore=*/ true));
        paster.paste_text("payload").unwrap();

        assert_eq!(
            clip.writes(),
            vec!["payload"],
            "the stale snapshot must not be written back"
        );
        assert_eq!(clip.text().unwrap(), "copied mid-paste");
    }

    /// Without a sequence number (Wayland) the change is detected by the
    /// contents no longer being the payload.
    #[test]
    fn restore_is_skipped_when_text_changed_mid_paste_without_a_sequence() {
        let clip = SequencedClipboard::new(/*sequenced=*/ false);
        clip.set_text("orig").unwrap();
        clip.clear_writes();
        let watched = Arc::clone(&clip);
        let uinput = HookUinput::new(move || watched.external_copy("copied mid-paste"));

        let paster = Paster::new(clip.clone(), uinput, cfg(0, /*restore=*/ true));
        paster.paste_text("payload").unwrap();

        assert_eq!(clip.writes(), vec!["payload"]);
        assert_eq!(clip.text().unwrap(), "copied mid-paste");
    }

    /// The guard must not fire on an ordinary paste: nothing changed, so
    /// the snapshot goes back.
    #[test]
    fn restore_runs_when_sequence_is_unchanged() {
        let clip = SequencedClipboard::new(true);
        clip.set_text("orig").unwrap();
        clip.clear_writes();

        let paster = Paster::new(clip.clone(), HookUinput::new(|| {}), cfg(0, true));
        paster.paste_text("payload").unwrap();

        assert_eq!(clip.writes(), vec!["payload", "orig"]);
        assert_eq!(clip.text().unwrap(), "orig");
    }

    /// A payload write that fails can still have cleared the clipboard.
    /// No keystroke went out and nobody else wrote, so the guard has
    /// nothing to protect and the snapshot must go back — an emptied
    /// clipboard must not read as "the user copied something new".
    #[test]
    fn a_failed_payload_write_still_restores_the_snapshot() {
        let clip = SequencedClipboard::new(/*sequenced=*/ false);
        clip.set_text("orig").unwrap();
        clip.clear_writes();
        clip.fail_next_write.store(true, Ordering::SeqCst);

        let uinput = HookUinput::new(|| {});
        let paster = Paster::new(clip.clone(), uinput.clone(), cfg(0, true));
        let err = paster.paste_text("payload").unwrap_err();

        assert!(matches!(err, PasteError::Clipboard(_)));
        assert!(uinput.sent_at.lock().unwrap().is_empty(), "no keystroke");
        assert_eq!(clip.writes(), vec!["payload", "orig"]);
        assert_eq!(clip.text().unwrap(), "orig");
    }
}
