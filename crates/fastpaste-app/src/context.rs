//! Composition root: bundles all platform services + Database into a single
//! struct that both Main Window and SelectionDialog receive.

use std::sync::{Arc, Mutex, RwLock};

use fastpaste_data::Database;
use fastpaste_platform::{
    Clipboard, GlobalHotkey, NullGlobalHotkey, NullPasteKeys, PasteKeys, SystemClipboard,
    SystemHotkeys, SystemPasteKeys,
};
use single_instance::SingleInstance;

use crate::clipboard_history::ClipboardHistory;
use crate::paster::Paster;
use crate::settings::Settings;

/// Account name under which the database passphrase is filed in the
/// credential store. One database per user, so one fixed name.
pub const PASSPHRASE_ACCOUNT: &str = "database-passphrase";

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
    pub uinput: Arc<dyn PasteKeys>,
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
    /// Where the database file lives. Kept because the encrypt / decrypt /
    /// change-passphrase operations act on the path with every connection
    /// closed, so they cannot get it from `db`.
    pub db_path: std::path::PathBuf,
    /// The OS credential store, for remembering the database passphrase.
    /// Always the real [`fastpaste_platform::SystemSecretStore`] — never
    /// substituted for [`fastpaste_platform::UnavailableSecretStore`] based
    /// on a startup probe, because a negative `is_available()` can mean
    /// "merely locked right now" rather than "will never work" (Task 6),
    /// and latching that verdict into the type would disable the Options
    /// dialog's Remember checkbox for the whole session with no recovery
    /// but a restart. Callers that care whether it can be used right now
    /// (the unlock dialog, the Options dialog) call `is_available()`
    /// themselves at the moment it matters.
    pub secret_store: Arc<dyn fastpaste_platform::SecretStore>,
}

impl AppContext {
    /// Snapshot of the active settings.
    pub fn settings(&self) -> Settings {
        self.settings
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Replace the active settings wholesale (Options-dialog Apply, and
    /// startup hotkey fallback).
    ///
    /// Also pushes the fields that live inside services rather than in the
    /// settings struct. Every field the Options dialog can change must be
    /// reachable from here — the README promises settings apply live, and
    /// anything missed is a control that silently needs a restart.
    pub fn set_settings(&self, s: Settings) {
        self.paster
            .set_config(s.paste.delay_ms, s.paste.restore_clipboard);
        self.clipboard_history
            .set_enabled(s.clipboard_history.enabled);
        self.clipboard_history
            .set_max_items(s.clipboard_history.max_items as usize);
        *self.settings.write().unwrap_or_else(|e| e.into_inner()) = s;
    }

    /// Assemble a context from already-constructed services.
    ///
    /// [`Self::build`] is the production wrapper that resolves XDG paths
    /// and opens the real platform backends; this is the seam that makes
    /// composition testable at all.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        db: Arc<Mutex<Database>>,
        clipboard: Arc<dyn Clipboard>,
        uinput: Arc<dyn PasteKeys>,
        hotkey: Arc<dyn GlobalHotkey>,
        settings: Settings,
        single_instance: Option<SingleInstance>,
        db_path: std::path::PathBuf,
        secret_store: Arc<dyn fastpaste_platform::SecretStore>,
    ) -> Self {
        let paster = Arc::new(Paster::new(
            clipboard.clone(),
            uinput.clone(),
            settings.paste.delay_ms,
            settings.paste.restore_clipboard,
        ));
        let clipboard_history = Arc::new(ClipboardHistory::new(
            settings.clipboard_history.max_items as usize,
            settings.clipboard_history.enabled,
        ));
        Self {
            db,
            paster,
            clipboard,
            uinput,
            hotkey,
            settings: RwLock::new(settings),
            clipboard_history,
            single_instance,
            db_path,
            secret_store,
        }
    }
}

/// Build the single-instance key for a data directory.
///
/// The path cannot go in verbatim. A Win32 named mutex may not contain a
/// backslash — the character is reserved for the `Global\\`/`Local\\`
/// namespace prefix — and every Windows data dir is full of them, so the
/// obvious `format!("...{}", dir.display())` produces a name the OS
/// rejects. Names are length-limited there too.
///
/// Replacing every non-alphanumeric character keeps the mapping distinct
/// enough for this purpose: two directories can only collide if they
/// differ solely in separators, which cannot happen for absolute paths
/// on one machine.
fn instance_key_for(data_dir: &std::path::Path) -> String {
    let sanitised: String = data_dir
        .display()
        .to_string()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    // Keep well inside the Windows limit while leaving the tail — the
    // part that actually distinguishes users — intact.
    const MAX: usize = 180;
    let tail: String = if sanitised.chars().count() > MAX {
        sanitised
            .chars()
            .skip(sanitised.chars().count() - MAX)
            .collect()
    } else {
        sanitised
    };
    format!("fastpaste_instance_{tail}")
}

/// What [`AppContext::probe`] learned before anything was opened.
///
/// Holds the single-instance guard for the gap between "we know whether a
/// passphrase is needed" and "we have one", so nothing can start a second
/// instance while the unlock dialog is up.
pub struct StartupProbe {
    /// Whether the database needs a passphrase.
    pub encryption: fastpaste_data::EncryptionState,
    /// Settings, loaded before the database so the unlock dialog can be
    /// shown in the user's language.
    pub settings: Settings,
    /// The resolved data directory `db_path` was derived from. Carried on
    /// the probe per the two-phase interface even though nothing reads it
    /// back today — `build_unlocked_from` only ever needs `db_path` — kept
    /// as a natural place for a future consumer (e.g. a diagnostic that
    /// wants the directory rather than `db_path.parent()`) to reach it
    /// without re-deriving it.
    #[allow(dead_code)]
    data_dir: std::path::PathBuf,
    db_path: std::path::PathBuf,
    /// Behind a `RefCell` because Task 9 hands out `&StartupProbe` so a
    /// wrong passphrase can be retried against the same probe, and the
    /// build path still has to move the guard out of it exactly once.
    single_instance: std::cell::RefCell<Option<SingleInstance>>,
}

impl AppContext {
    /// First half of startup: take the single-instance guard, load
    /// settings, and find out whether the database needs a passphrase.
    ///
    /// Deliberately opens nothing and claims no device. The caller may
    /// have to put a dialog on screen before [`Self::build_unlocked`] can
    /// run, and that must not happen with the clipboard or `/dev/uinput`
    /// already held.
    pub fn probe() -> anyhow::Result<StartupProbe> {
        let proj_dirs = crate::paths::project_dirs()?;
        let data_dir = proj_dirs.data_dir().to_path_buf();
        Self::probe_in(&data_dir, &Settings::config_path()?)
    }

    /// [`Self::probe`], parameterised on the data directory and the
    /// settings file. `probe()` is untestable as a zero-argument function:
    /// it resolves a real XDG data dir and takes a real single-instance
    /// key from it, and it calls `Settings::load()`, which reads the
    /// developer's actual `~/.config/fastpaste/config.toml`. This seam
    /// lets a test point both at a fresh `TempDir` — a distinct instance
    /// key that cannot collide with a running instance, and a settings
    /// file the test controls — while running the exact guard →
    /// clean-orphan → encryption-state → settings body `probe()` also
    /// runs, so a regression that made this function open the database or
    /// claim a device (the failure mode the ordering comment below
    /// exists to prevent) is one a test can actually catch.
    fn probe_in(
        data_dir: &std::path::Path,
        settings_path: &std::path::Path,
    ) -> anyhow::Result<StartupProbe> {
        let data_dir = data_dir.to_path_buf();

        // ---- Single-instance guard, FIRST ----------------------------------
        // Before the database is opened and before any OS resource is
        // claimed. Acquiring it last meant a second instance ran the
        // migration runner against a database the first instance had open,
        // and grabbed devices, before discovering it should not have
        // started. Migrations are idempotent today, so nothing was
        // corrupted — but the ordering made that a property the next
        // migration could quietly break.
        //
        // `single-instance` uses a Linux abstract unix socket (no file on
        // disk; the kernel auto-releases the name when the process exits)
        // and a named mutex on Windows. Either namespace is shared across
        // users, so the key derives from the per-user data-dir path and
        // each OS user gets their own slot.
        let instance_key = instance_key_for(&data_dir);
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

        std::fs::create_dir_all(&data_dir)?;
        let db_path = data_dir.join("fastpaste.sqlite");
        tracing::info!("database path: {}", db_path.display());

        // A conversion that was interrupted by a crash leaves a `.new`
        // file. Now — with the guard held and before anything reads the
        // directory — is the only safe moment to clear it.
        fastpaste_data::clean_orphaned_conversion(&db_path)?;

        let encryption = fastpaste_data::Database::encryption_state(&db_path)?;
        tracing::info!("database encryption state: {encryption:?}");

        let settings = Settings::load_from(settings_path)?;

        Ok(StartupProbe {
            encryption,
            settings,
            data_dir,
            db_path,
            single_instance: std::cell::RefCell::new(Some(single_instance)),
        })
    }

    /// Second half of startup: open the database with `key` and build
    /// every service, taking the probe's guard by reference so a caller
    /// can retry against the same probe.
    ///
    /// The guard is taken out of `probe` LAST, only once every fallible
    /// step below has already succeeded, immediately before
    /// [`Self::new`] is called. Taking it any earlier — e.g. right after
    /// opening the database — would mean a *later* failure (a platform
    /// service that could not start) drops the local holding it and
    /// releases the single-instance bind while unwinding, even though
    /// `probe` itself survives the `?`. The next retry against that same
    /// probe (a wrong passphrase, corrected on a second attempt — Task 9)
    /// would then run with no guard at all. Ending on a still-empty
    /// `RefCell` after any failure, and taking the guard only on the
    /// attempt that actually succeeds, is what makes retrying safe.
    ///
    /// Degradation policy is unchanged from the original `build`:
    /// `/dev/uinput` unavailable → [`NullPasteKeys`]; no X connection →
    /// [`NullGlobalHotkey`]; a failing clipboard backend is still fatal.
    /// A credential store that cannot be reached is likewise non-fatal —
    /// it costs the user a prompt each launch, not the app. Note that
    /// `secret_store` is always the real [`fastpaste_platform::SystemSecretStore`]:
    /// availability is a live, re-checkable property (Task 6), not a
    /// verdict to bake into the type at startup, so callers that care
    /// (the unlock dialog, the Options dialog) ask `is_available()`
    /// themselves at the moment it matters.
    pub fn build_unlocked_from(
        probe: &StartupProbe,
        key: Option<secrecy::SecretString>,
    ) -> anyhow::Result<Self> {
        let settings = probe.settings.clone();
        let db_path = probe.db_path.clone();

        let db = Arc::new(Mutex::new(fastpaste_data::Database::open_with_key(
            &db_path,
            false,
            key.as_ref(),
        )?));

        // ---- Platform services ---------------------------------------------
        let clipboard = Arc::new(SystemClipboard::new()?);
        let uinput: Arc<dyn PasteKeys> = match SystemPasteKeys::new() {
            Ok(u) => Arc::new(u),
            Err(e) => {
                tracing::warn!(
                    "/dev/uinput unavailable ({e}); \
                     paste will leave payload on clipboard"
                );
                Arc::new(NullPasteKeys)
            }
        };
        let hotkey: Arc<dyn GlobalHotkey> = match SystemHotkeys::new() {
            Ok(h) => Arc::new(h),
            Err(e) => {
                tracing::error!(
                    "global hotkeys unavailable ({e}); \
                     the tray icon and main window still work, but the \
                     shortcuts will not fire. XWayland is required."
                );
                Arc::new(NullGlobalHotkey::new())
            }
        };

        let secret_store: Arc<dyn fastpaste_platform::SecretStore> =
            Arc::new(fastpaste_platform::SystemSecretStore::new());
        if !secret_store.is_available() {
            tracing::warn!(
                "no credential store available at startup; \
                 an encrypted database will prompt until one appears \
                 (this is re-checked live, not latched)"
            );
        }

        tracing::info!(
            "loaded settings: paste.delay_ms={}, paste.restore_clipboard={}, \
             clipboard_history.enabled={}, clipboard_history.max_items={}",
            settings.paste.delay_ms,
            settings.paste.restore_clipboard,
            settings.clipboard_history.enabled,
            settings.clipboard_history.max_items,
        );

        // Every fallible step above succeeded. Only now claim the guard —
        // exactly once: a second build from the same probe would get
        // None and run without one. Treat that as a bug rather than
        // silently starting unguarded.
        let single_instance = probe
            .single_instance
            .borrow_mut()
            .take()
            .ok_or_else(|| anyhow::anyhow!("startup probe already consumed"))?;

        Ok(Self::new(
            db,
            clipboard,
            uinput,
            hotkey,
            settings,
            Some(single_instance),
            db_path,
            secret_store,
        ))
    }

    /// [`Self::build_unlocked_from`] taking the probe by value, for the
    /// common case — [`Self::build`], and `main()`'s happy path — where
    /// there is no retry and the probe can simply be consumed.
    pub fn build_unlocked(
        probe: StartupProbe,
        key: Option<secrecy::SecretString>,
    ) -> anyhow::Result<Self> {
        Self::build_unlocked_from(&probe, key)
    }

    /// Both halves at once, with no passphrase. Fails on an encrypted
    /// database — a caller that might meet one must use [`Self::probe`]
    /// and [`Self::build_unlocked`] so it can prompt.
    ///
    /// Construct all services. DB is opened (or created) at
    /// `~/.local/share/fastpaste/fastpaste.sqlite`.
    ///
    /// Degradation policy, in the order the services are built:
    /// - another instance already running → error, and `main()` exits
    ///   before anything else is touched;
    /// - `/dev/uinput` unavailable → [`NullPasteKeys`]; paste leaves the
    ///   payload on the clipboard for a manual Ctrl+V;
    /// - no XWayland / X connection → [`NullGlobalHotkey`]; the tray, the
    ///   main window and clipboard history all still work, so killing
    ///   startup over it would be a much larger failure than the one
    ///   actually encountered;
    /// - the clipboard backend failing is still fatal: without it neither
    ///   pasting nor history capture is possible, which is the whole app.
    pub fn build() -> anyhow::Result<Self> {
        let probe = Self::probe()?;
        if probe.encryption == fastpaste_data::EncryptionState::Encrypted {
            anyhow::bail!("the database is encrypted; build() cannot supply a passphrase");
        }
        Self::build_unlocked(probe, None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fastpaste_platform::{NullClipboard, NullGlobalHotkey, NullPasteKeys};

    fn ctx(settings: Settings) -> (tempfile::TempDir, AppContext) {
        let dir = tempfile::TempDir::new().unwrap();
        let db = Arc::new(Mutex::new(
            Database::open(&dir.path().join("t.sqlite"), false).unwrap(),
        ));
        let db_path = dir.path().join("t.sqlite");
        let ctx = AppContext::new(
            db,
            Arc::new(NullClipboard::new()),
            Arc::new(NullPasteKeys),
            Arc::new(NullGlobalHotkey::new()),
            settings,
            None,
            db_path,
            Arc::new(fastpaste_platform::NullSecretStore::new()),
        );
        (dir, ctx)
    }

    #[test]
    fn the_instance_key_survives_a_windows_path() {
        // A Win32 named mutex may not contain a backslash, and every
        // Windows data dir is full of them.
        let key = instance_key_for(std::path::Path::new(
            r"C:\Users\Ada\AppData\Roaming\fastpaste\data",
        ));
        assert!(
            !key.contains('\\'),
            "a backslash makes the name invalid: {key}"
        );
        assert!(!key.contains(':'), "a colon is reserved too: {key}");
        assert!(key.starts_with("fastpaste_instance_"));
        assert!(key.len() < 200, "names are length-limited on Windows");
    }

    #[test]
    fn different_data_dirs_get_different_keys() {
        let a = instance_key_for(std::path::Path::new("/home/ada/.local/share/fastpaste"));
        let b = instance_key_for(std::path::Path::new("/home/bob/.local/share/fastpaste"));
        assert_ne!(a, b, "two users must not share one instance slot");
    }

    #[test]
    fn a_very_long_path_keeps_its_distinguishing_tail() {
        let deep = format!("/home/ada/{}/fastpaste", "x".repeat(400));
        let key = instance_key_for(std::path::Path::new(&deep));
        assert!(key.len() < 220);
        assert!(
            key.ends_with("fastpaste"),
            "the tail identifies the dir: {key}"
        );
    }

    #[test]
    fn services_are_wired_from_settings_at_construction() {
        let mut s = Settings::default();
        s.clipboard_history.max_items = 25;
        s.clipboard_history.enabled = false;
        let (_dir, ctx) = ctx(s);

        assert_eq!(ctx.clipboard_history.max_items(), 25);
        assert!(!ctx.clipboard_history.enabled());
    }

    /// The README promises the options dialog applies live. Every field
    /// that lives inside a service rather than in the settings struct has
    /// to be pushed by `set_settings` — `max_items` was the one that was
    /// not, and it silently needed a restart.
    #[test]
    fn set_settings_reaches_every_service_backed_field() {
        let (_dir, ctx) = ctx(Settings::default());
        assert_eq!(ctx.clipboard_history.max_items(), 10);
        assert!(ctx.clipboard_history.enabled());

        let mut changed = Settings::default();
        changed.clipboard_history.max_items = 3;
        changed.clipboard_history.enabled = false;
        changed.paste.delay_ms = 250;
        changed.paste.restore_clipboard = false;
        ctx.set_settings(changed.clone());

        assert_eq!(
            ctx.clipboard_history.max_items(),
            3,
            "max_items applies live"
        );
        assert!(!ctx.clipboard_history.enabled(), "enabled applies live");
        assert_eq!(ctx.settings(), changed, "and the snapshot is updated");
    }

    /// Shrinking the bound through the dialog must trim what is already
    /// captured, not just affect future copies.
    #[test]
    fn shrinking_max_items_through_settings_trims_existing_entries() {
        let (_dir, ctx) = ctx(Settings::default());
        for i in 0..8 {
            ctx.clipboard_history
                .on_clipboard_changed(fastpaste_platform::ClipboardPayload {
                    text: format!("item{i}"),
                    source_process: String::new(),
                });
        }
        assert_eq!(ctx.clipboard_history.entries().len(), 8);

        let mut smaller = Settings::default();
        smaller.clipboard_history.max_items = 2;
        ctx.set_settings(smaller);

        assert_eq!(ctx.clipboard_history.entries().len(), 2);
    }

    #[test]
    fn paste_settings_reach_the_paster() {
        let dir = tempfile::TempDir::new().unwrap();
        let db_path = dir.path().join("t.sqlite");
        let db = Arc::new(Mutex::new(Database::open(&db_path, false).unwrap()));
        let clip = Arc::new(NullClipboard::new());
        let ctx = AppContext::new(
            db,
            clip.clone(),
            Arc::new(NullPasteKeys),
            Arc::new(NullGlobalHotkey::new()),
            Settings::default(),
            None,
            db_path,
            Arc::new(fastpaste_platform::NullSecretStore::new()),
        );
        clip.set_text("orig").unwrap();

        // Default restore_clipboard = true, but uinput is the Null backend
        // so no keystroke is sent and the payload must stay put.
        ctx.paster.paste_text("payload").unwrap();
        assert_eq!(clip.text().unwrap(), "payload");

        // Turning restore off must reach the Paster through set_settings.
        let mut s = Settings::default();
        s.paste.restore_clipboard = false;
        s.paste.delay_ms = 0;
        ctx.set_settings(s);
        ctx.paster.paste_text("second").unwrap();
        assert_eq!(clip.text().unwrap(), "second");
    }

    #[test]
    fn settings_snapshot_is_a_clone_not_a_borrow() {
        let (_dir, ctx) = ctx(Settings::default());
        let mut snapshot = ctx.settings();
        snapshot.general.language = "ru".into();
        assert_eq!(
            ctx.settings().general.language,
            "system",
            "mutating a snapshot must not touch the live settings"
        );
    }

    /// The whole point of the split: work out whether a passphrase is
    /// needed without opening the database or claiming any device.
    #[test]
    fn probe_reports_the_encryption_state_of_a_plaintext_database() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("fastpaste.sqlite");
        drop(fastpaste_data::Database::open(&path, false).unwrap());

        assert_eq!(
            fastpaste_data::Database::encryption_state(&path).unwrap(),
            fastpaste_data::EncryptionState::Plaintext
        );
    }

    #[test]
    fn a_context_built_with_a_key_reads_the_encrypted_database() {
        use secrecy::SecretString;

        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("fastpaste.sqlite");
        let key = SecretString::from("s3cret".to_string());

        {
            let db = fastpaste_data::Database::open(&path, false).unwrap();
            let mut item = fastpaste_data::Item::new_plain(0, "T", "B");
            db.insert(&mut item).unwrap();
        }
        fastpaste_data::encrypt_database(&path, &key).unwrap();

        let db = Arc::new(Mutex::new(
            fastpaste_data::Database::open_with_key(&path, false, Some(&key)).unwrap(),
        ));
        let ctx = AppContext::new(
            db,
            Arc::new(NullClipboard::new()),
            Arc::new(NullPasteKeys),
            Arc::new(NullGlobalHotkey::new()),
            Settings::default(),
            None,
            path.clone(),
            Arc::new(fastpaste_platform::NullSecretStore::new()),
        );

        let loaded = ctx.db.lock().unwrap().load_all().unwrap();
        assert_eq!(loaded[0].body_plain, "B");
        assert_eq!(ctx.db_path, path, "conversions need the path");
    }

    /// Exercises the real `probe()` body (via the `probe_in` seam) end to
    /// end into a real `build_unlocked`: a fresh directory probes as
    /// `Absent`, and the resulting context's database is genuinely open
    /// and usable. This is the test that would catch a regression where
    /// `probe()` started opening the database or claiming a device before
    /// `build_unlocked` runs — the exact class of bug the guard-first
    /// ordering comment on `probe_in` exists to prevent.
    #[test]
    fn probe_then_build_unlocked_produces_a_working_context() {
        let dir = tempfile::TempDir::new().unwrap();
        let data_dir = dir.path().join("data");
        let settings_path = dir.path().join("config.toml");

        let probe = AppContext::probe_in(&data_dir, &settings_path).unwrap();
        assert_eq!(
            probe.encryption,
            fastpaste_data::EncryptionState::Absent,
            "nothing has been written yet"
        );

        let expected_db_path = data_dir.join("fastpaste.sqlite");
        let ctx = AppContext::build_unlocked(probe, None).unwrap();
        assert_eq!(ctx.db_path, expected_db_path);
        assert!(
            ctx.db.lock().unwrap().load_all().unwrap().is_empty(),
            "a fresh database opens with no items"
        );
    }

    /// `probe_in` must report `Encrypted` for a database that was
    /// encrypted ahead of time, and `build_unlocked_from` must actually
    /// decrypt it given the right key.
    #[test]
    fn probe_reports_encrypted_for_an_encrypted_database() {
        use secrecy::SecretString;

        let dir = tempfile::TempDir::new().unwrap();
        let data_dir = dir.path().join("data");
        let settings_path = dir.path().join("config.toml");
        std::fs::create_dir_all(&data_dir).unwrap();
        let db_path = data_dir.join("fastpaste.sqlite");
        let key = SecretString::from("s3cret".to_string());
        {
            let db = fastpaste_data::Database::open(&db_path, false).unwrap();
            let mut item = fastpaste_data::Item::new_plain(0, "T", "B");
            db.insert(&mut item).unwrap();
        }
        fastpaste_data::encrypt_database(&db_path, &key).unwrap();

        let probe = AppContext::probe_in(&data_dir, &settings_path).unwrap();
        assert_eq!(probe.encryption, fastpaste_data::EncryptionState::Encrypted);

        let ctx = AppContext::build_unlocked_from(&probe, Some(key)).unwrap();
        let loaded = ctx.db.lock().unwrap().load_all().unwrap();
        assert_eq!(loaded[0].body_plain, "B");
    }

    /// The whole reason the guard lives behind a `RefCell`: a wrong
    /// passphrase must fail `build_unlocked_from` WITHOUT releasing the
    /// single-instance guard, so a second attempt against the same probe
    /// (Task 9's retry loop) still runs guarded.
    #[test]
    fn a_failed_attempt_leaves_the_guard_for_a_retry_that_then_succeeds() {
        use secrecy::SecretString;

        let dir = tempfile::TempDir::new().unwrap();
        let data_dir = dir.path().join("data");
        let settings_path = dir.path().join("config.toml");
        std::fs::create_dir_all(&data_dir).unwrap();
        let db_path = data_dir.join("fastpaste.sqlite");
        let key = SecretString::from("right".to_string());
        {
            let db = fastpaste_data::Database::open(&db_path, false).unwrap();
            let mut item = fastpaste_data::Item::new_plain(0, "T", "B");
            db.insert(&mut item).unwrap();
        }
        fastpaste_data::encrypt_database(&db_path, &key).unwrap();

        let probe = AppContext::probe_in(&data_dir, &settings_path).unwrap();

        let wrong = SecretString::from("nope".to_string());
        assert!(
            AppContext::build_unlocked_from(&probe, Some(wrong)).is_err(),
            "the wrong passphrase must not open the database"
        );

        // If the failed attempt above had already taken the guard, this
        // would fail with "startup probe already consumed" instead of
        // opening the database.
        let ctx = AppContext::build_unlocked_from(&probe, Some(key)).unwrap();
        let loaded = ctx.db.lock().unwrap().load_all().unwrap();
        assert_eq!(loaded[0].body_plain, "B");
    }

    /// A second `build_unlocked_from` against a probe that already
    /// succeeded once must fail loudly rather than silently starting with
    /// no single-instance guard.
    #[test]
    fn a_second_build_from_the_same_probe_is_reported_as_an_error() {
        let dir = tempfile::TempDir::new().unwrap();
        let data_dir = dir.path().join("data");
        let settings_path = dir.path().join("config.toml");

        let probe = AppContext::probe_in(&data_dir, &settings_path).unwrap();
        let _ctx = AppContext::build_unlocked_from(&probe, None).unwrap();

        let err = match AppContext::build_unlocked_from(&probe, None) {
            Ok(_) => panic!("the guard was already taken by the first build"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("already consumed"), "got: {err}");
    }
}
