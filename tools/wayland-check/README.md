# wayland-check

Runs fastpaste's **real** Linux clipboard snapshot/restore — empty, text,
files, and a 4000x3000 image (timed, and compared pixel for pixel after
the round trip) — against a headless KWin, never the session you work in.
It refuses to run unless `WAYLAND_DISPLAY` starts with `fp-`.

KWin gets its own socket and its own D-Bus session, so it registers
nothing on the desktop's bus:

```
dbus-run-session -- kwin_wayland --virtual --socket fp-test &
cp ../../Cargo.lock .
CARGO_TARGET_DIR=../../target cargo build --offline
WAYLAND_DISPLAY=fp-test ../../target/debug/wayland-check
```

Stop the compositor afterwards with `kill %1` (or by its PID); a
`pkill -f` on the socket name also matches the shell running it.
