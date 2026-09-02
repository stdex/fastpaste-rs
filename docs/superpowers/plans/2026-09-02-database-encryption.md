# Database Encryption at Rest Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give fastpaste opt-in, switchable SQLCipher whole-file database encryption, unlocked by a passphrase that can optionally be remembered in the OS keyring.

**Architecture:** `rusqlite` links SQLCipher instead of plain bundled SQLite, so one binary reads both plaintext and encrypted databases. `fastpaste-data` gains an encryption-state probe, a keyed open, and a conversion module. `fastpaste-platform` gains a `SecretStore` trait for the keyring, following the existing trait-plus-neutral-alias rule. `AppContext::build` splits into a probe (single-instance guard + settings + encryption state, no database) and a keyed build, so `main()` can show an unlock dialog before opening the database while `AppContext` itself keeps its current shape.

**Tech Stack:** Rust 1.95 / edition 2024, rusqlite 0.37 with `bundled-sqlcipher-vendored-openssl`, refinery 0.9, Slint 1.17, `keyring` 3, `secrecy` 0.10.

**Spec:** `docs/superpowers/specs/2026-09-02-database-encryption-design.md`

## Global Constraints

- Rust 1.95+, edition 2024. Workspace deps go in the root `Cargo.toml` `[workspace.dependencies]` and are referenced as `foo.workspace = true`.
- **Nothing above `fastpaste-platform` may contain a `cfg`** or name a concrete platform backend. Platform-specific deps go in a `[target.'cfg(...)'.dependencies]` section, never the common one.
- `cargo clippy --workspace --all-targets -- -D warnings` must pass. CI sets `RUSTFLAGS: "-D warnings"`.
- `cargo fmt --all` must leave the tree clean.
- Stock SQLCipher 4 parameters only. Do **not** set `kdf_iter`, `cipher_page_size`, or any other cipher pragma — stock defaults keep the file readable by the standard `sqlcipher` CLI for recovery.
- Recursive CTEs use `UNION`, never `UNION ALL`.
- User-visible strings live in `crates/fastpaste-gui/ui/translations.slint` with matching keys in all five files under `crates/fastpaste-gui/i18n/` (`en`, `ru`, `de`, `es`, `zh_CN`). Slint property names are kebab-case; the Rust setters are snake_case.
- Every new settings field carries `#[serde(default)]` or an explicit `default = "fn"`.
- Never write a plaintext backup of a database that is being encrypted.

---

### Task 1: Spike — does the tray/event-loop restructure hold?

**This task produces an answer, not code.** The spec flags it as a prerequisite: everything in Task 9 rests on an assumption about Slint's event-loop lifetime that has not been verified. Do this first and throw the code away.

**Files:**
- Create (throwaway, deleted at the end): `crates/fastpaste-gui/examples/loop_spike.rs`

**Interfaces:**
- Consumes: nothing.
- Produces: a written answer that Task 9 depends on. No code.

**Background.** `main.rs:265-281` chooses between `slint::run_event_loop()` (when a tray exists — the tray keeps the loop alive with no window open) and `slint::run_event_loop_until_quit()` (when it does not). Task 9 moves tray construction *inside* a dialog callback, after the loop has started, so that choice can no longer be made up front. The intended resolution is to always call `run_event_loop_until_quit()`.

- [ ] **Step 1: Write the spike**

```rust
// crates/fastpaste-gui/examples/loop_spike.rs
//! THROWAWAY. Answers one question for the encryption plan:
//! can a window be created and shown before `run_event_loop_until_quit`,
//! and can a *second* window plus a tray icon be created from inside a
//! callback after the loop is already running?
//!
//! Run: cargo run --example loop_spike
//! Needs a compositor. Expected: prints all four PROBE lines, then exits.

slint::slint! {
    export component Probe inherits Window {
        width: 300px;
        height: 120px;
        callback go();
        Text { text: "spike"; }
        TouchArea { clicked => { root.go(); } }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let first = Probe::new()?;
    println!("PROBE 1: window created before the loop");
    first.show()?;
    println!("PROBE 2: shown before the loop");

    // Simulate the unlock dialog's accept callback firing once the loop
    // is up: build a second window from inside the loop, then quit.
    let weak = first.as_weak();
    slint::Timer::single_shot(std::time::Duration::from_millis(500), move || {
        let second = Probe::new().expect("create a window from inside the loop");
        second.show().expect("show a window from inside the loop");
        println!("PROBE 3: second window created + shown from inside the loop");
        if let Some(w) = weak.upgrade() {
            let _ = w.hide();
        }
        // The first window is now hidden and only the second remains.
        // If `run_event_loop_until_quit` is doing its job, the loop is
        // still running here and only `quit_event_loop` ends it.
        slint::Timer::single_shot(std::time::Duration::from_millis(500), move || {
            println!("PROBE 4: loop still alive after the first window closed");
            let _ = second.hide();
            let _ = slint::quit_event_loop();
        });
    });

    slint::run_event_loop_until_quit()?;
    println!("PROBE 5: loop exited via quit_event_loop");
    Ok(())
}
```

- [ ] **Step 2: Run it**

Run: `cargo run --example loop_spike`
Expected: PROBE 1 through PROBE 5 all print, in order, and the process exits 0.

- [ ] **Step 3: Record the answer**

Append a short section to the spec at `docs/superpowers/specs/2026-09-02-database-encryption-design.md`, immediately after the "Prerequisite spike" paragraph in section 4, stating what actually happened. Write one of:

- **Confirmed** — all five probes printed. Task 9 proceeds as written: always `run_event_loop_until_quit`, and the no-tray path calls `quit_event_loop()` from the main window's close handler.
- **Refuted** — say which probe failed and what it did instead. Task 9 then uses the documented fallback: keep tray construction before the loop, keep the existing `has_tray()` branch untouched, and gate the tray's DB-dependent menu handlers on a locked-state flag so they no-op until unlock completes.

- [ ] **Step 4: Delete the spike**

```bash
rm crates/fastpaste-gui/examples/loop_spike.rs
```

- [ ] **Step 5: Commit the answer**

```bash
git add docs/superpowers/specs/2026-09-02-database-encryption-design.md
git commit -m "Record the event-loop spike result in the encryption design"
```

---

### Task 2: Link SQLCipher, and make the Windows build visible

**Files:**
- Modify: `crates/fastpaste-data/Cargo.toml:8`
- Modify: `Cargo.toml` (workspace deps — add `secrecy`)
- Modify: `.github/workflows/ci.yml`
- Modify: `AGENTS.md` ("Checking the Windows build" section, around line 43)

**Interfaces:**
- Consumes: nothing.
- Produces: a workspace that links SQLCipher. `PRAGMA cipher_version` returns a non-empty string at runtime. `secrecy::SecretString` is available to later tasks.

- [ ] **Step 1: Write the failing test**

Add to the `mod tests` block at the bottom of `crates/fastpaste-data/src/database.rs`:

```rust
    /// The whole feature rests on this: a plain `bundled` build answers
    /// `PRAGMA cipher_version` with nothing at all. If this ever goes
    /// quiet again, every database in the field silently stops being
    /// encrypted, so it is worth one test.
    #[test]
    fn the_build_actually_links_sqlcipher() {
        let dir = TempDir::new().unwrap();
        let db = Database::open(&dir.path().join("t.sqlite"), false).unwrap();
        let version: Option<String> = db
            .conn
            .query_row("PRAGMA cipher_version", [], |r| r.get(0))
            .optional()
            .unwrap();
        let version = version.unwrap_or_default();
        assert!(
            !version.is_empty(),
            "no SQLCipher in this build: PRAGMA cipher_version returned {version:?}"
        );
    }
```

If `OptionalExtension` is not already imported in the test module, add `use rusqlite::OptionalExtension;` at the top of `mod tests`.

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p fastpaste-data the_build_actually_links_sqlcipher`
Expected: FAIL — `PRAGMA cipher_version` returns no row on a plain SQLite build, so the assertion trips on an empty string.

- [ ] **Step 3: Swap the feature**

In `crates/fastpaste-data/Cargo.toml`, replace line 8:

```toml
# SQLCipher, not plain SQLite: whole-file page encryption, keyed with
# `PRAGMA key`. `bundled-sqlcipher` implies `bundled`, so this replaces
# rather than joins the old feature. `-vendored-openssl` builds OpenSSL
# from source, which is what keeps this working on a machine that has no
# system libcrypto — at the cost of a slower cold build.
rusqlite = { version = "0.37", features = ["bundled-sqlcipher-vendored-openssl", "chrono"] }
```

Add to `[workspace.dependencies]` in the root `Cargo.toml`:

```toml
secrecy = "0.10"
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p fastpaste-data the_build_actually_links_sqlcipher`
Expected: PASS. The first build is slow — OpenSSL compiles from source.

- [ ] **Step 5: Confirm nothing else broke**

Run: `cargo test --workspace`
Expected: PASS. Every existing `Database` test still passes, because an unkeyed SQLCipher connection behaves exactly like SQLite.

- [ ] **Step 6: Add the Windows CI job**

Append to `.github/workflows/ci.yml`:

```yaml
  windows:
    name: windows build + test
    runs-on: windows-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      # openssl-src assembles OpenSSL with NASM on Windows. Strawberry
      # Perl is already on the runner image; NASM is not.
      - name: Install NASM
        run: choco install nasm -y
      - name: Add NASM to PATH
        run: echo "C:\Program Files\NASM" >> $env:GITHUB_PATH
      - uses: Swatinem/rust-cache@v2
      - name: build
        run: cargo build --workspace
      - name: test
        run: cargo test --workspace
```

If the OpenSSL build fails on the runner despite NASM, set `OPENSSL_NO_ASM: 1` in that job's `env:` — slower crypto, but it builds without an assembler.

- [ ] **Step 7: Update the Windows note in AGENTS.md**

Replace the "Checking the Windows build" section body with:

```markdown
```
rustup target add x86_64-pc-windows-gnu
cargo check --target x86_64-pc-windows-gnu -p fastpaste-platform
```

That covers the crate where the platform code lives, and it is worth
running for any change to it. The rest of the workspace **cannot** be
checked this way without a C cross-compiler and a working OpenSSL
build: `rusqlite`'s `bundled-sqlcipher-vendored-openssl` feature builds
both SQLite and OpenSSL from source, so `-p fastpaste-data` and
anything downstream needs mingw (`x86_64-w64-mingw32-gcc`) plus perl
and NASM.

In practice, do not try. The `windows` job in `.github/workflows/ci.yml`
builds and tests the whole workspace on a real Windows runner, and that
is the check that counts. Nothing Windows can be *run* here at all.
```

- [ ] **Step 8: Commit**

```bash
git add Cargo.toml Cargo.lock crates/fastpaste-data/Cargo.toml crates/fastpaste-data/src/database.rs .github/workflows/ci.yml AGENTS.md
git commit -m "Link SQLCipher and test the whole workspace on Windows CI"
```

---

### Task 3: Report whether a database file is encrypted

**Files:**
- Create: `crates/fastpaste-data/src/crypto.rs`
- Modify: `crates/fastpaste-data/src/lib.rs`
- Modify: `crates/fastpaste-data/src/error.rs`

**Interfaces:**
- Consumes: SQLCipher linkage from Task 2.
- Produces:
  - `pub enum EncryptionState { Absent, Plaintext, Encrypted }` (derives `Debug, Clone, Copy, PartialEq, Eq`)
  - `pub fn Database::encryption_state(path: &Path) -> Result<EncryptionState, DataError>`
  - `DataError::EncryptedButUnsupported`
  - `pub(crate) fn crypto::is_not_a_database(e: &rusqlite::Error) -> bool`

- [ ] **Step 1: Write the failing test**

Create `crates/fastpaste-data/src/crypto.rs` with only a test module for now:

```rust
//! SQLCipher key application and whole-file conversion.
//!
//! Everything that knows a passphrase exists lives here. `database.rs`
//! calls [`apply_key`] and otherwise stays ignorant of encryption.

#[cfg(test)]
mod tests {
    use crate::{Database, EncryptionState};
    use tempfile::TempDir;

    #[test]
    fn encryption_state_reports_absent_and_plaintext() {
        let dir = TempDir::new().unwrap();

        let missing = dir.path().join("nope.sqlite");
        assert_eq!(
            Database::encryption_state(&missing).unwrap(),
            EncryptionState::Absent,
            "a file that is not there is a new install, not an error"
        );

        let plain = dir.path().join("plain.sqlite");
        drop(Database::open(&plain, false).unwrap());
        assert_eq!(
            Database::encryption_state(&plain).unwrap(),
            EncryptionState::Plaintext
        );
    }
}
```

The `Encrypted` arm is asserted in Task 4, once there is a way to make an encrypted file.

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p fastpaste-data encryption_state_reports_absent_and_plaintext`
Expected: FAIL to compile — `crypto` is not declared as a module, and neither `EncryptionState` nor `Database::encryption_state` exists.

- [ ] **Step 3: Write the implementation**

Add to `crates/fastpaste-data/src/error.rs`, inside `enum DataError`:

```rust
    /// The file is encrypted and this build cannot open it. Only reachable
    /// from a build that dropped the SQLCipher feature; reported
    /// separately so it does not masquerade as corruption.
    #[error("database is encrypted, but this build has no SQLCipher support")]
    EncryptedButUnsupported,
```

Add to the top of `crates/fastpaste-data/src/crypto.rs` (above the test module):

```rust
use std::path::Path;

use rusqlite::{Connection, OptionalExtension};

use crate::error::DataError;

/// Is this the "file header did not decrypt" error? SQLCipher reports a
/// wrong key exactly the same way it reports a file that was never a
/// database at all, because from its side those are the same observation.
pub(crate) fn is_not_a_database(e: &rusqlite::Error) -> bool {
    matches!(
        e,
        rusqlite::Error::SqliteFailure(inner, _)
            if inner.code == rusqlite::ErrorCode::NotADatabase
    )
}

/// Does this build link SQLCipher? A plain SQLite build answers
/// `PRAGMA cipher_version` with no row at all.
pub(crate) fn has_sqlcipher(conn: &Connection) -> bool {
    conn.query_row("PRAGMA cipher_version", [], |r| r.get::<_, String>(0))
        .optional()
        .ok()
        .flatten()
        .is_some_and(|v| !v.is_empty())
}

/// Touch the schema. This is the cheapest operation that forces SQLCipher
/// to decrypt page 1, which is what turns a wrong key into an error at
/// open time rather than midway through some unrelated query later.
pub(crate) fn probe_readable(conn: &Connection) -> Result<(), rusqlite::Error> {
    conn.query_row("SELECT count(*) FROM sqlite_schema", [], |r| {
        r.get::<_, i64>(0)
    })
    .map(|_| ())
}

/// Whether the database at `path` is encrypted, plaintext, or not there.
pub(crate) fn encryption_state(path: &Path) -> Result<EncryptionState, DataError> {
    if !path.exists() {
        return Ok(EncryptionState::Absent);
    }
    let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    match probe_readable(&conn) {
        Ok(()) => Ok(EncryptionState::Plaintext),
        Err(e) if is_not_a_database(&e) => {
            if has_sqlcipher(&conn) {
                Ok(EncryptionState::Encrypted)
            } else {
                Err(DataError::EncryptedButUnsupported)
            }
        }
        Err(e) => Err(DataError::from(e)),
    }
}

/// Whether a database file is readable as-is, needs a key, or is absent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncryptionState {
    /// No file yet — a new install.
    Absent,
    /// Readable with no key.
    Plaintext,
    /// Needs a passphrase.
    Encrypted,
}
```

Add the public method to `impl Database` in `crates/fastpaste-data/src/database.rs`, next to `open`:

```rust
    /// Whether the database at `path` needs a passphrase. Startup calls
    /// this before deciding whether to prompt.
    pub fn encryption_state(path: &Path) -> Result<EncryptionState, DataError> {
        crate::crypto::encryption_state(path)
    }
```

Add `use crate::crypto::EncryptionState;` to the imports at the top of `database.rs`.

Wire the module up in `crates/fastpaste-data/src/lib.rs`:

```rust
pub mod crypto;
pub mod database;
pub mod error;
pub mod item;

pub use crypto::EncryptionState;
pub use database::Database;
pub use error::DataError;
pub use item::{HISTORY_FOLDER_ID, HistoryPosition, Item, ItemKind};
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p fastpaste-data encryption_state_reports_absent_and_plaintext`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/fastpaste-data/src/crypto.rs crates/fastpaste-data/src/lib.rs crates/fastpaste-data/src/error.rs crates/fastpaste-data/src/database.rs
git commit -m "Report whether a database file is encrypted"
```

---

### Task 4: Open a database with a passphrase

**Files:**
- Modify: `crates/fastpaste-data/src/crypto.rs`
- Modify: `crates/fastpaste-data/src/database.rs:36-102` (the `open` function)
- Modify: `crates/fastpaste-data/src/error.rs`
- Modify: `crates/fastpaste-data/Cargo.toml`

**Interfaces:**
- Consumes: `crypto::{is_not_a_database, probe_readable, EncryptionState}` from Task 3.
- Produces:
  - `pub fn Database::open_with_key(path: &Path, read_only: bool, key: Option<&SecretString>) -> Result<Self, DataError>`
  - `Database::open(path, read_only)` retained as a wrapper delegating with `None` — **do not change its signature**, roughly forty existing call sites and tests depend on it
  - `pub(crate) fn crypto::apply_key(conn: &Connection, key: Option<&SecretString>) -> Result<(), DataError>`
  - `DataError::WrongPassphrase`

- [ ] **Step 1: Write the failing tests**

Add to the `mod tests` block in `crates/fastpaste-data/src/crypto.rs`:

```rust
    use crate::{DataError, Item};
    use secrecy::SecretString;

    fn pass(s: &str) -> SecretString {
        SecretString::from(s.to_string())
    }

    #[test]
    fn a_keyed_database_round_trips_with_the_same_passphrase() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("enc.sqlite");
        let p = pass("correct horse battery staple");

        {
            let db = Database::open_with_key(&path, false, Some(&p)).unwrap();
            let mut item = Item::new_plain(0, "Greeting", "Hello, world!");
            db.insert(&mut item).unwrap();
        }

        let db = Database::open_with_key(&path, false, Some(&p)).unwrap();
        let loaded = db.load_all().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].body_plain, "Hello, world!");
    }

    #[test]
    fn the_wrong_passphrase_is_reported_as_such() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("enc.sqlite");

        {
            let db = Database::open_with_key(&path, false, Some(&pass("right"))).unwrap();
            let mut item = Item::new_plain(0, "T", "B");
            db.insert(&mut item).unwrap();
        }

        let err = Database::open_with_key(&path, false, Some(&pass("wrong"))).unwrap_err();
        assert!(
            matches!(err, DataError::WrongPassphrase),
            "expected WrongPassphrase, got {err:?}"
        );
    }

    #[test]
    fn an_encrypted_database_will_not_open_unkeyed() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("enc.sqlite");

        {
            let db = Database::open_with_key(&path, false, Some(&pass("s3cret"))).unwrap();
            let mut item = Item::new_plain(0, "T", "B");
            db.insert(&mut item).unwrap();
        }

        let err = Database::open(&path, false).unwrap_err();
        assert!(
            matches!(err, DataError::WrongPassphrase),
            "expected WrongPassphrase, got {err:?}"
        );
    }

    #[test]
    fn encryption_state_detects_an_encrypted_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("enc.sqlite");
        {
            let db = Database::open_with_key(&path, false, Some(&pass("s3cret"))).unwrap();
            let mut item = Item::new_plain(0, "T", "B");
            db.insert(&mut item).unwrap();
        }
        assert_eq!(
            Database::encryption_state(&path).unwrap(),
            EncryptionState::Encrypted
        );
    }

    #[test]
    fn migrations_run_under_a_key() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("enc.sqlite");
        let db = Database::open_with_key(&path, false, Some(&pass("s3cret"))).unwrap();
        assert_eq!(db.schema_version().unwrap(), Some(1));
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p fastpaste-data --lib crypto::`
Expected: FAIL to compile — `Database::open_with_key`, `DataError::WrongPassphrase` and the `secrecy` dependency do not exist yet.

- [ ] **Step 3: Write the implementation**

Add `secrecy` to `crates/fastpaste-data/Cargo.toml` under `[dependencies]`:

```toml
secrecy.workspace = true
```

Add to `enum DataError` in `crates/fastpaste-data/src/error.rs`:

```rust
    /// The supplied passphrase did not decrypt the file — or no passphrase
    /// was supplied for a file that needs one. Indistinguishable from
    /// "this was never a database", which is why the message says both.
    #[error("wrong passphrase, or not a fastpaste database")]
    WrongPassphrase,
```

Add to `crates/fastpaste-data/src/crypto.rs`, above the test module:

```rust
use secrecy::{ExposeSecret, SecretString};

/// Key `conn`, then prove the key is right.
///
/// `PRAGMA key` has to be the first statement on the connection —
/// SQLCipher will not accept it once anything else has touched the file.
/// It also succeeds unconditionally: a wrong passphrase is only revealed
/// by trying to read a page, which is what [`probe_readable`] is for.
/// Without that probe a bad passphrase surfaces much later as an opaque
/// `NotADatabase` from inside some unrelated query.
///
/// `pragma_update` binds the value rather than formatting it, so a
/// passphrase containing a quote cannot break out of the pragma.
pub(crate) fn apply_key(conn: &Connection, key: Option<&SecretString>) -> Result<(), DataError> {
    if let Some(key) = key {
        conn.pragma_update(None, "key", key.expose_secret())?;
    }
    match probe_readable(conn) {
        Ok(()) => Ok(()),
        Err(e) if is_not_a_database(&e) => {
            if has_sqlcipher(conn) {
                Err(DataError::WrongPassphrase)
            } else {
                Err(DataError::EncryptedButUnsupported)
            }
        }
        Err(e) => Err(DataError::from(e)),
    }
}

/// Open a connection at `path` and key it. Shared by [`crate::Database`]
/// and by the conversion helpers.
pub(crate) fn open_keyed(
    path: &Path,
    read_only: bool,
    key: Option<&SecretString>,
) -> Result<Connection, DataError> {
    let flags = if read_only {
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
    } else {
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_CREATE
    };
    let conn = Connection::open_with_flags(path, flags)?;
    apply_key(&conn, key)?;
    Ok(conn)
}
```

In `crates/fastpaste-data/src/database.rs`, rename the existing `open` to `open_with_key`, give it the extra parameter, key the connection immediately after opening, and add a thin `open` wrapper. The rest of the function body — the version pre-check, the migration runner and its `MissingVersion` handling, the post-migration re-check — is unchanged:

```rust
    /// Open (and create if necessary) the database at `path`. Equivalent to
    /// [`Self::open_with_key`] with no key: for a plaintext database, or
    /// for creating one.
    pub fn open(path: &Path, read_only: bool) -> Result<Self, DataError> {
        Self::open_with_key(path, read_only, None)
    }

    /// Open (and create if necessary) the database at `path`, decrypting
    /// with `key`. When `read_only` is true and the file doesn't exist,
    /// returns `DataError::NotFound`. A key that does not decrypt the file
    /// returns `DataError::WrongPassphrase`.
    ///
    /// Note this touches the filesystem (`create_dir_all`) despite the
    /// crate's "no I/O beyond SQLite" rule. It is a deliberate convenience
    /// so a caller cannot get a confusing SQLite "unable to open" for a
    /// missing directory; the app layer creates the same directory first.
    pub fn open_with_key(
        path: &Path,
        read_only: bool,
        key: Option<&secrecy::SecretString>,
    ) -> Result<Self, DataError> {
        if read_only && !path.exists() {
            return Err(DataError::NotFound(path.to_path_buf()));
        }
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }

        let flags = if read_only {
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
        } else {
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_CREATE
        };
        let mut conn = Connection::open_with_flags(path, flags)?;

        // Before anything else touches the connection: SQLCipher requires
        // `PRAGMA key` first, and a wrong key must fail here rather than
        // inside the migration runner, whose errors describe schema
        // problems and would send the reader somewhere useless.
        crate::crypto::apply_key(&conn, key)?;

        // ... the rest of the existing body, verbatim, from the
        // `let expected = expected_schema_version();` line onward.
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p fastpaste-data`
Expected: PASS — the five new tests plus every pre-existing one, which still call the two-argument `open`.

- [ ] **Step 5: Commit**

```bash
git add crates/fastpaste-data/
git commit -m "Open a database with a passphrase"
```

---

### Task 5: Encrypt, decrypt, and change the passphrase

**Files:**
- Modify: `crates/fastpaste-data/src/crypto.rs`
- Modify: `crates/fastpaste-data/src/error.rs`
- Modify: `crates/fastpaste-data/src/lib.rs`

**Interfaces:**
- Consumes: `crypto::{apply_key, open_keyed}` from Task 4.
- Produces:
  - `pub fn crypto::encrypt_database(path: &Path, passphrase: &SecretString) -> Result<(), DataError>`
  - `pub fn crypto::decrypt_database(path: &Path, passphrase: &SecretString) -> Result<(), DataError>`
  - `pub fn crypto::change_passphrase(path: &Path, current: &SecretString, new: &SecretString) -> Result<(), DataError>`
  - `pub fn crypto::clean_orphaned_conversion(path: &Path) -> Result<(), DataError>`
  - `DataError::ConversionMismatch { expected: i64, got: i64 }`

**Caller contract:** every one of these needs exclusive access to the file. The caller must drop its `Database` first — Task 10 does this under the existing `Arc<Mutex<Database>>`, and the single-instance guard rules out a second process.

- [ ] **Step 1: Write the failing tests**

Add to the `mod tests` block in `crates/fastpaste-data/src/crypto.rs`:

```rust
    /// Seed a plaintext database with a folder and two snippets, and
    /// return the path. Used by the conversion tests.
    fn seeded(dir: &TempDir, name: &str) -> std::path::PathBuf {
        let path = dir.path().join(name);
        let db = Database::open(&path, false).unwrap();
        let mut folder = Item::new_folder(0, "Work");
        db.insert(&mut folder).unwrap();
        let mut a = Item::new_plain(folder.id.unwrap(), "Token", "abc123");
        db.insert(&mut a).unwrap();
        let mut b = Item::new_plain(0, "Address", "1 High St");
        db.insert(&mut b).unwrap();
        path
    }

    #[test]
    fn encrypting_preserves_every_item_and_the_migration_history() {
        let dir = TempDir::new().unwrap();
        let path = seeded(&dir, "lib.sqlite");
        let p = pass("s3cret");

        encrypt_database(&path, &p).unwrap();

        assert_eq!(
            Database::encryption_state(&path).unwrap(),
            EncryptionState::Encrypted
        );
        let db = Database::open_with_key(&path, false, Some(&p)).unwrap();
        let loaded = db.load_all().unwrap();
        assert_eq!(loaded.len(), 3);
        assert_eq!(loaded[1].body_plain, "abc123");
        assert_eq!(
            db.schema_version().unwrap(),
            Some(1),
            "sqlcipher_export must carry refinery_schema_history across"
        );
    }

    #[test]
    fn decrypting_restores_a_plaintext_file() {
        let dir = TempDir::new().unwrap();
        let path = seeded(&dir, "lib.sqlite");
        let p = pass("s3cret");

        encrypt_database(&path, &p).unwrap();
        decrypt_database(&path, &p).unwrap();

        assert_eq!(
            Database::encryption_state(&path).unwrap(),
            EncryptionState::Plaintext
        );
        let db = Database::open(&path, false).unwrap();
        assert_eq!(db.load_all().unwrap().len(), 3);
    }

    #[test]
    fn changing_the_passphrase_invalidates_the_old_one() {
        let dir = TempDir::new().unwrap();
        let path = seeded(&dir, "lib.sqlite");
        let old = pass("old one");
        let new = pass("new one");

        encrypt_database(&path, &old).unwrap();
        change_passphrase(&path, &old, &new).unwrap();

        let err = Database::open_with_key(&path, false, Some(&old)).unwrap_err();
        assert!(matches!(err, DataError::WrongPassphrase));

        let db = Database::open_with_key(&path, false, Some(&new)).unwrap();
        assert_eq!(db.load_all().unwrap().len(), 3);
    }

    #[test]
    fn changing_the_passphrase_needs_the_current_one() {
        let dir = TempDir::new().unwrap();
        let path = seeded(&dir, "lib.sqlite");
        encrypt_database(&path, &pass("right")).unwrap();

        let err = change_passphrase(&path, &pass("guess"), &pass("new")).unwrap_err();
        assert!(matches!(err, DataError::WrongPassphrase));
    }

    /// A crash between the export and the rename leaves a `.new` file. It
    /// must never be mistaken for the real database, and the next launch
    /// clears it.
    #[test]
    fn an_orphaned_conversion_file_is_cleaned_up() {
        let dir = TempDir::new().unwrap();
        let path = seeded(&dir, "lib.sqlite");
        let orphan = path.with_extension("sqlite.new");
        std::fs::write(&orphan, b"junk").unwrap();

        clean_orphaned_conversion(&path).unwrap();

        assert!(!orphan.exists(), "the orphan must be gone");
        assert!(path.exists(), "the real database must be untouched");
        assert_eq!(Database::open(&path, false).unwrap().load_all().unwrap().len(), 3);
    }

    /// The plaintext file this feature exists to eliminate must not
    /// survive as a backup.
    #[test]
    fn encrypting_leaves_no_plaintext_copy_behind() {
        let dir = TempDir::new().unwrap();
        let path = seeded(&dir, "lib.sqlite");
        encrypt_database(&path, &pass("s3cret")).unwrap();

        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .filter(|n| n != "lib.sqlite")
            .collect();
        assert!(
            leftovers.is_empty(),
            "no copy of the database may remain: {leftovers:?}"
        );
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p fastpaste-data --lib crypto::`
Expected: FAIL to compile — `encrypt_database`, `decrypt_database`, `change_passphrase` and `clean_orphaned_conversion` do not exist.

- [ ] **Step 3: Write the implementation**

Add to `enum DataError` in `crates/fastpaste-data/src/error.rs`:

```rust
    /// A conversion produced a file with a different number of items than
    /// it started with. The original is left in place and the partial
    /// file is deleted.
    #[error("conversion lost rows (expected {expected}, got {got}); the original is untouched")]
    ConversionMismatch { expected: i64, got: i64 },
```

Add to `crates/fastpaste-data/src/crypto.rs`, above the test module:

```rust
/// Where a conversion writes before it commits by rename. Beside the
/// original, so the rename stays on one filesystem and is therefore
/// atomic.
fn conversion_temp_path(path: &Path) -> std::path::PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".new");
    std::path::PathBuf::from(name)
}

fn item_count(conn: &Connection) -> Result<i64, DataError> {
    Ok(conn.query_row("SELECT count(*) FROM items", [], |r| r.get(0))?)
}

/// Copy every page of `conn`'s main schema into a fresh database at
/// `dest`, keyed with `key` (`None` writes plaintext).
///
/// `sqlcipher_export` is the only supported way to change a database's
/// encryption: SQLCipher cannot rewrite a file in place, because the
/// header itself is part of what changes. It copies the whole schema,
/// `refinery_schema_history` included, so migration state survives.
fn export_to(conn: &Connection, dest: &Path, key: Option<&SecretString>) -> Result<(), DataError> {
    let dest_str = dest.to_string_lossy().to_string();
    // An empty key means "plaintext" to ATTACH — that is the documented
    // way back out, not an oversight.
    let key_str = key.map(|k| k.expose_secret().to_string()).unwrap_or_default();

    conn.execute(
        "ATTACH DATABASE ?1 AS target KEY ?2",
        rusqlite::params![dest_str, key_str],
    )?;
    let exported = conn.query_row("SELECT sqlcipher_export('target')", [], |_| Ok(()));
    // Detach whatever happened, so a failed export cannot leave the
    // connection holding the partial file open.
    let detached = conn.execute("DETACH DATABASE target", []);
    exported?;
    detached?;
    Ok(())
}

/// Best-effort durability before the rename that commits a conversion.
fn fsync_file(path: &Path) -> Result<(), DataError> {
    std::fs::File::open(path)?.sync_all()?;
    Ok(())
}

/// Rewrite the database at `path` from key `from` to key `to`.
///
/// Never touches the original until the replacement has been opened under
/// its new key and confirmed to hold the same number of items. A crash at
/// any point before the rename leaves the original intact and an orphaned
/// `.new` beside it, which [`clean_orphaned_conversion`] removes.
fn convert(
    path: &Path,
    from: Option<&SecretString>,
    to: Option<&SecretString>,
) -> Result<(), DataError> {
    let tmp = conversion_temp_path(path);
    // A previous crash may have left one. It is not a backup of anything.
    let _ = std::fs::remove_file(&tmp);

    let expected = {
        let src = open_keyed(path, true, from)?;
        let n = item_count(&src)?;
        export_to(&src, &tmp, to)?;
        n
    };

    {
        let dst = open_keyed(&tmp, true, to)?;
        let got = item_count(&dst)?;
        if got != expected {
            let _ = std::fs::remove_file(&tmp);
            return Err(DataError::ConversionMismatch { expected, got });
        }
    }

    fsync_file(&tmp)?;
    // Atomic: same directory, therefore same filesystem. This both
    // installs the new file and destroys the old one — deliberately. A
    // `.bak` here would preserve exactly the readable copy that
    // encrypting was meant to remove.
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Encrypt a plaintext database in place.
///
/// The caller must hold no open connection to `path`.
pub fn encrypt_database(path: &Path, passphrase: &SecretString) -> Result<(), DataError> {
    convert(path, None, Some(passphrase))
}

/// Decrypt an encrypted database back to plaintext in place.
///
/// The caller must hold no open connection to `path`.
pub fn decrypt_database(path: &Path, passphrase: &SecretString) -> Result<(), DataError> {
    convert(path, Some(passphrase), None)
}

/// Change the passphrase of an already-encrypted database.
///
/// Unlike encrypting and decrypting, this is genuinely in place:
/// `PRAGMA rekey` rewrites each page under the new key without an export.
pub fn change_passphrase(
    path: &Path,
    current: &SecretString,
    new: &SecretString,
) -> Result<(), DataError> {
    let conn = open_keyed(path, false, Some(current))?;
    conn.pragma_update(None, "rekey", new.expose_secret())?;
    Ok(())
}

/// Delete the temporary file a crashed conversion may have left behind.
/// Safe to call at every launch; a missing file is not an error.
pub fn clean_orphaned_conversion(path: &Path) -> Result<(), DataError> {
    let tmp = conversion_temp_path(path);
    match std::fs::remove_file(&tmp) {
        Ok(()) => {
            tracing::warn!("removed an orphaned conversion file: {}", tmp.display());
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(DataError::from(e)),
    }
}
```

Add `tracing.workspace = true` to `crates/fastpaste-data/Cargo.toml` under `[dependencies]` if it is not already there.

Re-export from `crates/fastpaste-data/src/lib.rs`:

```rust
pub use crypto::{
    EncryptionState, change_passphrase, clean_orphaned_conversion, decrypt_database,
    encrypt_database,
};
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p fastpaste-data`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/fastpaste-data/
git commit -m "Encrypt, decrypt, and rekey a database in place"
```

---

### Task 6: A keyring behind a platform trait

**Files:**
- Create: `crates/fastpaste-platform/src/secret_store.rs`
- Create: `crates/fastpaste-platform/src/secret_store/keyring.rs`
- Modify: `crates/fastpaste-platform/src/lib.rs`
- Modify: `crates/fastpaste-platform/Cargo.toml`
- Modify: `README.md` (degraded-modes table)

**Interfaces:**
- Consumes: `secrecy::SecretString`.
- Produces:
  - `pub trait SecretStore: Send + Sync` with `is_available()`, `get(&self, account: &str)`, `set(&self, account: &str, secret: &SecretString)`, `delete(&self, account: &str)`
  - `pub struct NullSecretStore` — a working in-memory store for tests, mirroring `NullClipboard`
  - `pub struct UnavailableSecretStore` — every operation fails; `is_available()` is false
  - `pub use secret_store::KeyringSecretStore as SystemSecretStore`
  - `pub enum SecretStoreError`

**The layering rule matters here.** `AGENTS.md` forbids a `cfg` above this crate. The `keyring` crate is itself cross-platform, so no `cfg` is needed inside the backend either — but the trait and the neutral `SystemSecretStore` alias still go in, because callers must never name `KeyringSecretStore`.

- [ ] **Step 1: Write the failing tests**

Create `crates/fastpaste-platform/src/secret_store.rs`:

```rust
//! Storing one secret in the OS credential store.
//!
//! | Alias | Linux | Windows |
//! |---|---|---|
//! | [`crate::SystemSecretStore`] | Secret Service (libsecret) | Credential Manager |
//!
//! Used to remember the database passphrase so the app can start without
//! prompting. Whether that is a good trade is the user's call, made in
//! the Options dialog — this module only carries it out.

#[cfg(test)]
mod tests {
    use super::*;
    use secrecy::{ExposeSecret, SecretString};

    #[test]
    fn the_null_store_round_trips_a_secret() {
        let store = NullSecretStore::new();
        assert!(store.is_available());
        assert!(store.get("db").unwrap().is_none(), "empty to start with");

        store.set("db", &SecretString::from("s3cret".to_string())).unwrap();
        assert_eq!(
            store.get("db").unwrap().unwrap().expose_secret(),
            "s3cret"
        );

        store.delete("db").unwrap();
        assert!(store.get("db").unwrap().is_none(), "delete must clear it");
    }

    #[test]
    fn deleting_a_secret_that_is_not_there_is_not_an_error() {
        // The stale-keyring-entry path calls delete without checking
        // first; it must not turn a missing entry into a failure.
        let store = NullSecretStore::new();
        store.delete("db").unwrap();
    }

    #[test]
    fn the_unavailable_store_reports_itself_and_fails_every_operation() {
        // Linux with no Secret Service daemon. The Options dialog reads
        // `is_available` to disable the Remember checkbox with a reason.
        let store = UnavailableSecretStore;
        assert!(!store.is_available());
        assert!(store.get("db").is_err());
        assert!(store.set("db", &SecretString::from("x".to_string())).is_err());
        assert!(store.delete("db").is_err());
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p fastpaste-platform secret_store`
Expected: FAIL to compile — the module is not declared and none of the types exist.

- [ ] **Step 3: Write the implementation**

Add to `crates/fastpaste-platform/Cargo.toml` under `[dependencies]`:

```toml
# One credential-store API over Secret Service (Linux) and Credential
# Manager (Windows), so this backend needs no cfg of its own.
keyring = { version = "3", features = ["apple-native", "windows-native", "sync-secret-service"] }
secrecy.workspace = true
```

Add to `crates/fastpaste-platform/src/secret_store.rs`, above the test module:

```rust
use secrecy::SecretString;
use thiserror::Error;

pub mod keyring;

pub use keyring::KeyringSecretStore;

#[derive(Error, Debug)]
pub enum SecretStoreError {
    #[error("no credential store available: {0}")]
    Unavailable(String),

    #[error("credential store failed: {0}")]
    Backend(String),
}

/// One named secret in the OS credential store.
///
/// Implementations must treat "no such entry" as `Ok(None)` from
/// [`Self::get`] and as `Ok(())` from [`Self::delete`]: the caller clears
/// a possibly-absent stale entry without checking first.
pub trait SecretStore: Send + Sync {
    /// Whether this store can be used at all. False on a Linux session
    /// with no Secret Service daemon, which is a degraded mode rather
    /// than a failure — the app prompts every launch instead.
    fn is_available(&self) -> bool;

    fn get(&self, account: &str) -> Result<Option<SecretString>, SecretStoreError>;
    fn set(&self, account: &str, secret: &SecretString) -> Result<(), SecretStoreError>;
    fn delete(&self, account: &str) -> Result<(), SecretStoreError>;
}

/// In-memory stand-in, for tests and for headless runs. Mirrors
/// [`crate::NullClipboard`]: it works, it just isn't the OS.
#[derive(Debug, Default)]
pub struct NullSecretStore {
    entries: std::sync::Mutex<std::collections::HashMap<String, String>>,
}

impl NullSecretStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl SecretStore for NullSecretStore {
    fn is_available(&self) -> bool {
        true
    }

    fn get(&self, account: &str) -> Result<Option<SecretString>, SecretStoreError> {
        Ok(self
            .entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(account)
            .map(|v| SecretString::from(v.clone())))
    }

    fn set(&self, account: &str, secret: &SecretString) -> Result<(), SecretStoreError> {
        use secrecy::ExposeSecret;
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(account.to_string(), secret.expose_secret().to_string());
        Ok(())
    }

    fn delete(&self, account: &str) -> Result<(), SecretStoreError> {
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(account);
        Ok(())
    }
}

/// What the app falls back to when the real store cannot be reached.
/// Every operation fails, and [`SecretStore::is_available`] says so up
/// front so the UI can explain itself instead of erroring on click.
#[derive(Debug)]
pub struct UnavailableSecretStore;

impl SecretStore for UnavailableSecretStore {
    fn is_available(&self) -> bool {
        false
    }

    fn get(&self, _account: &str) -> Result<Option<SecretString>, SecretStoreError> {
        Err(SecretStoreError::Unavailable(
            "no credential store on this session".into(),
        ))
    }

    fn set(&self, _account: &str, _secret: &SecretString) -> Result<(), SecretStoreError> {
        Err(SecretStoreError::Unavailable(
            "no credential store on this session".into(),
        ))
    }

    fn delete(&self, _account: &str) -> Result<(), SecretStoreError> {
        Err(SecretStoreError::Unavailable(
            "no credential store on this session".into(),
        ))
    }
}
```

Create `crates/fastpaste-platform/src/secret_store/keyring.rs`:

```rust
//! The real credential store, via the `keyring` crate.

use secrecy::{ExposeSecret, SecretString};

use super::{SecretStore, SecretStoreError};

/// Service name under which entries are filed. Visible to the user in
/// their keyring UI, so it is the product name and nothing more.
const SERVICE: &str = "fastpaste";

#[derive(Debug)]
pub struct KeyringSecretStore {
    available: bool,
}

impl KeyringSecretStore {
    /// Probe the store once at construction. A `get` on a nonexistent
    /// entry is the cheapest round trip that proves the daemon answers:
    /// `NoEntry` means it is there and the entry simply is not.
    pub fn new() -> Self {
        let available = match ::keyring::Entry::new(SERVICE, "__probe__") {
            Ok(entry) => !matches!(
                entry.get_password(),
                Err(::keyring::Error::PlatformFailure(_)) | Err(::keyring::Error::NoStorageAccess(_))
            ),
            Err(e) => {
                tracing::warn!("credential store unavailable: {e}");
                false
            }
        };
        Self { available }
    }

    fn entry(&self, account: &str) -> Result<::keyring::Entry, SecretStoreError> {
        ::keyring::Entry::new(SERVICE, account)
            .map_err(|e| SecretStoreError::Backend(e.to_string()))
    }
}

impl Default for KeyringSecretStore {
    fn default() -> Self {
        Self::new()
    }
}

impl SecretStore for KeyringSecretStore {
    fn is_available(&self) -> bool {
        self.available
    }

    fn get(&self, account: &str) -> Result<Option<SecretString>, SecretStoreError> {
        match self.entry(account)?.get_password() {
            Ok(p) => Ok(Some(SecretString::from(p))),
            Err(::keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(SecretStoreError::Backend(e.to_string())),
        }
    }

    fn set(&self, account: &str, secret: &SecretString) -> Result<(), SecretStoreError> {
        self.entry(account)?
            .set_password(secret.expose_secret())
            .map_err(|e| SecretStoreError::Backend(e.to_string()))
    }

    fn delete(&self, account: &str) -> Result<(), SecretStoreError> {
        match self.entry(account)?.delete_credential() {
            Ok(()) | Err(::keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(SecretStoreError::Backend(e.to_string())),
        }
    }
}
```

Wire it into `crates/fastpaste-platform/src/lib.rs` — add the module, the re-exports, and one row to the doc table:

```rust
pub mod secret_store;

pub use secret_store::{
    NullSecretStore, SecretStore, SecretStoreError, UnavailableSecretStore,
};

// ---- Platform-neutral aliases -------------------------------------------
// (alongside the existing ones; no cfg needed — the keyring crate covers
// both platforms itself)
pub use secret_store::KeyringSecretStore as SystemSecretStore;
```

In the crate doc comment's table, add:

```
//! | [`SystemSecretStore`] | Secret Service | Credential Manager |
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p fastpaste-platform secret_store`
Expected: PASS

- [ ] **Step 5: Check the Windows build of the platform crate**

Run: `cargo check --target x86_64-pc-windows-gnu -p fastpaste-platform`
Expected: PASS. This is the one crate that cross-checks cleanly, and this task is the reason to run it.

- [ ] **Step 6: Document the degraded mode**

Add a row to the "Degraded modes" table in `README.md`:

```markdown
| A credential store (Secret Service / Credential Manager) | Only matters with an encrypted database: the passphrase cannot be remembered, so it is prompted at every launch. The *Remember* checkbox is disabled and says why |
```

- [ ] **Step 7: Commit**

```bash
git add crates/fastpaste-platform/ README.md Cargo.lock
git commit -m "Add a SecretStore trait over the OS credential store"
```

---

### Task 7: Split the composition root into probe and keyed build

**Files:**
- Modify: `crates/fastpaste-app/src/context.rs:145-230` (the `build` function)
- Modify: `crates/fastpaste-app/Cargo.toml`

**Interfaces:**
- Consumes: `Database::{encryption_state, open_with_key}`, `EncryptionState`, `crypto::clean_orphaned_conversion` (Tasks 3-5); `SystemSecretStore`, `UnavailableSecretStore`, `SecretStore` (Task 6).
- Produces:
  - `pub struct StartupProbe` with public fields `encryption: EncryptionState` and `settings: Settings`, plus private `data_dir`, `db_path`, `single_instance`
  - `pub fn AppContext::probe() -> anyhow::Result<StartupProbe>`
  - `pub fn AppContext::build_unlocked(probe: StartupProbe, key: Option<SecretString>) -> anyhow::Result<AppContext>`
  - `AppContext::build()` retained, delegating with no key
  - New public `AppContext` fields: `db_path: PathBuf`, `secret_store: Arc<dyn SecretStore>`
  - `pub const PASSPHRASE_ACCOUNT: &str = "database-passphrase"`

**Ordering that must not change:** the single-instance guard is still acquired first, before the database is opened or any device claimed. `probe()` takes it and `StartupProbe` owns it until `build_unlocked` moves it into the `AppContext`.

- [ ] **Step 1: Write the failing tests**

Add to the `mod tests` block in `crates/fastpaste-app/src/context.rs`:

```rust
    /// The whole point of the split: work out whether a passphrase is
    /// needed without opening the database or claiming any device.
    #[test]
    fn probe_reports_the_encryption_state_of_a_plaintext_database() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("fastpaste.sqlite");
        drop(fastpaste_data::Database::open(&path, false).unwrap());

        assert_eq!(
            fastpaste_data::Database::encryption_state(&path).unwrap(),
            fastpaste_data::EncryptionState::Plaintext
        );
    }

    #[test]
    fn a_context_built_with_a_key_reads_the_encrypted_database() {
        use secrecy::SecretString;

        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("fastpaste.sqlite");
        let key = SecretString::from("s3cret".to_string());

        {
            let db = fastpaste_data::Database::open(&path, false).unwrap();
            let mut item = fastpaste_data::Item::new_plain(0, "T", "B");
            db.insert(&mut item).unwrap();
        }
        fastpaste_data::encrypt_database(&path, &key).unwrap();

        let db = Arc::new(Mutex::new(
            fastpaste_data::Database::open_with_key(&path, false, Some(&key)).unwrap(),
        ));
        let ctx = AppContext::new(
            db,
            Arc::new(NullClipboard::new()),
            Arc::new(NullPasteKeys),
            Arc::new(NullGlobalHotkey::new()),
            Settings::default(),
            None,
            path.clone(),
            Arc::new(fastpaste_platform::NullSecretStore::new()),
        );

        let loaded = ctx.db.lock().unwrap().load_all().unwrap();
        assert_eq!(loaded[0].body_plain, "B");
        assert_eq!(ctx.db_path, path, "conversions need the path");
    }
```

Every existing call to `AppContext::new` in this test module gains the same two trailing arguments — update the `ctx` helper at the top of the module and the two inline constructions in `paste_settings_reach_the_paster` and `services_are_wired_from_settings_at_construction`.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p fastpaste-app`
Expected: FAIL to compile — `AppContext::new` takes six arguments, not eight, and `ctx.db_path` does not exist.

- [ ] **Step 3: Write the implementation**

Add to `crates/fastpaste-app/Cargo.toml` under `[dependencies]`:

```toml
secrecy.workspace = true
```

In `crates/fastpaste-app/src/context.rs`, add two fields to `struct AppContext`:

```rust
    /// Where the database file lives. Kept because the encrypt / decrypt /
    /// change-passphrase operations act on the path with every connection
    /// closed, so they cannot get it from `db`.
    pub db_path: std::path::PathBuf,
    /// The OS credential store, for remembering the database passphrase.
    /// Always present; [`fastpaste_platform::UnavailableSecretStore`]
    /// stands in when the session has no store, so callers never branch on
    /// an `Option`.
    pub secret_store: Arc<dyn fastpaste_platform::SecretStore>,
```

Extend `AppContext::new` with the two matching trailing parameters and assign them in the struct literal.

Add the account-name constant near the top of the file:

```rust
/// Account name under which the database passphrase is filed in the
/// credential store. One database per user, so one fixed name.
pub const PASSPHRASE_ACCOUNT: &str = "database-passphrase";
```

Replace `AppContext::build` with the two-phase pair. Everything from the tracing setup through the settings load is unchanged; it is only split across the two functions:

```rust
/// What [`AppContext::probe`] learned before anything was opened.
///
/// Holds the single-instance guard for the gap between "we know whether a
/// passphrase is needed" and "we have one", so nothing can start a second
/// instance while the unlock dialog is up.
pub struct StartupProbe {
    /// Whether the database needs a passphrase.
    pub encryption: fastpaste_data::EncryptionState,
    /// Settings, loaded before the database so the unlock dialog can be
    /// shown in the user's language.
    pub settings: Settings,
    data_dir: std::path::PathBuf,
    db_path: std::path::PathBuf,
    single_instance: SingleInstance,
}

impl AppContext {
    /// First half of startup: take the single-instance guard, load
    /// settings, and find out whether the database needs a passphrase.
    ///
    /// Deliberately opens nothing and claims no device. The caller may
    /// have to put a dialog on screen before [`Self::build_unlocked`] can
    /// run, and that must not happen with the clipboard or `/dev/uinput`
    /// already held.
    pub fn probe() -> anyhow::Result<StartupProbe> {
        let proj_dirs = crate::paths::project_dirs()?;
        let data_dir = proj_dirs.data_dir().to_path_buf();

        // ---- Single-instance guard, FIRST ----------------------------------
        // (unchanged: see the long comment on the original `build`)
        let instance_key = instance_key_for(&data_dir);
        let single_instance = SingleInstance::new(&instance_key).map_err(|e| {
            anyhow::anyhow!("failed to acquire single-instance key {instance_key}: {e}")
        })?;
        if !single_instance.is_single() {
            anyhow::bail!(
                "another fastpaste instance is running \
                 (single-instance key {instance_key})",
            );
        }
        tracing::info!("acquired single-instance key {instance_key}");

        std::fs::create_dir_all(&data_dir)?;
        let db_path = data_dir.join("fastpaste.sqlite");
        tracing::info!("database path: {}", db_path.display());

        // A conversion that was interrupted by a crash leaves a `.new`
        // file. Now — with the guard held and before anything reads the
        // directory — is the only safe moment to clear it.
        fastpaste_data::clean_orphaned_conversion(&db_path)?;

        let encryption = fastpaste_data::Database::encryption_state(&db_path)?;
        tracing::info!("database encryption state: {encryption:?}");

        let settings = Settings::load()?;

        Ok(StartupProbe {
            encryption,
            settings,
            data_dir,
            db_path,
            single_instance,
        })
    }

    /// Second half of startup: open the database with `key` and build
    /// every service. Consumes the probe, taking over its guard.
    ///
    /// Degradation policy is unchanged from the original `build`:
    /// `/dev/uinput` unavailable → [`NullPasteKeys`]; no X connection →
    /// [`NullGlobalHotkey`]; a failing clipboard backend is still fatal.
    /// A credential store that cannot be reached is likewise non-fatal —
    /// it costs the user a prompt each launch, not the app.
    pub fn build_unlocked(
        probe: StartupProbe,
        key: Option<secrecy::SecretString>,
    ) -> anyhow::Result<Self> {
        let StartupProbe {
            settings,
            data_dir,
            db_path,
            single_instance,
            ..
        } = probe;
        let _ = &data_dir;

        let db = Arc::new(Mutex::new(fastpaste_data::Database::open_with_key(
            &db_path,
            false,
            key.as_ref(),
        )?));

        let clipboard = Arc::new(fastpaste_platform::SystemClipboard::new()?);
        let uinput: Arc<dyn PasteKeys> = match SystemPasteKeys::new() {
            Ok(u) => Arc::new(u),
            Err(e) => {
                tracing::warn!(
                    "/dev/uinput unavailable ({e}); \
                     paste will leave payload on clipboard"
                );
                Arc::new(NullPasteKeys)
            }
        };
        let hotkey: Arc<dyn GlobalHotkey> = match SystemHotkeys::new() {
            Ok(h) => Arc::new(h),
            Err(e) => {
                tracing::error!(
                    "global hotkeys unavailable ({e}); \
                     the tray icon and main window still work, but the \
                     shortcuts will not fire. XWayland is required."
                );
                Arc::new(NullGlobalHotkey::new())
            }
        };

        let store = fastpaste_platform::SystemSecretStore::new();
        let secret_store: Arc<dyn fastpaste_platform::SecretStore> = if store.is_available() {
            Arc::new(store)
        } else {
            tracing::warn!(
                "no credential store on this session; \
                 an encrypted database will prompt at every launch"
            );
            Arc::new(fastpaste_platform::UnavailableSecretStore)
        };

        tracing::info!(
            "loaded settings: paste.delay_ms={}, paste.restore_clipboard={}, \
             clipboard_history.enabled={}, clipboard_history.max_items={}",
            settings.paste.delay_ms,
            settings.paste.restore_clipboard,
            settings.clipboard_history.enabled,
            settings.clipboard_history.max_items,
        );

        Ok(Self::new(
            db,
            clipboard,
            uinput,
            hotkey,
            settings,
            Some(single_instance),
            db_path,
            secret_store,
        ))
    }

    /// Both halves at once, with no passphrase. Fails on an encrypted
    /// database — a caller that might meet one must use [`Self::probe`]
    /// and [`Self::build_unlocked`] so it can prompt.
    pub fn build() -> anyhow::Result<Self> {
        let probe = Self::probe()?;
        if probe.encryption == fastpaste_data::EncryptionState::Encrypted {
            anyhow::bail!("the database is encrypted; build() cannot supply a passphrase");
        }
        Self::build_unlocked(probe, None)
    }
}
```

Add `use fastpaste_platform::SecretStore as _;` near the other imports so `store.is_available()` resolves.

Re-export the new names from `crates/fastpaste-app/src/lib.rs` — Tasks 9 and 10 reach them as `fastpaste_app::PASSPHRASE_ACCOUNT` and `fastpaste_app::StartupProbe`:

```rust
pub use context::{AppContext, PASSPHRASE_ACCOUNT, StartupProbe};
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p fastpaste-app`
Expected: PASS

- [ ] **Step 5: Confirm the GUI still compiles**

Run: `cargo build --workspace`
Expected: PASS. `main.rs:145` still calls `AppContext::build()`, which still exists; Task 9 changes that.

- [ ] **Step 6: Commit**

```bash
git add crates/fastpaste-app/
git commit -m "Split the composition root into a probe and a keyed build"
```

---

### Task 8: The unlock dialog

**Files:**
- Create: `crates/fastpaste-gui/ui/unlock_dialog.slint`
- Modify: `crates/fastpaste-gui/ui/main.slint` (export the new component)
- Modify: `crates/fastpaste-gui/ui/translations.slint`
- Modify: all five files in `crates/fastpaste-gui/i18n/`
- Modify: `crates/fastpaste-gui/src/main.rs` (`apply_translations`)
- Modify: `crates/fastpaste-gui/examples/ui_preview.rs`

**Interfaces:**
- Consumes: the `Translations` global.
- Produces: an `UnlockDialog` component with
  - `in-out property <string> passphrase`
  - `in-out property <string> error-message` (empty = no error shown)
  - `in property <bool> remember-available`
  - `in-out property <bool> remember`
  - `in property <bool> busy` (disables the buttons during the open attempt)
  - `callback unlock-clicked()`
  - `callback cancel-clicked()`

- [ ] **Step 1: Write the dialog**

Create `crates/fastpaste-gui/ui/unlock_dialog.slint`:

```slint
// Unlock Dialog: the passphrase prompt for an encrypted database.
//
// Shown before anything else exists — no tray, no hotkeys, no main
// window — because none of them can do anything useful until the
// database opens. The controller (main.rs) reads `passphrase` on
// `unlock-clicked`, tries to open, and either proceeds or pushes a
// message into `error-message` and leaves the dialog up.
//
// `remember-available` is false on a session with no credential store;
// the checkbox is then disabled and the hint says why, rather than
// failing on click.

import { Button, CheckBox, LineEdit, VerticalBox, HorizontalBox } from "std-widgets.slint";
import { Translations } from "translations.slint";

export component UnlockDialog inherits Window {
    title: Translations.unlock-title;
    icon: @image-url("../assets/icon.png");

    in-out property <string> passphrase: "";
    in-out property <string> error-message: "";
    in property <bool> remember-available: true;
    in-out property <bool> remember: false;
    in property <bool> busy: false;

    callback unlock-clicked();
    callback cancel-clicked();

    // Enter submits from the text field; Escape cancels from anywhere.
    forward-focus: pass-field;
    FocusScope {
        key-pressed(event) => {
            if (event.text == Key.Escape) {
                root.cancel-clicked();
                return accept;
            }
            return reject;
        }

        VerticalBox {
            padding: 16px;
            spacing: 10px;

            Text {
                text: Translations.unlock-prompt;
                wrap: word-wrap;
                horizontal-alignment: left;
            }

            HorizontalBox {
                padding: 0px;
                spacing: 8px;
                Text {
                    text: Translations.unlock-passphrase-label;
                    vertical-alignment: center;
                }
                pass-field := LineEdit {
                    input-type: password;
                    enabled: !root.busy;
                    text <=> root.passphrase;
                    accepted => { root.unlock-clicked(); }
                }
            }

            CheckBox {
                text: root.remember-available
                    ? Translations.unlock-remember
                    : Translations.unlock-remember-unavailable;
                enabled: root.remember-available && !root.busy;
                checked <=> root.remember;
            }

            // Reserves no space when empty, so the dialog does not jump
            // the first time a passphrase is wrong.
            if root.error-message != "": Text {
                text: root.error-message;
                color: #c0392b;
                wrap: word-wrap;
            }

            HorizontalBox {
                padding: 0px;
                spacing: 8px;
                Rectangle { }   // pushes the buttons right
                Button {
                    text: Translations.unlock-cancel;
                    enabled: !root.busy;
                    clicked => { root.cancel-clicked(); }
                }
                Button {
                    text: Translations.unlock-unlock;
                    primary: true;
                    enabled: !root.busy;
                    clicked => { root.unlock-clicked(); }
                }
            }
        }
    }
}
```

If `../assets/icon.png` is not the path the other windows use, copy whatever `main_window.slint` sets for `icon:` — or drop the line if they set none.

- [ ] **Step 2: Add the strings**

In `crates/fastpaste-gui/ui/translations.slint`, add to `global Translations`:

```slint
    // Unlock dialog.
    in-out property <string> unlock-title: "Unlock fastpaste";
    in-out property <string> unlock-prompt: "This database is encrypted. Enter your passphrase to open it.";
    in-out property <string> unlock-passphrase-label: "Passphrase:";
    in-out property <string> unlock-remember: "Remember in the system keyring";
    in-out property <string> unlock-remember-unavailable: "Remember in the system keyring (unavailable on this session)";
    in-out property <string> unlock-error-wrong: "Wrong passphrase.";
    in-out property <string> unlock-unlock: "Unlock";
    in-out property <string> unlock-cancel: "Cancel";
```

Add the same eight keys to each file in `crates/fastpaste-gui/i18n/`:

`en.ftl`
```
unlock-title = Unlock fastpaste
unlock-prompt = This database is encrypted. Enter your passphrase to open it.
unlock-passphrase-label = Passphrase:
unlock-remember = Remember in the system keyring
unlock-remember-unavailable = Remember in the system keyring (unavailable on this session)
unlock-error-wrong = Wrong passphrase.
unlock-unlock = Unlock
unlock-cancel = Cancel
```

`ru.ftl`
```
unlock-title = Разблокировать fastpaste
unlock-prompt = База данных зашифрована. Введите пароль, чтобы открыть её.
unlock-passphrase-label = Пароль:
unlock-remember = Запомнить в системном хранилище ключей
unlock-remember-unavailable = Запомнить в системном хранилище ключей (недоступно в этом сеансе)
unlock-error-wrong = Неверный пароль.
unlock-unlock = Разблокировать
unlock-cancel = Отмена
```

`de.ftl`
```
unlock-title = fastpaste entsperren
unlock-prompt = Diese Datenbank ist verschlüsselt. Geben Sie Ihre Passphrase ein, um sie zu öffnen.
unlock-passphrase-label = Passphrase:
unlock-remember = Im Schlüsselbund des Systems merken
unlock-remember-unavailable = Im Schlüsselbund des Systems merken (in dieser Sitzung nicht verfügbar)
unlock-error-wrong = Falsche Passphrase.
unlock-unlock = Entsperren
unlock-cancel = Abbrechen
```

`es.ftl`
```
unlock-title = Desbloquear fastpaste
unlock-prompt = Esta base de datos está cifrada. Introduzca su contraseña para abrirla.
unlock-passphrase-label = Contraseña:
unlock-remember = Recordar en el llavero del sistema
unlock-remember-unavailable = Recordar en el llavero del sistema (no disponible en esta sesión)
unlock-error-wrong = Contraseña incorrecta.
unlock-unlock = Desbloquear
unlock-cancel = Cancelar
```

`zh_CN.ftl`
```
unlock-title = 解锁 fastpaste
unlock-prompt = 此数据库已加密。请输入密码以打开。
unlock-passphrase-label = 密码：
unlock-remember = 记住到系统密钥环
unlock-remember-unavailable = 记住到系统密钥环（当前会话不可用）
unlock-error-wrong = 密码错误。
unlock-unlock = 解锁
unlock-cancel = 取消
```

Export the component from `crates/fastpaste-gui/ui/main.slint`, alongside the existing exports:

```slint
import { UnlockDialog } from "unlock_dialog.slint";
export { UnlockDialog }
```

Match whatever export form the file already uses for `OptionsDialog`.

Push the strings in `apply_translations` in `crates/fastpaste-gui/src/main.rs`, after the tray block:

```rust
    t.set_unlock_title(m("unlock-title").into());
    t.set_unlock_prompt(m("unlock-prompt").into());
    t.set_unlock_passphrase_label(m("unlock-passphrase-label").into());
    t.set_unlock_remember(m("unlock-remember").into());
    t.set_unlock_remember_unavailable(m("unlock-remember-unavailable").into());
    t.set_unlock_error_wrong(m("unlock-error-wrong").into());
    t.set_unlock_unlock(m("unlock-unlock").into());
    t.set_unlock_cancel(m("unlock-cancel").into());
```

- [ ] **Step 3: Add the preview mode**

In `crates/fastpaste-gui/examples/ui_preview.rs`, add `"unlock"` to the size table and the dispatch:

```rust
        "unlock" => (420u32, 260u32),
```

```rust
        "unlock" => Box::new(build_unlock(page == 1, ru)?),
```

And add the builder, following the shape of `build_options`:

```rust
/// `page == 1` renders the wrong-passphrase state, which is the one
/// worth looking at: the error text has to fit without pushing the
/// buttons off the bottom in the widest locale.
fn build_unlock(with_error: bool, ru: bool) -> Result<UnlockDialog, slint::PlatformError> {
    let d = UnlockDialog::new()?;
    d.set_passphrase("hunter2".into());
    d.set_remember_available(false);
    if with_error {
        d.set_error_message("Wrong passphrase.".into());
    }
    if ru {
        let t: Translations = slint::Global::get(&d);
        t.set_unlock_title("Разблокировать fastpaste".into());
        t.set_unlock_prompt(
            "База данных зашифрована. Введите пароль, чтобы открыть её.".into(),
        );
        t.set_unlock_passphrase_label("Пароль:".into());
        t.set_unlock_remember_unavailable(
            "Запомнить в системном хранилище ключей (недоступно в этом сеансе)".into(),
        );
        t.set_unlock_error_wrong("Неверный пароль.".into());
        t.set_unlock_unlock("Разблокировать".into());
        t.set_unlock_cancel("Отмена".into());
        if with_error {
            d.set_error_message("Неверный пароль.".into());
        }
    }
    Ok(d)
}
```

Update the usage comment at the top of the file to list the new mode.

- [ ] **Step 4: Render it and look at it**

Run:
```bash
cargo run --example ui_preview -- unlock 1 /tmp/unlock.ppm
ffmpeg -y -i /tmp/unlock.ppm /tmp/unlock.png
```
Expected: a PNG showing the dialog in Russian with the error line and the disabled *Remember* checkbox. Check that no label is clipped and the buttons are fully on screen — `AGENTS.md` §"Measure, don't eyeball" applies.

- [ ] **Step 5: Build and lint**

Run: `cargo clippy --workspace --all-targets -- -D warnings`
Expected: PASS

- [ ] **Step 6: Commit**

```bash
git add crates/fastpaste-gui/
git commit -m "Add the unlock dialog"
```

---

### Task 9: Prompt for the passphrase at startup

**Files:**
- Modify: `crates/fastpaste-gui/src/main.rs:130-290` (`main`, split into `main` + `start_app`)
- Modify: `README.md` (degraded-modes table, features list)

**Interfaces:**
- Consumes: `AppContext::{probe, build_unlocked}`, `StartupProbe`, `PASSPHRASE_ACCOUNT` (Task 7); `UnlockDialog` (Task 8); `SecretStore` (Task 6).
- Produces: `fn start_app(ctx: Arc<AppContext>) -> anyhow::Result<()>` holding everything `main` did after the composition root, up to but not including the event-loop call.

**Follow the Task 1 answer.** If the spike was *refuted*, use its fallback instead of the loop change below and say so in the commit message.

- [ ] **Step 1: Restructure `main` into `main` + `start_app`**

Move everything in `main` from the i18n locale block (`main.rs:150`) through `spawn_clipboard_drainer` and the "fastpaste ready" log (`main.rs:259`) into a new function, unchanged:

```rust
/// Everything that needs an open database: i18n, tray, hotkeys, the
/// hotkey-events thread, the tree-refresh worker and the clipboard
/// drainer. Called either directly (plaintext database) or from the
/// unlock dialog's accept handler, which is why it is a function and no
/// longer the body of `main`.
fn start_app(ctx: Arc<AppContext>) -> anyhow::Result<()> {
    // ... the moved body, verbatim ...
    Ok(())
}
```

- [ ] **Step 2: Write the new `main`**

```rust
fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    tracing::info!("fastpaste-gui starting");

    // ---- Phase one: guard, settings, encryption state -----------------------
    // Nothing is opened and no device is claimed here, because an
    // encrypted database puts a dialog on screen before phase two runs.
    let probe = AppContext::probe()?;
    *I18N_LOCALE.lock().unwrap_or_else(|e| e.into_inner()) =
        probe.settings.general.language.clone();

    if probe.encryption != EncryptionState::Encrypted {
        // Nothing to unlock: phase two immediately, exactly as before.
        let ctx = Arc::new(AppContext::build_unlocked(probe, None)?);
        start_app(ctx.clone())?;
        return run_loop(ctx);
    }

    // ---- Encrypted: try the keyring, then ask --------------------------------
    // The store is probed here rather than inside the context, because
    // the context does not exist until the passphrase is known.
    let store = fastpaste_platform::SystemSecretStore::new();
    let remembered = if store.is_available() {
        store.get(fastpaste_app::PASSPHRASE_ACCOUNT).unwrap_or_else(|e| {
            tracing::warn!("could not read the remembered passphrase: {e}");
            None
        })
    } else {
        None
    };

    if let Some(key) = remembered {
        match AppContext::build_unlocked_from(&probe, Some(key)) {
            Ok(ctx) => {
                let ctx = Arc::new(ctx);
                start_app(ctx.clone())?;
                return run_loop(ctx);
            }
            Err(e) => {
                // A remembered passphrase that no longer works is a stale
                // entry, not a failure: drop it and ask, rather than
                // leaving the user with an app that will not start.
                tracing::warn!("the remembered passphrase did not work ({e}); clearing it");
                let _ = store.delete(fastpaste_app::PASSPHRASE_ACCOUNT);
            }
        }
    }

    unlock_then_start(probe, store)
}
```

`build_unlocked` consumes its probe, which the retry loop cannot afford. Add a borrowing variant to `crates/fastpaste-app/src/context.rs` and express the consuming one in terms of it:

```rust
    /// Try `key` against the database named by `probe`, without consuming
    /// the probe — so a caller can retry after a wrong passphrase.
    ///
    /// On success the returned context has taken the probe's guard, so the
    /// probe must not be used again. Enforced by the caller, not the type
    /// system: the retry loop only reuses a probe whose attempt failed.
    pub fn build_unlocked_from(
        probe: &StartupProbe,
        key: Option<secrecy::SecretString>,
    ) -> anyhow::Result<Self> {
        // Prove the key before anything is claimed, so a wrong one costs
        // only a connection.
        let db = fastpaste_data::Database::open_with_key(&probe.db_path, false, key.as_ref())?;
        Self::finish(probe, db)
    }
```

Refactor the body of `build_unlocked` into `fn finish(probe: &StartupProbe, db: Database) -> anyhow::Result<Self>`, which builds the platform services and calls `Self::new`. Because `finish` needs the guard, change `StartupProbe::single_instance` to `Option<SingleInstance>` and have `finish` `take()` it — add `single_instance: std::cell::RefCell<Option<SingleInstance>>` if borrow-checking objects, and document why.

- [ ] **Step 3: Write the dialog driver and the loop helper**

```rust
/// Put the unlock dialog on screen and start the app once it succeeds.
///
/// The dialog is created and shown *before* the event loop, the same way
/// the tray is today. Its accept handler runs inside the loop and does
/// phase two there.
fn unlock_then_start(
    probe: fastpaste_app::StartupProbe,
    store: fastpaste_platform::SystemSecretStore,
) -> anyhow::Result<()> {
    let dialog = UnlockDialog::new()?;
    apply_translations(&dialog, &i18n());
    dialog.set_remember_available(store.is_available());

    let probe = Rc::new(probe);
    let store = Rc::new(store);
    let weak = dialog.as_weak();

    {
        let probe = probe.clone();
        let store = store.clone();
        let weak = weak.clone();
        dialog.on_unlock_clicked(move || {
            let Some(d) = weak.upgrade() else { return };
            let key = secrecy::SecretString::from(d.get_passphrase().to_string());
            d.set_busy(true);
            d.set_error_message("".into());

            match AppContext::build_unlocked_from(&probe, Some(key.clone())) {
                Ok(ctx) => {
                    if d.get_remember() && store.is_available() {
                        if let Err(e) = store.set(fastpaste_app::PASSPHRASE_ACCOUNT, &key) {
                            // Not fatal: the database is open. The user
                            // just gets prompted again next time.
                            tracing::warn!("could not remember the passphrase: {e}");
                        }
                    }
                    // Clear the field before the window goes away, so the
                    // passphrase does not sit in a live SharedString.
                    d.set_passphrase("".into());
                    let _ = d.hide();

                    let ctx = Arc::new(ctx);
                    if let Err(e) = start_app(ctx) {
                        tracing::error!("startup failed after unlock: {e}");
                        let _ = slint::quit_event_loop();
                    }
                }
                Err(e) => {
                    tracing::info!("unlock attempt rejected: {e}");
                    d.set_busy(false);
                    d.set_passphrase("".into());
                    d.set_error_message(i18n().msg("unlock-error-wrong").into());
                }
            }
        });
    }

    dialog.on_cancel_clicked(move || {
        tracing::info!("unlock cancelled; exiting");
        if let Some(d) = weak.upgrade() {
            let _ = d.hide();
        }
        let _ = slint::quit_event_loop();
    });

    dialog.show()?;
    // Always the until-quit form here: the dialog may be the only window
    // and hiding it must not end the process before `start_app` has put
    // the tray up. See the spike recorded in the design doc.
    let result = slint::run_event_loop_until_quit();
    ui_state::release_all();
    result.map_err(|e| anyhow::anyhow!("Slint event loop exited with error: {e}"))
}

/// Run the event loop for an already-started app, then flush and tear
/// down. This is the tail of the old `main`.
fn run_loop(ctx: Arc<AppContext>) -> anyhow::Result<()> {
    let result = if ui_state::has_tray() {
        slint::run_event_loop()
    } else {
        tracing::info!("no tray icon; the app quits when the main window is closed");
        slint::run_event_loop_until_quit()
    };
    flush_pending_edit(&ctx);
    ui_state::release_all();
    result.map_err(|e| anyhow::anyhow!("Slint event loop exited with error: {e}"))?;
    Ok(())
}
```

Add `use std::rc::Rc;` and `use fastpaste_data::EncryptionState;` to the imports, and `use fastpaste_platform::SecretStore as _;` so the trait methods resolve.

- [ ] **Step 4: Build and lint**

Run: `cargo clippy --workspace --all-targets -- -D warnings`
Expected: PASS

- [ ] **Step 5: Verify the plaintext path is untouched**

Run: `cargo test --workspace`
Expected: PASS

Then run the app against a plaintext database and confirm it starts with no prompt, the tray appears, and both hotkeys register:

Run: `cargo run --bin fastpaste-gui`
Expected: the "fastpaste ready" line in the log, exactly as before this change.

- [ ] **Step 6: Verify the encrypted path by hand**

There is no UI test harness here, so this is a manual check. With the app closed:

```bash
cargo run --example ui_preview -- unlock 0 /tmp/u.ppm   # sanity: the dialog builds
```

Then encrypt the real database using the Task 10 UI once it exists. Until then, confirm the prompt appears by pointing the app at an encrypted copy made in a `cargo test` scratch directory. Record the result in the commit message.

- [ ] **Step 7: Document the new modes**

Add to the "Degraded modes" table in `README.md`:

```markdown
| The passphrase for an encrypted database | The unlock dialog stays up with a "wrong passphrase" message; cancelling exits cleanly and releases the single-instance guard |
| A remembered passphrase that no longer works | The stale keyring entry is cleared and the dialog is shown, rather than the app refusing to start |
| A conversion interrupted by a crash | The original database is untouched; the partial `.sqlite.new` file is deleted at the next launch |
| A forgotten passphrase | Unrecoverable. Options -> Security says so before you set one |
```

- [ ] **Step 8: Commit**

```bash
git add crates/fastpaste-gui/src/main.rs crates/fastpaste-app/src/context.rs README.md
git commit -m "Prompt for the passphrase when the database is encrypted"
```

---

### Task 10: The Security options page

**Files:**
- Modify: `crates/fastpaste-gui/ui/options_dialog.slint`
- Modify: `crates/fastpaste-gui/ui/translations.slint`
- Modify: all five files in `crates/fastpaste-gui/i18n/`
- Modify: `crates/fastpaste-gui/src/main.rs` (options controller, `apply_translations`)
- Modify: `crates/fastpaste-gui/examples/ui_preview.rs`
- Modify: `README.md` (features list)

**Interfaces:**
- Consumes: `encrypt_database`, `decrypt_database`, `change_passphrase` (Task 5); `AppContext::{db_path, secret_store}`, `PASSPHRASE_ACCOUNT` (Task 7).
- Produces: page 4 of the Options dialog, with
  - `in property <bool> security-encrypted`
  - `in-out property <string> security-current-passphrase`
  - `in-out property <string> security-new-passphrase`
  - `in-out property <string> security-confirm-passphrase`
  - `in-out property <bool> security-remember`
  - `in property <bool> security-remember-available`
  - `in-out property <string> security-message`
  - `callback security-encrypt-clicked()`
  - `callback security-change-clicked()`
  - `callback security-remove-clicked()`

The fields live inline on the page rather than in a separate modal — the dialog is already page-based and a modal over a modal is worse.

- [ ] **Step 1: Add the page**

In `crates/fastpaste-gui/ui/options_dialog.slint`, add the properties and callbacks to the root alongside the existing ones, add a fifth sidebar entry, and add the page block after the paste page:

```slint
    // -- Security (page 4) --
    if root.active-page == 4: VerticalLayout {
        spacing: 8px;
        padding: 0px;

        FPageTitle { text: Translations.options-security; }

        Text {
            text: root.security-encrypted
                ? Translations.options-security-state-encrypted
                : Translations.options-security-state-plaintext;
            wrap: word-wrap;
        }

        // Plaintext: set a passphrase and convert.
        if !root.security-encrypted: VerticalLayout {
            spacing: 6px;
            GridLayout {
                spacing: 6px;
                Row {
                    Text { text: Translations.options-security-new-label; vertical-alignment: center; }
                    LineEdit { input-type: password; text <=> root.security-new-passphrase; }
                }
                Row {
                    Text { text: Translations.options-security-confirm-label; vertical-alignment: center; }
                    LineEdit { input-type: password; text <=> root.security-confirm-passphrase; }
                }
            }
            CheckBox {
                text: root.security-remember-available
                    ? Translations.unlock-remember
                    : Translations.unlock-remember-unavailable;
                enabled: root.security-remember-available;
                checked <=> root.security-remember;
            }
            Text {
                text: Translations.options-security-warning;
                wrap: word-wrap;
            }
            HorizontalLayout {
                spacing: 8px;
                Button {
                    text: Translations.options-security-encrypt;
                    primary: true;
                    clicked => { root.security-encrypt-clicked(); }
                }
                Rectangle { }
            }
        }

        // Encrypted: change or remove, both needing the current passphrase.
        if root.security-encrypted: VerticalLayout {
            spacing: 6px;
            GridLayout {
                spacing: 6px;
                Row {
                    Text { text: Translations.options-security-current-label; vertical-alignment: center; }
                    LineEdit { input-type: password; text <=> root.security-current-passphrase; }
                }
                Row {
                    Text { text: Translations.options-security-new-label; vertical-alignment: center; }
                    LineEdit { input-type: password; text <=> root.security-new-passphrase; }
                }
                Row {
                    Text { text: Translations.options-security-confirm-label; vertical-alignment: center; }
                    LineEdit { input-type: password; text <=> root.security-confirm-passphrase; }
                }
            }
            HorizontalLayout {
                spacing: 8px;
                Button {
                    text: Translations.options-security-change;
                    clicked => { root.security-change-clicked(); }
                }
                Button {
                    text: Translations.options-security-remove;
                    clicked => { root.security-remove-clicked(); }
                }
                Rectangle { }
            }
        }

        if root.security-message != "": Text {
            text: root.security-message;
            wrap: word-wrap;
        }
    }
```

- [ ] **Step 2: Add the strings**

Add to `translations.slint`:

```slint
    in-out property <string> options-security: "Security";
    in-out property <string> options-security-state-plaintext: "This database is not encrypted. Anyone who can read the file can read your snippets.";
    in-out property <string> options-security-state-encrypted: "This database is encrypted.";
    in-out property <string> options-security-current-label: "Current passphrase:";
    in-out property <string> options-security-new-label: "New passphrase:";
    in-out property <string> options-security-confirm-label: "Confirm:";
    in-out property <string> options-security-encrypt: "Encrypt database";
    in-out property <string> options-security-change: "Change passphrase";
    in-out property <string> options-security-remove: "Remove encryption";
    in-out property <string> options-security-warning: "There is no way to recover a forgotten passphrase. Encrypting protects the file from this point on: the unencrypted copy is deleted, but on an SSD or a journalling filesystem its contents may still be recoverable from the raw device.";
    in-out property <string> options-security-mismatch: "The two passphrases do not match.";
    in-out property <string> options-security-empty: "Enter a passphrase.";
    in-out property <string> options-security-done-encrypted: "The database is now encrypted.";
    in-out property <string> options-security-done-changed: "Passphrase changed.";
    in-out property <string> options-security-done-removed: "Encryption removed.";
```

Add the same fifteen keys to each file in `crates/fastpaste-gui/i18n/`.

`en.ftl`
```
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
```

`ru.ftl`
```
options-security = Безопасность
options-security-state-plaintext = База данных не зашифрована. Любой, кто может прочитать файл, увидит ваши записи.
options-security-state-encrypted = База данных зашифрована.
options-security-current-label = Текущий пароль:
options-security-new-label = Новый пароль:
options-security-confirm-label = Подтверждение:
options-security-encrypt = Зашифровать базу данных
options-security-change = Сменить пароль
options-security-remove = Снять шифрование
options-security-warning = Забытый пароль восстановить невозможно. Шифрование защищает файл с этого момента: незашифрованная копия удаляется, но на SSD или журналируемой файловой системе её содержимое всё ещё может быть восстановлено с самого устройства.
options-security-mismatch = Пароли не совпадают.
options-security-empty = Введите пароль.
options-security-done-encrypted = База данных зашифрована.
options-security-done-changed = Пароль изменён.
options-security-done-removed = Шифрование снято.
```

`de.ftl`
```
options-security = Sicherheit
options-security-state-plaintext = Diese Datenbank ist nicht verschlüsselt. Wer die Datei lesen kann, kann Ihre Schnipsel lesen.
options-security-state-encrypted = Diese Datenbank ist verschlüsselt.
options-security-current-label = Aktuelle Passphrase:
options-security-new-label = Neue Passphrase:
options-security-confirm-label = Bestätigen:
options-security-encrypt = Datenbank verschlüsseln
options-security-change = Passphrase ändern
options-security-remove = Verschlüsselung aufheben
options-security-warning = Eine vergessene Passphrase lässt sich nicht wiederherstellen. Die Verschlüsselung schützt die Datei ab diesem Zeitpunkt: die unverschlüsselte Kopie wird gelöscht, doch auf einer SSD oder einem journalisierenden Dateisystem kann ihr Inhalt weiterhin vom Rohgerät wiederherstellbar sein.
options-security-mismatch = Die beiden Passphrasen stimmen nicht überein.
options-security-empty = Bitte geben Sie eine Passphrase ein.
options-security-done-encrypted = Die Datenbank ist jetzt verschlüsselt.
options-security-done-changed = Passphrase geändert.
options-security-done-removed = Verschlüsselung aufgehoben.
```

`es.ftl`
```
options-security = Seguridad
options-security-state-plaintext = Esta base de datos no está cifrada. Cualquiera que pueda leer el archivo puede leer sus fragmentos.
options-security-state-encrypted = Esta base de datos está cifrada.
options-security-current-label = Contraseña actual:
options-security-new-label = Contraseña nueva:
options-security-confirm-label = Confirmar:
options-security-encrypt = Cifrar la base de datos
options-security-change = Cambiar la contraseña
options-security-remove = Quitar el cifrado
options-security-warning = No hay forma de recuperar una contraseña olvidada. El cifrado protege el archivo a partir de este momento: la copia sin cifrar se elimina, pero en un SSD o en un sistema de archivos con registro por diario su contenido aún puede ser recuperable desde el dispositivo.
options-security-mismatch = Las dos contraseñas no coinciden.
options-security-empty = Introduzca una contraseña.
options-security-done-encrypted = La base de datos ya está cifrada.
options-security-done-changed = Contraseña cambiada.
options-security-done-removed = Cifrado eliminado.
```

`zh_CN.ftl`
```
options-security = 安全
options-security-state-plaintext = 此数据库未加密。任何能读取该文件的人都能看到你的片段。
options-security-state-encrypted = 此数据库已加密。
options-security-current-label = 当前密码：
options-security-new-label = 新密码：
options-security-confirm-label = 确认：
options-security-encrypt = 加密数据库
options-security-change = 更改密码
options-security-remove = 取消加密
options-security-warning = 忘记的密码无法找回。加密只保护此后的文件：未加密的副本会被删除，但在固态硬盘或日志文件系统上，其内容仍可能从原始设备中恢复。
options-security-mismatch = 两次输入的密码不一致。
options-security-empty = 请输入密码。
options-security-done-encrypted = 数据库已加密。
options-security-done-changed = 密码已更改。
options-security-done-removed = 已取消加密。
```

Push all fifteen in `apply_translations` with the matching `t.set_options_security*` setters, beside the unlock strings from Task 8.

- [ ] **Step 3: Wire the controller**

In `crates/fastpaste-gui/src/main.rs`, where the Options dialog is constructed, seed the page state and attach the three callbacks. All three follow the same shape: validate, close the connection, convert, reopen, report.

```rust
    // Security page. `db_path` and `secret_store` come off the context;
    // every conversion needs the database closed, which is why each of
    // these takes the mutex and replaces the `Database` inside it rather
    // than working through an open handle.
    dialog.set_security_encrypted(
        fastpaste_data::Database::encryption_state(&ctx.db_path)
            .map(|s| s == EncryptionState::Encrypted)
            .unwrap_or(false),
    );
    dialog.set_security_remember_available(ctx.secret_store.is_available());

    {
        let ctx = ctx.clone();
        let weak = dialog.as_weak();
        dialog.on_security_encrypt_clicked(move || {
            let Some(d) = weak.upgrade() else { return };
            let new = d.get_security_new_passphrase().to_string();
            let confirm = d.get_security_confirm_passphrase().to_string();
            if new.is_empty() {
                d.set_security_message(i18n().msg("options-security-empty").into());
                return;
            }
            if new != confirm {
                d.set_security_message(i18n().msg("options-security-mismatch").into());
                return;
            }
            let key = secrecy::SecretString::from(new);

            match convert_database(
                &ctx,
                |path| fastpaste_data::encrypt_database(path, &key),
                Some(&key),
                None,
            ) {
                Ok(()) => {
                    if d.get_security_remember() && ctx.secret_store.is_available() {
                        if let Err(e) =
                            ctx.secret_store.set(fastpaste_app::PASSPHRASE_ACCOUNT, &key)
                        {
                            tracing::warn!("could not remember the passphrase: {e}");
                        }
                    }
                    d.set_security_encrypted(true);
                    d.set_security_message(i18n().msg("options-security-done-encrypted").into());
                }
                Err(e) => d.set_security_message(format!("{e}").into()),
            }
            clear_security_fields(&d);
        });
    }
```

Add the two helpers:

```rust
/// Run a whole-file conversion on `ctx`'s database.
///
/// Drops the live connection first — SQLCipher cannot rewrite a file that
/// is open — and reopens whatever the conversion produced. The
/// single-instance guard rules out another process racing us.
///
/// Both keys are passed in rather than read back from the credential
/// store, because after a passphrase change the store still holds the old
/// one. `key_on_success` opens the file the conversion produced;
/// `key_on_failure` opens the untouched original. Getting these the wrong
/// way round leaves the app holding no database at all.
fn convert_database<F>(
    ctx: &Arc<AppContext>,
    op: F,
    key_on_success: Option<&secrecy::SecretString>,
    key_on_failure: Option<&secrecy::SecretString>,
) -> Result<(), fastpaste_data::DataError>
where
    F: FnOnce(&std::path::Path) -> Result<(), fastpaste_data::DataError>,
{
    let mut guard = ctx.db.lock().unwrap_or_else(|e| e.into_inner());
    // Replace the live connection with a throwaway in-memory one for the
    // duration: `*guard` cannot be left uninitialised, and the file must
    // have no connection open while SQLCipher rewrites it.
    let placeholder = fastpaste_data::Database::open_in_memory()?;
    drop(std::mem::replace(&mut *guard, placeholder));

    let result = op(&ctx.db_path);

    // Reopen either way: after a success the new file, after a failure the
    // untouched original.
    let key = if result.is_ok() {
        key_on_success
    } else {
        key_on_failure
    };
    *guard = fastpaste_data::Database::open_with_key(&ctx.db_path, false, key)?;
    result
}

fn clear_security_fields(d: &OptionsDialog) {
    d.set_security_current_passphrase("".into());
    d.set_security_new_passphrase("".into());
    d.set_security_confirm_passphrase("".into());
}
```

`convert_database` needs a valid `Database` to park in the mutex while the file is rewritten, so add one to `crates/fastpaste-data/src/database.rs`:

```rust
    /// An empty database that lives only in RAM, migrated like any other.
    ///
    /// Exists so a caller that must close the real connection — the
    /// whole-file conversions in [`crate::crypto`] — has something valid
    /// to park in its slot meanwhile.
    pub fn open_in_memory() -> Result<Self, DataError> {
        let mut conn = Connection::open_in_memory()?;
        migrations::runner().run(&mut conn)?;
        let db = Self { conn };
        db.ensure_schema_current()?;
        Ok(db)
    }
```

With a test:

```rust
    #[test]
    fn an_in_memory_database_migrates_like_any_other() {
        let db = Database::open_in_memory().unwrap();
        assert_eq!(db.schema_version().unwrap(), Some(1));
    }
```

Then the other two callbacks:

```rust
    {
        let ctx = ctx.clone();
        let weak = dialog.as_weak();
        dialog.on_security_change_clicked(move || {
            let Some(d) = weak.upgrade() else { return };
            let current = d.get_security_current_passphrase().to_string();
            let new = d.get_security_new_passphrase().to_string();
            let confirm = d.get_security_confirm_passphrase().to_string();
            if current.is_empty() || new.is_empty() {
                d.set_security_message(i18n().msg("options-security-empty").into());
                return;
            }
            if new != confirm {
                d.set_security_message(i18n().msg("options-security-mismatch").into());
                return;
            }
            let current = secrecy::SecretString::from(current);
            let new = secrecy::SecretString::from(new);

            // `PRAGMA rekey` works in place, but it still needs the live
            // connection closed, so it goes through the same helper.
            match convert_database(
                &ctx,
                |path| fastpaste_data::change_passphrase(path, &current, &new),
                Some(&new),
                Some(&current),
            ) {
                Ok(()) => {
                    // Only touch the stored entry if there already was
                    // one: a user who never asked to be remembered must
                    // not silently acquire a keyring entry here.
                    if ctx.secret_store.is_available()
                        && ctx
                            .secret_store
                            .get(fastpaste_app::PASSPHRASE_ACCOUNT)
                            .ok()
                            .flatten()
                            .is_some()
                        && let Err(e) =
                            ctx.secret_store.set(fastpaste_app::PASSPHRASE_ACCOUNT, &new)
                    {
                        tracing::warn!("could not update the remembered passphrase: {e}");
                    }
                    d.set_security_message(i18n().msg("options-security-done-changed").into());
                }
                Err(e) => d.set_security_message(format!("{e}").into()),
            }
            clear_security_fields(&d);
        });
    }

    {
        let ctx = ctx.clone();
        let weak = dialog.as_weak();
        dialog.on_security_remove_clicked(move || {
            let Some(d) = weak.upgrade() else { return };
            let current = d.get_security_current_passphrase().to_string();
            if current.is_empty() {
                d.set_security_message(i18n().msg("options-security-empty").into());
                return;
            }
            let current = secrecy::SecretString::from(current);

            match convert_database(
                &ctx,
                |path| fastpaste_data::decrypt_database(path, &current),
                None,
                Some(&current),
            ) {
                Ok(()) => {
                    // The entry now unlocks nothing. Leaving it would sit
                    // a live passphrase in the keyring for no reason.
                    if let Err(e) = ctx.secret_store.delete(fastpaste_app::PASSPHRASE_ACCOUNT) {
                        tracing::warn!("could not clear the remembered passphrase: {e}");
                    }
                    d.set_security_encrypted(false);
                    d.set_security_message(i18n().msg("options-security-done-removed").into());
                }
                Err(e) => d.set_security_message(format!("{e}").into()),
            }
            clear_security_fields(&d);
        });
    }
```

The `&&`-chained `let` in the change handler is edition-2024 let-chaining, which this codebase already uses (see `Database::check_version` in `database.rs`).

- [ ] **Step 4: Add the preview mode**

In `ui_preview.rs`, `options` page 4 now renders the Security page. Extend `build_options` to seed it:

```rust
    d.set_security_encrypted(page == 5);
    d.set_security_remember_available(true);
```

Treat `options 4` as the plaintext state and `options 5` as the encrypted state, both rendering page index 4. Update the usage comment.

- [ ] **Step 5: Render both states and check them**

Run:
```bash
cargo run --example ui_preview -- options 4 /tmp/sec-plain.ppm
cargo run --example ui_preview -- options 5 /tmp/sec-enc.ppm
ffmpeg -y -i /tmp/sec-plain.ppm /tmp/sec-plain.png
ffmpeg -y -i /tmp/sec-enc.ppm /tmp/sec-enc.png
```
Expected: both render in Russian with no clipped labels. The warning paragraph is the long one — confirm it wraps rather than overflowing.

- [ ] **Step 6: Run the full suite**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check`
Expected: PASS

- [ ] **Step 7: Exercise the whole feature by hand**

This is the acceptance run from the spec. With a scratch data directory so the real library is not at risk:

1. Start the app, add two snippets. Confirm `sqlite3 <db> "select body_plain from items"` prints them.
2. Options → Security → set a passphrase → *Encrypt database*, with *Remember* off.
3. Confirm the same `sqlite3` command now fails with "file is not a database", and that both snippets are still listed in the app.
4. Quit and restart: the unlock dialog appears. Enter the wrong passphrase — an inline error, the dialog stays. Enter the right one — the app starts.
5. Restart and cancel the dialog: the process exits cleanly and a second launch is not blocked by a stale single-instance guard.
6. Options → Security → *Change passphrase*. Restart; only the new one works.
7. Options → Security → *Remove encryption*. Confirm `sqlite3` can read the file again and every snippet survived.
8. Repeat step 2 with *Remember* on, restart, and confirm no prompt appears.

Record what happened in the commit message. Any step that fails is a bug to fix before committing, not a note to file.

- [ ] **Step 8: Document the feature**

Add to the features list in `README.md`:

```markdown
- Optional database encryption: SQLCipher whole-file encryption, keyed by
  a passphrase you set in Options → Security. Off by default; you can turn
  it on and off again at any time. The passphrase can be remembered in the
  system keyring so launches stay silent, and there is no way to recover a
  forgotten one
```

- [ ] **Step 9: Commit**

```bash
git add crates/fastpaste-gui/ crates/fastpaste-data/ README.md
git commit -m "Add the Security options page"
```

---

## Notes for the reviewer

- **Tasks 3-5 are the security-critical ones.** They are the only places a passphrase meets a file. Review them against the spec's section 2 and section 5 rather than against intuition.
- **Task 5's `convert` never touches the original until the replacement is verified.** If a reviewer sees a code path that writes to `path` before the `rename`, that is a bug.
- **No plaintext backup, anywhere.** A `.bak` alongside an encrypted database defeats the entire feature. The test `encrypting_leaves_no_plaintext_copy_behind` exists to catch a well-meaning future addition.
- **Task 9 depends on Task 1's answer.** If the spike was refuted and the fallback was used, the loop-handling code will not match what is written here, and that is correct.
