# Paste Reliability Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make snippet insertion actually land the snippet (not the old clipboard), survive elevated/non-text targets, and stop silently losing clipboard history on Windows.

**Architecture:** The paste sequence stays in `fastpaste-app::Paster` behind the `Clipboard`/`PasteKeys` traits, so every behavior change is unit-testable with the existing fakes. Three new seams go in `fastpaste-platform`: a clipboard **sequence number** (detect a mid-paste user copy), a **foreground-window** trait (save the target and restore its focus before the keystroke), and a widened `PasteKeys` surface (scan codes, held-modifier release, Ctrl+V vs Shift+Insert) built from a **pure frame/INPUT builder** that is tested without a device. Positional `Paster::new` args become a `PasteConfig` struct so later tasks don't churn the signature.

**Tech Stack:** Rust 1.95 / edition 2024, `windows-sys` 0.59 (`SendInput`, `GetClipboardSequenceNumber`, `GetAsyncKeyState`, `GetForegroundWindow`/`SetForegroundWindow`, `MapVirtualKeyW`), `evdev` 0.13 (`/dev/uinput`), `x11rb` 0.13 (`GetInputFocus`/`SetInputFocus`), `arboard` 3.6 (text + image + `SetExt*` exclusions), Slint 1.17, Fluent i18n.

**Spec:** `../../../analysis/ANALYSIS.md` (the reverse-engineered reference implementation, esp. §5 and §9) plus the findings below. There is no separate design doc; the problem list here is the spec.

**Findings being fixed** (from the Wine replay of `Paster::paste_text` against the real `fastpaste-platform`, `analysis/wine-harness/`):

1. **CRITICAL — restore race.** The settle delay is spent *before* `send_ctrl_v`; the snapshot is restored *immediately after*. `SendInput`/uinput only queue the keystroke, so the target reads the clipboard after the restore already overwrote the payload. Wine replay: snippet pasted **0/12**, the user's old clipboard pasted **10/12**. A 25 ms wait after the keystroke → **12/12**.
2. **CRITICAL — Windows history is deaf.** `clipboard/windows.rs` message pump calls `DefWindowProcW` instead of `DispatchMessageW`, and `WM_CLIPBOARDUPDATE` arrives *posted*, so the window proc never runs and `changes()` never fires.
3. **HIGH — elevated target (UIPI) loses the snippet.** A rejected synthetic keystroke is treated as a transient failure and the snapshot is restored, so nothing is left to paste by hand into the elevated window.
4. **HIGH — no target capture.** The paste fires into whatever has focus after `hide()`; the intended target window is never recorded or re-focused.
5. **HIGH — non-text clipboard is destroyed.** A snapshot of an image/files clipboard reads `Empty`, so after paste there is no restore and the user's data is gone.
6. **MEDIUM — missing scan codes.** `SendInput` sends `wScan = 0`; RDP, VMs and raw-input apps ignore it.
7. **MEDIUM — held modifiers.** If the user is still holding Ctrl/Shift/Alt/Win, the chord is corrupted; the final Ctrl-up also desyncs a key the user is physically holding.
8. **LOW — no Shift+Insert mode.** Terminals/PuTTY need Shift+Insert; the reference app exposes this (option 541).
9. **LOW — Linux physical-key paste.** `KEY_V` is the physical key, so Ctrl+V misfires on Dvorak; document the limitation.

## Global Constraints

- Rust 1.95+, edition 2024. Workspace deps live in the root `Cargo.toml` `[workspace.dependencies]`, referenced as `foo.workspace = true`. Platform-only deps go in a `[target.'cfg(...)'.dependencies]` section of `fastpaste-platform`, never the common one.
- **Nothing above `fastpaste-platform` may contain a `cfg`** or name a concrete backend. New OS seams follow the existing *trait + neutral `System*` alias + `Null*`* pattern in `fastpaste-platform/src/lib.rs`.
- `cargo clippy --workspace --all-targets -- -D warnings` and `cargo fmt --all --check` must stay clean (CI sets `RUSTFLAGS: "-D warnings"`).
- `cargo test --workspace` is the Linux gate here. The Windows backends are **built and tested only by the `windows` CI job**; locally they are cross-checked with `cargo check --target x86_64-pc-windows-gnu -p fastpaste-platform` and exercised with the Wine harness in `analysis/wine-harness/` (copy the repo `Cargo.lock`, `cargo build --offline --target x86_64-pc-windows-gnu`, run under a Wine prefix with `HKCU\Software\Wine\Drivers Graphics=null` and `DISPLAY` unset). Do not claim a Windows behavior fixed without a Wine-harness run showing it.
- Any user-visible string touches all five `crates/fastpaste-gui/i18n/*.ftl` (`en`, `ru`, `de`, `es`, `zh_CN`), a property in `ui/translations.slint`, a `t.set_*` in `apply_translations` (`main.rs`), and the `.slint` binding. Slint props are kebab-case; Rust setters snake_case. i18n parity tests enforce this.
- New settings fields carry `#[serde(default = "...")]`; ranges are clamped in `Settings::clamp`, mirroring `DELAY_MS_RANGE`.
- Follow AGENTS.md: extract a pure function rather than leave logic behind a device; write each test so it **fails against the old code**; prefer a deterministic fake to a sleep. `EVENT_SPACING`/`DEVICE_SETTLE` and the Slint `hide()`-frees-the-window rule still hold.
- `VirtualDevice::emit` appends its own `SYN_REPORT` — never add one. A failed emit must leave no key logically held.

## Review Focus

- **Clipboard holds files or an image, not text** (Finding 5): the user's clipboard must not end up destroyed — covered by Task 7's `snapshot_non_text_is_preserved_or_paste_aborts`.
- **User copies something new during the settle/restore window** (Finding 1): the new copy must win, not be overwritten by the stale snapshot — covered by Task 1's `restore_is_skipped_when_clipboard_changed_mid_paste`.
- **Target window is elevated / rejects synthetic input** (Finding 3): the payload must stay on the clipboard for a manual paste, like the no-device path — covered by Task 3's `rejected_keystroke_leaves_payload_like_unavailable`.
- **User is still holding the hotkey modifiers when the chord fires** (Finding 7): the emitted chord must be exactly Ctrl+V (or Shift+Insert), not Ctrl+Alt+V — covered by Task 6's `held_modifiers_are_released_before_the_chord`.
- **No paste device / Wayland-native (no X focus)** (Findings 4, 9): the payload stays on the clipboard and the restore is skipped — covered by Task 1's existing `unavailable_uinput_leaves_the_payload...` retained green, and Task 4's `null_foreground_is_a_clean_degrade`.

---

### Task 1: Settle after the keystroke, and don't clobber a fresh copy

Fixes Finding 1. Introduces `PasteConfig` (so later tasks stop reordering `Paster::new`) and a clipboard **sequence number** seam.

**Files:**
- Modify: `crates/fastpaste-platform/src/clipboard.rs` (trait + `NullClipboard`)
- Modify: `crates/fastpaste-platform/src/clipboard/windows.rs`, `crates/fastpaste-platform/src/clipboard/wayland.rs`
- Modify: `crates/fastpaste-app/src/paster.rs` (`PasteConfig`, `paste_text`, tests)
- Modify: `crates/fastpaste-app/src/settings.rs` (`restore_delay_ms`), `crates/fastpaste-app/src/context.rs` (construct/set via `PasteConfig`)

**Interfaces:**
- Produces:
  - `Clipboard::sequence(&self) -> Option<u64>` — trait method, **default `None`**. Windows returns `Some(GetClipboardSequenceNumber())`; Wayland/Null keep the default.
  - `pub enum PasteMethod { CtrlV }` defined **in `fastpaste-platform::paste_keys`** and re-exported from the platform crate root (its single home; Task 5 builds on it, Task 8 adds `ShiftInsert`). `fastpaste-app` imports it.
  - `pub struct PasteConfig { pub delay_ms: u64, pub restore_clipboard: bool, pub restore_delay_ms: u64, pub method: PasteMethod }` in `paster.rs`. Until Task 8 the only value is `PasteMethod::CtrlV`.
  - `Paster::new(clipboard, uinput, cfg: PasteConfig)` and `Paster::set_config(&self, cfg: PasteConfig)` (positional `delay_ms`/`restore` args removed).
  - `PasteSettings::restore_delay_ms` (default `120`), `RESTORE_DELAY_MS_RANGE: 0..=5_000`, clamped.
- Consumes: existing `Clipboard`, `PasteKeys`.

- [ ] **Step 1: Write the failing tests in `paster.rs`**

```rust
// settle delay is spent AFTER the keystroke, before restore
fn restore_happens_after_the_keystroke_not_before() {
    // CountingUinput records the clipboard's last_set at send time;
    // assert the payload was still present when the keystroke fired.
}
// a user copy during the sequence wins over the stale snapshot
fn restore_is_skipped_when_clipboard_changed_mid_paste() {
    // Clipboard fake whose sequence() increments on an external set;
    // bump it between set_text(payload) and restore -> snapshot NOT written.
    // writes == ["payload"], final clipboard still the user's new copy.
}
fn restore_runs_when_sequence_is_unchanged() { /* writes == ["payload","orig"] */ }
```

- [ ] **Step 2: Run them — expect FAIL** (`cargo test -p fastpaste-app paster -- --nocapture`); the first fails because restore precedes the settle, the second because `sequence()` is unused.

- [ ] **Step 3: Add `Clipboard::sequence` (default `None`) and `PasteConfig`; implement.** In `paste_text`: read `seq_before = self.clipboard.sequence()` right after writing the payload; `emit_paste` sleeps `delay_ms` **before** `send` (keep — that is the focus-settle budget, Task 4) and the new `restore_delay_ms` **after** a successful send; restore only when `restore && emitted && snapshot.is_some()` **and** (`seq_before`/current agree, i.e. `sequence()` is unchanged, or `sequence()` is `None` and the current `text()` still equals the payload). Windows `sequence()` → `GetClipboardSequenceNumber`.

- [ ] **Step 4: Run the suite — expect PASS**, and the retained Finding-related tests (`unavailable_uinput_leaves_the_payload...`, `a_failed_keystroke_still_restores_the_clipboard`) still green: `cargo test -p fastpaste-app`.

- [ ] **Step 5: Thread `PasteConfig` through `context.rs` + settings; `cargo check --target x86_64-pc-windows-gnu -p fastpaste-platform`; commit** (`fix(paste): settle after the keystroke and guard restore on a mid-paste copy`).

---

### Task 2: Windows clipboard listener must dispatch

Fixes Finding 2. No unit test is possible (message pump behind a device); the Wine harness is the gate.

**Files:**
- Modify: `crates/fastpaste-platform/src/clipboard/windows.rs` (the `GetMessageW` loop in `listener_loop`)

**Interfaces:** none changed.

- [ ] **Step 1: Capture current behavior.** Build & run `analysis/wine-harness/` (per Global Constraints). Record: `history saw the copy: NO`.
- [ ] **Step 2: Fix the pump.** Replace the `DefWindowProcW(msg.hwnd, …)` call in the loop with `TranslateMessage(&msg); DispatchMessageW(&msg);` so posted `WM_CLIPBOARDUPDATE` reaches `wnd_proc`. Keep the window-proc's own `DefWindowProcW` fallthrough.
- [ ] **Step 3: Re-run the harness — expect `history saw the copy: YES`.**
- [ ] **Step 4:** `cargo check --target x86_64-pc-windows-gnu -p fastpaste-platform`; `cargo test -p fastpaste-platform` (Linux unaffected). **Commit** (`fix(clipboard): dispatch WM_CLIPBOARDUPDATE so Windows history records copies`).

---

### Task 3: A rejected/again-unavailable keystroke keeps the payload

Fixes Finding 3. The paster already leaves the payload when `available()==false`; extend that to a keystroke the OS *rejected* (UIPI), distinct from a transient device error.

**Files:**
- Modify: `crates/fastpaste-platform/src/paste_keys.rs` (classify the error; it already has `PasteKeyError::Rejected`)
- Modify: `crates/fastpaste-app/src/paster.rs` (restore decision + tests)

**Interfaces:**
- Produces: `PasteKeyError::is_rejected(&self) -> bool` (`true` for `Rejected`, else `false`).
- Consumes: `Paster` restore branch from Task 1.

- [ ] **Step 1: Failing test** `rejected_keystroke_leaves_payload_like_unavailable`: `CountingUinput` returns `Err(PasteKeyError::Rejected { sent: 1, expected: 4 })`; assert final clipboard == `"payload"` (not the snapshot) and the error is still returned.
- [ ] **Step 2: Run — FAIL** (today a `Rejected` error restores the snapshot). `cargo test -p fastpaste-app paster`.
- [ ] **Step 3: Implement.** In `paste_text`'s match, treat `Err(e) if e.is_rejected()` like the `Ok(false)` (no-device) arm — skip restore, keep payload, warn — while other `Err` still restores. Return the error either way.
- [ ] **Step 4: Run — PASS**, retained tests green.
- [ ] **Step 5: Commit** (`fix(paste): keep the payload when the OS rejects the synthetic keystroke (UIPI)`).

---

### Task 4: Capture the target window and restore its focus before pasting

Fixes Finding 4 (and underpins Findings 1/9 degrade paths). New `ForegroundWindow` seam in `fastpaste-platform`, following the trait+alias+Null pattern.

**Files:**
- Create: `crates/fastpaste-platform/src/foreground.rs` (+ `foreground/windows.rs`, `foreground/x11.rs`)
- Modify: `crates/fastpaste-platform/src/lib.rs` (`SystemForeground` alias, re-exports)
- Modify: `crates/fastpaste-app/src/paster.rs` (hold `Arc<dyn ForegroundWindow>`, restore before the keystroke) and `context.rs` (construct it)
- Modify: `crates/fastpaste-gui/src/main.rs` (capture at hotkey dispatch; carry the token into `spawn_paste`)

**Interfaces:**
- Produces:
  - `pub trait ForegroundWindow: Send + Sync { fn capture(&self) -> ForegroundToken; fn restore(&self, token: &ForegroundToken) -> bool; }`
  - `pub struct ForegroundToken(Option<...>)` — opaque; `Default` = "nothing captured". Windows wraps `HWND` (`GetForegroundWindow`/`SetForegroundWindow`), X11 wraps the focus window from `GetInputFocus` (restore via `SetInputFocus`, reusing the reader's connection pattern in `hotkey/x11.rs`), Null stores nothing and `restore` returns `false`.
  - `NullForegroundWindow`, `SystemForeground` alias.
  - `Paster::paste_text_to(&self, payload: &str, target: &ForegroundToken)`; `paste_text` keeps its signature and calls with `ForegroundToken::default()`.
- Consumes: Task 1 `PasteConfig`.

- [ ] **Step 1: Failing tests** (`foreground.rs` unit + `paster.rs`): `null_foreground_is_a_clean_degrade` (Null `capture`→default, `restore`→`false`); `paster_restores_focus_before_emitting` (a `RecordingForeground` fake asserts `restore(token)` is called before `send`).
- [ ] **Step 2: Run — FAIL** (`capture`/`restore` and `paste_text_to` don't exist).
- [ ] **Step 3: Implement** the trait + Null + the two backends; `Paster` calls `foreground.restore(target)` (best-effort; log on `false`) immediately before `emit_paste`, inside the existing settle budget. Wire `main.rs`: at `OPEN_DIALOG_ID` dispatch (before `show_dialog_and_paste`) capture a token, carry it to `spawn_paste`/`paste_text_to`.
- [ ] **Step 4: Run — PASS**; `cargo check --target x86_64-pc-windows-gnu -p fastpaste-platform`; Wine harness still 12/12.
- [ ] **Step 5: Commit** (`feat(paste): restore the target window's focus before the keystroke`).

---

### Task 5: Real scan codes, from a pure builder

Fixes Finding 6 and refactors the emit path into a testable builder. Introduces `PasteMethod` wiring end-to-end at the platform layer (value still only `CtrlV`).

**Files:**
- Modify: `crates/fastpaste-platform/src/paste_keys.rs`

**Interfaces:**
- Produces:
  - `PasteKeys::send_paste(&self, method: PasteMethod) -> Result<(), PasteKeyError>` (`PasteMethod` is the enum defined in Task 1); `send_ctrl_v` becomes `send_paste(PasteMethod::CtrlV)` (keep a thin `send_ctrl_v` wrapper or update callers — update callers).
  - Windows pure builder `fn windows_paste_inputs(method: PasteMethod, held: ModMask) -> Vec<KeyStroke>` where `KeyStroke { vk: u16, scan: u16, up: bool, extended: bool }`; `held` is `ModMask::NONE` until Task 6. Linux pure builder `fn paste_frames(method: PasteMethod, held: ModMask) -> Vec<(KeyCode, bool)>` generalising `ctrl_v_frames`.
- Consumes: Task 1 `PasteConfig.method`.

- [ ] **Step 1: Failing tests.** Windows-cfg test `ctrl_v_inputs_carry_scancodes`: every non-modifier `KeyStroke` has `scan != 0` and `V` uses `MapVirtualKeyW(VK_V, MAPVK_VK_TO_VSC)`. Retain/rename the Linux `ctrl_v_frames_*` tests against `paste_frames(CtrlV, NONE)`.
- [ ] **Step 2: Run — FAIL** (`scan` field / builder absent). Linux: `cargo test -p fastpaste-platform paste_keys`. Windows builder test is `#[cfg(windows)]` — gate it and rely on the Windows CI job + a `cargo check --target x86_64-pc-windows-gnu`.
- [ ] **Step 3: Implement** the builders and route `send_paste` through them. Windows: set `KEYBDINPUT.wScan` and `dwFlags |= KEYEVENTF_SCANCODE`; one `SendInput` call as today; the `Rejected`/stuck-modifier cleanup stays. Keep press/release ordering (Ctrl brackets V).
- [ ] **Step 4: Run — PASS**; `cargo check --target x86_64-pc-windows-gnu -p fastpaste-platform`; Wine harness still 12/12.
- [ ] **Step 5: Commit** (`fix(paste): emit scan codes and route Ctrl+V through a tested builder`).

---

### Task 6: Release held modifiers before the chord

Fixes Finding 7. Extends the Task 5 builder with a `ModMask` of currently-held modifiers.

**Files:**
- Modify: `crates/fastpaste-platform/src/paste_keys.rs`

**Interfaces:**
- Produces: `pub struct ModMask` (bits for Ctrl/Shift/Alt/Win) with `NONE`; on Windows `fn held_modifiers() -> ModMask` via `GetAsyncKeyState`. Builder prepends a key-up for each held modifier **except** the one the method itself uses (Ctrl for CtrlV), and does not re-press it.
- Consumes: Task 5 builders.

- [ ] **Step 1: Failing test** `held_modifiers_are_released_before_the_chord` (pure, both platforms): `windows_paste_inputs(CtrlV, ALT|SHIFT)` begins with Alt-up and Shift-up (order-independent) and contains no Alt/Shift press; the Ctrl+V core is unchanged. A `CtrlV` with `held = CTRL` must not emit a spurious extra Ctrl-up/же leave Ctrl released incorrectly — assert the net Ctrl transitions still balance.
- [ ] **Step 2: Run — FAIL** (builder ignores `held`).
- [ ] **Step 3: Implement**; `send_paste` reads `held_modifiers()` on Windows (`ModMask::NONE` on Linux — uinput cannot query host key state; document this as the Linux limitation and lean on the hotkey being long-released by dialog time).
- [ ] **Step 4: Run — PASS**; `cargo check --target x86_64-pc-windows-gnu`; harness unaffected.
- [ ] **Step 5: Commit** (`fix(paste): drop held modifiers so the chord is exactly Ctrl+V`).

---

### Task 7: Preserve a non-text clipboard (image), or refuse to destroy it

Fixes Finding 5. arboard can snapshot/restore **images** as well as text; anything it cannot represent must not be silently destroyed.

**Files:**
- Modify: `crates/fastpaste-platform/src/clipboard.rs` (trait: typed snapshot), both backends, `NullClipboard`
- Modify: `crates/fastpaste-app/src/paster.rs` (snapshot/restore a `ClipboardSnapshot`; tests)

**Interfaces:**
- Produces:
  - `pub enum ClipboardSnapshot { Text(String), Image(arboard::ImageData<'static>), Empty, Unsupported }` (owned/`'static`).
  - `Clipboard::snapshot(&self) -> ClipboardSnapshot` and `Clipboard::restore(&self, snap: &ClipboardSnapshot) -> Result<(), ClipboardError>` (default impls over `text`/`set_text` for Null; backends add image via arboard `get_image`/`set_image`; `Unsupported` = a format arboard sees but cannot read).
- Consumes: Task 1 restore branch (snapshot type replaces `Option<String>`).

- [ ] **Step 1: Failing tests** `snapshot_round_trips_an_image` (fake clipboard carrying an image; payload paste then restore yields the image back) and `snapshot_non_text_is_preserved_or_paste_aborts` (snapshot `Unsupported` → with `restore_clipboard` on, the paste is **not** performed, or is performed only after the user is warned; assert the user's clipboard is intact and `send` did not fire).
- [ ] **Step 2: Run — FAIL**.
- [ ] **Step 3: Implement.** Snapshot once up front; `Text`/`Image` restore as Task 1 dictates; `Unsupported` → skip the whole sequence, return an error the GUI can surface (do not overwrite). `Empty` behaves as today.
- [ ] **Step 4: Run — PASS**; `cargo check --target x86_64-pc-windows-gnu -p fastpaste-platform`; harness still 12/12 for text.
- [ ] **Step 5: Commit** (`fix(paste): snapshot images and never destroy an unsupported clipboard`).

---

### Task 8: Shift+Insert paste method (option, like reference 541)

Fixes Finding 8. Adds the `ShiftInsert` variant and the full settings/UI/i18n wiring.

**Files:**
- Modify: `crates/fastpaste-platform/src/paste_keys.rs` (`PasteMethod::ShiftInsert` in both builders; Windows `VK_INSERT` with `KEYEVENTF_EXTENDEDKEY`, Linux `KEY_LEFTSHIFT`+`KEY_INSERT`; declare `KEY_INSERT`/`KEY_LEFTSHIFT` in the `VirtualDevice` capability set)
- Modify: `crates/fastpaste-app/src/settings.rs` (`PasteSettings.method: PasteMethod` with `#[serde(default)]`), `context.rs` (pass into `PasteConfig`)
- Modify: `crates/fastpaste-gui/ui/options_dialog.slint` (a method dropdown on the Paste page), `ui/translations.slint`, all five `i18n/*.ftl`, `apply_translations`/load/apply in `main.rs`

**Interfaces:**
- Consumes: Task 5 builders, Task 1 `PasteConfig`.
- Produces: `PasteMethod::ShiftInsert`; `PasteSettings.method` round-trips through TOML and the Options dialog.

- [ ] **Step 1: Failing tests.** Builder: `shift_insert_inputs_bracket_insert_with_shift` (Windows: Shift-down, Insert-down+`KEYEVENTF_EXTENDEDKEY`, Insert-up, Shift-up, all with scan codes; Linux: the `KeyCode` analogue) and `shift_insert_keys_are_in_the_device_capability_set`. Settings: `paste_method_round_trips_toml` and a backward-compat test that a config without `method` loads as `CtrlV`.
- [ ] **Step 2: Run — FAIL**.
- [ ] **Step 3: Implement** the variant, settings field, dropdown + i18n (keys e.g. `options-paste-method-label`, `options-paste-method-ctrl-v`, `options-paste-method-shift-insert` in all five locales), and load/apply in `main.rs`.
- [ ] **Step 4: Run — PASS**; i18n parity tests green; `cargo run --example ui_preview -- options 3 out.ppm` to eyeball the Paste page (seeds Russian); `cargo check --target x86_64-pc-windows-gnu -p fastpaste-platform`.
- [ ] **Step 5: Commit** (`feat(paste): add a Shift+Insert paste method option`).

---

### Task 9: Document the Linux physical-key and Wayland limitations

Fixes Finding 9 (documentation, not code — a layout-independent Linux paste needs a keysym→keycode remap that is out of scope here).

**Files:**
- Modify: `README.md` (the Linux limitations/known-issues section), `AGENTS.md` (platform-layer notes)

**Interfaces:** none.

- [ ] **Step 1:** Add a note that `/dev/uinput` emits the **physical** `KEY_V`/`KEY_INSERT`, so on a non-QWERTY layout (e.g. Dvorak) the chord follows physical-key position; Shift+Insert is the layout-robust fallback for terminals. Note the existing Wayland-native "hotkeys rarely fire / no focus to restore" limitation now also means Task 4's `restore` returns `false` there (clean degrade: payload stays on the clipboard).
- [ ] **Step 2:** `cargo fmt --check` (docs only); **commit** (`docs: record the Linux physical-key and Wayland paste limitations`).

---

## Notes for the executor

- Tasks 1 → 5 → 6 → 8 share the paste-keys builder and `PasteConfig`/`PasteMethod`; do them in order. Tasks 2, 3, 4, 7 are independent of each other once Task 1 lands (2 is fully standalone and can go first if you want an early Windows-history win).
- "Do not claim a Windows behavior fixed without a Wine-harness run" is a hard rule here — Findings 1, 2, 3, 6, 7, 8 all have a Windows face the Linux `cargo test` cannot see.
- Keep `fastpaste-app` tests device-free: every new behavior gets a fake (`RecordingClipboard`/`RecordingForeground`/`CountingUinput`) assertion that fails against the pre-change code.
