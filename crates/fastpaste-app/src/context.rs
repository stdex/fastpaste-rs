//! Composition root: bundles all platform services + Database into a single
//! struct that both Main Window and SelectionDialog receive.

use std::sync::{Arc, Mutex, RwLock};

use fastpaste_data::Database;
use fastpaste_platform::{
    ArboardClipboard, Clipboard, EvdevUinputCtrlV, GlobalHotkey, NullUinputCtrlV, UinputCtrlV,
    X11GlobalHotkey,
};
use single_instance::SingleInstance;

use crate::clipboard_history::ClipboardHistory;
use crate::paster::Paster;
use crate::settings::Settings;

/// All long-lived services, bundled for passing to UI controllers.
///
/// `db` sits behind a `Mutex` (not a bare `Arc<Database>`) because
/// `rusqlite::Connection` is `!Sync` — it uses `RefCell` internally for its
/// statement cache. Wrapping in `Mutex` makes `AppContext: Send + Sync`,
/// which is required so the `Arc<AppContext>` shared between the
/// `hotkey-events` thread and the Slint event loop can cross thread
/// boundaries (that thread marshals window creation into the event loop
/// via `slint::invoke_from_event_loop`, whose closure must be `Send`).
/// The lock is uncontended in practice: DB reads run on the tree-refresh
/// worker, writes on the event-loop thread, and Slint callbacks are
/// non-reentrant.
pub struct AppContext {
    pub db: Arc<Mutex<Database>>,
    pub paster: Arc<Paster>,
    pub clipboard: Arc<dyn Clipboard>,
    pub uinput: Arc<dyn UinputCtrlV>,
    pub hotkey: Arc<dyn GlobalHotkey>,
    /// Active application settings, behind an `RwLock` so the Options
    /// dialog's Apply can swap the whole struct at runtime. Accessors
    /// hand out clones — cheap (a handful of small strings) — so no caller
    /// can hold a guard across a mutation sequence (the exact staleness
    /// bug this replaces).
    settings: RwLock<Settings>,
    /// Bounded clipboard history ring. Behind `Arc` so it
    /// can be cloned into the change-drainer task and any UI panel
    /// independently of the `AppContext` itself.
    pub clipboard_history: Arc<ClipboardHistory>,
    /// Kernel-held single-instance guard. On Linux this is
    /// an abstract unix socket bound to a per-user name: the kernel holds
    /// it for the process lifetime and auto-releases it when the process
    /// exits — no lock file on disk, no stale-lock cleanup, and dropping
    /// this field would release the exclusive bind. `None` is never
    /// produced today, but the `Option` keeps the field forward-compatible
    /// with a future "non-fatal" mode (e.g. CI runs where sockets are
    /// unavailable).
    #[allow(dead_code)]
    pub single_instance: Option<SingleInstance>,
}

impl AppContext {
    /// Snapshot of the active settings.
    pub fn settings(&self) -> Settings {
        self.settings
            .read()
            .expect("settings RwLock poisoned")
            .clone()
    }

    /// Replace the active settings wholesale (Options-dialog Apply, and
    /// startup hotkey fallback).
    pub fn set_settings(&self, s: Settings) {
        *self.settings.write().expect("settings RwLock poisoned") = s;
    }
}

impl AppContext {
    /// Construct all services. DB is opened (or created) at
    /// `~/.local/share/fastpaste/fastpaste.sqlite`.
    /// /dev/uinput unavailability degrades gracefully to NullUinputCtrlV.
    ///
    /// Additionally:
    /// - Loads `Settings` and wires `paste.delay_ms` /
    ///   `paste.restore_clipboard` into `Paster` (was hardcoded 70/true).
    /// - Builds `ClipboardHistory` from `clipboard_history.max_items` /
    ///   `.enabled`.
    /// - Binds a single-instance guard keyed on the data dir; if
    ///   another instance already holds it, returns an error so `main()`
    ///   can exit cleanly.
    pub fn build() -> anyhow::Result<Self> {
        // Resolve DB path.
        let proj_dirs = crate::paths::project_dirs()?;
        let data_dir = proj_dirs.data_dir();
        std::fs::create_dir_all(data_dir)?;
        let db_path = data_dir.join("fastpaste.sqlite");
        tracing::info!("database path: {}", db_path.display());

        let db = Arc::new(Mutex::new(Database::open(&db_path, false)?));

        let clipboard = Arc::new(ArboardClipboard::new()?);
        let uinput: Arc<dyn UinputCtrlV> = match EvdevUinputCtrlV::new() {
            Ok(u) => Arc::new(u),
            Err(e) => {
                tracing::warn!(
                    "/dev/uinput unavailable ({e}); \
                     paste will leave payload on clipboard"
                );
                Arc::new(NullUinputCtrlV)
            }
        };

        // ---- Settings ------------------------------------------------------
        // Loaded once at startup; `main()` may later wire a watcher to
        // re-read on SIGHUP / tray-menu "reload", but for now it is read once.
        let settings = Settings::load()?;
        tracing::info!(
            "loaded settings: paste.delay_ms={}, paste.restore_clipboard={}, \
             clipboard_history.enabled={}, clipboard_history.max_items={}",
            settings.paste.delay_ms,
            settings.paste.restore_clipboard,
            settings.clipboard_history.enabled,
            settings.clipboard_history.max_items,
        );

        // ---- Paster wired from Settings -----------------------------------
        let paster = Arc::new(Paster::new(
            clipboard.clone(),
            uinput.clone(),
            settings.paste.delay_ms,
            settings.paste.restore_clipboard,
        ));

        // ---- Clipboard history ---------------------------------------------
        let clipboard_history = Arc::new(ClipboardHistory::new(
            settings.clipboard_history.max_items as usize,
            settings.clipboard_history.enabled,
        ));

        let hotkey: Arc<dyn GlobalHotkey> = Arc::new(X11GlobalHotkey::new()?);

        // ---- Single-instance guard ----------------------------------------
        // `single-instance` binds a Linux abstract unix socket (no file on
        // disk; the kernel auto-releases the name when the process exits).
        // The abstract namespace is shared across users of the same network
        // namespace, so the key derives from the per-user data-dir path —
        // each OS user gets their own slot, matching the previous
        // lock-file-in-data-dir behavior.
        let instance_key = format!("fastpaste-instance:{}", data_dir.display());
        let single_instance = SingleInstance::new(&instance_key).map_err(|e| {
            anyhow::anyhow!("failed to acquire single-instance key {instance_key}: {e}")
        })?;
        if !single_instance.is_single() {
            anyhow::bail!(
                "another fastpaste instance is running \
                 (single-instance key {instance_key})",
            );
        }
        tracing::info!("acquired single-instance key {instance_key}");

        Ok(Self {
            db,
            paster,
            clipboard,
            uinput,
            hotkey,
            settings: RwLock::new(settings),
            clipboard_history,
            single_instance: Some(single_instance),
        })
    }
}
