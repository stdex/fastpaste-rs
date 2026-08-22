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
    fn default_open_dialog() -> String {
        "Ctrl+U".to_string()
    }
    fn default_open_main_window() -> String {
        "Ctrl+Shift+U".to_string()
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

impl Settings {
    /// Where the config file lives. Resolved via the shared [`crate::paths`]
    /// helper so config and data dirs derive from one `ProjectDirs` triple;
    /// on Linux this is `~/.config/fastpaste/config.toml`.
    fn config_path() -> anyhow::Result<std::path::PathBuf> {
        let proj_dirs = crate::paths::project_dirs()?;
        Ok(proj_dirs.config_dir().join("config.toml"))
    }

    /// Load settings from disk. If the file does not exist, returns
    /// [`Settings::default`] WITHOUT writing anything (the first `save()` will
    /// create the file). Any other I/O or parse error is propagated.
    ///
    /// We pre-check file existence rather than letting `confy::load_path`
    /// handle the missing-file case, because `load_path` would *create* the
    /// file and write defaults on first run — surprising side-effect for a
    /// function called `load`. Keeping load read-only means the only writer
    /// is `save()`.
    pub fn load() -> anyhow::Result<Self> {
        let path = Self::config_path()?;
        if !path.exists() {
            tracing::info!(
                "settings file not found at {}; using defaults",
                path.display()
            );
            return Ok(Self::default());
        }
        let settings = confy::load_path::<Settings>(&path)?;
        Ok(settings)
    }

    /// Persist settings to disk as TOML. Uses `confy::store_path`, which
    /// writes atomically and creates the parent directory itself.
    pub fn save(&self) -> anyhow::Result<()> {
        let path = Self::config_path()?;
        confy::store_path(&path, self)?;
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
        assert_eq!(s.hotkeys.open_dialog, "Ctrl+U");
        assert_eq!(s.hotkeys.open_main_window, "Ctrl+Shift+U");

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

    /// Save -> load must round-trip exactly. Uses a fresh `tempfile`
    /// temp dir (auto-removed on drop — including on assertion failure)
    /// so the test never touches the user's real
    /// `~/.config/fastpaste/`.
    ///
    /// We bypass `Settings::config_path()` (which is hardwired to the user's
    /// XDG dir) by writing/loading via the same `confy` primitives that
    /// `save()` uses, against an explicit path.
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

        // Write (same code path as `Settings::save`, minus the XDG path;
        // TempDir already created the parent).
        confy::store_path(&path, &original).expect("store_path must succeed");

        // Read back via the same primitive `Settings::load` uses.
        let loaded: Settings = confy::load_path(&path).expect("load_path must round-trip");

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

        let loaded: Settings = confy::load_path(&path).unwrap();
        assert_eq!(loaded.clipboard_history.position, HistoryPosition::Bottom);
        // Everything else picked up its default.
        assert!(loaded.clipboard_history.enabled);
        assert_eq!(loaded.clipboard_history.max_items, 10);
    }
}
