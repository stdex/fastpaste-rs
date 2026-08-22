// Entry point. Wires together:
//   - fastpaste-app:      AppContext (Settings + ClipboardHistory + DB +
//                          Paster + clipboard/uinput/hotkey + lock)
//   - fastpaste-data:     Database (CRUD) + Item / ItemKind +
//                          HISTORY_FOLDER_ID + HistoryPosition
//   - fastpaste-platform: OPEN_DIALOG_ID / OPEN_MAIN_WINDOW_ID,
//                          ClipboardPayload (via AppContext.clipboard)
//   - fastpaste-gui:      Slint MainWindow + SelectionDialog +
//                          OptionsDialog + FastpasteTray + i18n::I18n
//
// Two global hotkeys (driven by Settings.hotkeys, with startup fallback
// to the defaults so a bad config.toml never aborts the app):
//   Ctrl+U        (default) → OPEN_DIALOG_ID  → SelectionDialog
//   Ctrl+Shift+U  (default) → OPEN_MAIN_WINDOW_ID → MainWindow
//
// Tray icon (Slint native SystemTrayIcon, not the external tray-icon
// crate): left-click toggles Main Window, right-click shows a context
// menu (Open / Selection Dialog / Options / Quit). The tray keeps the
// process alive even when no window is visible — that's why
// `run_event_loop` (not `_until_quit`) is used.
//
// Clipboard monitor: one drainer thread feeds `ctx.clipboard_history`;
// whenever an entry is actually inserted it pings the tree-refresh
// worker, which rebuilds the Main Window tree off the UI thread and
// marshals the model swap into the Slint loop.
//
// All Slint windows/tray are created on the event-loop thread. Worker
// threads marshal UI work via `slint::invoke_from_event_loop`.

use std::sync::{Arc, Mutex};

use fastpaste_app::{AppContext, Settings};
use fastpaste_data::{Item, ItemKind};
use fastpaste_platform::{GlobalHotkey, OPEN_DIALOG_ID, OPEN_MAIN_WINDOW_ID};

// `Model` trait is in scope so we can call `.row_data()` on a `ModelRc` in
// `selected_row_id` / `current_parent_id`.
use slint::Model;

slint::include_modules!();

mod tree_builder;
use tree_builder::{build_tree_items_with_history, history_index_from_item_id};

use slint_tree_view::TreeItem;

mod i18n;
use i18n::I18n;

/// Module-level handle to the persistent Main Window. Without this, every
/// Ctrl+Shift+U press (or tray click) would build a fresh `MainWindow`,
/// stacking duplicate editor windows.
///
/// We hold a `Weak<MainWindow>` (not a strong ref) so the window is free to
/// be torn down when the user closes it; the Weak then fails to upgrade and
/// the next show builds a new one. The `Mutex` guards concurrent access
/// from the Slint event-loop thread (handlers run here).
static MAIN_WINDOW: Mutex<Option<slint::Weak<MainWindow>>> = Mutex::new(None);

/// Module-level handle to the Options Dialog. Same pattern as MAIN_WINDOW:
/// at most one dialog at a time. Held as Weak so closing the dialog frees it.
static OPTIONS_DIALOG: Mutex<Option<slint::Weak<OptionsDialog>>> = Mutex::new(None);

/// Module-level handle to the Selection Dialog — same singleton pattern as
/// the Options Dialog, so repeated hotkey presses repopulate one dialog
/// instead of stacking overlapping ones.
static SELECTION_DIALOG: Mutex<Option<slint::Weak<SelectionDialog>>> = Mutex::new(None);

/// Ping channel into the tree-refresh worker (see `start_tree_refresh_worker`).
static TREE_REFRESH_TX: Mutex<Option<std::sync::mpsc::Sender<()>>> = Mutex::new(None);

/// Folder ids (DB rowids plus `HISTORY_FOLDER_ID`) the user has collapsed
/// in the Main Window tree. Shared between the UI thread (expand/collapse
/// callbacks mutate it) and the tree-refresh worker (reads it when
/// flattening), hence the Mutex. Cleared entries mean "expanded"; an id
/// of a deleted folder simply lingers harmlessly until exit.
static COLLAPSED_FOLDERS: std::sync::LazyLock<Mutex<std::collections::HashSet<i64>>> =
    std::sync::LazyLock::new(|| Mutex::new(std::collections::HashSet::new()));

/// Module-level handle to the active locale code (e.g. "en", "ru").
///
/// We store the settings string (not a resolved translator) because the
/// language can change at runtime via the Options dialog, and building an
/// [`I18n`] is free — the translation bundles live in a `Sync` static
/// inside `i18n.rs` (fluent-templates' `static_loader!`), so `I18n::new`
/// is just one langid parse. See the `i18n()` accessor below.
static I18N_LOCALE: Mutex<String> = Mutex::new(String::new());

/// (code, label) pairs for the language combobox. `system` must remain
/// index 0 — `lang_index_for_code` falls back to it for unknown codes.
const LANGUAGES: &[(&str, &str)] = &[
    ("system", "System default"),
    ("en", "English"),
    ("ru", "Русский"),
    ("de", "Deutsch"),
    ("es", "Español"),
    ("zh_CN", "中文 (简体)"),
];

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    tracing::info!("fastpaste-gui starting");

    // ---- Composition root: build all services + open the DB -----------------
    // AppContext::build() loads Settings, wires Paster from
    // paste.delay_ms/restore_clipboard, builds ClipboardHistory from
    // clipboard_history.max_items/enabled, and takes the single-instance
    // advisory lock.
    let ctx = Arc::new(AppContext::build()?);

    // ---- i18n: stash the active locale so `i18n()` can build a fresh ----
    // translator on demand. See the I18N_LOCALE comment for why we don't
    // store the translator itself.
    *I18N_LOCALE.lock().unwrap() = ctx.settings().general.language.clone();
    {
        // Resolve the locale once at startup and log it; the Translations
        // global is filled after the tray is created (see below), because
        // a component handle is needed to reach the global in this Slint
        // version, and every window re-pushes at creation anyway.
        let startup_i18n = I18n::new(&ctx.settings().general.language);
        tracing::info!("i18n: active locale = {}", startup_i18n.locale());
    }

    // ---- Tray icon --------------------------------------------------------
    // Built before the event loop starts. Slint's native SystemTrayIcon shares
    // the same loop as the windows, so creating it before `run_event_loop`
    // is sufficient — no separate GTK loop needed (which would be the case
    // for the external `tray-icon` crate).
    let tray: Option<FastpasteTray> = match build_tray_icon(&ctx) {
        Ok(t) => Some(t),
        Err(e) => {
            // Non-fatal: if the platform can't show a tray (headless CI,
            // missing status-notifier daemon on Linux), keep running so the
            // hotkeys + Main Window still work. The user just loses the
            // tray affordance.
            tracing::warn!("failed to build tray icon; continuing without it: {e}");
            None
        }
    };
    // Fill the Translations global now that a component handle exists;
    // the tray's menu labels bind to it reactively. Windows created later
    // re-push at their own creation (covers the headless no-tray case).
    if let Some(t) = &tray {
        apply_translations(t, &i18n());
    }

    // ---- Hotkeys: from Settings, with startup fallback --------------------
    // A garbage sequence in config.toml must not abort the app: try the
    // configured value, fall back to the default, and record what is
    // actually live (session-only; the file on disk is left untouched).
    let defaults = Settings::default().hotkeys;
    let eff_dialog = register_hotkey_with_fallback(
        &ctx.hotkey,
        OPEN_DIALOG_ID,
        &ctx.settings().hotkeys.open_dialog,
        &defaults.open_dialog,
    );
    let eff_main = register_hotkey_with_fallback(
        &ctx.hotkey,
        OPEN_MAIN_WINDOW_ID,
        &ctx.settings().hotkeys.open_main_window,
        &defaults.open_main_window,
    );
    if eff_dialog != ctx.settings().hotkeys.open_dialog
        || eff_main != ctx.settings().hotkeys.open_main_window
    {
        let mut s = ctx.settings();
        s.hotkeys.open_dialog = eff_dialog;
        s.hotkeys.open_main_window = eff_main;
        ctx.set_settings(s);
    }
    tracing::info!(
        "registered global hotkeys: {} (dialog), {} (main window)",
        ctx.settings().hotkeys.open_dialog,
        ctx.settings().hotkeys.open_main_window
    );

    // Spawn the hotkey-events thread: drain `hotkey.events()` and marshal
    // window operations into the Slint loop (windows must be created on
    // the event-loop thread).
    let ctx_for_dialog = ctx.clone();
    let ctx_for_main = ctx.clone();
    let hotkey_backend = ctx.hotkey.clone();
    std::thread::Builder::new()
        .name("hotkey-events".into())
        .spawn(move || {
            let rx = hotkey_backend.events();
            while let Ok(id) = rx.recv() {
                let task: Box<dyn FnOnce() + Send + 'static> = match id {
                    OPEN_DIALOG_ID => {
                        let ctx = ctx_for_dialog.clone();
                        Box::new(move || show_dialog_and_paste(ctx))
                    }
                    OPEN_MAIN_WINDOW_ID => {
                        let ctx = ctx_for_main.clone();
                        Box::new(move || show_main_window(ctx))
                    }
                    other => {
                        tracing::warn!("hotkey-events: ignoring unknown id {other}");
                        continue;
                    }
                };
                if let Err(e) = slint::invoke_from_event_loop(task) {
                    tracing::warn!("invoke_from_event_loop failed (loop terminated?): {e}");
                }
            }
            tracing::info!("hotkey-events: channel closed");
        })?;

    // ---- Clipboard monitor + tree refresh ----------------------------------
    start_tree_refresh_worker(ctx.clone());
    spawn_clipboard_drainer(ctx.clone())?;

    tracing::info!(
        "fastpaste ready. Tray: {}, hotkeys: {} / {}, \
         clipboard history: {} (max {})",
        if tray.is_some() { "on" } else { "off" },
        ctx.settings().hotkeys.open_dialog,
        ctx.settings().hotkeys.open_main_window,
        if ctx.settings().clipboard_history.enabled {
            "on"
        } else {
            "off"
        },
        ctx.settings().clipboard_history.max_items,
    );

    // Run the Slint event loop. With a tray icon present, `run_event_loop`
    // stays alive until the tray's "Quit" menu item calls
    // `slint::quit_event_loop()` — a visible SystemTrayIcon keeps the loop
    // running by itself. Without a tray, the same call quits when the last
    // window closes (headless CI can still Ctrl-C the process).
    slint::run_event_loop()
        .map_err(|e| anyhow::anyhow!("Slint event loop exited with error: {e}"))?;

    Ok(())
}

/// Register `wanted`; on failure warn and try `fallback`. Returns the
/// sequence that is actually live (still `wanted` if both fail — the
/// hotkey is then inert for the session, which is logged, never fatal).
fn register_hotkey_with_fallback(
    hotkey: &Arc<dyn GlobalHotkey>,
    id: u32,
    wanted: &str,
    fallback: &str,
) -> String {
    match hotkey.register(id, wanted) {
        Ok(()) => wanted.to_string(),
        Err(e) => {
            tracing::warn!(
                "hotkey id={id}: sequence {wanted:?} rejected ({e}); \
                 trying default {fallback:?}"
            );
            match hotkey.register(id, fallback) {
                Ok(()) => fallback.to_string(),
                Err(e2) => {
                    tracing::error!(
                        "hotkey id={id}: default {fallback:?} also failed ({e2}); \
                         disabled for this session"
                    );
                    wanted.to_string()
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tray icon
// ---------------------------------------------------------------------------

/// Build the system tray icon + its menu, wire all callbacks to marshal UI
/// work into the Slint event loop. The tray handle is returned so `main()`
/// can decide between `run_event_loop` and `run_event_loop_until_quit`.
///
/// All callbacks use `slint::invoke_from_event_loop` because — even though
/// the Slint-native tray shares the loop with the windows — the safe way
/// to interact with Slint APIs from a non-event-loop callback is still to
/// marshal. (Slint's tray menu callbacks fire on the loop thread today,
/// but the explicit marshal future-proofs us against a backend change and
/// documents the intent.)
fn build_tray_icon(ctx: &Arc<AppContext>) -> anyhow::Result<FastpasteTray> {
    let tray = FastpasteTray::new()?;

    // Menu labels + tooltip come from the Translations global (default
    // bindings in tray_icon.slint), which `apply_translations()` filled
    // before this call — nothing to set by hand here, and a language
    // change re-localizes the tray with the rest of the UI.

    // Left-click + "Open Main Window" menu item → toggle Main Window.
    let ctx_toggle = ctx.clone();
    let ctx_open = ctx.clone();
    tray.on_toggle_main_window(move || {
        let ctx = ctx_toggle.clone();
        let _ = slint::invoke_from_event_loop(move || show_main_window(ctx));
    });
    tray.on_open_main_window(move || {
        let ctx = ctx_open.clone();
        let _ = slint::invoke_from_event_loop(move || show_main_window(ctx));
    });

    // "Selection Dialog" → show SelectionDialog.
    let ctx_dialog = ctx.clone();
    tray.on_open_selection_dialog(move || {
        let ctx = ctx_dialog.clone();
        let _ = slint::invoke_from_event_loop(move || show_dialog_and_paste(ctx));
    });

    // "Options..." → show Options Dialog.
    let ctx_opts = ctx.clone();
    tray.on_open_options(move || {
        let ctx = ctx_opts.clone();
        let _ = slint::invoke_from_event_loop(move || show_options_dialog(ctx));
    });

    // "Quit" → stop the event loop.
    tray.on_quit(|| {
        tracing::info!("tray Quit selected; exiting event loop");
        let _ = slint::quit_event_loop();
    });

    Ok(tray)
}

// ---------------------------------------------------------------------------
// Clipboard monitor + tree refresh
// ---------------------------------------------------------------------------

/// Spawn the clipboard-change drainer: the single owner of the platform
/// `changes()` receiver. For each payload it feeds the history ring; when
/// an entry was actually inserted it pings the tree-refresh worker.
/// Exits when the clipboard backend drops its sender (process shutdown).
fn spawn_clipboard_drainer(ctx: Arc<AppContext>) -> anyhow::Result<()> {
    let ctx_drain = ctx.clone();
    let changes_rx = ctx_drain.clipboard.changes();
    std::thread::Builder::new()
        .name("clipboard-drainer".into())
        .spawn(move || {
            tracing::debug!("clipboard-drainer: started");
            while let Ok(payload) = changes_rx.recv() {
                if ctx_drain.clipboard_history.on_clipboard_changed(payload) {
                    request_tree_refresh();
                }
            }
            tracing::info!("clipboard-drainer: changes channel closed; exiting");
        })?;
    Ok(())
}

/// Long-lived worker that rebuilds the Main Window tree OFF the UI thread.
/// Requests are coalesced (a clipboard burst produces one rebuild); the
/// built rows cross into the Slint loop via `invoke_from_event_loop`,
/// where the model swap + selection restore run. User mutations also ping
/// this worker after their DB write completes, so the UI always reflects
/// the latest commit without blocking the event loop on `load_all`.
fn start_tree_refresh_worker(ctx: Arc<AppContext>) {
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    *TREE_REFRESH_TX.lock().unwrap() = Some(tx);
    std::thread::Builder::new()
        .name("tree-refresh".into())
        .spawn(move || {
            loop {
                if rx.recv().is_err() {
                    return; // requesters gone — process shutdown
                }
                while rx.try_recv().is_ok() {} // coalesce bursts
                let rows = build_refresh_rows(&ctx);
                let weak = MAIN_WINDOW.lock().unwrap().clone();
                let Some(weak) = weak else { continue };
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(win) = weak.upgrade() {
                        swap_tree_model_preserving_selection(&win, rows);
                    }
                });
            }
        })
        .expect("spawn tree-refresh worker");
}

/// Ask for a Main Window tree rebuild. Cheap: just a channel ping — the
/// worker coalesces, builds rows off-thread, and marshals the swap.
fn request_tree_refresh() {
    if let Some(tx) = TREE_REFRESH_TX.lock().unwrap().as_ref() {
        let _ = tx.send(());
    }
}

/// Load DB items + clipboard history and flatten into tree rows. Runs on
/// the tree-refresh worker thread (never the UI thread).
fn build_refresh_rows(ctx: &AppContext) -> Vec<TreeItem> {
    let items = with_db(ctx, |db| {
        db.load_all().unwrap_or_else(|e| {
            tracing::error!("load_all during refresh: {e}");
            Vec::new()
        })
    })
    .unwrap_or_default();
    // The folder label is model data (not a binding), so localize it here;
    // a language change triggers a refresh via `apply_options`.
    let history_folder_label = i18n().msg("clipboard-history-folder");
    // Snapshot the collapsed set — the lock must not be held across the
    // (potentially slow) DB read below.
    let collapsed = COLLAPSED_FOLDERS.lock().unwrap().clone();
    build_tree_items_with_history(
        &items,
        &ctx.clipboard_history.entries(),
        ctx.settings().clipboard_history.position,
        &history_folder_label,
        &collapsed,
    )
}

/// Swap the tree model, keeping the selection pointing at the same item:
/// the previously-selected `internal_id` is re-located by scan after the
/// swap. This fixes the old behavior where an un-reset `current-index`
/// silently retargeted a subsequent title/body edit at the wrong row after
/// Add/Move/clipboard-triggered refreshes. History rows (positional
/// synthetic ids) and vanished ids (deleted) clear the selection.
fn swap_tree_model_preserving_selection(win: &MainWindow, rows: Vec<TreeItem>) {
    let prev_id = selected_row_id_i32(win);
    win.set_tree_model(slint::ModelRc::new(slint::VecModel::from(rows)));
    if prev_id <= 0 {
        // No selection, the virtual history folder, or a history row —
        // "restore" is meaningless there.
        clear_editor(win);
        return;
    }
    let model = win.get_tree_model();
    for i in 0..model.row_count() {
        if model.row_data(i).is_some_and(|r| r.internal_id == prev_id) {
            // Setting the index from Rust does not invoke the
            // current-changed callback, but the editor already mirrors
            // this row (it is the same item as before the swap).
            win.set_current_index(i as i32);
            return;
        }
    }
    clear_editor(win); // id vanished — deleted while selected
}

fn clear_editor(win: &MainWindow) {
    win.set_current_index(-1);
    win.set_editor_title("".into());
    win.set_editor_body("".into());
    win.set_editor_enabled(false);
}

// ---------------------------------------------------------------------------
// Main Window (CRUD editor)
// ---------------------------------------------------------------------------

/// Lock the shared Database and run `op` with the guard. Returns `None`
/// (and logs) when the mutex is poisoned — a prior panic left it wedged;
/// the app can continue, the op is just skipped. `op` reports its own
/// errors through its return value.
fn with_db<R>(ctx: &AppContext, op: impl FnOnce(&fastpaste_data::Database) -> R) -> Option<R> {
    match ctx.db.lock() {
        Ok(db) => Some(op(&db)),
        Err(e) => {
            tracing::error!("Database mutex poisoned: {e}");
            None
        }
    }
}

/// Upgrade a singleton window slot: `Some(handle)` if the window is still
/// alive, `None` if it was never stored or has been torn down. The lock
/// is released before returning, so callers can show/hide freely.
fn live<T: slint::ComponentHandle>(slot: &Mutex<Option<slint::Weak<T>>>) -> Option<T> {
    slot.lock().unwrap().as_ref().and_then(|w| w.upgrade())
}

/// Toggle the Main Window: if it exists and is visible, hide it; if it exists
/// but is hidden, show it; if it has been torn down (or never built), create
/// a fresh one and wire it up.
///
/// Must be called on the Slint event-loop thread (use
/// `slint::invoke_from_event_loop` from worker threads).
fn show_main_window(ctx: Arc<AppContext>) {
    if let Some(win) = live(&MAIN_WINDOW) {
        // Existing window — toggle visibility, then bring the model up to
        // date (a hidden window missed any clipboard-driven refreshes).
        let visible = win.window().is_visible();
        if let Err(e) = if visible { win.hide() } else { win.show() } {
            tracing::error!(
                "failed to {} Main Window: {e}",
                if visible { "hide" } else { "show" }
            );
        }
        if !visible {
            request_tree_refresh();
        }
        return;
    }
    // No live window — build one and store its Weak *before* showing, so a
    // clipboard ping landing between show and store can't be missed.
    let win = match build_main_window(ctx) {
        Some(w) => w,
        None => return,
    };
    let weak = win.as_weak();
    *MAIN_WINDOW.lock().unwrap() = Some(weak);
    if let Err(e) = win.show() {
        tracing::error!("failed to show MainWindow: {e}");
    }
    // Initial load — the tree model starts empty and is filled by the
    // worker (one frame of empty tree on first open at most).
    request_tree_refresh();
}

/// Run a mutating DB operation and request a tree refresh when it
/// succeeded. The shared tail of the toolbar callbacks: lock (graceful on
/// poison), run, log errors, refresh.
fn db_write_then_refresh(
    ctx: &AppContext,
    what: &str,
    op: impl FnOnce(&fastpaste_data::Database) -> Result<(), fastpaste_data::DataError>,
) {
    match with_db(ctx, op) {
        Some(Ok(())) => request_tree_refresh(),
        Some(Err(e)) => tracing::error!("{what}: {e}"),
        None => {}
    }
}

/// Construct a fresh `MainWindow` and wire all CRUD callbacks against the
/// shared Database. The initial tree load is requested separately (see
/// `show_main_window`); mutating callbacks ping the tree-refresh worker
/// after a successful write.
fn build_main_window(ctx: Arc<AppContext>) -> Option<MainWindow> {
    let win = match MainWindow::new() {
        Ok(w) => w,
        Err(e) => {
            tracing::error!("failed to create MainWindow: {e}");
            return None;
        }
    };
    // Ensure the UI language is current even in the headless no-tray path.
    apply_translations(&win, &i18n());

    // Tree palette hook-in. TreeViewStyle is a per-compilation singleton
    // global; Slint 1.17 has no in-.slint way to assign global properties,
    // so this is the documented Rust-side hook (one tree in the app → no
    // per-instance conflicts). Colors only: the dimension setters take a
    // raw Coord (physical px) and at HiDPI scale factors blanked the
    // ListView in testing, so row-height/indentation keep their 28px/20px
    // defaults — compact enough next to the 28px toolbar controls.
    {
        use slint::Global;
        let rgb = slint::Color::from_rgb_u8;
        let style = slint_tree_view::TreeViewStyle::get(&win);
        style.set_background_color(rgb(0xff, 0xff, 0xff));
        style.set_text_color(rgb(0x21, 0x25, 0x29));
        style.set_highlight_color(rgb(0x25, 0x63, 0xeb));
        style.set_highlighted_text_color(rgb(0xff, 0xff, 0xff));
        style.set_hover_color(rgb(0xee, 0xf3, 0xfb));
        style.set_branch_indicator_color(rgb(0x6a, 0x73, 0x7d));
    }

    // ---- Add Folder — inside selected folder, or top-level ----------------
    {
        let ctx_clone = ctx.clone();
        let weak = win.as_weak();
        win.on_add_folder_clicked(move || {
            let Some(w) = weak.upgrade() else { return };
            let parent_id = current_parent_id(&w);
            let mut item = Item::new_folder(parent_id, "New Folder");
            db_write_then_refresh(&ctx_clone, "insert folder", |db| db.insert(&mut item));
        });
    }

    // ---- Add Plain — inside selected folder, or top-level ----------------
    {
        let ctx_clone = ctx.clone();
        let weak = win.as_weak();
        win.on_add_plain_clicked(move || {
            let Some(w) = weak.upgrade() else { return };
            let parent_id = current_parent_id(&w);
            let mut item = Item::new_plain(parent_id, "New Snippet", "");
            db_write_then_refresh(&ctx_clone, "insert plain", |db| db.insert(&mut item));
        });
    }

    // ---- Delete selected subtree -------------------------------------------
    // remove_subtree recursively deletes the item and all its descendants
    // (folders take their children with them) — matches the C++ reference.
    // The tree-refresh swap clears the selection automatically when the
    // deleted id disappears from the model.
    //
    // History rows have negative ids and never reach the DB; if the user
    // somehow selects one and hits delete, we treat it as a no-op rather
    // than calling remove_subtree with a negative id (which SQLite would
    // happily no-op, but explicit is clearer).
    {
        let ctx_clone = ctx.clone();
        let weak = win.as_weak();
        win.on_delete_clicked(move || {
            let Some(w) = weak.upgrade() else { return };
            let Some(id) = selected_row_id(&w) else {
                return;
            };
            db_write_then_refresh(&ctx_clone, "delete subtree", |db| db.remove_subtree(id));
        });
    }

    // ---- Move Up -----------------------------------------------------------
    {
        let ctx_clone = ctx.clone();
        let weak = win.as_weak();
        win.on_move_up_clicked(move || {
            let Some(w) = weak.upgrade() else { return };
            let Some(id) = selected_row_id(&w) else {
                return;
            };
            db_write_then_refresh(&ctx_clone, "move up", |db| db.move_up(id));
        });
    }

    // ---- Move Down ---------------------------------------------------------
    {
        let ctx_clone = ctx.clone();
        let weak = win.as_weak();
        win.on_move_down_clicked(move || {
            let Some(w) = weak.upgrade() else { return };
            let Some(id) = selected_row_id(&w) else {
                return;
            };
            db_write_then_refresh(&ctx_clone, "move down", |db| db.move_down(id));
        });
    }

    // ---- Title edited ------------------------------------------------------
    // Persists to DB but does NOT refresh the tree — a refresh here can
    // race the editor's focus loss.
    {
        let ctx_clone = ctx.clone();
        let weak = win.as_weak();
        win.on_title_edited(move |new_title: slint::SharedString| {
            let Some(w) = weak.upgrade() else { return };
            let Some(id) = selected_row_id(&w) else {
                return;
            };
            if let Some(Err(e)) = with_db(&ctx_clone, |db| {
                let Some(mut item) = db.get(id)? else {
                    return Ok(());
                };
                item.title = new_title.to_string();
                db.update(&item)
            }) {
                tracing::error!("update title (id={id}): {e}");
            }
        });
    }

    // ---- Body edited -------------------------------------------------------
    {
        let ctx_clone = ctx.clone();
        let weak = win.as_weak();
        win.on_body_edited(move |new_body: slint::SharedString| {
            let Some(w) = weak.upgrade() else { return };
            // History rows can be previewed/edited in the editor; persisting
            // an edit to a history entry isn't meaningful (history is a ring
            // buffer, not a CRUD table), so we no-op the body-edit for those.
            if history_index_from_item_id(selected_row_id_i32(&w)).is_some() {
                return;
            }
            let Some(id) = selected_row_id(&w) else {
                return;
            };
            if let Some(Err(e)) = with_db(&ctx_clone, |db| {
                let Some(mut item) = db.get(id)? else {
                    return Ok(());
                };
                item.body_plain = new_body.to_string();
                db.update(&item)
            }) {
                tracing::error!("update body (id={id}): {e}");
            }
        });
    }

    // ---- Row selected (TreeView click / keyboard) -------------------------
    // The TreeView emits `current-changed(id)` after updating its own
    // `current-index`; the controller mirrors the row into the editor
    // pane (see `mirror_selection_to_editor`).
    {
        let weak = win.as_weak();
        win.on_current_changed(move |_id: i32| {
            let Some(w) = weak.upgrade() else { return };
            mirror_selection_to_editor(&w);
        });
    }

    // ---- Item activated (double-click / Enter) ---------------------------
    // v1 has no per-row action; we accept the callback so it doesn't warn
    // at runtime. A future controller can wire this to "open folder" or
    // "paste this snippet immediately".
    {
        let weak = win.as_weak();
        win.on_item_activated(move |_id: i32| {
            let Some(_w) = weak.upgrade() else { return };
            // No-op for now; hook exists so future features can plug in.
        });
    }

    // ---- Expand / collapse (▶/▼ click, Left/Right keys, double-click) ----
    // The collapsed-set is the single source of truth; each mutation pings
    // the refresh worker, whose rebuilt flat model omits the hidden
    // descendants (that is all "collapse" means for a flat-list view).
    {
        win.on_item_expand_requested(move |id: i32| {
            COLLAPSED_FOLDERS.lock().unwrap().remove(&(id as i64));
            request_tree_refresh();
        });
        win.on_item_collapse_requested(move |id: i32| {
            COLLAPSED_FOLDERS.lock().unwrap().insert(id as i64);
            request_tree_refresh();
        });
    }

    // ---- Recursive expand/collapse (Asterisk / Shift+double-click) --------
    // Walks the *visible* flat model: every folder inside the target's
    // subtree (the contiguous run of deeper rows) joins the set operation.
    // `levels` caps the depth when >= 0 (-1 = unlimited).
    {
        let weak = win.as_weak();
        win.on_recursive_expand_requested(move |id: i32, expand: bool, levels: i32| {
            let Some(w) = weak.upgrade() else { return };
            let Some(subtree) = visible_subtree_folder_ids(&w, id, levels) else {
                return;
            };
            let mut set = COLLAPSED_FOLDERS.lock().unwrap();
            if expand {
                set.remove(&(id as i64));
                set.retain(|fid| !subtree.contains(fid));
            } else {
                set.insert(id as i64);
                set.extend(subtree);
            }
            drop(set);
            request_tree_refresh();
        });
    }

    // ---- Left-arrow at a leaf/collapsed row: jump to the parent ----------
    // The TreeView emits the parent id (Slint can't search the model from
    // inside it); we locate the row, set the index programmatically (a
    // Rust-side set does NOT fire current-changed), and mirror the editor.
    {
        let weak = win.as_weak();
        win.on_current_parent_change_requested(move |_id: i32, parent_id: i32| {
            let Some(w) = weak.upgrade() else { return };
            let model = w.get_tree_model();
            for i in 0..model.row_count() {
                if model
                    .row_data(i)
                    .is_some_and(|r| r.internal_id == parent_id)
                {
                    w.set_current_index(i as i32);
                    mirror_selection_to_editor(&w);
                    return;
                }
            }
        });
    }

    Some(win)
}

/// Mirror the current tree row into the editor pane. Shared by the
/// `current-changed` callback (TreeView mouse/keyboard navigation) and
/// the `current-parent-change-requested` handler (Left-arrow jump to
/// parent), which sets the index programmatically.
fn mirror_selection_to_editor(win: &MainWindow) {
    let idx = win.get_current_index();
    if idx < 0 {
        win.set_editor_title("".into());
        win.set_editor_body("".into());
        win.set_editor_enabled(false);
        return;
    }
    let Some(row) = win.get_tree_model().row_data(idx as usize) else {
        return;
    };
    win.set_editor_title(row.text);
    win.set_editor_body(row.user_data);
    // Folders are not directly editable in v1 (no body to edit).
    // Qt-style: TreeItem has no `is-folder`; we look at `item-type`
    // (which the model populates with `ItemKind::Folder.as_i64()`).
    win.set_editor_enabled(row.item_type != ItemKind::Folder.as_i64() as i32);
}

/// Folder ids of every *visible* folder strictly inside the subtree of
/// `root_id`, optionally capped to `levels` below it (-1 = unlimited).
/// Returns None when `root_id` isn't in the model. Walks the flat model:
/// a subtree is the contiguous run of rows deeper than the root's depth.
fn visible_subtree_folder_ids(win: &MainWindow, root_id: i32, levels: i32) -> Option<Vec<i64>> {
    let model = win.get_tree_model();
    let mut out = Vec::new();
    let mut root_depth = 0;
    let mut started = false;
    for i in 0..model.row_count() {
        let row = model.row_data(i)?;
        if !started {
            if row.internal_id == root_id {
                root_depth = row.depth;
                started = true;
            }
            continue;
        }
        // The contiguous deeper-than-root run ended → subtree closed.
        if row.depth <= root_depth {
            break;
        }
        if row.has_children && (levels < 0 || row.depth - root_depth <= levels) {
            out.push(row.internal_id as i64);
        }
    }
    started.then_some(out)
}

/// Pull the DB rowid of the currently-selected tree row, or None if nothing
/// is selected / the index is out of bounds / the row is a virtual folder
/// (the Clipboard History folder has id HISTORY_FOLDER_ID; history entries
/// have negative ids and are intercepted by `history_index_from_item_id`).
fn selected_row_id(win: &MainWindow) -> Option<i64> {
    let id = selected_row_id_i32(win);
    if id <= 0 { None } else { Some(id as i64) }
}

/// Same as `selected_row_id` but returns the raw i32 the Slint TreeItem
/// carries, including negative ids for history rows. Used by callers that
/// need to distinguish a history row (id < -1) from a no-selection (-1 /
/// out of bounds).
fn selected_row_id_i32(win: &MainWindow) -> i32 {
    let idx = win.get_current_index();
    if idx < 0 {
        return -1;
    }
    let Some(row) = win.get_tree_model().row_data(idx as usize) else {
        return -1;
    };
    row.internal_id
}

/// Determine the parent_id for a new item: if a folder is currently selected,
/// add inside it; otherwise add at top level (parent_id=0).
fn current_parent_id(win: &MainWindow) -> i64 {
    let idx = win.get_current_index();
    if idx < 0 {
        return 0;
    }
    let Some(row) = win.get_tree_model().row_data(idx as usize) else {
        return 0;
    };
    // Qt-style: TreeItem has no `is-folder`; check `item-type` instead.
    // The model populates item-type with `ItemKind::Folder.as_i64()` for
    // folders.
    if row.item_type == ItemKind::Folder.as_i64() as i32 && row.internal_id > 0 {
        row.internal_id as i64
    } else {
        0
    }
}

// ---------------------------------------------------------------------------
// Selection Dialog (quick paste)
// ---------------------------------------------------------------------------

/// Build and show the SelectionDialog — or repopulate the existing one if
/// it is still alive (repeated hotkey presses must not stack dialogs).
/// Must be called on the Slint event-loop thread.
fn show_dialog_and_paste(ctx: Arc<AppContext>) {
    // Singleton: if one is already alive, refresh its contents.
    if let Some(d) = live(&SELECTION_DIALOG) {
        repopulate_selection_dialog(&d, &ctx);
        let _ = d.show();
        return;
    }

    let dialog = match SelectionDialog::new() {
        Ok(d) => d,
        Err(e) => {
            tracing::error!("failed to create SelectionDialog: {e}");
            return;
        }
    };
    // Ensure the UI language is current even in the headless no-tray path.
    apply_translations(&dialog, &i18n());
    repopulate_selection_dialog(&dialog, &ctx);

    let dialog_weak2 = dialog.as_weak();
    dialog.on_closed(move || {
        if let Some(d) = dialog_weak2.upgrade() {
            d.hide().ok();
        }
    });

    if let Err(e) = dialog.show() {
        tracing::error!("failed to show SelectionDialog: {e}");
    }
    *SELECTION_DIALOG.lock().unwrap() = Some(dialog.as_weak());
}

/// (Re)load the snippet list into the dialog and re-install the paste
/// callback with a fresh capture, so it pastes what is currently listed.
fn repopulate_selection_dialog(d: &SelectionDialog, ctx: &Arc<AppContext>) {
    let plain_snippets: Vec<Item> = with_db(ctx, |db| {
        db.load_all().unwrap_or_else(|e| {
            tracing::error!("load_all failed for dialog: {e}");
            Vec::new()
        })
    })
    .unwrap_or_default()
    .into_iter()
    .filter(|i| i.kind == ItemKind::Plain)
    .collect();

    let model_rows: Vec<SnippetRow> = plain_snippets
        .iter()
        .map(|i| SnippetRow {
            title: i.title.as_str().into(),
            body: i.body_plain.as_str().into(),
        })
        .collect();
    d.set_snippets(slint::ModelRc::new(slint::VecModel::from(model_rows)));
    d.set_selected_index(0);

    // Enter → paste on a background thread so the UI thread is free to hide
    // the window and the paste delay doesn't block the event loop.
    let paster = ctx.paster.clone();
    let dialog_weak = d.as_weak();
    d.on_paste_selected(move |idx: i32| {
        let idx = idx as usize;
        if let Some(s) = plain_snippets.get(idx) {
            let body = s.body_plain.clone();
            let paster = paster.clone();
            if let Err(e) = std::thread::Builder::new()
                .name("paste-worker".into())
                .spawn(move || {
                    if let Err(e) = paster.paste_text(&body) {
                        tracing::error!("paste failed: {e}");
                    }
                })
            {
                tracing::error!("failed to spawn paste-worker: {e}");
            }
        }
        if let Some(d) = dialog_weak.upgrade() {
            d.hide().ok();
        }
    });
}

// ---------------------------------------------------------------------------
// Options Dialog
// ---------------------------------------------------------------------------

/// Show the Options Dialog. If one is already open, just show it again
/// (`show()` on an already-visible window is harmless).
///
/// The dialog's fields are seeded from the live settings snapshot; on
/// OK/Apply, `apply_options` validates + persists + swaps the active
/// settings (see its doc for the ordering).
fn show_options_dialog(ctx: Arc<AppContext>) {
    // If one is already open, focus it instead of stacking.
    if let Some(d) = live(&OPTIONS_DIALOG) {
        let _ = d.show();
        return;
    }

    let dialog = match OptionsDialog::new() {
        Ok(d) => d,
        Err(e) => {
            tracing::error!("failed to create OptionsDialog: {e}");
            return;
        }
    };
    // Ensure the UI language is current even in the headless no-tray path.
    apply_translations(&dialog, &i18n());

    // Seed language options from the single LANGUAGES table (codes must
    // match what `Settings::general.language` accepts).
    let lang_rows: Vec<LanguageOption> = LANGUAGES
        .iter()
        .map(|&(code, label)| LanguageOption {
            code: code.into(),
            label: label.into(),
        })
        .collect();
    let lang_labels: Vec<slint::SharedString> = lang_rows.iter().map(|l| l.label.clone()).collect();
    dialog.set_languages(slint::ModelRc::new(slint::VecModel::from(lang_rows)));
    dialog.set_language_labels(slint::ModelRc::new(slint::VecModel::from(lang_labels)));

    // Initial values from the current settings.
    let s = ctx.settings();
    let lang_idx = lang_index_for_code(&s.general.language);
    dialog.set_language_index(lang_idx as i32);
    dialog.set_hotkey_open_dialog(s.hotkeys.open_dialog.as_str().into());
    dialog.set_hotkey_open_main_window(s.hotkeys.open_main_window.as_str().into());
    dialog.set_history_enabled(s.clipboard_history.enabled);
    dialog.set_history_max_items(s.clipboard_history.max_items as i32);
    // The dialog's combobox declares 0 = Top, 1 = Bottom.
    dialog.set_history_position_index(matches!(
        s.clipboard_history.position,
        fastpaste_data::HistoryPosition::Bottom
    ) as i32);
    dialog.set_paste_delay_ms(s.paste.delay_ms as i32);
    dialog.set_paste_restore_clipboard(s.paste.restore_clipboard);

    // OK → apply + hide.
    let ctx_ok = ctx.clone();
    let weak_ok = dialog.as_weak();
    dialog.on_ok_clicked(move || {
        if let Some(d) = weak_ok.upgrade() {
            apply_options(&ctx_ok, &d);
            let _ = d.hide();
        }
    });

    // Apply → apply but keep open.
    let ctx_apply = ctx.clone();
    let weak_apply = dialog.as_weak();
    dialog.on_apply_clicked(move || {
        if let Some(d) = weak_apply.upgrade() {
            apply_options(&ctx_apply, &d);
        }
    });

    // Cancel → just hide.
    let weak_cancel = dialog.as_weak();
    dialog.on_cancel_clicked(move || {
        if let Some(d) = weak_cancel.upgrade() {
            let _ = d.hide();
        }
    });

    if let Err(e) = dialog.show() {
        tracing::error!("failed to show OptionsDialog: {e}");
    }

    *OPTIONS_DIALOG.lock().unwrap() = Some(dialog.as_weak());
}

/// Read the OptionsDialog's fields back into a `Settings`, apply runtime
/// side-effects, and persist. Ordering:
///
/// 1. Snapshot the current settings (a clone — no lock held).
/// 2. Build the candidate from the snapshot + the dialog overrides;
///    bail out (no-op) if nothing changed.
/// 3. Register hotkeys — with the platform's atomic `register`, a
///    failure leaves the old sequence live. On failure the dialog's
///    hotkey field snaps back to the live value; nothing is saved and
///    nothing is applied.
/// 4. Persist to disk. A save failure applies for this session only.
/// 5. Swap `ctx.settings` to the new value — subsequent diff checks and
///    dialog re-opens see the fresh state.
/// 6. Side effects (history toggle, paster retune, i18n re-localization,
///    tree refresh), each gated on its own diff against the snapshot.
fn apply_options(ctx: &Arc<AppContext>, d: &OptionsDialog) {
    // 1. Snapshot current (a clone — no lock held beyond this line).
    let current = ctx.settings();

    // 2. Build the candidate from the live one + the dialog overrides.
    let mut new_settings: Settings = current.clone();

    // Language: decode index back into the code via the dialog's `languages`.
    let lang_idx = d.get_language_index() as usize;
    let langs = d.get_languages();
    if let Some(row) = langs.row_data(lang_idx) {
        new_settings.general.language = row.code.to_string();
    }

    // Hotkeys.
    new_settings.hotkeys.open_dialog = d.get_hotkey_open_dialog().to_string();
    new_settings.hotkeys.open_main_window = d.get_hotkey_open_main_window().to_string();

    // Clipboard history.
    new_settings.clipboard_history.enabled = d.get_history_enabled();
    new_settings.clipboard_history.max_items = d.get_history_max_items().max(1) as u32;
    new_settings.clipboard_history.position = if d.get_history_position_index() == 0 {
        fastpaste_data::HistoryPosition::Top
    } else {
        fastpaste_data::HistoryPosition::Bottom
    };

    // Paste options.
    new_settings.paste.delay_ms = d.get_paste_delay_ms().max(0) as u64;
    new_settings.paste.restore_clipboard = d.get_paste_restore_clipboard();

    // Nothing changed — no side effects, no disk write.
    if new_settings == current {
        return;
    }

    // 3. Hotkeys first. Reject the two hotkeys being the same sequence —
    //    both grabs would succeed but only the first would ever fire,
    //    and later re-registration would silently kill the other's grab.
    //    Then both must register; on failure the hotkey is unchanged
    //    (atomic platform register), the field snaps back, and we stop —
    //    nothing saved, nothing applied.
    if new_settings.hotkeys != current.hotkeys {
        if new_settings.hotkeys.open_dialog == new_settings.hotkeys.open_main_window {
            tracing::error!(
                "both hotkeys set to {:?}; the two actions need distinct sequences",
                new_settings.hotkeys.open_dialog,
            );
            d.set_hotkey_open_dialog(current.hotkeys.open_dialog.as_str().into());
            d.set_hotkey_open_main_window(current.hotkeys.open_main_window.as_str().into());
            return;
        }
        if let Err(e) = ctx
            .hotkey
            .register(OPEN_DIALOG_ID, &new_settings.hotkeys.open_dialog)
        {
            tracing::error!(
                "re-register open-dialog hotkey {:?}: {e}",
                new_settings.hotkeys.open_dialog,
            );
            d.set_hotkey_open_dialog(current.hotkeys.open_dialog.as_str().into());
            return;
        }
        if let Err(e) = ctx
            .hotkey
            .register(OPEN_MAIN_WINDOW_ID, &new_settings.hotkeys.open_main_window)
        {
            tracing::error!(
                "re-register main-window hotkey {:?}: {e}",
                new_settings.hotkeys.open_main_window,
            );
            // Roll the first back so the live grabs match `current` again.
            if let Err(e2) = ctx
                .hotkey
                .register(OPEN_DIALOG_ID, &current.hotkeys.open_dialog)
            {
                tracing::error!(
                    "rollback of open-dialog hotkey failed ({e2}); \
                     session hotkeys inconsistent"
                );
            }
            d.set_hotkey_open_main_window(current.hotkeys.open_main_window.as_str().into());
            return;
        }
    }

    // 4. Persist. Failure here does NOT undo the (already-live) hotkeys;
    //    apply to memory for this session and log — the user's keypresses
    //    are authoritative feedback that the new value works.
    if let Err(e) = new_settings.save() {
        tracing::error!("failed to save settings: {e} (applied for this session only)");
    }

    // 5. Swap the active settings — the next Options open seeds from
    //    `new_settings`, and every diff below already ran against
    //    `current`, so toggling a value A→B→A applies both times.
    let language_changed = new_settings.general.language != current.general.language;
    let history_changed = new_settings.clipboard_history != current.clipboard_history;
    let paste_changed = new_settings.paste != current.paste;
    let paste_delay_ms = new_settings.paste.delay_ms;
    let paste_restore = new_settings.paste.restore_clipboard;
    ctx.set_settings(new_settings);

    // 6. Side effects, each gated on its own diff.
    if history_changed {
        ctx.clipboard_history
            .set_enabled(ctx.settings().clipboard_history.enabled);
        // max_items resize stays restart-only (ring capacity is fixed at
        // construction).
    }
    if paste_changed {
        // Paste tunables are live — the Paster reads them per paste.
        ctx.paster.set_config(paste_delay_ms, paste_restore);
    }
    if language_changed {
        let new_locale = ctx.settings().general.language.clone();
        *I18N_LOCALE.lock().unwrap() = new_locale.clone();
        let i18n = I18n::new(&new_locale);
        tracing::info!(
            "i18n: language changed to {} (resolved {}); UI re-localized",
            new_locale,
            i18n.locale()
        );
        // Re-push every string into the Translations global (the dialog's
        // own handle reaches it): open windows, dialogs, and the tray
        // re-render through their bindings — no restart needed.
        apply_translations(d, &i18n);
    }
    // The history folder's label lives in the tree model (i18n) and the
    // folder's position is model data too — rebuild via the worker.
    if language_changed || history_changed {
        request_tree_refresh();
    }

    tracing::info!("settings applied + persisted");
}

/// Map a `Settings.general.language` code to its index in the language
/// combobox. Defaults to 0 ("system") for unknown codes.
fn lang_index_for_code(code: &str) -> usize {
    LANGUAGES.iter().position(|&(c, _)| c == code).unwrap_or(0)
}

// ---------------------------------------------------------------------------
// i18n accessor
// ---------------------------------------------------------------------------

/// Build a fresh translator from the active locale. Cheap (the bundles
/// live in a `static` inside `i18n.rs`; this only resolves the langid);
/// see the `I18N_LOCALE` comment.
///
/// Falls back to English if the locale slot is empty (only possible before
/// `main()` runs, e.g. in a panic path).
fn i18n() -> I18n {
    let locale = I18N_LOCALE.lock().unwrap().clone();
    if locale.is_empty() {
        I18n::new("en")
    } else {
        I18n::new(&locale)
    }
}

/// Push every user-visible string from Fluent into the `Translations`
/// global that all .slint surfaces bind to (window titles, toolbar,
/// editor labels, OptionsDialog, SelectionDialog title, tray menu).
/// The global is shared by every component of this compilation, so one
/// push — via any live component handle — (re-)localizes all of them
/// reactively. Called after the tray is built at startup, when any
/// window/dialog is created, and from `apply_options` on language change.
///
/// The `slint::Global` trait is implemented for every component of this
/// compilation (including the tray, which is not a `ComponentHandle`),
/// which is why the handle is generic.
fn apply_translations<C>(handle: &C, i18n: &I18n)
where
    for<'a> Translations<'a>: slint::Global<'a, C>,
{
    let t: Translations = slint::Global::get(handle);
    let m = |key: &str| i18n.msg(key);

    t.set_app_title(m("app-title").into());
    t.set_main_window_title(m("main-window-title").into());
    // Toolbar buttons keep their emoji icons; only the text localizes.
    // The ↑/↓ arrows are universal symbols and keep their defaults.
    t.set_toolbar_add_folder(format!("📁 {}", m("toolbar-add-folder")).into());
    t.set_toolbar_add_snippet(format!("📄 {}", m("toolbar-add-snippet")).into());
    t.set_toolbar_delete(format!("🗑 {}", m("toolbar-delete")).into());
    t.set_editor_title_label(m("editor-title-label").into());
    t.set_editor_body_label(m("editor-body-label").into());

    t.set_selection_dialog_title(m("selection-dialog-title").into());

    t.set_tray_open_main_window(m("tray-open-main-window").into());
    t.set_tray_selection_dialog(m("tray-selection-dialog").into());
    t.set_tray_options(m("tray-options").into());
    t.set_tray_quit(m("tray-quit").into());

    t.set_options_title(m("options-title").into());
    t.set_options_general(m("options-general").into());
    t.set_options_hotkeys(m("options-hotkeys").into());
    t.set_options_clipboard_history(m("options-clipboard-history").into());
    t.set_options_paste(m("options-paste").into());
    t.set_options_language_label(m("options-language-label").into());
    t.set_options_language_hint(m("options-language-hint").into());
    t.set_options_open_dialog_label(m("options-open-dialog-label").into());
    t.set_options_open_main_window_label(m("options-open-main-window-label").into());
    t.set_options_hotkeys_hint(m("options-hotkeys-hint").into());
    t.set_options_capture_history(m("options-capture-history").into());
    t.set_options_max_items_label(m("options-max-items-label").into());
    t.set_options_folder_position_label(m("options-folder-position-label").into());
    t.set_options_position_top(m("options-position-top").into());
    t.set_options_position_bottom(m("options-position-bottom").into());
    t.set_options_paste_delay_label(m("options-paste-delay-label").into());
    t.set_options_restore_clipboard(m("options-restore-clipboard").into());
    t.set_options_ok(m("options-ok").into());
    t.set_options_cancel(m("options-cancel").into());
    t.set_options_apply(m("options-apply").into());

    t.set_clipboard_history_folder(m("clipboard-history-folder").into());
}
