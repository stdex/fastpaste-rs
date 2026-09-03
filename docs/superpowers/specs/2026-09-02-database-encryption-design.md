# Database encryption at rest (SQLCipher)

Status: approved design, not yet implemented
Date: 2026-09-02

## Problem

`fastpaste.sqlite` is a plaintext SQLite file at
`~/.local/share/fastpaste/fastpaste.sqlite`. Anyone who obtains the file —
from a backup, a synced folder, a stolen laptop, another account on the
machine, or root — can read every snippet with the `sqlite3` CLI. The
snippet library is exactly the kind of thing people keep credentials and
tokens in.

## Threat model

**Defended against:** an attacker who obtains the database file.

**Not defended against:** an attacker running code as the user while the
app is unlocked. Once unlocked, the key is in the process and the
plaintext is reachable; nothing in this design changes that.

Explicit non-goals:

- **Idle re-lock / lock-on-screensaver.** The threat is file theft, not
  shoulder-surfing. A re-lock timer adds UI and state for a threat that
  was not chosen. Straightforward to add later.
- **Hiding the fact that fastpaste is installed**, or that a database
  exists. Only its contents are protected.
- **Secure erasure of the pre-encryption plaintext.** See section 5 — this is a
  stated limitation, not an oversight.
- **Per-item encryption** ("vault" entries inside an otherwise plaintext
  library). Whole-database or nothing.

## Decisions

| Decision | Choice |
|---|---|
| Mechanism | SQLCipher, whole-file page encryption |
| Key source | Passphrase, optionally remembered in the OS keyring |
| Rollout | Opt-in, switchable in both directions |

### Why SQLCipher

SQLCipher encrypts pages, indices, the freelist, the journal and the WAL.
An attacker holding the file learns nothing — not the content, not the
item count, not the shape of the folder tree. It is long-established,
widely reviewed code rather than a crypto composition assembled for this
project.

Its costs are accepted deliberately:

- The build compiles OpenSSL from source (`openssl-src`, new to the
  lockfile) on top of the existing bundled C SQLite build. Cold builds get
  slower and the Windows cross-build gets harder. Section 1 addresses this
  with a real Windows CI job.
- Its KDF is PBKDF2-HMAC-SHA512, not Argon2id — weaker against GPU
  cracking of a human-chosen passphrase than a modern memory-hard KDF
  would be. Accepted in exchange for stock-default compatibility with the
  standard `sqlcipher` CLI, which is the difference between an awkward
  recovery and an impossible one.

### Alternatives rejected

**Application-level AEAD on the text columns.** Encrypt `title`,
`body_plain`, `body_rtf` and `comment` in `fastpaste-data` with
XChaCha20-Poly1305 under an Argon2id-derived key. Pure Rust, no build
system change, both platforms identical. Rejected because it leaks
metadata — item count, folder tree shape, folder-vs-snippet, timestamps,
approximate plaintext lengths — and because it is a hand-assembled
composition where SQLCipher is a reviewed artefact. Recorded here because
it remains the fallback if the OpenSSL build cost proves unacceptable in
practice; filtering already happens in Rust (`paste_candidates`,
`main.rs:1289`) and no query orders by a text column, so the query layer
would not need to change.

**Encrypted container decrypted on unlock.** Either plaintext lands in a
temp file, defeating the purpose, or every write re-serializes the whole
database, breaking crash-safety and the single-connection model.

## 1. Build and dependencies

`crates/fastpaste-data/Cargo.toml`:

```toml
rusqlite = { version = "0.37", features = ["bundled-sqlcipher-vendored-openssl", "chrono"] }
```

Verified against the pinned versions: `rusqlite` 0.37.0 and
`libsqlite3-sys` 0.35.0 both expose this feature, and
`bundled-sqlcipher = ["bundled"]`, so the existing `bundled` feature is
implied rather than conflicting. `rusqlite` appears only in
`fastpaste-data`, so this is the only manifest affected.

New workspace dependencies: `keyring` (section 3), `secrecy`/`zeroize`
(section 3).

**CI and the Windows build.** `AGENTS.md` documents that the workspace
cannot be cross-checked past `fastpaste-platform` without mingw, because
of the C SQLite build; OpenSSL additionally needs perl. Two changes
follow, and both are part of this work rather than follow-ups:

- Add a `windows-latest` job to `.github/workflows` that builds the
  workspace and runs the test suite. A Linux-only CI no longer tells us
  whether the project builds, and this change is precisely the kind that
  breaks Windows silently.
- Update the "Checking the Windows build" section of `AGENTS.md` to
  describe what the cross-check now does and does not cover.

## 2. Storage seam (`fastpaste-data`)

A SQLCipher-linked build reads a plaintext database normally when no key
is set. That is what makes opt-in, switchable-both-ways affordable: one
binary handles both states, with no `cfg` and no second code path.

**Encryption-state probe.** `Database::encryption_state(path)` opens the
file and runs `SELECT count(*) FROM sqlite_schema`:

- success → `Plaintext`
- `SQLITE_NOTADB` → `Encrypted`
- file absent → `Absent` (new install)

Startup uses this to decide whether to prompt at all.

**Keyed open.** `Database::open` gains a key parameter
(an `Option` over the `secrecy` wrapper from section 3; `None` means
plaintext). When a key is present it is
applied with `conn.pragma_update(None, "key", …)` as the first statement
after open — SQLCipher requires the key before any other access, and
`pragma_update` handles quoting, so a passphrase containing a quote
cannot break out of the pragma.

**Wrong passphrase is a typed error.** Immediately after keying, the same
`sqlite_schema` probe runs and a failure becomes
`DataError::WrongPassphrase`. Without this, a bad passphrase surfaces
later as an opaque `NotADatabase` from inside an unrelated query. This
check sits *before* the existing schema-version pre-check, so the
`CorruptSchema` / refinery `MissingVersion` handling at
`database.rs:57-95` is untouched.

**Stock SQLCipher 4 parameters.** PBKDF2-HMAC-SHA512 at 256,000
iterations, HMAC-SHA512 page authentication, 4096-byte pages. Tuning
`kdf_iter` is an explicit non-goal: stock defaults mean the standard
`sqlcipher` CLI can open the file for recovery.

**No schema migration.** Encryption sits below the schema. `items` is
unchanged and no `V002` migration is added.

New `DataError` variants: `WrongPassphrase`, and one for "this database is
encrypted but this build has no SQLCipher support".

## 3. Key handling and the keyring

SQLCipher owns key derivation. The passphrase goes to `PRAGMA key` and
that is the entire chain — no Argon2, no separate data key, no key
wrapping.

**The keyring belongs in `fastpaste-platform`.** `AGENTS.md` requires that
nothing above that crate contains a `cfg` or names a concrete backend, and
a keyring is an OS facility. So: a `SecretStore` trait, with
`SystemSecretStore` (the `keyring` crate — Secret Service on Linux,
Credential Manager on Windows) exported under that neutral alias, and a
`NullSecretStore` for tests and for the no-daemon degraded path.

**The keyring stores the passphrase itself.** The alternative is
SQLCipher raw-key mode (`PRAGMA key = "x'…'"`, key plus salt as hex),
which would keep the passphrase off the keyring, but it complicates
`PRAGMA rekey` and the export paths in section 5 and buys nothing against
the stated threat: anyone who can read the keyring is already running as
the user. The accepted cost is that any process running as the user can
read that passphrase, and a passphrase reused elsewhere is exposed beyond
fastpaste. The UI must not imply otherwise.

**Memory hygiene is best-effort, and is described that way.** The
passphrase is carried in `secrecy`/`zeroize` types in transit, but
SQLCipher copies the key into its own allocations and Slint's text input
holds a `SharedString` outside our control. Wiping reduces exposure; it
does not guarantee absence.

## 4. Unlock lifecycle and the `main()` restructure

Today `AppContext::build()` opens the database at `main.rs:145`, before
the event loop exists. The passphrase prompt is a Slint window, and
re-entering `run_event_loop` after it returns is not reliable across
backends, so startup has to be reshaped.

**Split `AppContext::build()` in two.** A probe call takes the
single-instance guard, loads settings, and reports the encryption state —
no database opened, no devices claimed. It hands ownership of that guard
to a second call that takes the key and performs everything today's
`build()` does, unchanged. The guard is still acquired first, preserving
the ordering property `context.rs` documents.

**Pre-unlock the app shows only the unlock dialog** — no tray, no
hotkeys, no clipboard capture. This is how password-gated applications
conventionally behave, and it keeps `AppContext` at its current shape:
`db` stays a non-optional `Arc<Mutex<Database>>` and none of the many
`ctx.db.lock()` call sites in `main.rs` change. Threading an
`Option<Database>` through the context instead would ripple across the
file for no user-visible gain.

Startup becomes:

1. Probe: single-instance guard, settings, encryption state, i18n locale.
2. If `Plaintext`/`Absent`, or the keyring yields a key that opens the
   database, build the context and run today's startup path verbatim.
3. Otherwise create and show the unlock dialog before the loop starts —
   the same way the tray is created before the loop today. Its accept
   callback opens the database, builds the context, and calls the
   extracted `start_app(ctx)` holding everything that follows the
   composition root today.

**Prerequisite spike — resolve before building on this.** `AGENTS.md`
records that a visible tray keeps `run_event_loop` alive, which is why the
no-tray path uses `run_event_loop_until_quit`. Under this restructure the
tray is built inside a callback, after the loop has started, so that
choice cannot be made up front. The intended resolution is to always use
`run_event_loop_until_quit` and have the no-tray path call
`quit_event_loop()` explicitly when the main window closes. This is a
contained change to an already-delicate, already-documented quirk, but it
is an assumption about Slint's loop lifetime rather than a verified fact.
Prove it with a throwaway spike before implementing section 4; if it does
not hold, the fallback is to keep the tray built pre-loop and gate its
DB-dependent handlers on the locked state.

**Spike result (2026-09-02) — Confirmed, on the actual construct.** A first
pass at this spike used a plain `Window` for every probe and inferred, by
extension, that a tray icon would behave the same way — review correctly
rejected that inference, because `FastpasteTray` inherits `SystemTrayIcon`
(`crates/fastpaste-gui/ui/tray_icon.slint:26`), a different Slint element
with its own platform registration path. The spike was corrected to
construct the app's real `FastpasteTray` component (via
`slint::include_modules!()`, the same mechanism `main.rs` uses) directly, in
place of the inference.

A throwaway example (`crates/fastpaste-gui/examples/loop_spike.rs`, deleted
after the run) created a plain window and showed it before calling
`slint::run_event_loop_until_quit()`; then, from a `slint::Timer` callback
firing after the loop was already running, it called `FastpasteTray::new()`
followed by `.show()`, then separately created and showed a second plain
window, hid the first, and confirmed the loop was still alive before calling
`slint::quit_event_loop()`. Run twice against the live Wayland session on the
development machine — which has a KDE `StatusNotifierWatcher` /
`StatusNotifierHost` running, so a genuine tray daemon was present — on
Slint 1.17.1; both runs printed all six PROBE lines in order and exited 0:

```
PROBE 1: window created before the loop
PROBE 2: shown before the loop
PROBE 3: FastpasteTray (SystemTrayIcon) created + shown from inside the loop
PROBE 4: second window created + shown from inside the loop
PROBE 5: loop still alive after the first window closed
PROBE 6: loop exited via quit_event_loop
```

`FastpasteTray::new()` and `.show()` both succeeded from inside the
post-loop-start callback (PROBE 3), and the loop continued running
afterward and shut down cleanly on `quit_event_loop()` (PROBE 4-6). The
assumption holds on direct observation of the real tray component, not by
extension from a plain window: `SystemTrayIcon`-rooted components can be
constructed and shown from a callback firing after the loop has started, on
this platform. Task 9 proceeds as written: always call
`run_event_loop_until_quit`, with the no-tray path calling `quit_event_loop()`
explicitly from the main window's close handler.

## 5. Encrypt, decrypt, change passphrase

The Options dialog gains a Security page: a state line, then *Encrypt
database…* / *Change passphrase…* / *Remove encryption…* as the current
state allows, plus a *Remember passphrase in system keyring* checkbox,
disabled with a stated reason when no `SecretStore` is available.

SQLCipher cannot encrypt a file in place. Both conversions use the
documented export path — `ATTACH DATABASE … KEY '<pass>'`, then
`SELECT sqlcipher_export(…)`, then `DETACH` — with an empty key meaning
plaintext on the way back out. `sqlcipher_export` copies the entire
schema including `refinery_schema_history`, so migration state survives
the round trip.

Changing the passphrase is different and much cheaper: `PRAGMA rekey`,
in place, no export.

The live connection is replaced under the existing mutex
(`*guard = new_db`), so `AppContext` needs no type change. The
single-instance guard means no other process is racing the conversion.

**Atomicity.** Export to `fastpaste.sqlite.new` beside the original,
fsync, verify it opens under the new key and its row count matches, then
`rename()` over the original — same filesystem, so the rename is atomic. A
crash mid-conversion leaves the original intact and an orphaned `.new`
file, which the next launch deletes.

**The plaintext original is deleted, not kept as a `.bak`.** A backup
would preserve exactly the readable file this feature exists to eliminate.

**Stated limitation, surfaced in the UI.** On a journaling or
copy-on-write filesystem, or any SSD, `unlink` does not erase the previous
contents; an attacker with the raw device may still recover the
pre-encryption plaintext. Encryption protects everything from that point
forward and cannot retroactively unwrite what was already on disk. Users
converting an existing library need to know this before they rely on it.

## 6. Failure modes

Extends the degraded-modes table in `README.md`:

| Situation | Effect |
|---|---|
| No Secret Service / Credential Manager | *Remember* disabled with a stated reason; passphrase prompted every launch |
| Keyring holds a stale passphrase | Open fails **with `WrongPassphrase`**, the stale entry is cleared, the prompt is shown — not a hard error. A failed open for any other reason (a corrupt schema from a newer build, a fatal platform-backend failure, an I/O fault) does *not* mean the passphrase was wrong, and must not be treated as a stale entry: the keyring is left untouched and the real error is surfaced instead |
| Wrong passphrase at the prompt | Inline error, retry. No lockout counter: an attacker holding the file ignores our UI, so a counter only punishes the legitimate user |
| Prompt cancelled | Clean exit; the single-instance guard is released. The same applies to the window-close gesture (title-bar close, Alt+F4) — it is not a distinct case from Cancel/Escape, and must not leave the process running with the dialog hidden and the guard still held |
| Encrypted database, build without SQLCipher | Reported as "encrypted database, this build lacks SQLCipher support", not as corrupt schema |
| Crash mid-conversion | Original intact; orphaned `.new` removed on next launch |
| Forgotten passphrase | Unrecoverable. Stated plainly where the passphrase is set, not only in documentation |

## 7. Testing

`fastpaste-data` carries most of it, with `tempfile` and no display:

- keyed open, then reopen with the same key
- wrong key yields `DataError::WrongPassphrase`
- `encryption_state` for each of plaintext, encrypted, absent
- encrypt → verify contents and row count → decrypt round trip
- `PRAGMA rekey` changes the passphrase and the old one stops working
- migrations run under a key
- conversion preserves `refinery_schema_history`
- `load_all_lenient` behaviour is unchanged

`fastpaste-platform`: `NullSecretStore`, plus a deliberately failing fake
covering the no-daemon degraded path.

`fastpaste-gui`: `ui_preview` gains an `unlock` mode and the Security
options page, following the convention `README.md` documents for
reviewing UI without a compositor.

CI: the `windows-latest` job from section 1.

## Acceptance criteria

1. A fresh install is unencrypted and behaves exactly as it does today.
2. *Encrypt database…* converts in place, and afterwards the file cannot
   be read by the plain `sqlite3` CLI while every snippet is intact in the
   app.
3. With *Remember* off, launching prompts for the passphrase; a correct
   one starts the app fully, a wrong one re-prompts, and cancelling exits
   cleanly.
4. With *Remember* on, launching is silent.
5. *Change passphrase…* invalidates the old passphrase and updates the
   keyring entry.
6. *Remove encryption…* requires the current passphrase and restores a
   file the `sqlite3` CLI can read.
7. Killing the app mid-conversion leaves a database that still opens.
   This holds unconditionally for *Encrypt* and *Remove encryption*: both
   are export-then-atomic-rename (section 5), so a crash at any point
   before the rename leaves the original untouched and only an orphaned
   `.new` behind, which the next launch removes. *Change passphrase…* is
   different — it is an in-place `PRAGMA rekey` with no export and no
   rename to roll back to, so its crash-safety rests entirely on
   SQLCipher's own journal, not on the rollback mechanism described here.
8. The workspace builds and its tests pass on both Linux and Windows CI.
