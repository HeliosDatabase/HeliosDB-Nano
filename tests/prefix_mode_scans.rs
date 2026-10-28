//! Spec P1.1b, part 2: no scan of the engine's store walks across key
//! families with prefix-mode read options.
//!
//! # Why
//!
//! The engine opens RocksDB with a 5-byte fixed prefix extractor over full
//! bloom filters (`StorageEngine` open). An iterator with the default read
//! options is then a *prefix-mode* iterator, defined only within one 5-byte
//! prefix. Walked across families it loses keys silently: when it steps onto a
//! key a range tombstone covers, RocksDB re-seeks the older files to the
//! tombstone's end, and every file whose prefix filter holds no key with that
//! end's prefix is dropped from the scan — with every later key it held (the
//! mechanism `SnapshotManager::recover_snapshots_with_policy` documents, and
//! `recovery_reads_every_record_past_a_range_tombstone` reproduces). The open
//! path's `load_counters` and the logical-WAL TRUNCATE replay were such walks:
//! after a range deletion a reopened table could lose its row counter (row ids
//! restarting at 1 under the history TRUNCATE keeps), and a replayed TRUNCATE
//! could leave rows behind.
//!
//! # What
//!
//! * Behaviour, red before the fix: the two shapes above, each on a store laid
//!   out as the reproduction needs — the engine's keys in older files, one key
//!   of a family that sorts before them in a newer file, and a range tombstone
//!   over that key in the newest (auto-compaction off, so each flush stays a
//!   file of its own).
//! * A tripwire: every `IteratorMode::Start` / `IteratorMode::End` scan under
//!   `src/` must be listed in [`SITES`] with its verdict, and the verdict is
//!   checked against the code — [`Scan::TotalOrder`] means the call is
//!   `iterator_opt` / `iterator_cf_opt` with read options the enclosing
//!   function sets `set_total_order_seek(true)` on; [`Scan::OwnStore`] means
//!   the file opens its own RocksDB store, with no prefix extractor. A new
//!   unlisted site fails, naming its `file:line` and function; so does a stale
//!   entry. The scans that seek to their family (`IteratorMode::From`) are not
//!   in scope: they stay inside one prefix of at least five bytes, or are
//!   total-order (the audit in the spec P1.1b report lists them).
//!
//! # Controls
//!
//! * `the_detector_reports_each_scan_with_its_function_and_options` runs the
//!   detector on crafted source: comments and strings are not sites, test code
//!   is, and the total-order check reads the options of the call itself.
//! * `the_scan_reaches_the_known_sites` requires the walk to see `src/` and
//!   the sites known to exist, so a broken walk cannot pass by seeing nothing.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use heliosdb_nano::config::DestructiveDdlReplay;
use heliosdb_nano::storage::{WalOperation, WalSyncMode, WriteAheadLog, WriteGate};
use heliosdb_nano::{Config, EmbeddedDatabase};
use regex::Regex;
use rocksdb::{Options, WriteBatch, DB};
use std::path::{Path, PathBuf};
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Behaviour
// ---------------------------------------------------------------------------

/// A persistent primary whose RocksDB never compacts on file count, so the
/// layout `cover_with_a_range_tombstone` builds survives the reopen.
fn config(path: &Path) -> Config {
    let mut config = Config::default();
    config.storage.path = Some(path.to_path_buf());
    config.storage.memory_only = false;
    config.storage.wal_enabled = true;
    config.storage.rocksdb_level0_file_num_compaction_trigger = Some(1_000);
    config
}

/// A raw handle on a CLOSED store, opened the way the engine opens its own in
/// the two respects that decide how a scan behaves (the 5-byte prefix
/// extractor over full bloom filters — the files RocksDB writes while
/// recovering the engine's WAL here get them too), with compaction off.
/// Retried: the engine's background threads release the lock asynchronously.
fn open_raw(path: &Path) -> Arc<DB> {
    let mut block = rocksdb::BlockBasedOptions::default();
    block.set_bloom_filter(14.0, false);
    block.set_whole_key_filtering(true);
    let mut options = Options::default();
    options.create_if_missing(false);
    options.set_prefix_extractor(rocksdb::SliceTransform::create_fixed_prefix(5));
    options.set_block_based_table_factory(&block);
    options.set_disable_auto_compactions(true);
    let mut last = None;
    for _ in 0..100 {
        match DB::open(&options, path) {
            Ok(db) => return Arc::new(db),
            Err(e) => {
                last = Some(e);
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
    }
    panic!("raw RocksDB open failed: {last:?}");
}

/// A columnar sidecar of a table that no longer exists — a family (`col:`)
/// that sorts before `counter:` and `data:` — and the range deletion a purge
/// of it would write.
const COVERED: &[u8] = b"col:gone:c:0";
const COVERED_FROM: &[u8] = b"col:gone:";
const COVERED_TO: &[u8] = b"col:gone;";

/// Everything the engine wrote goes to files older than two more: one holding
/// [`COVERED`], and the newest holding a range tombstone over it. A scan that
/// steps onto [`COVERED`] re-seeks the older files to [`COVERED_TO`]; none of
/// the engine's files holds a key with that prefix (`col:g`).
fn cover_with_a_range_tombstone(db: &DB) {
    db.flush().unwrap();
    db.put(COVERED, b"a batch of a dropped table").unwrap();
    db.flush().unwrap();
    let mut range = WriteBatch::default();
    range.delete_range(COVERED_FROM, COVERED_TO);
    db.write(range).unwrap();
    db.flush().unwrap();
}

/// TRUNCATE leaves no row to reseed a counter from, so the durable
/// `counter:{t}` is the only record of the ids already handed out (and the
/// `v:` history TRUNCATE keeps is filed under them). Red before the fix:
/// `load_counters` walked from the start of the keyspace in prefix mode and
/// lost `counter:t` past the tombstone, so the next row reused id 1.
#[test]
fn truncate_then_reopen_keeps_the_row_counter_past_a_range_tombstone() {
    let dir = tempfile::tempdir().unwrap();
    let high_water = {
        let db = EmbeddedDatabase::with_config(config(dir.path())).unwrap();
        db.execute("CREATE TABLE t (id INT PRIMARY KEY, v TEXT)").unwrap();
        db.execute("INSERT INTO t VALUES (1, 'a'), (2, 'b'), (3, 'c')").unwrap();
        db.execute("TRUNCATE TABLE t").unwrap();
        assert!(db.query("SELECT id FROM t", &[]).unwrap().is_empty());
        let mark = db
            .storage
            .row_counter_high_water("t")
            .expect("the inserts moved the counter");
        assert!(mark >= 3, "vacuity: three rows were inserted, counter at {mark}");
        mark
    };
    {
        let raw = open_raw(dir.path());
        assert!(
            raw.get(b"counter:t").unwrap().is_some(),
            "vacuity: the clean close persisted the counter"
        );
        cover_with_a_range_tombstone(&raw);
    }

    let db = EmbeddedDatabase::with_config(config(dir.path())).unwrap();
    assert_eq!(
        db.storage.row_counter_high_water("t"),
        Some(high_water),
        "the reopen lost the row counter of a truncated table"
    );
    db.execute("INSERT INTO t VALUES (4, 'd')").unwrap();
    let rows = db.storage.scan_table("t").unwrap();
    assert_eq!(rows.len(), 1);
    let row_id = rows[0].row_id.expect("a stored row has an id");
    assert!(
        row_id > high_water,
        "the row inserted after the reopen reused row id {row_id} (high water {high_water})"
    );
}

/// The crash window logical-WAL redo exists for: a TRUNCATE whose entry is
/// durable above the checkpoint while its row deletes never landed. Replayed
/// (with the destructive-DDL policy at `apply`), it must delete every row. Red
/// before the fix: the replay arm walked from the start of the keyspace in
/// prefix mode, lost the rows' file past the tombstone, and left all of them.
#[test]
fn a_replayed_truncate_deletes_every_row_past_a_range_tombstone() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = EmbeddedDatabase::with_config(config(dir.path())).unwrap();
        db.execute("CREATE TABLE t (id INT PRIMARY KEY, v TEXT)").unwrap();
        db.execute("INSERT INTO t VALUES (1, 'a'), (2, 'b'), (3, 'c')").unwrap();
        assert_eq!(db.query("SELECT id FROM t", &[]).unwrap().len(), 3);
    }
    {
        let raw = open_raw(dir.path());
        let checkpoint = raw
            .get(b"wal:checkpoint")
            .unwrap()
            .map(|bytes| u64::from_le_bytes(bytes.as_slice().try_into().expect("an 8-byte checkpoint")))
            .expect("vacuity: the clean close stamped the logical-WAL checkpoint");
        let wal = WriteAheadLog::open(Arc::clone(&raw), WriteGate::open(), WalSyncMode::Sync).unwrap();
        let lsn = wal.append(WalOperation::Truncate { table: "t".into() }).unwrap();
        assert!(lsn > checkpoint, "vacuity: the entry is redo ({lsn} > {checkpoint})");
        drop(wal);
        cover_with_a_range_tombstone(&raw);
    }

    let mut replaying = config(dir.path());
    replaying.storage.wal_replay_destructive_ddl = DestructiveDdlReplay::Apply;
    let db = EmbeddedDatabase::with_config(replaying).unwrap();
    assert!(
        db.query("SELECT id FROM t", &[]).unwrap().is_empty(),
        "rows of 't' survived the replayed TRUNCATE"
    );
}

// ---------------------------------------------------------------------------
// The tripwire: every scan from either end of the keyspace, with its verdict
// ---------------------------------------------------------------------------

/// Why a scan from the start (or end) of the keyspace is correct.
#[derive(Debug, Clone, Copy)]
enum Scan {
    /// Total-order read options: checked against the code.
    TotalOrder,
    /// The file opens a RocksDB store of its own, without a prefix
    /// extractor; the scan never reads the engine's store. Checked.
    OwnStore(&'static str),
}

/// `(file, enclosing function, sites in it, verdict)`. Keep it short: each
/// entry is a scan that reads every family between the keyspace's start and
/// the keys it wants.
const SITES: &[(&str, &str, usize, Scan)] = &[
    // TRUNCATE's key collection, `data:{t}:` only (it stops past `d`).
    ("src/sql/executor/ddl.rs", "handle_truncate", 1, Scan::TotalOrder),
    // ALTER TABLE's in-place row rewrites, `data:{t}:` only.
    ("src/storage/engine.rs", "migrate_column_storage", 1, Scan::TotalOrder),
    ("src/storage/engine.rs", "add_column_to_rows", 1, Scan::TotalOrder),
    ("src/storage/engine.rs", "drop_column_from_rows", 1, Scan::TotalOrder),
    // `AS OF` row-id collection, `data:{t}:` only.
    (
        "src/storage/engine_timetravel_extension.rs",
        "scan_table_at_snapshot",
        1,
        Scan::TotalOrder,
    ),
    // Open-time recovery reads two families (`snapshot:`, `vmeta:`) in one pass.
    (
        "src/storage/time_travel.rs",
        "recover_snapshots_with_policy",
        1,
        Scan::TotalOrder,
    ),
    // Spec P5.1 §4 step 4b: the promotion writer's reverse walk to the highest
    // retained `wal:entries:` key (both bounds set, total order).
    (
        "src/replication/physical.rs",
        "highest_logical_entry",
        1,
        Scan::TotalOrder,
    ),
    // The sync change log (`src/sync` is not compiled: `// pub mod sync;`).
    ("src/sync/change_log.rs", "query", 1, Scan::TotalOrder),
    ("src/sync/change_log.rs", "compact", 1, Scan::TotalOrder),
    ("src/sync/change_log.rs", "get_stats", 1, Scan::TotalOrder),
    ("src/sync/change_log.rs", "reset", 1, Scan::TotalOrder),
    (
        "src/sync/offline_queue.rs",
        "drain",
        1,
        Scan::OwnStore("OfflineQueue's private queue store, Options::default(); src/sync is not compiled"),
    ),
    (
        "src/sync/offline_queue.rs",
        "count",
        1,
        Scan::OwnStore("OfflineQueue's private queue store, Options::default(); src/sync is not compiled"),
    ),
    (
        "src/sync/offline_queue.rs",
        "clear",
        1,
        Scan::OwnStore("OfflineQueue's private queue store, Options::default(); src/sync is not compiled"),
    ),
];

// --- Lexer: `code_only` and its helpers are tests/write_gate_funnel.rs's,
// --- verbatim (one rule for what is code in both tripwires).

fn is_ident_start(c: u8) -> bool {
    c == b'_' || c.is_ascii_alphabetic()
}

fn is_ident_continue(c: u8) -> bool {
    c == b'_' || c.is_ascii_alphanumeric()
}

/// Length of the UTF-8 sequence that starts with `lead`.
fn utf8_len(lead: u8) -> usize {
    match lead {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        _ => 4,
    }
}

/// Overwrite `out[from..to]` with spaces, keeping newlines (and so line numbers).
fn blank(out: &mut [u8], from: usize, to: usize) {
    let len = out.len();
    for byte in &mut out[from.min(len)..to.min(len)] {
        if *byte != b'\n' {
            *byte = b' ';
        }
    }
}

/// Index of the closing quote of a (non-raw) string whose body starts at `j`.
fn string_end(b: &[u8], mut j: usize) -> usize {
    while j < b.len() && b[j] != b'"' {
        j += if b[j] == b'\\' { 2 } else { 1 };
    }
    j.min(b.len())
}

/// `src` with every comment and every string, char and raw-string body
/// replaced by spaces. Same length and line structure as `src`.
fn code_only(src: &str) -> String {
    let b = src.as_bytes();
    let mut out = b.to_vec();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if b[i..].starts_with(b"//") {
            let end = b[i..].iter().position(|&x| x == b'\n').map_or(b.len(), |p| i + p);
            blank(&mut out, i, end);
            i = end;
        } else if b[i..].starts_with(b"/*") {
            let mut depth = 0usize;
            let mut j = i;
            while j < b.len() {
                if b[j..].starts_with(b"/*") {
                    depth += 1;
                    j += 2;
                } else if b[j..].starts_with(b"*/") {
                    depth -= 1;
                    j += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    j += 1;
                }
            }
            blank(&mut out, i, j);
            i = j;
        } else if c == b'\'' {
            // A char literal (`'x'`, `'\''`, `'é'`) or a lifetime / label (`'a`).
            match b.get(i + 1) {
                Some(b'\\') => {
                    let mut j = i + 3;
                    while j < b.len() && b[j] != b'\'' {
                        j += 1;
                    }
                    blank(&mut out, i + 1, j);
                    i = j + 1;
                }
                Some(&lead) => {
                    let after = i + 1 + utf8_len(lead);
                    if b.get(after) == Some(&b'\'') {
                        blank(&mut out, i + 1, after);
                        i = after + 1;
                    } else {
                        i += 1;
                    }
                }
                None => i += 1,
            }
        } else if c == b'"' {
            let end = string_end(b, i + 1);
            blank(&mut out, i + 1, end);
            i = end + 1;
        } else if is_ident_start(c) {
            let start = i;
            while i < b.len() && is_ident_continue(b[i]) {
                i += 1;
            }
            let ident = &b[start..i];
            if matches!(ident, b"r" | b"br" | b"cr") && matches!(b.get(i), Some(b'"' | b'#')) {
                let mut quote = i;
                while quote < b.len() && b[quote] == b'#' {
                    quote += 1;
                }
                // `r#ident` is a raw identifier, not a string.
                if b.get(quote) == Some(&b'"') {
                    let fence: Vec<u8> = std::iter::once(b'"')
                        .chain(std::iter::repeat(b'#').take(quote - i))
                        .collect();
                    let body = quote + 1;
                    let close = (body..b.len()).find(|&j| b[j..].starts_with(&fence)).unwrap_or(b.len());
                    blank(&mut out, body, close);
                    i = (close + fence.len()).min(b.len());
                }
            } else if matches!(ident, b"b" | b"c") && b.get(i) == Some(&b'"') {
                let end = string_end(b, i + 1);
                blank(&mut out, i + 1, end);
                i = end + 1;
            }
        } else {
            i += 1;
        }
    }
    String::from_utf8(out).expect("blanking only writes ASCII spaces over whole tokens")
}

fn line_of(code: &str, offset: usize) -> usize {
    code[..offset].bytes().filter(|&c| c == b'\n').count() + 1
}

/// Every `fn` item with a body: its name and the byte range of its body. The
/// body opens at the first `{` after the name outside brackets (a `;` there
/// first — a declaration — has none; `[u8; 4]` in a signature does not end it).
fn functions(code: &str) -> Vec<(String, std::ops::Range<usize>)> {
    let declaration = Regex::new(r"\bfn\s+([A-Za-z_][A-Za-z0-9_]*)").unwrap();
    let b = code.as_bytes();
    let mut out = Vec::new();
    for caps in declaration.captures_iter(code) {
        let name = caps.get(1).expect("function name");
        let mut j = name.end();
        let mut nesting = 0i32;
        while j < b.len() {
            match b[j] {
                b'(' | b'[' => nesting += 1,
                b')' | b']' => nesting -= 1,
                b'{' | b';' if nesting == 0 => break,
                _ => {}
            }
            j += 1;
        }
        if j >= b.len() || b[j] == b';' {
            continue;
        }
        let open = j;
        let mut depth = 0i32;
        while j < b.len() {
            match b[j] {
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                _ => {}
            }
            j += 1;
        }
        out.push((name.as_str().to_string(), open..j.min(b.len())));
    }
    out
}

/// One scan from an end of the keyspace.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Site {
    file: String,
    line: usize,
    /// The innermost enclosing function.
    function: Option<String>,
    /// `Start` or `End`.
    end: String,
    /// The call is `iterator_opt` / `iterator_cf_opt` and its read options are
    /// set total-order in the enclosing function.
    total_order: bool,
}

impl std::fmt::Display for Site {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}:{}: IteratorMode::{} in fn {}",
            self.file,
            self.line,
            self.end,
            self.function.as_deref().unwrap_or("<none>")
        )
    }
}

/// Every `IteratorMode::Start` / `IteratorMode::End` in the code of one file.
fn sites(file: &str, src: &str) -> Vec<Site> {
    let code = code_only(src);
    let token = Regex::new(r"IteratorMode\s*::\s*(Start|End)\b").unwrap();
    // The call the token is the mode argument of: `.iterator_opt(` or
    // `.iterator_cf_opt(cf, `, with an optional `rocksdb::` on the mode.
    let opt_call = Regex::new(
        r"\.\s*(?:iterator_opt|iterator_cf_opt)\s*\(\s*(?:&?\s*[A-Za-z_][A-Za-z0-9_.]*\s*,\s*)?(?:rocksdb\s*::\s*)?\z",
    )
    .unwrap();
    let options_argument = Regex::new(r"\A\s*,\s*([A-Za-z_][A-Za-z0-9_]*)\s*\)").unwrap();
    let bodies = functions(&code);
    let mut out = Vec::new();
    for caps in token.captures_iter(&code) {
        let whole = caps.get(0).expect("whole match");
        let start = whole.start();
        let function = bodies
            .iter()
            .filter(|(_, body)| body.contains(&start))
            .min_by_key(|(_, body)| body.len());
        let window = &code[start.saturating_sub(256)..start];
        let total_order = opt_call.is_match(window)
            && options_argument
                .captures(&code[whole.end()..])
                .and_then(|options| {
                    let name = options.get(1)?.as_str();
                    let (_, body) = function?;
                    let set = Regex::new(&format!(
                        r"\b{}\s*\.\s*set_total_order_seek\s*\(\s*true\s*\)",
                        regex::escape(name)
                    ))
                    .ok()?;
                    Some(set.is_match(&code[body.clone()]))
                })
                .unwrap_or(false);
        out.push(Site {
            file: file.to_string(),
            line: line_of(&code, start),
            function: function.map(|(name, _)| name.clone()),
            end: caps[1].to_string(),
            total_order,
        });
    }
    out
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// Every `.rs` file under `dir`, compiled or not: a scan in a module whose
/// `mod` line is commented out today is one a re-enable would compile.
fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap_or_else(|e| panic!("read_dir {}: {e}", dir.display())) {
        let path = entry.expect("directory entry").path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

/// `(files scanned, every site)`.
fn scan_src() -> (usize, Vec<Site>) {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    rust_files(&root.join("src"), &mut files);
    files.sort();
    let mut all = Vec::new();
    for path in &files {
        let file = path.strip_prefix(root).unwrap_or(path).display().to_string();
        all.extend(sites(&file, &read(path)));
    }
    (files.len(), all)
}

fn listed(site: &Site) -> Option<&'static (&'static str, &'static str, usize, Scan)> {
    SITES
        .iter()
        .find(|(file, function, _, _)| site.file == *file && site.function.as_deref() == Some(*function))
}

#[test]
fn every_scan_from_an_end_of_the_keyspace_is_listed_and_its_verdict_holds() {
    let (_, all) = scan_src();
    let unlisted: Vec<String> = all
        .iter()
        .filter(|s| listed(s).is_none())
        .map(ToString::to_string)
        .collect();
    assert!(
        unlisted.is_empty(),
        "scans from an end of the keyspace that no SITES entry justifies. A prefix-mode iterator is defined \
         only within one 5-byte prefix, and a range tombstone on the way drops whole files from such a walk: \
         seek to the family instead (`IteratorMode::From` + a starts_with break), or make the scan total-order \
         (`ReadOptions::set_total_order_seek(true)` + `iterator_opt`) and list it:\n{}",
        unlisted.join("\n")
    );
    let mut wrong = Vec::new();
    for site in &all {
        let Some((file, _, _, verdict)) = listed(site) else {
            continue;
        };
        match verdict {
            Scan::TotalOrder if !site.total_order => wrong.push(format!(
                "{site}: listed as total-order, but the call is not `iterator_opt` / `iterator_cf_opt` with read \
                 options set `set_total_order_seek(true)` in the function"
            )),
            Scan::OwnStore(reason) => {
                let code = code_only(&read(&Path::new(env!("CARGO_MANIFEST_DIR")).join(file)));
                if reason.is_empty() || code.contains("set_prefix_extractor") || !code.contains("DB::open") {
                    wrong.push(format!(
                        "{site}: listed as reading a store of its own, but the file does not open one without a \
                         prefix extractor"
                    ));
                }
            }
            Scan::TotalOrder => {}
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

#[test]
fn every_listed_site_still_exists() {
    let (_, all) = scan_src();
    for (file, function, count, _) in SITES {
        let found = all
            .iter()
            .filter(|s| s.file == *file && s.function.as_deref() == Some(*function))
            .count();
        assert_eq!(
            found, *count,
            "SITES lists {count} scan(s) in {file} fn {function}, the code has {found}: update the entry"
        );
    }
}

#[test]
fn the_scan_reaches_the_known_sites() {
    let (files, all) = scan_src();
    assert!(files > 300, "the walk of src/ found only {files} files");
    for (file, function) in [
        ("src/storage/time_travel.rs", "recover_snapshots_with_policy"),
        ("src/sql/executor/ddl.rs", "handle_truncate"),
        ("src/sync/offline_queue.rs", "drain"),
    ] {
        assert!(
            all.iter()
                .any(|s| s.file == file && s.function.as_deref() == Some(function)),
            "the detector missed the scan in {file} fn {function}"
        );
    }
}

#[test]
fn the_detector_reports_each_scan_with_its_function_and_options() {
    let src = r##"
fn total(db: &DB) {
    let mut read = ReadOptions::default();
    read.set_total_order_seek(true);
    for item in db.iterator_opt(IteratorMode::Start, read) {}
}
fn prefix_mode(db: &DB) {
    for item in db.iterator(rocksdb::IteratorMode::Start) {}
}
fn other_options(db: &DB, cf: &ColumnFamily) {
    let mut unrelated = ReadOptions::default();
    unrelated.set_total_order_seek(true);
    let read = ReadOptions::default();
    let _ = db.iterator_cf_opt(cf, IteratorMode::End, read);
}
fn sized(x: [u8; 4]) -> usize {
    // db.iterator(IteratorMode::Start) in a comment
    let text = "IteratorMode::Start";
    x.len()
}
fn declared(db: &DB);
#[cfg(test)]
mod tests {
    fn in_test(db: &DB) {
        let _ = db.iterator(IteratorMode::Start);
    }
}
"##;
    let found: Vec<(usize, Option<String>, String, bool)> = sites("crafted.rs", src)
        .into_iter()
        .map(|s| (s.line, s.function, s.end, s.total_order))
        .collect();
    assert_eq!(
        found,
        vec![
            (5, Some("total".to_string()), "Start".to_string(), true),
            (8, Some("prefix_mode".to_string()), "Start".to_string(), false),
            (14, Some("other_options".to_string()), "End".to_string(), false),
            (25, Some("in_test".to_string()), "Start".to_string(), false),
        ]
    );
}
