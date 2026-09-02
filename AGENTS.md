# AGENTS.md

Working notes for anyone — human or agent — changing this repository.

This is not a rewrite of `README.md`. The README says what the app does
and how to build it; this file records the things that cost time to find
out, and the conventions that keep the code honest. Where the two
disagree, the code wins and both files are wrong.

## The shape of the thing

The crates are layered bottom-up: `app` imports `data` + `platform`,
and `gui` imports the others.

| Crate | Owns | Never contains |
|---|---|---|
| `fastpaste-data` | SQLite storage, the `Item` value type, the tree invariant | GUI, platform, I/O beyond SQLite |
| `fastpaste-platform` | X11 hotkeys, clipboard, `/dev/uinput` | Business logic |
| `fastpaste-app` | Composition root, settings, paste sequence, history ring | Widgets, direct OS calls |
| `fastpaste-gui` | Slint UI, controllers, i18n | Storage or platform logic |

The layering is the design. If a fix wants to reach across it, that is
usually the signal that it belongs in a different crate.

## Platforms

One codebase. Every OS call sits behind a trait in `fastpaste-platform`,
which re-exports exactly one implementation of each under a neutral
alias — `SystemHotkeys`, `SystemClipboard`, `SystemPasteKeys`. **Nothing
above that crate contains a `cfg`**, and nothing outside it names a
concrete backend. Keep it that way: a `cfg` leaking into `app` or `gui`
is the first step toward two codebases.

Backends live in submodules (`hotkey/x11.rs`, `hotkey/windows.rs`,
`clipboard/wayland.rs`, `clipboard/windows.rs`) and the shared file
above them holds only the trait, the error type and the null backend.

Platform-specific dependencies belong in a target section of
`Cargo.toml`, never the common one — `evdev`, `x11rb` and
`wl-clipboard-watch` do not exist on Windows, and `arboard` needs its
`wayland-data-control` feature only on Linux.

### Checking the Windows build

```
rustup target add x86_64-pc-windows-gnu
cargo check --target x86_64-pc-windows-gnu -p fastpaste-platform
```

That covers the crate where the platform code lives, and it is worth
running for any change to it. The rest of the workspace **cannot** be
checked this way without a C cross-compiler and a working OpenSSL
build: `rusqlite`'s `bundled-sqlcipher-vendored-openssl` feature builds
both SQLite and OpenSSL from source, so `-p fastpaste-data` and
anything downstream needs mingw (`x86_64-w64-mingw32-gcc`) plus perl
and NASM.

In practice, do not try. The `windows` job in `.github/workflows/ci.yml`
builds and tests the whole workspace on a real Windows runner, and that
is the check that counts. Nothing Windows can be *run* here at all.

## Everyday commands

```
cargo test --workspace
cargo clippy --workspace --all-targets
cargo fmt --check
cargo run --release --bin fastpaste-gui
```

**Keep it at zero.** The workspace builds with no warnings and no clippy
lints; a new one is a regression, not background noise.

---

## Seeing the UI

The app needs a Wayland compositor, so "does this look right" is not a
question you can answer on a build machine — and it is not a question
the type checker answers either. **Every UI change should be looked at.**

```
cargo run --example ui_preview -- options 2 out.ppm   # options dialog, page 0-3
cargo run --example ui_preview -- main    0 out.ppm   # main window
cargo run --example ui_preview -- main    1 out.ppm   # …with the delete confirmation up
cargo run --example ui_preview -- select  0 out.ppm   # selection dialog
ffmpeg -i out.ppm out.png                             # P6 -> PNG
```

`examples/ui_preview.rs` installs a `slint::platform::Platform` backed by
`MinimalSoftwareWindow` and renders straight into a pixel buffer. No
display, no compositor, deterministic. It writes binary PPM so it needs
no image-encoder dependency.

Notes that matter:

- **It seeds Russian**, the widest shipped locale. Label clipping and
  column overflow only appear in the longest strings; testing in English
  hides the bugs you are looking for.
- **Keep it faithful.** It has to replicate anything the app configures
  at runtime, or it renders something the user never sees. It already
  mirrors the `TreeViewStyle` palette for exactly this reason — before
  that it was showing the widget's stock style.
- The `software-renderer-systemfonts` feature it needs is a
  **dev-dependency**, so the shipped binary is unaffected. Test builds
  are larger because of it.
- The software renderer has no colour emoji, so the tree's 📁/📄/🕒 icons
  do not appear in a preview. That is a preview limitation, not a bug.
- Grabbing the real window with `ffmpeg -f x11grab` does **not** work
  here: the app renders on Wayland and the X11 capture comes back black.

### Measure, don't eyeball

When a size looks wrong, read the pixels. Parsing the PPM header and
sampling is a few lines of Python and it turns "the buttons look too
tall" into "the buttons are 54px and should be 30". Both of the layout
bugs in this repo's history were found that way, and one of them was
*not* what reading the source suggested.

---

## Slint: the traps in this codebase

All of these were live bugs here. They are easy to reintroduce.

### A bare `Rectangle` stretches

`Rectangle` defaults to `stretch: 1` on both axes. A widget root that
sets only `min-height` will be inflated to fill whatever layout holds
it. This produced a 1px separator rendering as a **245px grey column**
and a 28px spin box rendering 100px tall.

Every control pins `min-height` + `max-height` + `vertical-stretch: 0`.
Every separator pins its thickness *and* `horizontal-stretch: 0` /
`vertical-stretch: 0`.

### `max-height` only constrains the main axis

On the **cross** axis a layout stretches its children regardless of
their `max-height`. A button inside a `HorizontalLayout` taller than the
control height is sized by **that layout's vertical padding**, not by
its own constraint. The options footer and the main-window toolbar set
that padding deliberately; without it the buttons fill the whole bar.

### One-way `:` into a property the widget writes is silently dropped

`Property::set` removes a non-intercepting binding, and interactive
widgets assign their own `text` / `current-index` / `value` as the user
interacts. So:

```slint
text: root.editor-body;      // WRONG — dead after the first keystroke
text <=> root.editor-body;   // right
```

This exact mistake appeared three times and caused two data-loss bugs
(one item's text written into another item's row; the tree highlight
diverging from the selection that Delete acts on). **When adding any
binding to an interactive element, ask whether the widget assigns that
property itself.**

A plain grep over these property names also matches ordinary display
`Text` elements, which are fine — only elements that *write* the
property matter. Narrow to the ones inside a `TextInput`:

```
for f in crates/fastpaste-gui/ui/*.slint; do
  awk -v F="$f" '/TextInput \{/{t=NR}
    t && NR<=t+18 && /^[[:space:]]*(text|value|checked):/ {printf "%s:%d %s\n", F, NR, $0}' "$f"
done
```

One hit is expected: `FSpinBox`'s `text: root.value`, deliberately
compensated by a `changed value` tracker. A second hit is a bug. Also
check any `current-index` / `selected-index` bound into a `TreeView` or
`ListView` by hand.

### `GridLayout` sizing

Two separate surprises:

- A cell's `max-width` **caps the whole column**. This silently elided
  long checkbox captions that shared a column with a capped field. Set
  field width as a `min-width` and let a stretchy spacer column absorb
  the slack.
- A cell is stretched to its column **regardless of the child's
  `max-width`**. A narrow control that must keep its size goes inside a
  `HorizontalLayout { alignment: start; }`.

### Ids declared inside `if` are not in scope outside it

Moving the selection dialog's `ListView` out of an `if` block was
required for the root-level `scroll-into-view` function to reference it.

### Globals are per root component, not per compilation

Each root component instantiates its own `SharedGlobals`, so there are
**four** `Translations` instances (MainWindow, SelectionDialog,
OptionsDialog, FastpasteTray) — not one shared global. A single push
reaches only the surface it was made through. `relocalize_all_surfaces`
exists because of this; a language change that skips a surface leaves it
in the old language until it is destroyed.

### Component handles

`ComponentHandle::show()` holds the only extra strong reference and
`hide()` drops it — so a window retained only through a `Weak` is
**freed when hidden**, and the next open rebuilds it from scratch. The
strong handles live in `src/ui_state.rs`, in a thread-local: the
generated components are neither `Clone` nor `Send`, and `FastpasteTray`
is not even a `ComponentHandle` (no `as_weak`).

The `Weak` statics remain because they are `Send` and the worker threads
need to address windows from off-thread.

### When a Slint assumption is load-bearing, read the generated code

```
target/debug/build/fastpaste-gui-*/out/main.rs
```

It settles questions that guessing does not: whether `<=>` produced a
real two-way link (`link_two_way` vs `set_property_binding`), how many
`SharedGlobals::new` calls exist, whether a `changed` handler compiled
to a `ChangeTracker`. It is generated, enormous, and searchable.

### A library module keeps its own globals

`slint-tree-view` ships its `.slint` as a Slint **library module**, and a
library module has its own globals table. So
`TreeViewStyle::get(&main_window)` from Rust reaches *our* table, not the
one the widget reads — the setters return successfully and change
nothing. A global also cannot be re-opened from a consumer `.slint`;
that is a parse error, not an override.

The tree is therefore themed **on the instance**, in
`main_window.slint`, where it can read our `Theme` global directly
(`highlight-color: Theme.selection-bg;`). This needs
`slint-tree-view >= 0.3`, which added the per-instance properties for
exactly this reason.

The general lesson outlives this one crate: **a "global" from a
dependency compiled as a library module is not reachable from here.** If
setting one appears to do nothing, that is why — and the failure is
silent, so it will not look like an error.

---

## i18n

Fluent. The catalogues in `i18n/` are embedded at compile time, with a
per-key fallback to English.

Adding a user-visible string means touching every one of:

- the key in **all** `i18n/*.ftl` files;
- a property in `ui/translations.slint`;
- a `t.set_*` call in `apply_translations` (`src/main.rs`);
- the binding in the `.slint` that uses it.

Tests enforce this and will fail if you miss a step: key parity across
locales, "every key this file requests exists in `en.ftl`", and "the
English catalogue has no unused keys". Keys chosen at runtime cannot be
scraped and are listed by hand in `DYNAMIC_KEYS`.

Never hardcode a user-visible string in a `.slint` file.

---

## Threading

Everything UI happens on the Slint event-loop thread. Three workers feed
it and **must** marshal through `slint::invoke_from_event_loop`:

- `hotkey-events` — drains the platform hotkey channel;
- `clipboard-drainer` — feeds the history ring, pings the refresher;
- `tree-refresh` — rebuilds the tree model off-thread, marshals only the
  model swap.

`ui_state` is thread-local by design: from any other thread its slots
are empty, which encodes the rule rather than documenting it.

Mutex policy: `unwrap_or_else(|e| e.into_inner())`, not `expect`. A
panic inside a Slint callback unwinds through the event loop and takes
the process down; every critical section here is a clone or an
assignment and cannot leave torn state.

---

## Platform layer

The riskiest crate: raw X11, a `poll(2)` loop over FFI, a virtual input
device.

- **Hotkeys are not truly global.** `XGrabKey` on the XWayland root
  window only sees a keystroke while an X11/XWayland window has focus.
  On a Wayland-native desktop they rarely fire. See the README's
  limitation note before promising otherwise.
- **Key resolution never goes through keysyms.** Physical evdev keycodes
  (+8) keep shortcuts working under any layout; a keysym path breaks
  under Cyrillic. Modifier *bits* do come from `GetModifierMapping`.
- **Mask the event state before matching.** `KeyPress.state` carries
  pointer buttons (bits 8-12) and the XKB group (13-14); comparing it
  raw makes hotkeys stop matching while a mouse button is held, and
  under a second layout. `REAL_MODS` exists for this.
- **Grabs are owned across ids.** Retiring a combination another id
  holds silently kills a live shortcut. All three paths that ungrab —
  `register`, its rollback, and `unregister` — go through the same
  ownership guard.
- **Own clipboard writes are announced by content with a TTL**, not by a
  counter. A counter desynchronised permanently and swallowed the user's
  next real copies; the polling fallback can miss a set→restore round
  trip entirely.
- `VirtualDevice::emit` appends its own `SYN_REPORT`. Do not add one.

### Two shortcuts should differ by key, not by modifier

A pair separated only by Shift is fragile: `Alt+Shift` is the layout
switcher on many setups, a compositor can claim the longer combination
first, and anything normalising Shift away collapses the two into one.
The defaults are `Ctrl+Alt+V` / `Ctrl+Alt+M` for that reason.

---

## Data layer

- `Database` owns the tree invariant. `insert`, `update` and
  `move_to_parent` all reject a parent that is the row itself, one of
  its descendants, a sentinel, or missing.
- **Recursive CTEs use `UNION`, never `UNION ALL`.** Without dedup a
  `parent_id` cycle never terminates — while holding the connection
  mutex on the UI thread.
- `load_all` is strict; `load_all_lenient` skips and counts bad rows.
  The GUI uses the lenient one, because one undecodable row must not
  render as "all your snippets are gone".
- Config values are clamped on load and an unparseable `config.toml` is
  moved to `.bak` rather than failing startup. The backward-compat
  promise — missing fields, and missing whole sections, take defaults —
  is covered by tests; keep it.

---

## Testing

**Extract a pure function rather than leaving logic unreachable.** The
highest-severity bugs in this repo all lived inside closures or behind
an X connection where no test could reach them. The seams that exist now
(`match_registration`, `combos_to_retire`, `poll_outcome`,
`drain_wake_from`, `ctrl_v_frames`, `index_of_id`, `parent_for_new_item`,
`subtree_folder_ids`) were each carved out of something untestable, and
each pins a real defect.

Write the test so it **fails against the old code**. Several tests here
were rewritten after review because they passed either way: one computed
its expected value with the same expression as the code under test,
another was timing-dependent and would have passed against the racy
version.

Prefer a deterministic harness to a sleep. `BlockingClipboard` in
`paster.rs` parks a write on a channel so a concurrency test cannot pass
by luck — and note it is *armed* after setup, because a trap that fires
on the test's own fixture deadlocks the test.

---

## Debugging

```
RUST_LOG=fastpaste_platform=debug cargo run --bin fastpaste-gui
```

Log targets follow the crate names. The hotkey reader logs each
registration with its resolved keycode and modifier mask, and each fire
with the id that claimed it plus the raw state that arrived — which is
what distinguishes "nothing fired" from "the wrong thing fired". A
mismatch between the arriving state and the registered mask means the
compositor or the keymap changed it on the way.

`getfacl /dev/uinput` tells you whether paste can work at all; without
access the app degrades to leaving the payload on the clipboard.
