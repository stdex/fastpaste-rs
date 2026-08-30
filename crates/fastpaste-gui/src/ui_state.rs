//! Keep-alive slots for the UI surfaces, owned on the Slint event-loop
//! thread.
//!
//! Two problems this solves.
//!
//! **The "singletons" were not singletons.** `ComponentHandle::show()`
//! keeps the only extra strong reference to a window and `hide()` drops
//! it, so with only `Weak`s retained a hidden window was *freed*. The
//! `if !visible { show() }` branch of the main-window toggle was
//! therefore unreachable, and every hotkey press rebuilt the window from
//! scratch — losing its geometry, its tree model and its editor state,
//! and putting a full construction on the latency path of a popup whose
//! entire point is being quick. Holding a strong handle here makes the
//! `Weak`s elsewhere upgrade for as long as we want the surface to live.
//!
//! **The tray had no slot at all.** `FastpasteTray` is not a
//! `ComponentHandle` (no `as_weak`), so it lived as a local in `main()`
//! and nothing else could reach it — which is why a live language switch
//! never re-localized the tray menu.
//!
//! Strong handles cannot live in a `static`: none of these types are
//! `Send`, and the generated components are not `Clone` either. A
//! thread-local is the right home, and it encodes the rule that worker
//! threads marshal through `slint::invoke_from_event_loop` rather than
//! reach in directly — from any other thread these slots are simply
//! empty.

use std::cell::RefCell;

use crate::{FastpasteTray, MainWindow, OptionsDialog, SelectionDialog};

/// Strong handles to every UI surface that can be open at once.
#[derive(Default)]
struct Ui {
    main_window: Option<MainWindow>,
    options: Option<OptionsDialog>,
    selection: Option<SelectionDialog>,
    tray: Option<FastpasteTray>,
}

thread_local! {
    static UI: RefCell<Ui> = RefCell::new(Ui::default());
}

/// Keep `win` alive until it is explicitly released.
pub fn keep_main_window(win: MainWindow) {
    UI.with(|ui| ui.borrow_mut().main_window = Some(win));
}

pub fn keep_options(dialog: OptionsDialog) {
    UI.with(|ui| ui.borrow_mut().options = Some(dialog));
}

pub fn keep_selection(dialog: SelectionDialog) {
    UI.with(|ui| ui.borrow_mut().selection = Some(dialog));
}

pub fn keep_tray(tray: FastpasteTray) {
    UI.with(|ui| ui.borrow_mut().tray = Some(tray));
}

/// Whether a tray icon is present. `main()` uses this to choose between
/// `run_event_loop` and `run_event_loop_until_quit`.
pub fn has_tray() -> bool {
    UI.with(|ui| ui.borrow().tray.is_some())
}

/// Run `f` against the tray, if there is one.
///
/// Borrow-based rather than handing the handle out, because
/// `FastpasteTray` is neither `Clone` nor a `ComponentHandle`. Keep `f`
/// short and do not call back into this module from inside it.
pub fn with_tray<R>(f: impl FnOnce(&FastpasteTray) -> R) -> Option<R> {
    UI.with(|ui| ui.borrow().tray.as_ref().map(f))
}

/// Release every surface, dropping the tray last.
///
/// Called on the way out of `main()` so the tray icon disappears
/// promptly rather than at process teardown.
pub fn release_all() {
    // Take first, drop after the borrow ends. No component's `Drop`
    // re-enters this module today, but dropping four handles *while* the
    // `RefCell` is mutably borrowed is exactly the re-entrancy this
    // module's own doc warns about, and the two-step costs nothing.
    let released = UI.with(|ui| std::mem::take(&mut *ui.borrow_mut()));
    drop(released);
}
