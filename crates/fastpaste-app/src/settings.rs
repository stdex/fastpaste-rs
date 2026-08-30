//! Persistent application settings, serialized to TOML via `confy`.
//!
//! The config file lives at `~/.config/fastpaste/config.toml` (resolved via
//! `directories::ProjectDirs` for portability). The top-level [`Settings`]
//! struct groups options by concern; each group is a plain serializable struct
//! so the on-disk TOML stays readable and so we can hand sub-groups to the
//! services that consume them.
//!
//! Forward-compat note: every field carries `#[serde(default)]` (or an explicit
//! `default = "fn"`). That means an old config file missing a newly-added
//! field will still deserialize — the new field simply picks up its default.
//! This is the same pattern the C++ fastpaste config has always relied on.

use fastpaste_data::HistoryPosition;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Top-level
// ---------------------------------------------------------------------------

/// All persisted application configuration. The root of `config.toml`.
///
/// `#[derive(Default)]` here just composes the `Default` impls of the
/// sub-structs (which are written by hand to give non-trivial defaults such
/// as `enabled = true`). Don't add fields to this struct without also
/// extending the corresponding sub-struct's `Default`.
///
/// `PartialEq` powers the no-op short-circuit in the Options-dialog apply
/// path (skip side effects when nothing changed).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Settings {
    #[serde(default)]
    pub general: GeneralSettings,
    #[serde(default)]
    pub hotkeys: HotkeySettings,
    #[serde(default)]
    pub clipboard_history: ClipboardHistorySettings,
    #[serde(default)]
    pub paste: PasteSettings,
}

// ---------------------------------------------------------------------------
// Sub-structs
// ---------------------------------------------------------------------------

/// UI locale. `"system"` (the default) means "follow the OS"; any other value
/// is interpreted as a BCP-47 tag (e.g. `"en-US"`, `"ru"`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GeneralSettings {
    #[serde(default = "GeneralSettings::default_language")]
    pub language: String,
}

impl GeneralSettings {
    fn default_language() -> String {
        "system".to_string()
    }
}

impl Default for GeneralSettings {
    fn default() -> Self {
        Self {
            language: Self::default_language(),
        }
    }
}

/// Global hotkey accelerators, in the `<modifier>+<key>` grammar used by the
/// platform layer (X11 first; Wayland to follow).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HotkeySettings {
    #[serde(default = "HotkeySettings::default_open_dialog")]
    pub open_dialog: String,
    #[serde(default = "HotkeySettings::default_open_main_window")]
    pub open_main_window: String,
}

impl HotkeySettings {
    // Ctrl+U was the original default and was a poor choice twice over:
    // it is View Source in Chrome and Firefox, and it is the line-kill
    // in every readline-based shell. A global shortcut takes the key
    // away from every application that wanted it, so a default has to be
    // one nobody else is using. Ctrl+Alt+V keeps the paste mnemonic and
    // is the conventional slot for clipboard history on KDE.
    //
    // The two defaults deliberately differ by the KEY, not by Shift.
    // A pair that differs only by a modifier is fragile: Alt+Shift is
    // the layout switcher on many setups, a compositor can claim the
    // longer combination before it reaches us, and anything that
    // normalises Shift away collapses the two into one — reported in
    // practice as "the four-key combination triggers the three-key
    // action". Different keys cannot collapse.
    //
    // Existing installations are unaffected: a `config.toml` that names
    // its hotkeys explicitly keeps those values, since these defaults
    // only fill in fields that are absent.
    fn default_open_dialog() -> String {
        "Ctrl+Alt+V".to_string()
    }
    fn default_open_main_window() -> String {
        "Ctrl+Alt+M".to_string()
    }
}

impl Default for HotkeySettings {
    fn default() -> Self {
        Self {
            open_dialog: Self::default_open_dialog(),
            open_main_window: Self::default_open_main_window(),
        }
    }
}

/// The virtual Clipboard History folder (a folder whose contents are filled
/// automatically by observing the system clipboard).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipboardHistorySettings {
    /// `true` enables history capture. Note: `bool`'s natural `Default` is
    /// `false`, which is NOT what we want here, so we override it.
    #[serde(default = "ClipboardHistorySettings::default_enabled")]
    pub enabled: bool,
    #[serde(default = "ClipboardHistorySettings::default_max_items")]
    pub max_items: u32,
    /// Where the history folder appears in the tree. On disk this is the
    /// lowercase word (`"top"` / `"bottom"`); unknown values load as
    /// `Bottom` (see the enum's serde attributes).
    #[serde(default = "ClipboardHistorySettings::default_position")]
    pub position: HistoryPosition,
}

impl ClipboardHistorySettings {
    fn default_enabled() -> bool {
        true
    }
    fn default_max_items() -> u32 {
        10
    }
    fn default_position() -> HistoryPosition {
        HistoryPosition::Bottom
    }
}

impl Default for ClipboardHistorySettings {
    fn default() -> Self {
        Self {
            enabled: Self::default_enabled(),
            max_items: Self::default_max_items(),
            position: Self::default_position(),
        }
    }
}

/// Tunables for the paste sequence (`Paster`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PasteSettings {
    /// Milliseconds to wait between setting the clipboard and emitting Ctrl+V.
    /// 70ms is the empirically-tuned production default (see git history).
    #[serde(default = "PasteSettings::default_delay_ms")]
    pub delay_ms: u64,
    /// Whether to restore the user's previous clipboard contents after paste.
    #[serde(default = "PasteSettings::default_restore_clipboard")]
    pub restore_clipboard: bool,
}

impl PasteSettings {
    fn default_delay_ms() -> u64 {
        70
    }
    fn default_restore_clipboard() -> bool {
        true
    }
}

impl Default for PasteSettings {
    fn default() -> Self {
        Self {
            delay_ms: Self::default_delay_ms(),
            restore_clipboard: Self::default_restore_clipboard(),
        }
    }
}

// ---------------------------------------------------------------------------
// Persistence
// ---------------------------------------------------------------------------

/// Accepted range for `clipboard_history.max_items`.
///
/// The value reaches `Vec` operations and the tree builder straight from a
/// user-editable file, so it is clamped rather than trusted: an unbounded
/// upper end turns a typo into a startup-time allocation failure, and 0
/// would make the feature silently inert.
pub const MAX_ITEMS_RANGE: std::ops::RangeInclusive<u32> = 1..=500;

/// Accepted range for `paste.delay_ms`. The upper bound keeps a typo from
/// wedging the paste worker thread for what looks like forever.
pub const DELAY_MS_RANGE: std::ops::RangeInclusive<u64> = 0..=5_000;

impl Settings {
    /// Where the config file lives. Resolved via the shared [`crate::paths`]
    /// helper so config and data dirs derive from one `ProjectDirs` triple;
    /// on Linux this is `~/.config/fastpaste/config.toml`.
    fn config_path() -> anyhow::Result<std::path::PathBuf> {
        let proj_dirs = crate::paths::project_dirs()?;
        Ok(proj_dirs.config_dir().join("config.toml"))
    }

    /// Bring out-of-range values into their accepted ranges.
    ///
    /// Validation lives here rather than in the Options dialog because the
    /// config file is hand-editable: the dialog is not the only way a value
    /// arrives, and it should not be the only thing standing between a
    /// typo and a failed allocation.
    pub fn clamp(&mut self) {
        let max_items = self
            .clipboard_history
            .max_items
            .clamp(*MAX_ITEMS_RANGE.start(), *MAX_ITEMS_RANGE.end());
        if max_items != self.clipboard_history.max_items {
            tracing::warn!(
                "clipboard_history.max_items {} out of range {:?}; using {max_items}",
                self.clipboard_history.max_items,
                MAX_ITEMS_RANGE,
            );
            self.clipboard_history.max_items = max_items;
        }

        let delay_ms = self
            .paste
            .delay_ms
            .clamp(*DELAY_MS_RANGE.start(), *DELAY_MS_RANGE.end());
        if delay_ms != self.paste.delay_ms {
            tracing::warn!(
                "paste.delay_ms {} out of range {:?}; using {delay_ms}",
                self.paste.delay_ms,
                DELAY_MS_RANGE,
            );
            self.paste.delay_ms = delay_ms;
        }
    }

    /// Load settings from disk. If the file does not exist, returns
    /// [`Settings::default`] WITHOUT writing anything (the first `save()` will
    /// create the file).
    ///
    /// We pre-check file existence rather than letting `confy::load_path`
    /// handle the missing-file case, because `load_path` would *create* the
    /// file and write defaults on first run — surprising side-effect for a
    /// function called `load`. Keeping load read-only means the only writer
    /// is `save()`.
    ///
    /// A file that exists but cannot be parsed is moved aside and defaults
    /// are used. Propagating that error instead killed startup outright,
    /// and for a tray application launched from a desktop entry there is
    /// nowhere for the message to go — the app simply never appeared.
    pub fn load() -> anyhow::Result<Self> {
        Self::load_from(&Self::config_path()?)
    }

    /// [`Self::load`] against an explicit path. Separated so the tests can
    /// exercise the real function rather than reimplementing it.
    pub fn load_from(path: &std::path::Path) -> anyhow::Result<Self> {
        if !path.exists() {
            tracing::info!(
                "settings file not found at {}; using defaults",
                path.display()
            );
            return Ok(Self::default());
        }
        let mut settings = match confy::load_path::<Settings>(path) {
            Ok(s) => s,
            Err(e) => {
                let backup = path.with_extension("toml.bak");
                tracing::error!(
                    "could not parse {}: {e}; moving it to {} and starting from defaults",
                    path.display(),
                    backup.display(),
                );
                if let Err(e) = std::fs::rename(path, &backup) {
                    tracing::error!("could not move the unreadable config aside: {e}");
                }
                Settings::default()
            }
        };
        settings.clamp();
        Ok(settings)
    }

    /// Persist settings to disk as TOML. Uses `confy::store_path`, which
    /// writes atomically and creates the parent directory itself.
    pub fn save(&self) -> anyhow::Result<()> {
        self.save_to(&Self::config_path()?)
    }

    /// [`Self::save`] against an explicit path.
    pub fn save_to(&self, path: &std::path::Path) -> anyhow::Result<()> {
        confy::store_path(path, self)?;
        tracing::debug!("settings saved to {}", path.display());
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// `Default::default()` must give us the documented production defaults.
    /// If this test fails, a sub-struct's `Default` impl drifted.
    #[test]
    fn defaults_are_sensible() {
        let s = Settings::default();

        // general
        assert_eq!(s.general.language, "system");

        // hotkeys
        assert_eq!(s.hotkeys.open_dialog, "Ctrl+Alt+V");
        assert_eq!(s.hotkeys.open_main_window, "Ctrl+Alt+M");

        // clipboard history
        assert!(s.clipboard_history.enabled, "`enabled` must default true");
        assert_eq!(s.clipboard_history.max_items, 10);
        assert_eq!(s.clipboard_history.position, HistoryPosition::Bottom);

        // paste
        assert_eq!(s.paste.delay_ms, 70);
        assert!(
            s.paste.restore_clipboard,
            "`restore_clipboard` must default true"
        );
    }

    /// Save -> load must round-trip exactly, through the real
    /// `save_to`/`load_from` (which `save`/`load` delegate to). Uses a
    /// fresh `tempfile` temp dir — auto-removed on drop, including on
    /// assertion failure — so the test never touches the user's real
    /// `~/.config/fastpaste/`.
    #[test]
    fn round_trip_via_temp_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("config.toml");

        // Build a non-default set of values so we'd notice any field getting
        // reset on the way through.
        let original = Settings {
            general: GeneralSettings {
                language: "ru".to_string(),
            },
            hotkeys: HotkeySettings {
                open_dialog: "Alt+Space".to_string(),
                open_main_window: "Alt+Shift+Space".to_string(),
            },
            clipboard_history: ClipboardHistorySettings {
                enabled: false,
                max_items: 42,
                position: HistoryPosition::Top,
            },
            paste: PasteSettings {
                delay_ms: 123,
                restore_clipboard: false,
            },
        };

        original.save_to(&path).expect("save_to must succeed");
        let loaded = Settings::load_from(&path).expect("load_from must round-trip");

        assert_eq!(loaded.general.language, "ru");
        assert_eq!(loaded.hotkeys.open_dialog, "Alt+Space");
        assert_eq!(loaded.hotkeys.open_main_window, "Alt+Shift+Space");
        assert!(!loaded.clipboard_history.enabled);
        assert_eq!(loaded.clipboard_history.max_items, 42);
        assert_eq!(loaded.clipboard_history.position, HistoryPosition::Top);
        assert_eq!(loaded.paste.delay_ms, 123);
        assert!(!loaded.paste.restore_clipboard);

        // `dir` removes itself (and config.toml) on drop.
    }

    /// A config with a position value this build doesn't know (future
    /// version, hand-edit) must still load — `#[serde(other)]` on the enum
    /// maps it to Bottom instead of failing the whole file.
    #[test]
    fn unknown_position_loads_as_bottom() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[clipboard_history]\nposition = \"middle\"\n").unwrap();

        let loaded = Settings::load_from(&path).unwrap();
        assert_eq!(loaded.clipboard_history.position, HistoryPosition::Bottom);
        // Everything else picked up its default.
        assert!(loaded.clipboard_history.enabled);
        assert_eq!(loaded.clipboard_history.max_items, 10);
    }

    /// The README's compatibility promise, asserted in full rather than
    /// for one section: a config written by an older version — i.e. one
    /// missing whole sections — keeps loading, and every absent field
    /// takes its default.
    #[test]
    fn a_config_missing_whole_sections_loads_with_defaults() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[clipboard_history]\nmax_items = 25\n").unwrap();

        let loaded = Settings::load_from(&path).unwrap();
        let defaults = Settings::default();

        assert_eq!(
            loaded.clipboard_history.max_items, 25,
            "the one set field wins"
        );
        assert_eq!(loaded.general.language, defaults.general.language);
        assert_eq!(loaded.hotkeys.open_dialog, defaults.hotkeys.open_dialog);
        assert_eq!(
            loaded.hotkeys.open_main_window,
            defaults.hotkeys.open_main_window
        );
        assert_eq!(
            loaded.clipboard_history.enabled,
            defaults.clipboard_history.enabled
        );
        assert_eq!(
            loaded.clipboard_history.position,
            defaults.clipboard_history.position
        );
        assert_eq!(loaded.paste.delay_ms, defaults.paste.delay_ms);
        assert_eq!(
            loaded.paste.restore_clipboard,
            defaults.paste.restore_clipboard
        );
    }

    /// The forward half of the same promise: a key this build has never
    /// heard of must not fail the file. Pins the absence of
    /// `#[serde(deny_unknown_fields)]`.
    #[test]
    fn an_unknown_key_from_a_future_version_is_ignored() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "[general]\nlanguage = \"ru\"\nfuture_key = 1\n\n\
             [paste]\ndelay_ms = 90\nfuture_paste_option = true\n",
        )
        .unwrap();

        let loaded = Settings::load_from(&path).unwrap();
        assert_eq!(loaded.general.language, "ru");
        assert_eq!(loaded.paste.delay_ms, 90);
    }

    #[test]
    fn a_missing_file_loads_defaults_without_creating_it() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("config.toml");

        let loaded = Settings::load_from(&path).unwrap();
        assert_eq!(loaded, Settings::default());
        assert!(!path.exists(), "load must not write anything");
    }

    /// A corrupt config used to be fatal to startup, with the message
    /// going to a stderr nobody sees. It must degrade instead — and keep
    /// the user's file so they can salvage it.
    #[test]
    fn a_corrupt_config_is_moved_aside_and_defaults_are_used() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "this is not { valid toml [[[").unwrap();

        let loaded = Settings::load_from(&path).expect("must not fail startup");
        assert_eq!(loaded, Settings::default());

        // Assert the LITERAL name: computing it with the same
        // `with_extension` expression the code uses would pass for any
        // naming bug.
        let backup = dir.path().join("config.toml.bak");
        assert!(backup.exists(), "the unreadable file must be preserved");
        assert!(!path.exists(), "and moved out of the way");
        assert_eq!(
            std::fs::read_to_string(&backup).unwrap(),
            "this is not { valid toml [[[",
            "the salvageable original must be untouched"
        );
    }

    // ---- range clamping -------------------------------------------------

    #[test]
    fn out_of_range_values_are_clamped_on_load() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        // A hand-edited file: max_items would be a ~160 GB allocation if
        // it were used as a capacity, delay_ms would wedge the paste
        // worker for eleven days.
        std::fs::write(
            &path,
            "[clipboard_history]\nmax_items = 4000000000\n\n\
             [paste]\ndelay_ms = 999999999\n",
        )
        .unwrap();

        let loaded = Settings::load_from(&path).unwrap();
        assert_eq!(loaded.clipboard_history.max_items, *MAX_ITEMS_RANGE.end());
        assert_eq!(loaded.paste.delay_ms, *DELAY_MS_RANGE.end());
    }

    #[test]
    fn zero_max_items_is_clamped_up() {
        let mut s = Settings::default();
        s.clipboard_history.max_items = 0;
        s.clamp();
        assert_eq!(s.clipboard_history.max_items, *MAX_ITEMS_RANGE.start());
    }

    #[test]
    fn clamp_leaves_in_range_values_alone() {
        let mut s = Settings::default();
        let before = s.clone();
        s.clamp();
        assert_eq!(s, before);

        let mut s = Settings::default();
        s.clipboard_history.max_items = 250;
        s.paste.delay_ms = 3_000;
        s.clamp();
        assert_eq!(s.clipboard_history.max_items, 250);
        assert_eq!(s.paste.delay_ms, 3_000);
    }

    #[test]
    fn defaults_are_inside_the_accepted_ranges() {
        let d = Settings::default();
        assert!(MAX_ITEMS_RANGE.contains(&d.clipboard_history.max_items));
        assert!(DELAY_MS_RANGE.contains(&d.paste.delay_ms));
    }
}
