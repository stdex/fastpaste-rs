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
//   Ctrl+Alt+V        (default) → OPEN_DIALOG_ID      → SelectionDialog
//   Ctrl+Alt+M        (default) → OPEN_MAIN_WINDOW_ID → MainWindow
//
// Tray icon (Slint native SystemTrayIcon, not the external tray-icon
// crate): left-click toggles Main Window, right-click shows a context
// menu (Open / Selection Dialog / Options / Quit). A visible tray icon
// keeps `run_event_loop` alive with no window open — which is why the
// no-tray path has to use `run_event_loop_until_quit` instead, and why
// closing the main window quits when there is no tray.
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

mod ui_state;

/// `Weak` handle to the Main Window, reachable from any thread.
///
/// The *strong* handle lives in [`ui_state`] on the event-loop thread and
/// is what keeps the window alive across `hide()`; this slot exists
/// because `Weak` is `Send + Sync` and the tree-refresh worker needs to
/// address the window from off-thread. Upgrading it yields `None` only
/// once the surface has genuinely been released.
static MAIN_WINDOW: Mutex<Option<slint::Weak<MainWindow>>> = Mutex::new(None);

/// `Weak` handle to the Options Dialog. Same arrangement as MAIN_WINDOW:
/// at most one at a time, strong handle in [`ui_state`].
static OPTIONS_DIALOG: Mutex<Option<slint::Weak<OptionsDialog>>> = Mutex::new(None);

/// `Weak` handle to the Selection Dialog — same arrangement, so repeated
/// hotkey presses repopulate one dialog instead of constructing a new one
/// on the latency path of a quick-paste popup.
static SELECTION_DIALOG: Mutex<Option<slint::Weak<SelectionDialog>>> = Mutex::new(None);

/// Ping channel into the tree-refresh worker (see `start_tree_refresh_worker`).
static TREE_REFRESH_TX: Mutex<Option<std::sync::mpsc::Sender<()>>> = Mutex::new(None);

/// Folder ids (DB rowids plus `HISTORY_FOLDER_ID`) the user has collapsed
/// in the Main Window tree. Shared between the UI thread (expand/collapse
/// callbacks mutate it) and the tree-refresh worker (reads it when
/// flattening), hence the Mutex. Absent means "expanded"; ids that no
/// longer exist are pruned on each refresh, because SQLite reuses rowids
/// and a new folder must not inherit a deleted one's collapsed state.
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

/// How long the editor waits after the last keystroke before committing
/// to the database.
///
/// Slint's `edited` callback fires per character, and each commit is a
/// full row read-modify-write inside a SQLite transaction, on the
/// event-loop thread, under the shared DB mutex. Typing a 500-character
/// snippet was 500 durable rewrites blocking the UI.
const EDITOR_COMMIT_DELAY: std::time::Duration = std::time::Duration::from_millis(300);

/// An editor change that has not reached the database yet.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingEdit {
    id: i64,
    title: Option<String>,
    body: Option<String>,
}

thread_local! {
    /// The uncommitted editor change, if any, and the timer that will
    /// commit it. Both live on the event-loop thread, where every editor
    /// callback runs.
    static PENDING_EDIT: std::cell::RefCell<Option<PendingEdit>> =
        const { std::cell::RefCell::new(None) };
    static COMMIT_TIMER: std::cell::RefCell<Option<slint::Timer>> =
        const { std::cell::RefCell::new(None) };
}

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
    *I18N_LOCALE.lock().unwrap_or_else(|e| e.into_inner()) =
        ctx.settings().general.language.clone();
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
    match build_tray_icon(&ctx) {
        Ok(t) => {
            // Fill the Translations global now that a component handle
            // exists; the tray's menu labels bind to it reactively.
            apply_translations(&t, &i18n());
            ui_state::keep_tray(t);
        }
        Err(e) => {
            // Non-fatal: if the platform can't show a tray (headless CI,
            // missing status-notifier daemon on Linux), keep running so the
            // hotkeys + Main Window still work. The user just loses the
            // tray affordance.
            tracing::warn!("failed to build tray icon; continuing without it: {e}");
        }
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
        if ui_state::has_tray() { "on" } else { "off" },
        ctx.settings().hotkeys.open_dialog,
        ctx.settings().hotkeys.open_main_window,
        if ctx.settings().clipboard_history.enabled {
            "on"
        } else {
            "off"
        },
        ctx.settings().clipboard_history.max_items,
    );

    // Run the Slint event loop.
    //
    // `run_event_loop` returns once the last window is closed AND the last
    // visible tray icon is hidden. With a tray that is what we want: it
    // stays alive with no windows open, until the tray's Quit item calls
    // `quit_event_loop`.
    //
    // Without a tray there are zero windows and zero tray icons at this
    // point, so that exit condition is already satisfied — the app would
    // fall straight out of the loop and take the global hotkeys with it.
    // `run_event_loop_until_quit` runs until `quit_event_loop` is called
    // explicitly, which is the only sensible reading of "no tray": the
    // hotkeys keep working, and closing the main window quits (see the
    // close handler in `build_main_window`).
    let result = if ui_state::has_tray() {
        slint::run_event_loop()
    } else {
        tracing::info!("no tray icon; the app quits when the main window is closed");
        slint::run_event_loop_until_quit()
    };

    // Belt and braces: whatever route the loop exited by, an editor
    // change that is still only in memory gets written.
    flush_pending_edit(&ctx);

    // Drop the tray before returning so its icon disappears promptly
    // rather than at process teardown.
    ui_state::release_all();
    result.map_err(|e| anyhow::anyhow!("Slint event loop exited with error: {e}"))?;

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

    // "Quit" → stop the event loop. Commit any in-flight editor change
    // first: the debounce means a keystroke from the last 300 ms is still
    // only in memory, and the loop exiting would drop it.
    let ctx_quit = ctx.clone();
    tray.on_quit(move || {
        tracing::info!("tray Quit selected; exiting event loop");
        flush_pending_edit(&ctx_quit);
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
    *TREE_REFRESH_TX.lock().unwrap_or_else(|e| e.into_inner()) = Some(tx);
    std::thread::Builder::new()
        .name("tree-refresh".into())
        .spawn(move || {
            loop {
                if rx.recv().is_err() {
                    return; // requesters gone — process shutdown
                }
                while rx.try_recv().is_ok() {} // coalesce bursts
                let rows = build_refresh_rows(&ctx);
                let weak = MAIN_WINDOW
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone();
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
    if let Some(tx) = TREE_REFRESH_TX
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
    {
        let _ = tx.send(());
    }
}

/// Load DB items + clipboard history and flatten into tree rows. Runs on
/// the tree-refresh worker thread (never the UI thread).
fn build_refresh_rows(ctx: &AppContext) -> Vec<TreeItem> {
    // Lenient: one undecodable row must not render as "every snippet is
    // gone". The strict `load_all` aborts the whole load on the first bad
    // row, and this call site can only report it to a log the user never
    // sees.
    let items = with_db(ctx, |db| {
        db.load_all_lenient().unwrap_or_else(|e| {
            tracing::error!("load_all during refresh: {e}");
            (Vec::new(), 0)
        })
    })
    .map(|(items, skipped)| {
        if skipped > 0 {
            tracing::error!(
                "{skipped} item row(s) could not be decoded and are not shown; \
                 the database may be damaged"
            );
        }
        items
    })
    .unwrap_or_default();
    // The folder label is model data (not a binding), so localize it here;
    // a language change triggers a refresh via `apply_options`.
    let history_folder_label = i18n().msg("clipboard-history-folder");
    // Snapshot the collapsed set, dropping ids that no longer exist.
    // SQLite reuses a rowid once the highest row is deleted, so a
    // lingering id is not merely untidy: a newly created folder can
    // inherit it and appear collapsed for no reason.
    let collapsed = {
        let live: std::collections::HashSet<i64> = items.iter().filter_map(|i| i.id).collect();
        let mut set = COLLAPSED_FOLDERS.lock().unwrap_or_else(|e| e.into_inner());
        set.retain(|id| *id == fastpaste_data::HISTORY_FOLDER_ID || live.contains(id));
        set.clone()
    };
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
    match index_of_id(&win.get_tree_model(), prev_id) {
        // Setting the index from Rust does not invoke the current-changed
        // callback, but the editor already mirrors this row (it is the
        // same item as before the swap).
        Some(i) => win.set_current_index(i as i32),
        None => clear_editor(win), // id vanished — deleted while selected
    }
}

/// Row index of `id` in a tree model, or `None` if it is not present.
///
/// A free function over the model so it can be tested: this relocation is
/// what keeps the selection pointing at the same *item* across a rebuild,
/// and getting it wrong retargets a later edit or delete at a different
/// row than the one the user is looking at.
fn index_of_id(model: &slint::ModelRc<TreeItem>, id: i32) -> Option<usize> {
    (0..model.row_count()).find(|&i| model.row_data(i).is_some_and(|r| r.internal_id == id))
}

fn clear_editor(win: &MainWindow) {
    win.set_current_index(-1);
    clear_editor_fields(win);
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
    // Recover from poisoning rather than propagating: these critical
    // sections are a clone or an assignment and cannot leave inconsistent
    // state, whereas a panic here unwinds through a Slint callback and
    // takes the event loop — and the process — down with it.
    slot.lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .and_then(|w| w.upgrade())
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
    *MAIN_WINDOW.lock().unwrap_or_else(|e| e.into_inner()) = Some(win.as_weak());
    if let Err(e) = win.show() {
        tracing::error!("failed to show MainWindow: {e}");
    }
    // Hand the strong handle to the keep-alive slot. `show()` holds the
    // only other strong reference and `hide()` drops it, so without this
    // the window would be freed the moment it is hidden — and every
    // reopen would rebuild it, losing its geometry, tree model and
    // editor state.
    ui_state::keep_main_window(win);
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
        // These mirror the `Theme` tokens in widgets.slint (Slint 1.17
        // offers no way to read a global from another compilation unit,
        // so they are repeated here rather than shared).
        //
        // The selection was a saturated accent fill with white text,
        // which made the tree the loudest thing on screen and disagreed
        // with the selection styling everywhere else in the app.
        // These mirror the `Theme` tokens in widgets.slint (Slint 1.17
        // offers no way to read a global from another compilation unit,
        // so they are repeated here rather than shared).
        //
        // CAVEAT, measured rather than assumed: not all of these take
        // effect. `slint-tree-view` is compiled as a separate Slint
        // library module, and only the properties it reads for *text*
        // are honoured from here — `background-color` and
        // `highlight-color`, both read in a `background:` binding, are
        // silently ignored and the widget keeps the fluent style's
        // white-ish backdrop and saturated-blue selection.
        //
        // So `highlighted-text-color` must stay WHITE: it is applied,
        // while the light selection background that would justify dark
        // text is not. Setting the pair to the app's own selection
        // tokens produced dark blue on saturated blue — unreadable.
        // Verified with examples/ui_preview: setting each colour to a
        // loud value and sampling the rendered pixels.
        style.set_background_color(rgb(0xff, 0xff, 0xff)); // ignored, see above
        style.set_text_color(rgb(0x21, 0x25, 0x29)); // Theme.text — applied
        style.set_highlight_color(rgb(0xdb, 0xea, 0xfe)); // ignored, see above
        style.set_highlighted_text_color(rgb(0xff, 0xff, 0xff)); // must pair with the fluent blue
        style.set_hover_color(rgb(0xe9, 0xec, 0xef)); // Theme.hover-bg
        style.set_branch_indicator_color(rgb(0x6a, 0x73, 0x7d)); // Theme.text-muted
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
            // `remove_subtree` takes every descendant with it and there is
            // no undo, so ask first. A childless leaf is cheap to recreate
            // and confirming it every time would train the user to click
            // through the dialog they need to read.
            let Some((title, is_folder)) = with_db(&ctx_clone, |db| db.get(id))
                .and_then(|r| r.ok())
                .flatten()
                .map(|item| {
                    let is_folder = item.is_folder();
                    (item.title, is_folder)
                })
            else {
                return;
            };
            // Folders take their whole subtree with them, so they always
            // ask. A childless leaf is cheap to recreate, and confirming
            // every one of those would train the user to click through
            // the dialog they need to read — so leaves go straight
            // through.
            if !is_folder {
                db_write_then_refresh(&ctx_clone, "delete subtree", |db| db.remove_subtree(id));
                return;
            }
            let t = i18n();
            w.set_confirm_delete_message(
                format!("{}\n\n{}", t.msg("confirm-delete-folder"), title).into(),
            );
            w.set_confirm_delete_visible(true);
        });
    }

    // ---- Delete confirmed --------------------------------------------------
    {
        let ctx_clone = ctx.clone();
        let weak = win.as_weak();
        win.on_delete_confirmed(move || {
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
    {
        let ctx_clone = ctx.clone();
        let weak = win.as_weak();
        win.on_title_edited(move |new_title: slint::SharedString| {
            let Some(w) = weak.upgrade() else { return };
            if history_index_from_item_id(selected_row_id_i32(&w)).is_some() {
                return; // history rows are a read-only preview
            }
            let Some(id) = selected_row_id(&w) else {
                return;
            };
            queue_edit(&ctx_clone, id, Some(new_title.to_string()), None);
        });
    }

    // ---- Body edited -------------------------------------------------------
    {
        let ctx_clone = ctx.clone();
        let weak = win.as_weak();
        win.on_body_edited(move |new_body: slint::SharedString| {
            let Some(w) = weak.upgrade() else { return };
            // History rows can be previewed in the editor; persisting an
            // edit to a history entry isn't meaningful (it is a ring
            // buffer, not a CRUD table).
            if history_index_from_item_id(selected_row_id_i32(&w)).is_some() {
                return;
            }
            let Some(id) = selected_row_id(&w) else {
                return;
            };
            queue_edit(&ctx_clone, id, None, Some(new_body.to_string()));
        });
    }

    // ---- Row selected (TreeView click / keyboard) -------------------------
    // The TreeView emits `current-changed(id)` after updating its own
    // `current-index`; the controller mirrors the row into the editor
    // pane (see `mirror_selection_to_editor`).
    {
        let ctx_clone = ctx.clone();
        let weak = win.as_weak();
        win.on_current_changed(move |_id: i32| {
            let Some(w) = weak.upgrade() else { return };
            // Commit whatever the user typed into the row they are
            // leaving, before the editor is re-seeded from the new one.
            flush_pending_edit(&ctx_clone);
            mirror_selection_to_editor(&w, &ctx_clone);
        });
    }

    // ---- Item activated (double-click / Enter) ---------------------------
    // Paste the row and get out of the way — the same action the
    // selection dialog performs, so double-click and Enter do the
    // obvious thing instead of nothing.
    {
        let ctx_clone = ctx.clone();
        let weak = win.as_weak();
        win.on_item_activated(move |_id: i32| {
            let Some(w) = weak.upgrade() else { return };
            let Some(text) = activated_row_text(&w, &ctx_clone) else {
                return;
            };
            let _ = w.hide();
            spawn_paste(&ctx_clone, text);
        });
    }

    // ---- Window closed -----------------------------------------------------
    {
        let ctx_clone = ctx.clone();
        win.window().on_close_requested(move || {
            // An in-flight edit must not be lost to the close.
            flush_pending_edit(&ctx_clone);
            if !ui_state::has_tray() {
                // Nothing else can bring the app back or quit it, so
                // closing the window is the quit gesture.
                tracing::info!("main window closed and no tray is present; quitting");
                let _ = slint::quit_event_loop();
            }
            slint::CloseRequestResponse::HideWindow
        });
    }

    // ---- Expand / collapse (▶/▼ click, Left/Right keys, double-click) ----
    // The collapsed-set is the single source of truth; each mutation pings
    // the refresh worker, whose rebuilt flat model omits the hidden
    // descendants (that is all "collapse" means for a flat-list view).
    {
        win.on_item_expand_requested(move |id: i32| {
            COLLAPSED_FOLDERS
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&(id as i64));
            request_tree_refresh();
        });
        win.on_item_collapse_requested(move |id: i32| {
            COLLAPSED_FOLDERS
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(id as i64);
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
            let mut set = COLLAPSED_FOLDERS.lock().unwrap_or_else(|e| e.into_inner());
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
        let ctx_clone = ctx.clone();
        let weak = win.as_weak();
        win.on_current_parent_change_requested(move |_id: i32, parent_id: i32| {
            let Some(w) = weak.upgrade() else { return };
            let Some(i) = index_of_id(&w.get_tree_model(), parent_id) else {
                return;
            };
            flush_pending_edit(&ctx_clone);
            w.set_current_index(i as i32);
            mirror_selection_to_editor(&w, &ctx_clone);
        });
    }

    Some(win)
}

/// Record an editor change and (re)arm the commit timer.
///
/// Coalesces per-keystroke `edited` callbacks into one write. If the
/// pending change belongs to a different row than `id` — the user typed,
/// then clicked another item fast enough to beat the timer — the older
/// one is committed first rather than dropped.
fn queue_edit(ctx: &Arc<AppContext>, id: i64, title: Option<String>, body: Option<String>) {
    let stale = PENDING_EDIT.with(|p| {
        let mut p = p.borrow_mut();
        if let Some(pending) = p.as_mut()
            && pending.id == id
        {
            if title.is_some() {
                pending.title = title;
            }
            if body.is_some() {
                pending.body = body;
            }
            return None;
        }
        // A different row (or nothing pending): hand any old one back so
        // it is committed rather than dropped.
        p.replace(PendingEdit { id, title, body })
    });
    if let Some(stale) = stale {
        commit_edit(ctx, stale);
    }

    let ctx = ctx.clone();
    COMMIT_TIMER.with(|slot| {
        let timer = slint::Timer::default();
        // SingleShot, restarted on every keystroke: the write lands once,
        // EDITOR_COMMIT_DELAY after the user stops typing.
        timer.start(
            slint::TimerMode::SingleShot,
            EDITOR_COMMIT_DELAY,
            move || {
                flush_pending_edit(&ctx);
            },
        );
        *slot.borrow_mut() = Some(timer);
    });
}

/// Commit any pending editor change immediately.
///
/// Called by the debounce timer, when the selection changes, and when the
/// window closes — anywhere the change would otherwise be silently lost.
fn flush_pending_edit(ctx: &Arc<AppContext>) {
    COMMIT_TIMER.with(|slot| {
        if let Some(timer) = slot.borrow().as_ref() {
            timer.stop();
        }
    });
    let pending = PENDING_EDIT.with(|p| p.borrow_mut().take());
    if let Some(pending) = pending {
        commit_edit(ctx, pending);
    }
}

/// Write one pending change through to the database and refresh the tree.
///
/// The refresh matters: the editor is re-seeded from the database (see
/// `mirror_selection_to_editor`), but the tree *label* comes from the
/// model, so without this a rename never appeared in the tree.
fn commit_edit(ctx: &Arc<AppContext>, pending: PendingEdit) {
    let id = pending.id;
    let result = with_db(ctx, |db| {
        let Some(mut item) = db.get(id)? else {
            // The row was deleted while it was being edited. Dropping the
            // edit is the correct outcome, and not an error.
            tracing::debug!("editor commit: item {id} no longer exists; discarding edit");
            return Ok::<bool, fastpaste_data::DataError>(false);
        };
        if let Some(title) = pending.title {
            item.title = title;
        }
        if let Some(body) = pending.body {
            item.body_plain = body;
        }
        db.update(&item)?;
        Ok(true)
    });
    match result {
        Some(Ok(true)) => request_tree_refresh(),
        Some(Ok(false)) => {}
        Some(Err(e)) => tracing::error!("editor commit (id={id}): {e}"),
        None => {}
    }
}

/// Mirror the current tree row into the editor pane. Shared by the
/// `current-changed` callback (TreeView mouse/keyboard navigation) and
/// the `current-parent-change-requested` handler (Left-arrow jump to
/// parent), which sets the index programmatically.
///
/// Real rows are seeded from the DATABASE, not from the tree model. The
/// model lags a rename until the refresh worker has rebuilt it, and
/// seeding from it meant selecting away and back restored the *old*
/// title into the editor — where the next keystroke wrote it straight
/// back over the new one.
fn mirror_selection_to_editor(win: &MainWindow, ctx: &AppContext) {
    let idx = win.get_current_index();
    if idx < 0 {
        clear_editor_fields(win);
        return;
    }
    let Some(row) = win.get_tree_model().row_data(idx as usize) else {
        clear_editor_fields(win);
        return;
    };

    // History rows are not in the database; their text is the model's.
    // They are a read-only preview: the editor used to accept typing on
    // them and silently discard it.
    if history_index_from_item_id(row.internal_id).is_some() {
        win.set_editor_title(row.text);
        win.set_editor_body(row.user_data);
        win.set_editor_enabled(false);
        win.set_title_enabled(false);
        return;
    }

    let is_folder = row.item_type == ItemKind::Folder.as_i64() as i32;
    if row.internal_id > 0
        && let Some(Ok(Some(item))) = with_db(ctx, |db| db.get(row.internal_id as i64))
    {
        win.set_editor_title(item.title.as_str().into());
        win.set_editor_body(item.body_plain.as_str().into());
    } else {
        // The virtual history folder, or a row that vanished between the
        // model build and now — fall back to what the model carries.
        win.set_editor_title(row.text);
        win.set_editor_body(row.user_data);
    }
    // Folders are not directly editable in v1 (no body to edit), but they
    // are renameable — so the title field stays live for them.
    // Qt-style: TreeItem has no `is-folder`; we look at `item-type`
    // (which the model populates with `ItemKind::Folder.as_i64()`).
    win.set_editor_enabled(!is_folder && row.internal_id > 0);
    win.set_title_enabled(row.internal_id > 0);
}

fn clear_editor_fields(win: &MainWindow) {
    win.set_editor_title("".into());
    win.set_editor_body("".into());
    win.set_editor_enabled(false);
    win.set_title_enabled(false);
}

/// The text an activated (double-clicked / Entered) row should paste, or
/// `None` for rows that have nothing to paste (folders, empty bodies).
fn activated_row_text(win: &MainWindow, ctx: &AppContext) -> Option<String> {
    let idx = win.get_current_index();
    if idx < 0 {
        return None;
    }
    let row = win.get_tree_model().row_data(idx as usize)?;
    if history_index_from_item_id(row.internal_id).is_some() {
        // Take the text from the ROW, not by re-indexing the ring.
        // `push_history_folder` stores the full, untruncated entry in
        // `user_data`, and the editor preview already reads it from
        // there. Resolving positionally instead meant the index was
        // whatever the last tree rebuild baked in, while
        // `on_clipboard_changed` inserts at position 0 — so every capture
        // shifts the ring under it and the user could paste an entry they
        // never selected, one the preview was not even showing.
        let text = row.user_data.to_string();
        return (!text.is_empty()).then_some(text);
    }
    if row.item_type == ItemKind::Folder.as_i64() as i32 || row.internal_id <= 0 {
        return None;
    }
    with_db(ctx, |db| db.get(row.internal_id as i64))
        .and_then(|r| r.ok())
        .flatten()
        .map(|item| item.body_plain)
        .filter(|t| !t.is_empty())
}

/// Paste `text` on a worker thread.
///
/// Never on the event loop: the paste sequence sleeps for the configured
/// settle delay and then blocks on `/dev/uinput`.
fn spawn_paste(ctx: &Arc<AppContext>, text: String) {
    let paster = ctx.paster.clone();
    if let Err(e) = std::thread::Builder::new()
        .name("paste-worker".into())
        .spawn(move || {
            if let Err(e) = paster.paste_text(&text) {
                tracing::error!("paste failed: {e}");
            }
        })
    {
        tracing::error!("failed to spawn paste-worker: {e}");
    }
}

/// Folder ids of every *visible* folder strictly inside the subtree of
/// `root_id`, optionally capped to `levels` below it (-1 = unlimited).
/// Returns None when `root_id` isn't in the model. Walks the flat model:
/// a subtree is the contiguous run of rows deeper than the root's depth.
fn visible_subtree_folder_ids(win: &MainWindow, root_id: i32, levels: i32) -> Option<Vec<i64>> {
    let model = win.get_tree_model();
    let rows: Vec<TreeItem> = (0..model.row_count())
        .filter_map(|i| model.row_data(i))
        .collect();
    subtree_folder_ids(&rows, root_id, levels)
}

/// Folder ids strictly inside `root_id`'s visible subtree, capped to
/// `levels` below it (-1 = unlimited). `None` when `root_id` is not in
/// the rows at all.
///
/// A free function over the flat rows so it can be tested. Folders are
/// identified by `item_type`, not by `has_children`: an empty folder
/// truthfully reports `has_children == false`, and keying off that meant
/// recursive collapse silently skipped every empty folder in the subtree.
fn subtree_folder_ids(rows: &[TreeItem], root_id: i32, levels: i32) -> Option<Vec<i64>> {
    let folder_type = ItemKind::Folder.as_i64() as i32;
    let start = rows.iter().position(|r| r.internal_id == root_id)?;
    let root_depth = rows[start].depth;
    let mut out = Vec::new();
    for row in &rows[start + 1..] {
        // The contiguous deeper-than-root run ended → subtree closed.
        if row.depth <= root_depth {
            break;
        }
        if row.item_type == folder_type && (levels < 0 || row.depth - root_depth <= levels) {
            out.push(row.internal_id as i64);
        }
    }
    Some(out)
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
    parent_for_new_item(&row)
}

/// Where a newly added item belongs, given the currently selected row.
///
/// A selected folder receives the new item; a selected snippet puts it
/// alongside itself, i.e. in that snippet's own parent. Dropping it at
/// the root instead — as this used to — silently moved the user's new
/// item out of the folder they were working in.
///
/// Virtual rows (the history folder and its entries, all with ids ≤ 0)
/// resolve to the root: nothing can be created inside the history.
fn parent_for_new_item(row: &TreeItem) -> i64 {
    // Qt-style: TreeItem has no `is-folder`; check `item-type` instead.
    // The model populates item-type with `ItemKind::Folder.as_i64()` for
    // folders.
    let is_folder = row.item_type == ItemKind::Folder.as_i64() as i32;
    if is_folder && row.internal_id > 0 {
        return row.internal_id as i64;
    }
    if !is_folder && row.internal_id > 0 && row.parent_internal_id > 0 {
        return row.parent_internal_id as i64;
    }
    0
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
        d.set_filter_text("".into());
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

    // Filtering happens in Rust so the index `paste-selected` reports
    // always refers to the list actually on screen.
    let ctx_filter = ctx.clone();
    let dialog_weak3 = dialog.as_weak();
    dialog.on_filter_changed(move |_text: slint::SharedString| {
        if let Some(d) = dialog_weak3.upgrade() {
            repopulate_selection_dialog(&d, &ctx_filter);
        }
    });

    if let Err(e) = dialog.show() {
        tracing::error!("failed to show SelectionDialog: {e}");
    }
    *SELECTION_DIALOG.lock().unwrap_or_else(|e| e.into_inner()) = Some(dialog.as_weak());
    // Keep it alive across `hide()` so the next hotkey press repopulates
    // this dialog instead of constructing a new one on the latency path.
    ui_state::keep_selection(dialog);
}

/// One entry offered by the selection dialog.
struct PasteCandidate {
    title: String,
    /// Second line under the title. Empty for clipboard-history rows,
    /// whose title already *is* the text.
    preview: String,
    /// Short right-aligned type marker; empty for saved snippets.
    tag: String,
    text: String,
}

/// Everything the selection dialog can paste: the clipboard history
/// first (it is what the user most recently had in hand), then the plain
/// snippets. Filtered by `filter`, case-insensitively, over both the
/// title and the body.
///
/// History used to be absent entirely — a captured entry could be looked
/// at in the tree and nothing else, even though the quick-paste popup is
/// exactly where a user goes looking for it.
fn paste_candidates(ctx: &AppContext, filter: &str) -> Vec<PasteCandidate> {
    // The row's type travels as a short tag rendered right-aligned, not
    // as a prefix on the title: the folder name is identical on every
    // history row, and pasting it in front of each one pushed the part
    // the user is reading toward the right edge.
    let history_tag = i18n().msg("selection-tag-history");
    let mut out: Vec<PasteCandidate> = ctx
        .clipboard_history
        .entries()
        .into_iter()
        .filter(|e| !e.text.is_empty())
        .map(|e| PasteCandidate {
            title: tree_builder::collapse_newlines(&e.text),
            preview: String::new(),
            tag: history_tag.clone(),
            text: e.text,
        })
        .collect();

    let snippets = with_db(ctx, |db| {
        db.load_all_lenient().unwrap_or_else(|e| {
            tracing::error!("load_all failed for dialog: {e}");
            (Vec::new(), 0)
        })
    })
    .map(|(items, _)| items)
    .unwrap_or_default();
    out.extend(
        snippets
            .into_iter()
            .filter(|i: &Item| i.kind == ItemKind::Plain)
            .map(|i| PasteCandidate {
                title: i.title,
                preview: tree_builder::collapse_newlines(&i.body_plain),
                tag: String::new(),
                text: i.body_plain,
            }),
    );

    let needle = filter.trim().to_lowercase();
    if needle.is_empty() {
        return out;
    }
    out.retain(|c| {
        c.title.to_lowercase().contains(&needle) || c.text.to_lowercase().contains(&needle)
    });
    out
}

/// (Re)load the candidate list into the dialog and re-install the paste
/// callback with a fresh capture, so it pastes what is currently listed.
fn repopulate_selection_dialog(d: &SelectionDialog, ctx: &Arc<AppContext>) {
    let candidates = paste_candidates(ctx, d.get_filter_text().as_str());

    let model_rows: Vec<SnippetRow> = candidates
        .iter()
        .map(|c| SnippetRow {
            title: c.title.as_str().into(),
            body: c.preview.as_str().into(),
            tag: c.tag.as_str().into(),
        })
        .collect();
    d.set_snippets(slint::ModelRc::new(slint::VecModel::from(model_rows)));
    // Not `set_selected_index(0)`: a Rust-side set does not run the
    // dialog's `set-selection`, so the viewport would stay scrolled where
    // it was with the selection off-screen.
    d.invoke_reset_view();

    let ctx_paste = ctx.clone();
    let dialog_weak = d.as_weak();
    d.on_paste_selected(move |idx: i32| {
        let text = usize::try_from(idx)
            .ok()
            .and_then(|i| candidates.get(i))
            .map(|c| c.text.clone());

        // Hide FIRST. The paste sequence's settle delay is the entire
        // budget for the compositor to unmap this window and hand focus
        // back to the target application; starting the clock before the
        // hide was even requested meant Ctrl+V could land while the
        // dialog still had focus, and the paste silently went nowhere.
        if let Some(d) = dialog_weak.upgrade() {
            d.hide().ok();
        }
        if let Some(text) = text {
            spawn_paste(&ctx_paste, text);
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
    // If one is already alive, re-seed and show it.
    //
    // Re-seeding is not optional now that the dialog survives `hide()`.
    // Before it did, every open constructed a fresh dialog and therefore
    // always started from `ctx.settings()`. Keeping it alive without
    // re-seeding meant Cancel stopped being durable: cancel some edits,
    // reopen, press OK, and `apply_options` diffs the *stale fields*
    // against the live settings and applies the very changes the user
    // rejected.
    if let Some(d) = live(&OPTIONS_DIALOG) {
        seed_options_dialog(&d, &ctx.settings());
        d.set_status_message("".into());
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

    // Language options come from the single LANGUAGES table (codes must
    // match what `Settings::general.language` accepts). Static for the
    // life of the dialog, so unlike the values below they are set once.
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

    seed_options_dialog(&dialog, &ctx.settings());

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

    *OPTIONS_DIALOG.lock().unwrap_or_else(|e| e.into_inner()) = Some(dialog.as_weak());
    // Keep it alive across `hide()`; see `ui_state`.
    ui_state::keep_options(dialog);
}

/// Fill the dialog's fields from `s`, and push the accepted numeric
/// ranges so the spin boxes cannot cap a legal value at a bound of their
/// own. `Settings` is the single authority for those ranges — the dialog
/// used to hardcode a narrower pair, which silently rewrote a
/// hand-edited `max_items = 200` down to 50 the moment the user touched
/// the control.
fn seed_options_dialog(d: &OptionsDialog, s: &Settings) {
    use fastpaste_app::settings::{DELAY_MS_RANGE, MAX_ITEMS_RANGE};

    d.set_history_max_items_min(*MAX_ITEMS_RANGE.start() as i32);
    d.set_history_max_items_max(*MAX_ITEMS_RANGE.end() as i32);
    d.set_paste_delay_min(*DELAY_MS_RANGE.start() as i32);
    d.set_paste_delay_max(*DELAY_MS_RANGE.end() as i32);

    d.set_language_index(lang_index_for_code(&s.general.language) as i32);
    d.set_hotkey_open_dialog(s.hotkeys.open_dialog.as_str().into());
    d.set_hotkey_open_main_window(s.hotkeys.open_main_window.as_str().into());
    d.set_history_enabled(s.clipboard_history.enabled);
    d.set_history_max_items(s.clipboard_history.max_items as i32);
    // The dialog's combobox declares 0 = Top, 1 = Bottom.
    d.set_history_position_index(matches!(
        s.clipboard_history.position,
        fastpaste_data::HistoryPosition::Bottom
    ) as i32);
    d.set_paste_delay_ms(s.paste.delay_ms as i32);
    d.set_paste_restore_clipboard(s.paste.restore_clipboard);
}

/// Read the dialog's fields into a candidate `Settings`.
///
/// Pure apart from reading the dialog, and clamped through
/// `Settings::clamp` so a value cannot reach the config file outside its
/// accepted range.
fn settings_from_dialog(current: &Settings, d: &OptionsDialog) -> Settings {
    let mut s = current.clone();

    // Language: decode index back into the code via the dialog's `languages`.
    let lang_idx = d.get_language_index() as usize;
    if let Some(row) = d.get_languages().row_data(lang_idx) {
        s.general.language = row.code.to_string();
    }

    s.hotkeys.open_dialog = d.get_hotkey_open_dialog().trim().to_string();
    s.hotkeys.open_main_window = d.get_hotkey_open_main_window().trim().to_string();

    s.clipboard_history.enabled = d.get_history_enabled();
    s.clipboard_history.max_items = d.get_history_max_items().max(0) as u32;
    s.clipboard_history.position = if d.get_history_position_index() == 0 {
        fastpaste_data::HistoryPosition::Top
    } else {
        fastpaste_data::HistoryPosition::Bottom
    };

    s.paste.delay_ms = d.get_paste_delay_ms().max(0) as u64;
    s.paste.restore_clipboard = d.get_paste_restore_clipboard();

    s.clamp();
    s
}

/// Try to move the two hotkeys to `wanted`, rolling back on failure.
///
/// Returns the fluent key of a message to show the user, or `None` on
/// success. The live grabs always end up matching either `wanted` (on
/// success) or `current` (on any failure).
fn reregister_hotkeys(
    ctx: &Arc<AppContext>,
    current: &Settings,
    wanted: &Settings,
) -> Option<&'static str> {
    use fastpaste_platform::HotkeyError;

    if wanted.hotkeys.open_dialog == wanted.hotkeys.open_main_window {
        // Both grabs would succeed but only the first would ever fire.
        return Some("options-error-hotkey-duplicate");
    }

    fn key_for(e: &HotkeyError) -> &'static str {
        match e {
            HotkeyError::AlreadyGrabbed { .. } => "options-error-hotkey-taken",
            _ => "options-error-hotkey-invalid",
        }
    }

    if let Err(e) = ctx
        .hotkey
        .register(OPEN_DIALOG_ID, &wanted.hotkeys.open_dialog)
    {
        tracing::error!(
            "re-register open-dialog hotkey {:?}: {e}",
            wanted.hotkeys.open_dialog
        );
        return Some(key_for(&e));
    }
    if let Err(e) = ctx
        .hotkey
        .register(OPEN_MAIN_WINDOW_ID, &wanted.hotkeys.open_main_window)
    {
        tracing::error!(
            "re-register main-window hotkey {:?}: {e}",
            wanted.hotkeys.open_main_window
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
        return Some(key_for(&e));
    }
    None
}

/// Read the OptionsDialog's fields back into a `Settings`, apply runtime
/// side-effects, and persist. Ordering:
///
/// 1. Snapshot the current settings (a clone — no lock held).
/// 2. Build the candidate from the snapshot + the dialog overrides;
///    bail out (no-op) if nothing changed.
/// 3. Register hotkeys — with the platform's atomic `register`, a
///    failure leaves the old sequence live. A rejected hotkey reverts
///    only the hotkey fields and reports why in the dialog's status
///    line; it no longer discards the *other* settings the user changed
///    in the same Apply, which used to vanish without a word.
/// 4. Persist to disk. A save failure applies for this session only.
/// 5. Swap `ctx.settings` to the new value — subsequent diff checks and
///    dialog re-opens see the fresh state.
/// 6. Side effects (i18n re-localization, tree refresh) — the rest are
///    pushed by `AppContext::set_settings`, which owns the "every field
///    applies live" contract.
fn apply_options(ctx: &Arc<AppContext>, d: &OptionsDialog) {
    let t = i18n();
    d.set_status_message("".into());

    // 1-2. Snapshot, then build the candidate from it + the overrides.
    let current = ctx.settings();
    let mut new_settings = settings_from_dialog(&current, d);

    // Reflect any clamping back into the dialog so the user sees the
    // value that was actually accepted.
    d.set_history_max_items(new_settings.clipboard_history.max_items as i32);
    d.set_paste_delay_ms(new_settings.paste.delay_ms as i32);

    // Nothing changed — no side effects, no disk write.
    if new_settings == current {
        return;
    }

    // 3. Hotkeys.
    if new_settings.hotkeys != current.hotkeys
        && let Some(msg_key) = reregister_hotkeys(ctx, &current, &new_settings)
    {
        // Keep every other change the user made; only the hotkeys revert.
        new_settings.hotkeys = current.hotkeys.clone();
        d.set_hotkey_open_dialog(current.hotkeys.open_dialog.as_str().into());
        d.set_hotkey_open_main_window(current.hotkeys.open_main_window.as_str().into());
        d.set_status_message(t.msg(msg_key).into());
        if new_settings == current {
            return; // the hotkeys were the only change
        }
    }

    // 4. Persist. Failure here does NOT undo the (already-live) hotkeys;
    //    apply to memory for this session and say so.
    if let Err(e) = new_settings.save() {
        tracing::error!("failed to save settings: {e} (applied for this session only)");
        if d.get_status_message().is_empty() {
            d.set_status_message(t.msg("options-error-save-failed").into());
        }
    }

    // 5. Swap the active settings — the next Options open seeds from
    //    `new_settings`, and every diff below already ran against
    //    `current`, so toggling a value A→B→A applies both times.
    //    `set_settings` also pushes paste tunables and the history
    //    enabled/max_items into their services.
    let language_changed = new_settings.general.language != current.general.language;
    let history_changed = new_settings.clipboard_history != current.clipboard_history;
    ctx.set_settings(new_settings);

    // 6. Side effects that are not service state.
    if language_changed {
        let new_locale = ctx.settings().general.language.clone();
        *I18N_LOCALE.lock().unwrap_or_else(|e| e.into_inner()) = new_locale.clone();
        let i18n = I18n::new(&new_locale);
        tracing::info!(
            "i18n: language changed to {} (resolved {}); UI re-localized",
            new_locale,
            i18n.locale()
        );
        // Every live surface has its own Translations instance; push to
        // all of them or the ones missed keep the old language.
        relocalize_all_surfaces(&i18n);
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
    let locale = I18N_LOCALE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    if locale.is_empty() {
        I18n::new("en")
    } else {
        I18n::new(&locale)
    }
}

/// Re-localize every UI surface that is currently live.
///
/// Each *root* component instantiates its own `SharedGlobals`, and
/// therefore its own `Translations` instance — there are four in this
/// compilation (MainWindow, SelectionDialog, OptionsDialog,
/// FastpasteTray), not one shared global. A single push reaches only the
/// surface it was made through, which is why a language change used to
/// leave the tray menu in the startup language for the life of the
/// process, and an open main window stale until it was torn down.
fn relocalize_all_surfaces(i18n: &I18n) {
    if let Some(win) = live(&MAIN_WINDOW) {
        apply_translations(&win, i18n);
    }
    if let Some(d) = live(&SELECTION_DIALOG) {
        apply_translations(&d, i18n);
    }
    if let Some(d) = live(&OPTIONS_DIALOG) {
        apply_translations(&d, i18n);
    }
    ui_state::with_tray(|t| apply_translations(t, i18n));
}

/// Push every user-visible string from Fluent into ONE surface's
/// `Translations` global (window titles, toolbar, editor labels,
/// OptionsDialog, SelectionDialog title, tray menu).
///
/// Call [`relocalize_all_surfaces`] rather than this directly when the
/// language changes; this is for a surface being created, which has no
/// strings yet.
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
    // The arrows themselves stay as icons; these are the accessible
    // labels and tooltips for them.
    t.set_toolbar_move_up(m("toolbar-move-up").into());
    t.set_toolbar_move_down(m("toolbar-move-down").into());
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

    t.set_confirm_delete_title(m("confirm-delete-title").into());
    t.set_confirm_yes(m("confirm-yes").into());
    t.set_confirm_no(m("confirm-no").into());

    t.set_selection_filter_placeholder(m("selection-filter-placeholder").into());
    t.set_selection_empty(m("selection-empty").into());
    t.set_selection_tag_history(m("selection-tag-history").into());
    t.set_selection_hint(m("selection-hint").into());

    t.set_clipboard_history_folder(m("clipboard-history-folder").into());
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------
//
// These cover the controller helpers that were extracted out of the Slint
// callbacks. Everything here is a plain function over plain data — no
// window, no event loop — which is exactly why they were extracted: the
// bugs they pin all lived inside closures that no test could reach.

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: i32, parent: i32, depth: i32, kind: ItemKind) -> TreeItem {
        let mut r = if kind == ItemKind::Folder {
            TreeItem::branch(id, parent, depth, "row")
        } else {
            TreeItem::leaf(id, parent, depth, "row", "body")
        };
        r.item_type = kind.as_i64() as i32;
        r
    }

    fn model(rows: Vec<TreeItem>) -> slint::ModelRc<TreeItem> {
        slint::ModelRc::new(slint::VecModel::from(rows))
    }

    // ---- index_of_id ----------------------------------------------------

    #[test]
    fn index_of_id_finds_the_row() {
        let m = model(vec![
            row(7, -1, 0, ItemKind::Folder),
            row(9, 7, 1, ItemKind::Plain),
            row(4, -1, 0, ItemKind::Plain),
        ]);
        assert_eq!(index_of_id(&m, 7), Some(0));
        assert_eq!(index_of_id(&m, 9), Some(1));
        assert_eq!(index_of_id(&m, 4), Some(2));
    }

    #[test]
    fn index_of_id_reports_a_vanished_row() {
        let m = model(vec![row(7, -1, 0, ItemKind::Plain)]);
        assert_eq!(index_of_id(&m, 99), None);
    }

    #[test]
    fn index_of_id_on_an_empty_model_is_none() {
        assert_eq!(index_of_id(&model(vec![]), 1), None);
    }

    /// The relocation that keeps a selection on the same *item* across a
    /// rebuild. If it returned a stale index, a later edit or delete
    /// would act on a different row than the highlighted one.
    #[test]
    fn index_of_id_follows_an_item_that_moved() {
        let before = model(vec![
            row(1, -1, 0, ItemKind::Plain),
            row(2, -1, 0, ItemKind::Plain),
            row(3, -1, 0, ItemKind::Plain),
        ]);
        assert_eq!(index_of_id(&before, 3), Some(2));

        // A clipboard capture inserted a history row at the top and the
        // items shifted down.
        let after = model(vec![
            row(-1000, -1, 0, ItemKind::Folder),
            row(1, -1, 0, ItemKind::Plain),
            row(2, -1, 0, ItemKind::Plain),
            row(3, -1, 0, ItemKind::Plain),
        ]);
        assert_eq!(index_of_id(&after, 3), Some(3));
    }

    // ---- parent_for_new_item --------------------------------------------

    #[test]
    fn a_selected_folder_receives_the_new_item() {
        assert_eq!(parent_for_new_item(&row(5, -1, 0, ItemKind::Folder)), 5);
    }

    /// Adding next to a snippet inside a folder must land in that folder,
    /// not silently at the root — which is where it used to go.
    #[test]
    fn a_selected_snippet_puts_the_new_item_beside_it() {
        assert_eq!(parent_for_new_item(&row(9, 5, 1, ItemKind::Plain)), 5);
    }

    #[test]
    fn a_top_level_snippet_puts_the_new_item_at_the_root() {
        // parent_internal_id is NO_PARENT (-1) for a root-level row.
        assert_eq!(parent_for_new_item(&row(9, -1, 0, ItemKind::Plain)), 0);
    }

    #[test]
    fn virtual_rows_never_become_a_parent() {
        // The history folder and its entries all carry ids <= 0; nothing
        // can be created inside the history.
        let history_folder = row(
            fastpaste_data::HISTORY_FOLDER_ID as i32,
            -1,
            0,
            ItemKind::Folder,
        );
        assert_eq!(parent_for_new_item(&history_folder), 0);
        let history_entry = row(
            -2,
            fastpaste_data::HISTORY_FOLDER_ID as i32,
            1,
            ItemKind::Plain,
        );
        assert_eq!(parent_for_new_item(&history_entry), 0);
    }

    // ---- subtree_folder_ids ---------------------------------------------

    /// root(1) > child folder(2) > grandchild folder(3), plus a leaf and
    /// an unrelated sibling after the subtree.
    fn nested_rows() -> Vec<TreeItem> {
        vec![
            row(1, -1, 0, ItemKind::Folder),
            row(2, 1, 1, ItemKind::Folder),
            row(3, 2, 2, ItemKind::Folder),
            row(4, 3, 3, ItemKind::Plain),
            row(5, -1, 0, ItemKind::Folder),
        ]
    }

    #[test]
    fn subtree_collects_every_nested_folder() {
        assert_eq!(subtree_folder_ids(&nested_rows(), 1, -1), Some(vec![2, 3]));
    }

    #[test]
    fn subtree_stops_at_the_sibling_after_it() {
        // 5 is a folder at the same depth as the root — outside the run.
        let ids = subtree_folder_ids(&nested_rows(), 1, -1).unwrap();
        assert!(!ids.contains(&5));
    }

    #[test]
    fn subtree_respects_the_level_cap() {
        assert_eq!(subtree_folder_ids(&nested_rows(), 1, 1), Some(vec![2]));
        assert_eq!(subtree_folder_ids(&nested_rows(), 1, 2), Some(vec![2, 3]));
    }

    #[test]
    fn subtree_of_an_unknown_root_is_none() {
        assert_eq!(subtree_folder_ids(&nested_rows(), 42, -1), None);
    }

    #[test]
    fn subtree_of_a_leaf_is_empty_not_none() {
        assert_eq!(subtree_folder_ids(&nested_rows(), 4, -1), Some(vec![]));
    }

    /// Folders are identified by `item_type`, not `has_children`. An
    /// empty folder truthfully reports `has_children == false`, and
    /// keying off that skipped it — so recursive collapse quietly left
    /// every empty folder in the subtree expanded.
    #[test]
    fn empty_folders_are_included() {
        let mut rows = vec![
            row(1, -1, 0, ItemKind::Folder),
            row(2, 1, 1, ItemKind::Folder),
        ];
        // What `build_tree_items_with_history` emits for a childless
        // folder: it is still a folder, but reports no children.
        rows[1].has_children = false;
        assert_eq!(subtree_folder_ids(&rows, 1, -1), Some(vec![2]));
    }

    // ---- lang_index_for_code --------------------------------------------

    #[test]
    fn language_codes_map_to_their_combobox_rows() {
        for (i, (code, _)) in LANGUAGES.iter().enumerate() {
            assert_eq!(lang_index_for_code(code), i, "{code}");
        }
    }

    #[test]
    fn an_unknown_language_code_falls_back_to_system() {
        assert_eq!(lang_index_for_code("kl"), 0);
        assert_eq!(lang_index_for_code(""), 0);
        assert_eq!(LANGUAGES[0].0, "system", "index 0 must stay `system`");
    }

    // ---- translation coverage -------------------------------------------

    /// Every fluent key this file asks for must exist in the English
    /// catalogue. A missing one renders as the literal key in the UI, and
    /// the key-parity test in `i18n.rs` only compares locales against
    /// each other — it cannot see a key that no catalogue has.
    ///
    /// Scrapes both spellings: the `m("…")` closure inside
    /// `apply_translations` and the direct `…msg("…")` call sites, which
    /// the first version of this test could not see at all.
    #[test]
    fn every_translation_key_this_file_requests_exists_in_english() {
        let defined = english_keys();
        let requested = scraped_keys();

        assert!(
            requested.len() > 30,
            "the scraper should find every m(…) / msg(…) call; found {}",
            requested.len()
        );
        let missing: Vec<&String> = requested.iter().filter(|k| !defined.contains(*k)).collect();
        assert!(missing.is_empty(), "keys missing from en.ftl: {missing:?}");
    }

    /// The keys `reregister_hotkeys` returns reach `msg` through a
    /// variable, so no scraper can see them. Enumerate them by hand —
    /// this family is the one most likely to grow without a catalogue
    /// entry.
    #[test]
    fn dynamically_selected_error_keys_exist_in_english() {
        let defined = english_keys();
        for key in DYNAMIC_KEYS {
            assert!(defined.contains(*key), "{key} is missing from en.ftl");
        }
    }

    /// A key translated into five languages and requested nowhere is pure
    /// maintenance cost, so it should not accumulate silently.
    #[test]
    fn the_english_catalogue_has_no_unused_keys() {
        let requested: std::collections::HashSet<String> = scraped_keys()
            .into_iter()
            .chain(DYNAMIC_KEYS.iter().map(|k| k.to_string()))
            .collect();
        let unused: Vec<String> = english_keys()
            .into_iter()
            .filter(|k| !requested.contains(k))
            .collect();
        assert!(unused.is_empty(), "keys defined but never used: {unused:?}");
    }

    /// Keys chosen at runtime rather than written at a call site.
    const DYNAMIC_KEYS: &[&str] = &[
        "options-error-hotkey-duplicate",
        "options-error-hotkey-taken",
        "options-error-hotkey-invalid",
        "options-error-save-failed",
    ];

    fn english_keys() -> std::collections::HashSet<String> {
        include_str!("../i18n/en.ftl")
            .lines()
            .filter(|l| !l.trim_start().starts_with('#'))
            .filter_map(|l| l.split_once(" ="))
            .map(|(k, _)| k.trim().to_string())
            .collect()
    }

    /// Every literal fluent key this file passes to `m(` or `msg(`.
    fn scraped_keys() -> Vec<String> {
        let source = include_str!("../src/main.rs");
        let mut out = Vec::new();
        for opener in ["m(\"", "msg(\""] {
            for (i, _) in source.match_indices(opener) {
                // For the bare `m(` form, skip a match that is only the
                // tail of a longer identifier (`confirm(`, `from(` …).
                if opener == "m(\"" {
                    let before = source[..i].chars().next_back();
                    if before.is_some_and(|c| c.is_alphanumeric() || c == '_') {
                        continue;
                    }
                }
                // Fluent keys here are all kebab-case, which also filters
                // out any unrelated string literal that slips through.
                if let Some((key, _)) = source[i + opener.len()..].split_once('"')
                    && key.contains('-')
                {
                    out.push(key.to_string());
                }
            }
        }
        out
    }
}
