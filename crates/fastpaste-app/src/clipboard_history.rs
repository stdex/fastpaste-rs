//! Clipboard history ring buffer.
//!
//! Consumes [`ClipboardPayload`]s from the platform layer's
//! [`Clipboard::changes`][fastpaste_platform::Clipboard::changes] stream
//! and maintains a bounded, newest-first list of recent clipboard
//! contents. [`ClipboardHistory::on_clipboard_changed`] returns whether an
//! entry was actually inserted, so the draining thread can trigger UI
//! refreshes without a separate notification channel.
//!
//! Threading model: [`ClipboardHistory`] is `Send + Sync`. The entry list
//! sits behind a `Mutex` (uncontended in practice — one drainer thread
//! writing, UI thread reading), and the capture flag is an `AtomicBool`
//! so `set_enabled` doesn't need the lock.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use fastpaste_platform::ClipboardPayload;

/// One captured clipboard snapshot.
///
/// `timestamp` uses `chrono::DateTime<Utc>` so the UI can format it per
/// locale. We capture it at insert time rather than lazily so the
/// timestamp reflects *when the user copied*, not when the UI happened
/// to read the list.
#[derive(Debug, Clone)]
pub struct HistoryEntry {
    pub text: String,
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

/// Bounded clipboard history, newest-first.
///
/// Construction binds two static parameters (`max_items`, initial
/// `enabled`); runtime state lives behind interior-mutex / atomic so a
/// shared `Arc<ClipboardHistory>` can be held by both the change-drainer
/// task and the UI.
pub struct ClipboardHistory {
    entries: Arc<Mutex<Vec<HistoryEntry>>>,
    max_items: usize,
    enabled: AtomicBool,
}

impl ClipboardHistory {
    /// New empty history. `max_items == 0` is legal but means every
    /// payload is trimmed away — useful only for tests; production
    /// callers should pass a sensible bound (e.g. from
    /// `ClipboardHistorySettings::max_items`).
    pub fn new(max_items: usize, enabled: bool) -> Self {
        Self {
            entries: Arc::new(Mutex::new(Vec::with_capacity(max_items))),
            max_items,
            enabled: AtomicBool::new(enabled),
        }
    }

    /// Consume a clipboard-change payload. Returns `true` when a new entry
    /// was inserted (the caller's cue to refresh any UI); `false` when the
    /// payload was filtered out.
    ///
    /// Filters (each in order):
    /// 1. **Disabled** — `set_enabled(false)` pauses capture without
    ///    dropping already-captured entries.
    /// 2. **Empty text** — never recorded (a no-op clear should not
    ///    pollute history).
    /// 3. **Duplicate of newest** — if the most recent entry equals the
    ///    payload, skip entirely. This catches repeated identical copies
    ///    (e.g. the user Ctrl+C'ing the same selection twice) without
    ///    producing a redundant entry.
    /// 4. **Re-promote of an older entry** — if the payload matches any
    ///    older entry, remove that older copy first, then insert at
    ///    position 0. This matches user expectations: copy X, copy Y,
    ///    copy X again → history is `[X, Y]` (X re-promoted), not
    ///    `[X, Y, X]` (duplicate).
    pub fn on_clipboard_changed(&self, payload: ClipboardPayload) -> bool {
        if !self.enabled.load(Ordering::Acquire) {
            return false;
        }
        if payload.text.is_empty() {
            return false;
        }

        let mut entries = self.entries.lock().expect("entries mutex poisoned");
        // Fast path: identical to newest — pure dedup, no entry added.
        if entries.first().is_some_and(|e| e.text == payload.text) {
            return false;
        }
        // Re-promote: drop any older matching entry so the list doesn't
        // end up with two copies of the same text after the insert.
        // `retain` is O(n) but n is bounded by `max_items` (≤ ~50 in
        // practice), so this stays cheap.
        let text = payload.text;
        entries.retain(|e| e.text != text);

        let entry = HistoryEntry {
            text,
            timestamp: chrono::Utc::now(),
        };
        entries.insert(0, entry);
        if entries.len() > self.max_items {
            entries.truncate(self.max_items);
        }
        true
    }

    /// Snapshot of current entries, newest-first. Clones the strings;
    /// callers that need long-lived ownership should consider cloning
    /// only what they render.
    pub fn entries(&self) -> Vec<HistoryEntry> {
        self.entries.lock().expect("entries mutex poisoned").clone()
    }

    /// Toggle capture on/off. Does **not** clear existing entries —
    /// callers that want a hard reset should also drain `entries()`.
    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Release);
    }

    /// Read current enabled state (useful for tests and UI checkboxes).
    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Acquire)
    }

    /// Maximum number of entries the buffer will retain. Fixed at
    /// construction; runtime resizing is not supported.
    pub fn max_items(&self) -> usize {
        self.max_items
    }
}

impl Default for ClipboardHistory {
    fn default() -> Self {
        // Mirrors ClipboardHistorySettings defaults.
        Self::new(/*max_items=*/ 10, /*enabled=*/ true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload(text: &str) -> ClipboardPayload {
        ClipboardPayload {
            text: text.to_string(),
            source_process: String::new(),
        }
    }

    #[test]
    fn insert_puts_newest_first() {
        let h = ClipboardHistory::new(10, true);
        h.on_clipboard_changed(payload("first"));
        h.on_clipboard_changed(payload("second"));
        h.on_clipboard_changed(payload("third"));

        let texts: Vec<_> = h.entries().into_iter().map(|e| e.text).collect();
        assert_eq!(texts, vec!["third", "second", "first"]);
    }

    #[test]
    fn dedup_skips_repeated_copy_of_newest() {
        let h = ClipboardHistory::new(10, true);
        h.on_clipboard_changed(payload("a"));
        h.on_clipboard_changed(payload("a")); // dedup
        h.on_clipboard_changed(payload("a")); // dedup

        let entries = h.entries();
        assert_eq!(entries.len(), 1, "duplicate of newest must be skipped");
        assert_eq!(entries[0].text, "a");
    }

    #[test]
    fn dedup_allows_repromote_of_older_item() {
        // Re-copying an older (non-newest) entry is allowed — it bubbles
        // back to position 0. This matches user expectations: copy X,
        // copy Y, copy X again → X is now the most recent.
        let h = ClipboardHistory::new(10, true);
        h.on_clipboard_changed(payload("x"));
        h.on_clipboard_changed(payload("y"));
        h.on_clipboard_changed(payload("x"));

        let texts: Vec<_> = h.entries().into_iter().map(|e| e.text).collect();
        assert_eq!(
            texts,
            vec!["x", "y"],
            "older item should re-promote, not dedup"
        );
    }

    #[test]
    fn trim_respects_max_items() {
        let h = ClipboardHistory::new(3, true);
        for i in 0..10 {
            h.on_clipboard_changed(payload(&format!("item-{i}")));
        }

        let entries = h.entries();
        assert_eq!(entries.len(), 3, "must trim to max_items");
        // Newest three: item-9, item-8, item-7 (in that order).
        assert_eq!(entries[0].text, "item-9");
        assert_eq!(entries[1].text, "item-8");
        assert_eq!(entries[2].text, "item-7");
    }

    #[test]
    fn empty_text_is_skipped() {
        let h = ClipboardHistory::new(10, true);
        h.on_clipboard_changed(payload(""));
        h.on_clipboard_changed(payload("real"));

        let entries = h.entries();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].text, "real");
    }

    #[test]
    fn disabled_state_skips_capture() {
        let h = ClipboardHistory::new(10, /*enabled=*/ false);
        h.on_clipboard_changed(payload("while-disabled"));

        assert!(h.entries().is_empty(), "disabled history must not capture");

        // Toggling back on resumes capture (without losing state).
        h.set_enabled(true);
        h.on_clipboard_changed(payload("after-enable"));
        let entries = h.entries();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].text, "after-enable");
    }

    #[test]
    fn set_enabled_does_not_clear_entries() {
        let h = ClipboardHistory::new(10, true);
        h.on_clipboard_changed(payload("captured"));
        h.set_enabled(false);
        h.set_enabled(true);

        let entries = h.entries();
        assert_eq!(entries.len(), 1, "disabling must not drop existing entries");
        assert_eq!(entries[0].text, "captured");
    }

    #[test]
    fn insert_reports_true_and_skips_report_false() {
        let h = ClipboardHistory::new(10, true);
        // Real insert → true.
        assert!(h.on_clipboard_changed(payload("hello")));
        // Dedup of newest → false.
        assert!(!h.on_clipboard_changed(payload("hello")));
        // Empty text → false.
        assert!(!h.on_clipboard_changed(payload("")));
        // Re-promote of an older entry is a real insert → true.
        assert!(h.on_clipboard_changed(payload("world")));
        assert!(h.on_clipboard_changed(payload("hello")));
    }

    #[test]
    fn disabled_capture_reports_false() {
        let h = ClipboardHistory::new(10, false);
        assert!(!h.on_clipboard_changed(payload("x")));
        assert!(h.entries().is_empty());
    }

    #[test]
    fn timestamps_are_populated_and_ordered() {
        let h = ClipboardHistory::new(10, true);
        h.on_clipboard_changed(payload("old"));
        // Sleep a hair so the timestamps differ at microsecond precision.
        std::thread::sleep(std::time::Duration::from_millis(2));
        h.on_clipboard_changed(payload("new"));

        let entries = h.entries();
        assert!(
            entries[0].timestamp >= entries[1].timestamp,
            "newest entry must have a timestamp >= the older one"
        );
    }

    #[test]
    fn default_matches_settings_defaults() {
        let h = ClipboardHistory::default();
        assert_eq!(h.max_items(), 10);
        assert!(h.enabled());
    }
}
