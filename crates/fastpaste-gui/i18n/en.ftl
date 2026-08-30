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
options-hotkeys-hint = Modifiers: Ctrl, Alt, Shift, Super. Combine with "+". Example: Ctrl+U
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
