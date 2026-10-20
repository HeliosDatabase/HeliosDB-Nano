//! Tripwire C (spec P0.2): no raw RocksDB write outside the `WriteGate`.
//!
//! # Why
//!
//! A physical standby's default column family must stay a byte-for-byte copy
//! of its primary's. The engine decides once, at open, whether it may write
//! (`OpenMode` → `storage::WriteGate`), and every storage component that holds
//! the raw `Arc<DB>` writes through that gate. A raw `db.put(..)` added anywhere
//! later would bypass the decision silently — on a standby it would move the
//! sequence number and poison the replica. This test turns that into a failure
//! naming the call's `file:line`.
//!
//! # How
//!
//! * The scanned files are the ones the crate compiles: a walk of `mod name;`
//!   declarations from `src/lib.rs` and `src/main.rs`. A raw write in a file no
//!   build compiles (today: `src/sync`, `src/wasm`, …, whose `mod` lines are
//!   commented out) is not a write; re-enabling such a module brings its files
//!   into the walk, and this test then names their raw writes.
//! * Each file is lexed down to code only — comments, string, char and raw
//!   string contents are blanked, byte offsets and newlines kept — so a call
//!   inside a comment or a message cannot match and a multi-line method chain
//!   (`self.db\n    .put(..)`) still does.
//! * Items under `#[cfg(test)]` or `#[cfg(all(test, ..))]` are skipped (a brace
//!   scan from the attribute); so are module files declared under them.
//! * A call is a raw write when a [`WRITE_METHODS`] method with at least one
//!   argument is called on a receiver whose last segment is `db` (`db`,
//!   `self.db`, `storage.db`, `engine.db`, `self.storage.db`, or the `db()`
//!   accessor), or on a name that file's non-test code declares with a RocksDB
//!   handle type (`name: DB`, `&DB`, `Arc<DB>`, `rocksdb::DB`). A zero-argument
//!   `.write()` is a lock, not a batch write.
//!
//! # Controls
//!
//! * `the_detector_reports_every_raw_write_shape_and_nothing_else` runs the
//!   detector on crafted source: every raw shape is reported with its line, and
//!   the gated call, the lock, the comment, the string and the test modules are
//!   not.
//! * `the_scan_reaches_the_storage_layer_and_finds_the_known_raw_writes`: the
//!   walk must reach the files that hold the gated components, and the detector
//!   must report the raw writes that exist by design in the excluded files —
//!   so a broken walk or a dead detector cannot pass by seeing nothing.
//! * Every [`EXCLUDED`] entry must name a compiled source, so a renamed module
//!   cannot leave a stale exemption behind.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use regex::Regex;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Files allowed to write raw, each with the reason. Keep this short: every
/// entry is a place the gate cannot protect.
const EXCLUDED: &[(&str, &str)] = &[
    (
        "src/storage/write_gate.rs",
        "the gate itself — the one place a raw write is the point",
    ),
    (
        "src/replication/physical.rs",
        "the physical applier, a standby's only writer; it becomes the `LocalWriter` (P0.3), which owns the \
         cursor invariant instead",
    ),
    (
        "src/replication/checkpoint_transfer.rs",
        "operates on staging databases it opened itself, never on an engine's store",
    ),
];

/// Every `rocksdb::DB` method that changes the store's keys. Longest names
/// first, so the alternation never stops at a shorter prefix.
const WRITE_METHODS: &[&str] = &[
    "ingest_external_file_cf_opts",
    "ingest_external_file_opts",
    "ingest_external_file_cf",
    "ingest_external_file",
    "delete_file_in_range_cf",
    "delete_file_in_range",
    "delete_range_cf_opt",
    "delete_range_opt",
    "delete_range_cf",
    "delete_range",
    "write_without_wal",
    "delete_cf_opt",
    "merge_cf_opt",
    "put_cf_opt",
    "delete_opt",
    "delete_cf",
    "write_opt",
    "merge_opt",
    "merge_cf",
    "put_opt",
    "put_cf",
    "delete",
    "merge",
    "write",
    "put",
];

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

/// `true` for every byte of `code` inside an item gated on `#[cfg(test)]` or
/// `#[cfg(all(test, ..))]`: from the attribute to the end of the item it
/// applies to (its closing brace, or its `;` when it has no body).
fn test_mask(code: &str) -> Vec<bool> {
    let attribute = Regex::new(r"#\s*\[\s*cfg\s*\(\s*(?:test\s*\)|all\s*\(\s*test\b)").unwrap();
    let b = code.as_bytes();
    let mut mask = vec![false; b.len()];
    for found in attribute.find_iter(code) {
        let mut j = found.start() + code[found.start()..].find('[').expect("attribute bracket");
        let mut depth = 0i32;
        while j < b.len() {
            match b[j] {
                b'[' => depth += 1,
                b']' => {
                    depth -= 1;
                    if depth == 0 {
                        j += 1;
                        break;
                    }
                }
                _ => {}
            }
            j += 1;
        }
        let mut braces = 0i32;
        let mut opened = false;
        while j < b.len() {
            match b[j] {
                b'{' => {
                    braces += 1;
                    opened = true;
                }
                b'}' => {
                    braces -= 1;
                    if opened && braces == 0 {
                        j += 1;
                        break;
                    }
                }
                b';' if !opened => {
                    j += 1;
                    break;
                }
                _ => {}
            }
            j += 1;
        }
        for flag in &mut mask[found.start()..j.min(b.len())] {
            *flag = true;
        }
    }
    mask
}

fn line_of(code: &str, offset: usize) -> usize {
    code[..offset].bytes().filter(|&c| c == b'\n').count() + 1
}

/// The last segment of the receiver of the method call whose `.` is at `dot`:
/// `db` for `self.storage.db.put(..)`, and for the accessor form `storage.db().put(..)`.
fn receiver_segment(code: &str, dot: usize) -> Option<&str> {
    let b = code.as_bytes();
    let mut end = dot;
    while end > 0 && b[end - 1].is_ascii_whitespace() {
        end -= 1;
    }
    if end > 0 && b[end - 1] == b')' {
        let mut open = end - 1;
        while open > 0 && b[open - 1].is_ascii_whitespace() {
            open -= 1;
        }
        if open == 0 || b[open - 1] != b'(' {
            return None;
        }
        end = open - 1;
        while end > 0 && b[end - 1].is_ascii_whitespace() {
            end -= 1;
        }
    }
    let mut start = end;
    while start > 0 && is_ident_continue(b[start - 1]) {
        start -= 1;
    }
    (start < end).then(|| &code[start..end])
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Hit {
    file: String,
    line: usize,
    call: String,
}

impl std::fmt::Display for Hit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}: {}", self.file, self.line, self.call)
    }
}

/// Every raw write in the non-test code of one source file.
fn raw_writes(file: &str, src: &str) -> Vec<Hit> {
    let code = code_only(src);
    let mask = test_mask(&code);
    let declaration = Regex::new(
        r"\b([A-Za-z_][A-Za-z0-9_]*)\s*:\s*&?\s*(?:mut\s+)?(?:(?:std::sync::)?Arc\s*<\s*)?(?:rocksdb::)?DB\b",
    )
    .unwrap();
    let mut receivers: BTreeSet<&str> = BTreeSet::from(["db"]);
    for caps in declaration.captures_iter(&code) {
        let name = caps.get(1).expect("declared name");
        if !mask[name.start()] {
            receivers.insert(name.as_str());
        }
    }
    let call = Regex::new(&format!(r"\.\s*({})\s*\(\s*[^)\s]", WRITE_METHODS.join("|"))).unwrap();
    let mut hits = Vec::new();
    for caps in call.captures_iter(&code) {
        let dot = caps.get(0).expect("whole match").start();
        if mask[dot] {
            continue;
        }
        if let Some(receiver) = receiver_segment(&code, dot) {
            if receivers.contains(receiver) {
                hits.push(Hit {
                    file: file.to_string(),
                    line: line_of(&code, dot),
                    call: format!("{receiver}.{}(..)", &caps[1]),
                });
            }
        }
    }
    hits
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// The directory a file's `mod name;` declarations resolve in.
fn module_dir(file: &Path) -> PathBuf {
    let parent = file.parent().expect("source file has a parent directory");
    match file.file_name().and_then(|name| name.to_str()) {
        Some("lib.rs" | "main.rs" | "mod.rs") => parent.to_path_buf(),
        _ => parent.join(file.file_stem().expect("source file stem")),
    }
}

/// Every source file the crate compiles (under some feature set), found by
/// following `mod name;` from the two crate roots. Modules declared inside a
/// test-gated item are test code and are not followed.
fn compiled_sources(root: &Path) -> BTreeSet<PathBuf> {
    let declaration = Regex::new(r"(?:^|[^A-Za-z0-9_])mod\s+([A-Za-z_][A-Za-z0-9_]*)\s*;").unwrap();
    let path_attribute = Regex::new(r"#\s*\[\s*path\s*=").unwrap();
    let mut seen = BTreeSet::new();
    let mut pending = vec![root.join("src/lib.rs"), root.join("src/main.rs")];
    while let Some(file) = pending.pop() {
        if !seen.insert(file.clone()) {
            continue;
        }
        let code = code_only(&read(&file));
        assert!(
            !path_attribute.is_match(&code),
            "{} uses #[path]; teach compiled_sources to follow it",
            file.display()
        );
        let mask = test_mask(&code);
        let dir = module_dir(&file);
        for caps in declaration.captures_iter(&code) {
            let name = caps.get(1).expect("module name");
            if mask[name.start()] {
                continue;
            }
            let flat = dir.join(format!("{}.rs", name.as_str()));
            let nested = dir.join(name.as_str()).join("mod.rs");
            let child = if flat.is_file() {
                flat
            } else if nested.is_file() {
                nested
            } else {
                panic!(
                    "{}: `mod {};` resolves to neither {} nor {}",
                    file.display(),
                    name.as_str(),
                    flat.display(),
                    nested.display()
                );
            };
            pending.push(child);
        }
    }
    seen
}

fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root).unwrap_or(path).display().to_string()
}

/// `(compiled sources, raw writes outside EXCLUDED, raw writes inside EXCLUDED)`.
fn scan_src() -> (BTreeSet<String>, Vec<Hit>, Vec<Hit>) {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = BTreeSet::new();
    let mut outside = Vec::new();
    let mut inside = Vec::new();
    for path in compiled_sources(root) {
        let file = relative(root, &path);
        let hits = raw_writes(&file, &read(&path));
        if EXCLUDED.iter().any(|(excluded, _)| *excluded == file) {
            inside.extend(hits);
        } else {
            outside.extend(hits);
        }
        files.insert(file);
    }
    (files, outside, inside)
}

#[test]
fn no_raw_rocksdb_write_outside_the_write_gate() {
    let (_, outside, _) = scan_src();
    assert!(
        outside.is_empty(),
        "raw RocksDB writes bypass storage::WriteGate — route each through the owning component's gate \
         (`gate.write(&db, batch, opts)`, `gate.put(..)`, `gate.delete(..)`):\n{}",
        outside.iter().map(ToString::to_string).collect::<Vec<_>>().join("\n")
    );
}

#[test]
fn the_scan_reaches_the_storage_layer_and_finds_the_known_raw_writes() {
    let (files, _, inside) = scan_src();
    assert!(files.len() > 300, "the module walk found only {} files", files.len());
    for expected in [
        "src/storage/engine.rs",
        "src/storage/wal.rs",
        "src/storage/transaction.rs",
        "src/storage/time_travel.rs",
        "src/storage/branch.rs",
        "src/storage/columnar.rs",
        "src/vector/persistent.rs",
        "src/git_integration/commit_tracker.rs",
        "src/git_integration/ddl_versioning/mod.rs",
    ] {
        assert!(files.contains(expected), "the module walk missed {expected}");
    }
    // The gate and the physical applier write raw by design; a detector that
    // cannot see them cannot see anything.
    for (file, minimum) in [("src/storage/write_gate.rs", 3), ("src/replication/physical.rs", 1)] {
        let found = inside.iter().filter(|hit| hit.file == file).count();
        assert!(
            found >= minimum,
            "expected at least {minimum} raw writes in {file}, the detector found {found}"
        );
    }
}

#[test]
fn every_exclusion_names_a_compiled_source() {
    let (files, _, _) = scan_src();
    for (file, reason) in EXCLUDED {
        assert!(!reason.is_empty(), "{file}: an exclusion needs its reason");
        assert!(
            files.contains(*file),
            "stale exclusion: {file} is not a compiled source"
        );
    }
}

#[test]
fn the_detector_reports_every_raw_write_shape_and_nothing_else() {
    let src = r##"
struct Store {
    handle: Arc<rocksdb::DB>,
}

impl Store {
    fn plain(&self) {
        self.db.put(b"k", b"v").unwrap();
    }
    fn chained(&self, batch: WriteBatch) -> Result<()> {
        self.storage
            .db
            .write(batch)?;
        Ok(())
    }
    fn declared(&self) {
        self.handle.delete_range_cf(cf, b"a", b"z").unwrap();
    }
    fn accessor(&self) {
        storage.db().merge(b"k", b"v").unwrap();
    }
    fn gated(&self, batch: WriteBatch) -> Result<()> {
        self.gate.write(&self.db, batch, None)?;
        self.gate
            .put(&self.db, b"k", b"v", None)
    }
    fn not_writes(&self) {
        let _guard = self.db.write();
        let text = "db.put(k, v)";
        let raw = r#"self.db.delete(k)"#;
        let quote = '"';
        // db.write(batch) in a comment
        /* self.db.put(k, v) in a block comment */
        batch.put(b"k", b"v");
    }
}

#[cfg(test)]
mod tests {
    fn helper() {
        db.put(b"x", b"y").unwrap();
    }
}

#[cfg(all(test, target_os = "linux"))]
mod linux_tests {
    fn helper() {
        db.delete(b"x").unwrap();
    }
}

fn after_the_test_modules(db: &DB) {
    db.delete_opt(b"k", &options).unwrap();
}
"##;
    let hits: Vec<(usize, String)> = raw_writes("crafted.rs", src)
        .into_iter()
        .map(|hit| (hit.line, hit.call))
        .collect();
    assert_eq!(
        hits,
        vec![
            (8, "db.put(..)".to_string()),
            (13, "db.write(..)".to_string()),
            (17, "handle.delete_range_cf(..)".to_string()),
            (20, "db.merge(..)".to_string()),
            (53, "db.delete_opt(..)".to_string()),
        ]
    );
}

#[test]
fn the_lexer_keeps_offsets_and_blanks_only_comments_and_literals() {
    let src = "let a = \"x\\\"y\"; // c\nlet b = 'q'; let c: &'static str = r#\"z\"#;\n";
    let code = code_only(src);
    assert_eq!(code.len(), src.len());
    assert_eq!(code.lines().count(), src.lines().count());
    assert!(code.starts_with("let a = \""));
    assert!(!code.contains('x') && !code.contains('z') && !code.contains("// c"));
    assert!(code.contains("'static"), "a lifetime is not a char literal: {code}");
}
