# English source strings for fastpaste.
#
# This is the canonical key set: every other locale file (ru, de, es, zh_CN)
# must define exactly the same keys. Lookups for a missing key fall back to
# English (and then to the literal key), so do NOT delete a key here without
# removing every consumer in Rust first.
#
# v1 scope: only the keys the current UI surfaces need (toolbar,
# editor, selection dialog, tray, options tabs). The C++ original ships
# 117 lexemes; we are intentionally porting only the essential ones for now.

app-title = fastpaste
main-window-title = fastpaste
toolbar-add-folder = Add Folder
toolbar-add-snippet = Add Snippet
toolbar-delete = Delete
toolbar-move-up = Up
toolbar-move-down = Down
editor-title-label = Title:
editor-body-label = Body:
selection-dialog-title = Select a string to paste
tray-open-main-window = Open Main Window
tray-selection-dialog = Selection Dialog
tray-options = Options...
tray-quit = Quit
options-title = Options
options-general = General
options-hotkeys = Hot Keys
options-clipboard-history = Clipboard History
options-paste = Paste Options
options-ok = OK
options-cancel = Cancel
options-apply = Apply
options-language-label = Language:
options-language-hint = Select the application language. "System" follows your OS locale.
options-open-dialog-label = Open selection dialog:
options-open-main-window-label = Open Main Window:
options-hotkeys-hint = Modifiers: Ctrl, Alt, Shift, Super. Combine with "+". Example: Ctrl+Alt+V
options-capture-history = Capture clipboard changes
options-max-items-label = Maximum items:
options-folder-position-label = Folder position:
options-position-top = Top
options-position-bottom = Bottom
options-paste-delay-label = Paste delay (ms):
options-restore-clipboard = Restore clipboard contents after pasting
clipboard-history-folder = Clipboard History

# Delete confirmation, options validation feedback, and the
# selection dialog's filter/empty states.
confirm-delete-title = Delete?
confirm-delete-folder = This will permanently delete this folder and everything inside it:
confirm-yes = Delete
confirm-no = Cancel
options-error-hotkey-duplicate = The two shortcuts must be different.
options-error-hotkey-taken = That shortcut is already used by another application.
options-error-hotkey-invalid = That shortcut could not be registered.
options-error-save-failed = Settings applied for this session, but could not be saved to disk.
selection-filter-placeholder = Type to filter
selection-empty = No snippets yet.
selection-tag-history = History
selection-hint = ↑↓ select · Enter paste · Esc close
selection-section-snippets = Snippets

unlock-title = Unlock fastpaste
unlock-prompt = This database is encrypted. Enter your passphrase to open it.
unlock-passphrase-label = Passphrase:
unlock-remember = Remember in the system keyring
unlock-remember-unavailable = Remember in the system keyring (unavailable on this session)
unlock-error-wrong = Wrong passphrase.
unlock-unlock = Unlock
unlock-cancel = Cancel

options-security = Security
options-security-state-plaintext = This database is not encrypted. Anyone who can read the file can read your snippets.
options-security-state-encrypted = This database is encrypted.
options-security-current-label = Current passphrase:
options-security-new-label = New passphrase:
options-security-confirm-label = Confirm:
options-security-encrypt = Encrypt database
options-security-change = Change passphrase
options-security-remove = Remove encryption
options-security-warning = There is no way to recover a forgotten passphrase. Encrypting protects the file from this point on: the unencrypted copy is deleted, but on an SSD or a journalling filesystem its contents may still be recoverable from the raw device.
options-security-mismatch = The two passphrases do not match.
options-security-empty = Enter a passphrase.
options-security-done-encrypted = The database is now encrypted.
options-security-done-changed = Passphrase changed.
options-security-done-removed = Encryption removed.
