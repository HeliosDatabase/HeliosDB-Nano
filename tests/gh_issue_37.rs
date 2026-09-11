//! GH #37 — a data directory the process cannot write produces a raw RocksDB
//! lock error with no hint about ownership or permissions.
//!
//! Land as `tests/gh_issue_37.rs`.
//!
//! # What is broken
//!
//! `StorageEngine::open_with_mode` (src/storage/engine.rs:2100) ends with the
//! ONLY two production data-directory opens in the crate:
//!
//! ```ignore
//! // engine.rs:2160-2161  (read-only twin)
//! DB::open_for_read_only(&opts, path, false)
//!     .map_err(|e| Error::storage(format!("Failed to open RocksDB read-only: {}", e)))?
//! // engine.rs:2163
//! DB::open(&opts, path).map_err(|e| Error::storage(format!("Failed to open RocksDB: {}", e)))?
//! ```
//!
//! Nothing between engine.rs:2101 and engine.rs:2159 touches the filesystem, so
//! the ONLY thing an operator ever sees is whatever librocksdb-sys emitted from
//! `PosixFileSystem::LockFile` (rocksdb/env/fs_posix.cc) rendered by `IOError`
//! (rocksdb/env/io_posix.cc):
//!
//! * `EACCES` on `open(LOCK, O_RDWR|O_CREAT)` →
//!   `IO error: while open a file for lock: <dir>/LOCK: Permission denied`
//! * a second open of the same directory by the same process →
//!   `IO error: lock hold by current process, acquire time … acquiring thread …: No locks available`
//! * a cross-process `fcntl` conflict →
//!   `IO error: While lock file: <dir>/LOCK: Resource temporarily unavailable`
//!
//! The first of those is what a 3.x → 4.x container upgrade hits: the 4.x image
//! sets `USER heliosdb` (Dockerfile.binary:15) so the server runs as a system
//! uid (999 on debian trixie), while a 3.x volume is owned by uid 0. It reads
//! as store corruption when it is a `chown`. There is no `geteuid`, no `stat`
//! of the data dir, and no `chown`/`chmod` string anywhere in `src/` today.
//!
//! Because engine.rs:2163 is the single choke point, the missing diagnostic is
//! missing for EVERY surface: `heliosdb-nano start`, `init`, `repl`, `dump`,
//! `restore`, `EmbeddedDatabase::new` / `with_config` / `open_read_only`, and
//! the Python binding. A tarball or `cargo install` user hits exactly the same
//! wall as a Docker user, which is why the fix belongs in the binary and not
//! only in the image.
//!
//! # Executor families
//!
//! This defect is in store OPEN, upstream of both DML executor families
//! (`db.execute()` → `execute_in_transaction_inner`, and `db.execute_params()`
//! → `execute_plan_with_params_inner`). There is no family-specific behaviour to
//! split. The persistence positive control below nevertheless drives BOTH
//! families across a reopen, so the file proves the harness exercises each one.
//!
//! # Why the uid-0-vs-uid-999 half is not tested here
//!
//! A test cannot create a directory owned by a *different* uid without root, so
//! the `owner_uid != process_uid` branch is covered by the pure-function matrix
//! in `tests/gh_issue_37_classify.rs`. What IS reachable unprivileged is the
//! other permission branch — a directory we own with the write bit removed —
//! and that goes through exactly the same failure path (engine.rs:2163) and the
//! same renderer.
//!
//! # Expected outcome
//!
//! FAIL on the current tree, PASS after the fix:
//!   * `unwritable_data_dir_error_names_the_cause_and_the_fix`
//!         — today's text is `Storage error: Failed to open RocksDB: IO error:
//!           while open a file for lock: <dir>/LOCK: Permission denied`.
//!           Assertions (a) name-the-dir, (b) says-permission and (e) keeps-the-
//!           raw-error already hold today; (c) `uid <N>` is the FIRST assertion
//!           that fails, then (d) chmod/chown. The test is therefore not
//!           vacuous: it fails for the reason the issue names.
//!   * `read_only_open_of_an_unreadable_data_dir_also_gets_the_diagnostic`
//!         — pins engine.rs:2161, the twin call site the fix must not forget.
//!   * `lock_held_by_this_process_is_not_misreported_as_a_permission_problem`
//!         — today the message never says the store is already open.
//!
//! PASS before AND after (controls; if any of these fails the file is worthless):
//!   * `positive_control_in_memory_database_round_trips`
//!   * `positive_control_writable_data_dir_opens_and_persists_on_both_executor_families`
//!   * `permission_diagnostic_is_not_sticky`
//!   * `corrupt_store_is_not_misdiagnosed_as_an_ownership_problem`
//!     (anti-false-positive: an unrelated failure must never grow chown advice)
//!
//! Both permission tests SKIP (loudly, with a printed reason) when the test
//! process is root or holds CAP_DAC_OVERRIDE, because mode bits do not apply
//! there. That skip is announced, never silent.

#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use heliosdb_nano::{EmbeddedDatabase, Value};

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// A scratch directory that removes itself, restoring modes first so a 0o500 /
/// 0o000 directory left behind by a failed assertion does not leak.
struct Scratch {
    root: PathBuf,
}

impl Scratch {
    fn new(tag: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "hdb_gh37_{}_{}_{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        fs::create_dir_all(&root).expect("create scratch root");
        Self { root }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // Restore write+search on every directory under the root, otherwise
        // remove_dir_all cannot descend into the 0o500 / 0o000 cases.
        restore_modes(&self.root);
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn restore_modes(dir: &Path) {
    let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            if entry.path().is_dir() {
                restore_modes(&entry.path());
            }
        }
    }
}

/// This process's effective uid, without a `libc` dependency in the test: a
/// freshly created file is owned by the creating process's euid.
fn process_uid(scratch: &Scratch) -> u32 {
    let probe = scratch.path(".uid_probe");
    fs::write(&probe, b"x").expect("write uid probe");
    let uid = fs::metadata(&probe).expect("stat uid probe").uid();
    let _ = fs::remove_file(&probe);
    uid
}

/// True when mode bits do not constrain this process (root, or CAP_DAC_OVERRIDE):
/// creating a file inside a mode-0o500 directory still succeeds.
fn mode_bits_are_ignored(dir: &Path) -> bool {
    let probe = dir.join(".dac_probe");
    match fs::File::create(&probe) {
        Ok(_) => {
            let _ = fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// Create an empty directory the process owns but cannot write into.
fn unwritable_dir(scratch: &Scratch, name: &str) -> PathBuf {
    let dir = scratch.path(name);
    fs::create_dir_all(&dir).expect("create data dir");
    // r-x------ : we can traverse and list it, but not create LOCK/CURRENT/LOG.
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o500)).expect("chmod 0500");
    dir
}

fn err_text(path: &Path) -> String {
    match EmbeddedDatabase::new(path) {
        Ok(_) => panic!(
            "EmbeddedDatabase::new({}) unexpectedly SUCCEEDED — the fixture did not \
             reproduce a store-open failure, so nothing below would be meaningful. \
             Fix the fixture; do not weaken the assertions.",
            path.display()
        ),
        Err(e) => e.to_string(),
    }
}

fn assert_contains(haystack: &str, needle: &str, why: &str) {
    assert!(
        haystack.contains(needle),
        "{why}\n  expected the startup error to contain: {needle:?}\n  actual error was:\n    {haystack}"
    );
}

/// The three properties every permission-shaped diagnostic must have, asserted
/// in one place so the read-only twin and the read-write path cannot drift.
fn assert_permission_diagnostic(text: &str, dir: &Path, uid: u32, what: &str) {
    let lower = text.to_ascii_lowercase();

    // (a) Name the directory the operator has to act on — not only the
    //     internal LOCK/CURRENT path. (Holds today: the raw RocksDB text
    //     embeds `<dir>/LOCK`.)
    assert_contains(
        text,
        &dir.display().to_string(),
        &format!("{what}: the diagnostic must name the data directory"),
    );

    // (b) Say, in words, that this is a permissions/ownership problem.
    //     (Holds today via the literal "Permission denied".)
    assert!(
        lower.contains("permission") || lower.contains("not writable") || lower.contains("ownership"),
        "{what}: the diagnostic must state that the data directory is not writable by this \
         process; got:\n    {text}"
    );

    // (c) THE ISSUE. Name the uid the process runs as. "uid 999 vs uid 0" is
    //     exactly the information the raw RocksDB error withholds. FAILS TODAY.
    assert_contains(
        text,
        &format!("uid {uid}"),
        &format!("{what}: the diagnostic must name the uid this process runs as"),
    );

    // (d) Print an executable remedy. FAILS TODAY.
    assert!(
        text.contains("chown") || text.contains("chmod"),
        "{what}: the diagnostic must print the exact command that fixes it (chown/chmod); \
         got:\n    {text}"
    );

    // (e) Lose nothing: the underlying store error stays in the message so a
    //     support ticket keeps the original evidence. Holds today; must keep
    //     holding after the fix.
    assert!(
        lower.contains("permission denied") || lower.contains("io error"),
        "{what}: the underlying store error must still be reported alongside the diagnostic; \
         got:\n    {text}"
    );
}

// ---------------------------------------------------------------------------
// 0. Positive controls — these pass on the unfixed tree and after the fix.
//    If either of them fails, the harness itself is broken and every verdict
//    below is worthless.
// ---------------------------------------------------------------------------

#[test]
fn positive_control_in_memory_database_round_trips() {
    let db = EmbeddedDatabase::new_in_memory().expect("in-memory database");
    db.execute("CREATE TABLE t (id INT PRIMARY KEY, v VARCHAR(32))")
        .expect("create");
    db.execute("INSERT INTO t (id, v) VALUES (1, 'a')").expect("insert");
    let rows = db.query("SELECT v FROM t WHERE id = 1", &[]).expect("select");
    assert_eq!(rows.len(), 1, "one row expected");
    assert_eq!(rows[0].values[0], Value::String("a".into()), "round-trip value");
}

/// Drives BOTH executor families across a close/reopen of a real on-disk data
/// directory, so the file demonstrably exercises the params family (the one the
/// PostgreSQL EXTENDED protocol and the REST layer use) as well as the text one.
#[test]
fn positive_control_writable_data_dir_opens_and_persists_on_both_executor_families() {
    let scratch = Scratch::new("ok");
    let dir = scratch.path("good");

    {
        let db = EmbeddedDatabase::new(&dir).expect("a writable data dir must open");
        db.execute("CREATE TABLE t (id INT PRIMARY KEY, v VARCHAR(32))")
            .expect("create");
        // text family
        db.execute("INSERT INTO t (id, v) VALUES (7, 'seven')")
            .expect("insert (text family)");
        // params family
        db.execute_params(
            "INSERT INTO t (id, v) VALUES ($1, $2)",
            &[Value::Int4(8), Value::String("eight".into())],
        )
        .expect("insert (params family)");
    }

    let db = EmbeddedDatabase::new(&dir).expect("reopen");
    let rows = db.query("SELECT v FROM t WHERE id = 7", &[]).expect("select (text)");
    assert_eq!(rows.len(), 1, "the text-family row must survive the reopen");
    assert_eq!(rows[0].values[0], Value::String("seven".into()));

    let rows = db
        .query_params("SELECT v FROM t WHERE id = $1", &[Value::Int4(8)])
        .expect("select (params)");
    assert_eq!(rows.len(), 1, "the params-family row must survive the reopen");
    assert_eq!(rows[0].values[0], Value::String("eight".into()));
}

// ---------------------------------------------------------------------------
// 1. THE REPRO — a data directory this process cannot write.
//    Same failure path (engine.rs:2163) and same renderer as the uid-999 /
//    uid-0 Docker case in the issue.
// ---------------------------------------------------------------------------

#[test]
fn unwritable_data_dir_error_names_the_cause_and_the_fix() {
    let scratch = Scratch::new("perm");
    let uid = process_uid(&scratch);
    let dir = unwritable_dir(&scratch, "locked");

    if mode_bits_are_ignored(&dir) {
        eprintln!(
            "SKIP gh_issue_37::unwritable_data_dir_error_names_the_cause_and_the_fix — \
             this process (uid {uid}) bypasses directory mode bits (root / CAP_DAC_OVERRIDE), \
             so a 0o500 directory cannot be made unwritable for it. Run the suite as a \
             non-root user to exercise this test."
        );
        return;
    }

    let text = err_text(&dir);
    assert_permission_diagnostic(&text, &dir, uid, "read-write open");

    // (f) The advice must fit the branch. WE own this directory, so telling the
    //     operator to `chown -R` it is wrong advice that fixes nothing. This
    //     pins the fix against the lazy shape "always print the chown line".
    assert!(
        !text.contains("chown -R"),
        "we already own {}: a recursive chown is the wrong remedy for a missing owner write \
         bit — the message must say chmod; got:\n    {text}",
        dir.display()
    );

    // (g) Fail-closed: the open must still ERROR. The fix is a diagnosis, never
    //     a "continue anyway" or a silent fallback to a fresh store.
    assert!(
        EmbeddedDatabase::new(&dir).is_err(),
        "opening a store we cannot lock must never silently succeed"
    );
}

// ---------------------------------------------------------------------------
// 1b. The read-only twin, engine.rs:2160-2161. `EmbeddedDatabase::open_read_only`
//     is public and is used by a2h's concurrent manifest reader, so a fix that
//     only touches the read-write arm leaves half the choke point raw.
// ---------------------------------------------------------------------------

#[test]
fn read_only_open_of_an_unreadable_data_dir_also_gets_the_diagnostic() {
    let scratch = Scratch::new("ro");
    let uid = process_uid(&scratch);
    let dir = scratch.path("store");

    // Build a real, healthy store first, then close it.
    {
        let db = EmbeddedDatabase::new(&dir).expect("create store");
        db.execute("CREATE TABLE t (id INT PRIMARY KEY)").expect("create");
    }

    // Remove every permission bit: no traverse, so even reading CURRENT fails
    // with EACCES for an unprivileged process.
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o000)).expect("chmod 0000");

    if fs::read_dir(&dir).is_ok() {
        eprintln!(
            "SKIP gh_issue_37::read_only_open_of_an_unreadable_data_dir_also_gets_the_diagnostic \
             — this process (uid {uid}) bypasses directory mode bits (root / CAP_DAC_OVERRIDE)."
        );
        let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(0o700));
        return;
    }

    let text = match EmbeddedDatabase::open_read_only(&dir) {
        Ok(_) => panic!(
            "open_read_only({}) succeeded on a mode-0000 directory — the fixture did not \
             reproduce a failure",
            dir.display()
        ),
        Err(e) => e.to_string(),
    };

    // Guard against a RocksDB build that reports this as something other than
    // EACCES: skip loudly rather than assert on a message we cannot classify.
    if !text.to_ascii_lowercase().contains("permission") {
        eprintln!(
            "SKIP gh_issue_37::read_only_open_of_an_unreadable_data_dir_also_gets_the_diagnostic \
             — the store did not report a permission failure; got:\n    {text}"
        );
        let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(0o700));
        return;
    }

    assert_permission_diagnostic(&text, &dir, uid, "read-only open");

    let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(0o700));
}

// ---------------------------------------------------------------------------
// 2. Anti-misdiagnosis — a lock genuinely held by a live handle must NOT be
//    reported as an ownership problem. This is the failure mode a naive fix
//    introduces (stat the dir, blame ownership, tell the operator to chown a
//    perfectly good directory while another server is running on it).
// ---------------------------------------------------------------------------

#[test]
fn lock_held_by_this_process_is_not_misreported_as_a_permission_problem() {
    let scratch = Scratch::new("lock");
    let dir = scratch.path("held");

    // First handle takes the RocksDB lock and keeps it for the whole test.
    let _held = EmbeddedDatabase::new(&dir).expect("first open must succeed");

    // librocksdb-sys refuses a second open of the same path from the same
    // process (fs_posix.cc `locked_files` set, errno ENOLCK).
    let text = match EmbeddedDatabase::new(&dir) {
        Ok(_) => panic!(
            "a second EmbeddedDatabase::new({}) succeeded while the first handle is alive. \
             That is itself a defect — HeliosDB Nano allows exactly one writer per data \
             directory and the RocksDB single-writer lock did not engage.",
            dir.display()
        ),
        Err(e) => e.to_string(),
    };
    let lower = text.to_ascii_lowercase();

    // (a) Must say the store is already open. FAILS TODAY (the raw text is
    //     "IO error: lock hold by current process … No locks available").
    assert!(
        lower.contains("already open")
            || lower.contains("already in use")
            || lower.contains("another process")
            || lower.contains("in use by"),
        "a held lock must be reported as 'the store is already open', not as a raw RocksDB \
         lock error; got:\n    {text}"
    );

    // (b) Must NOT send the operator off to chown a directory that is fine.
    //     PASSES TODAY and must keep passing — this is the anti-misdiagnosis pin.
    assert!(
        !lower.contains("chown"),
        "a held lock must NOT be misdiagnosed as an ownership problem — suggesting chown here \
         would have an operator rewrite the ownership of a live store; got:\n    {text}"
    );

    // (c) Must name the directory. (Holds today via `<dir>/LOCK` in the raw text.)
    assert_contains(
        &text,
        &dir.display().to_string(),
        "the diagnostic must name the data directory",
    );
}

// ---------------------------------------------------------------------------
// 3. Anti-false-positive — a failure that is NOT about permissions must never
//    grow ownership advice. Passes before and after the fix; it is what stops
//    the implementer from unconditionally appending the chown paragraph.
// ---------------------------------------------------------------------------

#[test]
fn corrupt_store_is_not_misdiagnosed_as_an_ownership_problem() {
    let scratch = Scratch::new("corrupt");
    let dir = scratch.path("store");

    {
        let db = EmbeddedDatabase::new(&dir).expect("create store");
        db.execute("CREATE TABLE t (id INT PRIMARY KEY)").expect("create");
    }

    // The directory stays perfectly owned and writable; only the store content
    // is broken.
    fs::write(dir.join("CURRENT"), b"not-a-manifest-name\n").expect("clobber CURRENT");

    let text = match EmbeddedDatabase::new(&dir) {
        Ok(_) => {
            eprintln!(
                "SKIP gh_issue_37::corrupt_store_is_not_misdiagnosed_as_an_ownership_problem — \
                 the store tolerated a clobbered CURRENT, so there is no unrelated failure to \
                 classify."
            );
            return;
        }
        Err(e) => e.to_string(),
    };
    let lower = text.to_ascii_lowercase();

    assert!(
        !lower.contains("chown"),
        "a corrupt store on a healthy, self-owned directory must NOT be diagnosed as an \
         ownership problem — that would send the operator to a command that cannot help and \
         hide the real cause; got:\n    {text}"
    );
    assert!(
        !lower.contains("owned by uid"),
        "a corrupt store must not be blamed on ownership; got:\n    {text}"
    );
    // The real error must still reach the operator.
    assert!(
        lower.contains("corrupt")
            || lower.contains("no such file")
            || lower.contains("io error")
            || lower.contains("manifest"),
        "the underlying store error must survive; got:\n    {text}"
    );
}

// ---------------------------------------------------------------------------
// 4. Control — the diagnostic is a diagnosis, not a new refusal. Once the mode
//    is fixed the very same directory opens. Passes before and after the fix;
//    it exists to prove the fix does not turn "unwritable" into a permanent
//    poison flag on the directory.
// ---------------------------------------------------------------------------

#[test]
fn permission_diagnostic_is_not_sticky() {
    let scratch = Scratch::new("recover");
    let uid = process_uid(&scratch);
    let dir = unwritable_dir(&scratch, "fixable");

    if mode_bits_are_ignored(&dir) {
        eprintln!(
            "SKIP gh_issue_37::permission_diagnostic_is_not_sticky — this process (uid {uid}) \
             bypasses directory mode bits (root / CAP_DAC_OVERRIDE)."
        );
        return;
    }

    assert!(
        EmbeddedDatabase::new(&dir).is_err(),
        "precondition: a 0o500 data directory must fail to open"
    );

    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).expect("chmod 0700");

    let db = EmbeddedDatabase::new(&dir).expect("after chmod the same directory must open");
    db.execute("CREATE TABLE t (id INT PRIMARY KEY)").expect("create");
    db.execute("INSERT INTO t (id) VALUES (1)").expect("insert");
    assert_eq!(
        db.query("SELECT id FROM t", &[]).expect("select").len(),
        1,
        "the recovered store must be fully usable"
    );
}
