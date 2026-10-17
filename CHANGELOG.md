# Changelog

All notable changes to fastpaste are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions
follow [Semantic Versioning](https://semver.org/).

Each release moves the `Unreleased` entries under a `## [X.Y.Z] - date`
heading. The release workflow uses that section as the release notes, and
fails if there is none for the tag it is building (see "Releasing" in the
README).

## [Unreleased]

First release.

### Added

- Clipboard history: a bounded ring fed by watching the system clipboard,
  shown as a virtual folder beside the snippet folders.
- Snippet library: a folder tree with an editor pane, stored in SQLite.
  Optional SQLCipher encryption, with the passphrase remembered in the
  system keyring if you choose.
- Selection dialog (`Ctrl+Alt+V` by default): type-to-filter quick paste
  over the history and the snippets. The main window is on `Ctrl+Alt+M`.
- Paste by emulating Ctrl+V, or Shift+Insert for terminals, into the
  window that had focus, then putting the previous clipboard (text, an
  image or copied files) back: `/dev/uinput` on Linux, `SendInput` on
  Windows.
- Options dialog, applied live; tray icon; English, Russian, German,
  Spanish and Simplified Chinese.
- Packages: a `.deb` and an `.rpm` for Linux, which install a udev rule
  giving the logged-in user access to `/dev/uinput`, and a per-user
  installer and a portable zip for Windows.
- `fastpaste --version`.

### Known limitations

- On Linux the global hotkeys only fire while an X11/XWayland window has
  focus; the tray icon and the main window always work. See the README.
