//! Internationalization for the fastpaste GUI.
//!
//! Each supported locale lives as a `.ftl` (Mozilla Fluent) file under
//! `i18n/` next to this crate's `Cargo.toml`. The
//! [`fluent_templates::static_loader!`] macro embeds every file into the
//! binary at compile time (`include_str!` under the hood — no runtime file
//! access) and exposes a `Sync` static loader, so translations live in a
//! plain `static` and lookups are cheap. Adding a locale means dropping a
//! new `<tag>.ftl` file into `i18n/` — the macro scans the directory at
//! compile time, no code changes needed.
//!
//! Resolution order for [`I18n::new`]:
//!
//! 1. `"system"` (the `Settings::general.language` default) → ask the OS
//!    via `sys-locale` (`LC_ALL`/`LC_CTYPE`/`LANG` on Linux) and match the
//!    language subtag against the embedded set (`ru-RU` → `ru`; any `zh*`
//!    collapses to the one Chinese locale we ship).
//! 2. Any explicit tag we ship (`"en"`, `"ru"`, `"de"`, `"es"`, `"zh_CN"`
//!    — the underscore form parses and canonicalizes to `zh-CN`).
//! 3. Anything else (or a locale we don't ship) → English.
//!
//! Lookup order for [`I18n::msg`]:
//!
//! 1. The chosen locale's bundle.
//! 2. The English bundle (fluent-templates walks the fallback chain to the
//!    fallback language — a missing key in `ru.ftl` degrades to English).
//! 3. The literal `key` (only if even English lacks it — a programming
//!    error, logged at `warn`, but better than panicking in the UI).

use fluent_templates::{LanguageIdentifier, Loader, langid};

// Embedded translations: one flat `i18n/<tag>.ftl` file per locale, with
// English as the fallback language. The macro scans the directory at
// compile time and emits a `Sync` static loader.
fluent_templates::static_loader! {
    static LOCALES = {
        locales: "./i18n",
        fallback_language: "en",
    };
}

/// The English fallback. Returned by [`resolve_locale`] when nothing in the
/// input (or the OS locale) matches a locale we ship.
fn fallback() -> LanguageIdentifier {
    langid!("en")
}

/// A resolved locale plus the machinery to look up messages in it.
///
/// Thin by design: the bundles live in the [`LOCALES`] static, so an `I18n`
/// value is just the chosen `LanguageIdentifier` and [`I18n::new`] costs one
/// langid parse. The struct is `Send + Sync`, but we still rebuild it on
/// demand from the settings string rather than caching it globally — the
/// language can change at runtime via the Options dialog, and the rebuild
/// is free.
pub struct I18n {
    locale: LanguageIdentifier,
}

impl I18n {
    /// Build a translator for the given `language` setting.
    ///
    /// `language` corresponds to `Settings::general.language`:
    ///   - `"system"` → resolve from the OS via `sys-locale`.
    ///   - `"en"` / `"ru"` / `"de"` / `"es"` / `"zh_CN"` → that locale.
    ///   - anything else → English (logged at `warn`).
    ///
    /// Never panics: any unexpected input degrades to English.
    pub fn new(language: &str) -> Self {
        let resolved = resolve_locale(language);
        if language != "system" && !is_supported_tag(language) {
            tracing::warn!("i18n: unknown language setting {language:?}; falling back to English");
        }
        Self { locale: resolved }
    }

    /// Resolve a message by key. Falls back to English, then to the key
    /// literal — never returns an empty string, never panics.
    ///
    /// Does NOT take positional args yet because none of the v1 strings
    /// are parameterized. When the first interpolated string lands, use
    /// `LOCALES.lookup_with_args` directly rather than complicating this
    /// signature.
    pub fn msg(&self, key: &str) -> String {
        LOCALES.try_lookup(&self.locale, key).unwrap_or_else(|| {
            // Programming error: a Rust call site asked for a key that isn't
            // in en.ftl. The key-as-fallback keeps the UI functional.
            tracing::warn!("i18n: missing message key {key:?} (no English fallback)");
            key.to_string()
        })
    }

    /// The locale tag we ended up loading (canonical BCP-47 form, e.g.
    /// `"zh-CN"` for the `zh_CN` setting). Useful for tests and for
    /// surfacing in the Options dialog (language selector).
    pub fn locale(&self) -> String {
        self.locale.to_string()
    }
}

/// Map a `Settings::general.language` value to one of the embedded locales.
fn resolve_locale(language: &str) -> LanguageIdentifier {
    // An explicit tag we ship. Note the canonicalization: "zh_CN" parses
    // to the same LanguageIdentifier as "zh-CN", so both spellings work.
    if let Ok(id) = language.parse::<LanguageIdentifier>()
        && is_embedded(&id)
    {
        return id;
    }

    if language != "system" {
        return fallback();
    }

    // Ask the OS. sys-locale returns a BCP-47-ish tag such as "ru-RU", or
    // None in a bare environment (no LANG) — degrade to English then.
    let requested = sys_locale::get_locale().and_then(|tag| tag.parse::<LanguageIdentifier>().ok());

    // Match on the language subtag: "ru-RU" → "ru", "de-CH" → "de". Any
    // "zh*" (Hans or Hant, any region) collapses to the single Chinese
    // locale we ship, mirroring the C++ reference's zh-CN-only support.
    requested
        .and_then(|req| {
            LOCALES
                .locales()
                .find(|avail| avail.language == req.language)
                .cloned()
        })
        .unwrap_or_else(fallback)
}

/// Whether `language` names a locale we actually ship (after langid
/// canonicalization, so `"zh_CN"` counts).
fn is_supported_tag(language: &str) -> bool {
    language
        .parse::<LanguageIdentifier>()
        .is_ok_and(|id| is_embedded(&id))
}

fn is_embedded(id: &LanguageIdentifier) -> bool {
    LOCALES.locales().any(|avail| avail == id)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// The macro scans `i18n/` at compile time; touching the loader forces
    /// it to parse every embedded file, so a malformed `.ftl` fails here
    /// instead of panicking on a user's first lookup. Also pins the shipped
    /// locale set (note `zh_CN.ftl` canonicalizes to `zh-CN`).
    #[test]
    fn loader_embeds_the_five_shipped_locales() {
        let mut tags: Vec<String> = LOCALES.locales().map(|l| l.to_string()).collect();
        tags.sort_unstable();
        assert_eq!(tags, vec!["de", "en", "es", "ru", "zh-CN"]);
    }

    /// Every `.ftl` under `i18n/` must parse cleanly, and every locale must
    /// define the same set of keys as the English source. A missing key in
    /// (say) `ru.ftl` doesn't crash the app — lookups fall back to English —
    /// but it's a porting bug worth catching in CI. Scans the directory
    /// rather than a hand-written file list so a new locale can't be missed.
    #[test]
    fn all_ftl_files_parse_and_share_the_english_key_set() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("i18n");
        let mut by_tag: Vec<(String, HashSet<String>)> = std::fs::read_dir(&dir)
            .expect("i18n/ directory must exist")
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.is_file())
            .map(|p| {
                let tag = p.file_stem().unwrap().to_string_lossy().into_owned();
                assert_eq!(
                    p.extension().and_then(|e| e.to_str()),
                    Some("ftl"),
                    "unexpected non-.ftl file in i18n/: {p:?}"
                );
                let src = std::fs::read_to_string(&p).unwrap();
                (tag, keys_of(&src).into_iter().collect())
            })
            .collect();
        by_tag.sort_unstable_by(|a, b| a.0.cmp(&b.0));

        let en_keys = by_tag
            .iter()
            .find(|(tag, _)| tag == "en")
            .expect("en.ftl must exist")
            .1
            .clone();
        for (tag, keys) in &by_tag {
            let missing: Vec<_> = en_keys.difference(keys).collect();
            let extra: Vec<_> = keys.difference(&en_keys).collect();
            assert!(
                missing.is_empty() && extra.is_empty(),
                "{tag}.ftl key set diverges from en.ftl: missing={missing:?}, extra={extra:?}"
            );
        }
    }

    /// `I18n::new("en")` should hand back English text for a known key,
    /// and the raw key for an unknown one (programming-error path).
    #[test]
    fn english_lookup_and_missing_key() {
        let i18n = I18n::new("en");
        assert_eq!(i18n.locale(), "en");
        assert_eq!(i18n.msg("options-ok"), "OK");
        assert_eq!(i18n.msg("tray-quit"), "Quit");
        // Unknown key → echoed back.
        assert_eq!(
            i18n.msg("this-key-does-not-exist"),
            "this-key-does-not-exist"
        );
    }

    /// Russian returns Russian, not English, for keys that have a real
    /// translation. (Audience is Russian-speaking, so this is the one
    /// locale we hold to a higher bar.)
    #[test]
    fn russian_returns_russian() {
        let i18n = I18n::new("ru");
        assert_eq!(i18n.locale(), "ru");
        assert_eq!(i18n.msg("toolbar-delete"), "Удалить");
        assert_eq!(i18n.msg("tray-quit"), "Выход");
        assert_eq!(i18n.msg("options-cancel"), "Отмена");
    }

    /// A locale code we don't ship falls back to English silently (no panic,
    /// no `unwrap` blowing up). The `tracing::warn!` is logged but the test
    /// just asserts the lookup path still works.
    #[test]
    fn unknown_language_falls_back_to_english() {
        let i18n = I18n::new("klingon");
        assert_eq!(i18n.locale(), "en");
        assert_eq!(i18n.msg("options-ok"), "OK");
    }

    /// The settings file stores the underscore spelling; it must resolve to
    /// the canonical tag, not to English.
    #[test]
    fn underscore_chinese_tag_resolves_to_the_shipped_locale() {
        let i18n = I18n::new("zh_CN");
        assert_eq!(i18n.locale(), "zh-CN");
        assert_eq!(i18n.msg("options-ok"), "确定");
    }

    /// `"system"` must not panic, and must resolve to one of the tags we
    /// actually ship. The exact tag depends on the test machine's `LANG`,
    /// so we only assert membership in the supported set.
    #[test]
    fn system_resolves_to_a_supported_locale() {
        let i18n = I18n::new("system");
        let supported: Vec<String> = LOCALES.locales().map(|l| l.to_string()).collect();
        assert!(
            supported.contains(&i18n.locale()),
            "system resolved to {:?}, expected one of {:?}",
            i18n.locale(),
            supported
        );
    }

    /// Helper: extract the top-level message keys from a `.ftl` source.
    /// Uses `fluent_syntax::parse` so we're testing against the actual
    /// fluent grammar, not a regex that could lie. Panics on a parse error,
    /// which is exactly what this test wants to surface.
    fn keys_of(src: &str) -> Vec<String> {
        let mut out = Vec::new();
        let res =
            fluent_syntax::parser::parse(src).expect("ftl source must parse for key extraction");
        for entry in &res.body {
            if let fluent_syntax::ast::Entry::Message(msg) = entry {
                out.push(msg.id.name.to_string());
            }
        }
        out
    }
}
