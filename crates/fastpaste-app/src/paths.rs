//! Single source of truth for XDG directory resolution.
//!
//! Both the data dir (DB, see [`crate::context`]) and the config dir
//! (settings file, see [`crate::settings`]) derive from one
//! `ProjectDirs` triple here, so they can never diverge.

/// Resolve the project directories, or fail with a readable error (only
/// possible when the required XDG environment variables are unset in a way
/// that leaves `directories` no path to build on).
pub fn project_dirs() -> anyhow::Result<directories::ProjectDirs> {
    directories::ProjectDirs::from("dev", "fastpaste", "fastpaste")
        .ok_or_else(|| anyhow::anyhow!("cannot resolve fastpaste project directories"))
}
