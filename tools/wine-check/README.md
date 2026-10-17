# wine-check

Runs fastpaste's **real** Windows paste path — `Paster` over
`SystemClipboard`, `SystemPasteKeys` and `SystemForeground` — under Wine,
pasting into an EDIT control and reporting what arrived. It is how the
paste-sequence findings in the history were measured and verified.

Needs the `x86_64-pc-windows-gnu` target, mingw (`x86_64-w64-mingw32-gcc`),
perl and Wine 10. Build from this directory, sharing the workspace's
lockfile and target directory so nothing is resolved or built twice:

```
cp ../../Cargo.lock .
CARGO_TARGET_DIR=../../target cargo build --offline --target x86_64-pc-windows-gnu
```

Run in a scratch prefix with the null graphics driver: no window on
screen, and Wine does not sync its clipboard with the host's.

```
export WINEPREFIX=/path/to/a/scratch/prefix
env -u DISPLAY -u WAYLAND_DISPLAY wineboot -i
env -u DISPLAY -u WAYLAND_DISPLAY wine reg add 'HKCU\Software\Wine\Drivers' /v Graphics /t REG_SZ /d null /f
env -u DISPLAY -u WAYLAND_DISPLAY wine ../../target/x86_64-pc-windows-gnu/debug/wine-check.exe
```

| Scenario | What it shows |
|---|---|
| `listener` | Clipboard history sees an ordinary copy |
| `paste` | Snippet vs. old clipboard pasted, per `restore_delay_ms`; own writes stay out of history |
| `focus` | The paste goes to the captured window, not the one that took focus; a recycled handle is refused |
| `scan` | Key messages carry scan codes |
| `held` | A held Alt+Shift does not corrupt the chord |
| `nontext` | Image and file clipboards come back; an unknown format cancels the paste |
| `shiftins` | The Shift+Insert method pastes |
| `midcopy` | A copy made mid-paste survives (runs last: it opens a second clipboard instance) |
| `bigimage` | Snapshot/restore time of a 4000x3000 image (only when named) |

Name scenarios as arguments to run only those. Wine is not Windows: there
are no integrity levels (so no UIPI), and two clipboard instances in one
process can close each other's sessions.
