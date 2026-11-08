//! Tripwire A (spec P0.1): every key-shaped string literal in `src/` is
//! classified by `storage::keyspace::classify`.
//!
//! # Why
//!
//! A physical standby keeps its in-memory derived state in step with the
//! batches it applies by classifying every key (`src/storage/keyspace.rs`). A
//! key family added without a `PREFIXES` / `FIXED_KEYS` row would still be
//! replicated byte-for-byte, but nothing on the standby would know what it is —
//! silently. This test turns that into a failure naming the literal's
//! `file:line`.
//!
//! # How
//!
//! A small Rust lexer (line and nested block comments, char literals versus
//! lifetimes, raw strings with `#` fences) yields every string literal under
//! `src/`, so a quote inside a comment, a char or a raw string cannot
//! desynchronise it. A literal is a *candidate* when it is
//!
//! * a byte string `b"…"`, the first argument of `format!(` (whitespace and
//!   newlines allowed in between) or the receiver of `.as_bytes()`, and starts
//!   with `<ident>:` followed by the end, an identifier character or a `{`
//!   placeholder — never whitespace, `/` or a second `:`, which is how an error
//!   message, a URL or a Rust path differs from a key — or with `__<ident>`; or
//! * whatever its context, a chain of `<ident>:` segments ending in `:` — the
//!   shape of a prefix constant such as `"meta:function:"`.
//!
//! Every candidate must classify as something other than `Unknown` or be
//! covered by an [`ALLOW`] entry, which is scoped to a path so it cannot hide a
//! key written anywhere else. Test code is scanned too: a fixture key that is
//! not a real family is listed in `ALLOW` with its reason. `src/storage/keyspace.rs`
//! is the one file skipped — its tables are what is being checked, and its unit
//! tests hold deliberately unknown keys.
//!
//! # Controls
//!
//! * The lexer is exercised on crafted source (`the_lexer_*`), and the scan must
//!   find literals known to exist in `src/`, so a broken scan cannot pass by
//!   seeing nothing.
//! * Red proof, on every run: `a_missing_prefix_is_reported_by_file_and_line`
//!   repeats the scan with `colpm:` removed from the classifier and requires both
//!   of its constructors to be reported.
//! * No guessed or stale rows: every `PREFIXES` / `FIXED_KEYS` entry must be
//!   backed by a literal in `src/`, and every `ALLOW` entry must be needed, must
//!   be `Unknown` itself and must not cover a classified family.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use heliosdb_nano::storage::keyspace::{classify, FIXED_KEYS, PREFIXES};
use heliosdb_nano::storage::KeyClass;
use regex::Regex;
use std::path::{Path, PathBuf};

/// `(path prefix, literal prefix)` pairs for literals that are provably not
/// keys of the engine's keyspace. A candidate is skipped only when its file
/// starts with the path AND its text starts with the literal prefix, so an
/// allowance for one file can never hide a real key written elsewhere.
const ALLOW: &[(&str, &str)] = &[
    // --- Names, not keys ---
    // `expr_to_logical`: EXTRACT's internal scalar-function name.
    ("src/sql/planner.rs", "__extract_"),
    // `mv_data_table_name` and its users: an MV's data-table NAME; its rows
    // live under `data:__mv_*`.
    ("src/storage/materialized_view.rs", "__mv_"),
    ("src/storage/mv_incremental.rs", "__mv_"),
    // `source_ref` column values: stored inside `_hdb_graph_*` rows, i.e. under
    // `data:` keys.
    ("src/graph_rag/", "code_symbol:"),
    ("src/graph_rag/", "doc:"),
    ("src/graph_rag/", "docling:"),
    ("src/graph_rag/", "email:"),
    ("src/graph_rag/", "ext:"),
    ("src/graph_rag/", "issue:"),
    ("src/graph_rag/", "person:"),
    ("src/graph_rag/", "qa:"),
    // --- In-memory map keys that never reach RocksDB ---
    // `parse_by_name`: the tree-sitter parser cache.
    ("src/code_graph/parse.rs", "dyn:"),
    // `get_rate_limit_key`: rate-limit buckets.
    ("src/api/middleware/rate_limit.rs", "ip:"),
    ("src/api/middleware/rate_limit.rs", "user:"),
    // --- Text that is parsed or displayed ---
    // `branch@lsn:N` / `branch@scn:N` diff targets.
    ("src/git_integration/diff/mod.rs", "lsn:"),
    ("src/git_integration/diff/mod.rs", "scn:"),
    // A `query_hash` column value.
    ("src/sql/system_views.rs", "table:"),
    // Prometheus alert text a unit test asserts on.
    ("src/sql/explain_integrations.rs", "alert:"),
    // host:port inputs of an address-validation unit test.
    ("src/config.rs", "localhost:"),
    // --- Keys, but not of the engine's keyspace ---
    // `vacuum_table`: a compact_range bound for a `t:` family nothing writes.
    ("src/storage/engine.rs", "t:"),
    // `physical::FAST_FORWARD_KEY` (spec P4.1): the record of a fast-forward
    // in progress, a node-local key of the `__nano_local` family, like the
    // receiver cursor — never in the replicated default family.
    ("src/replication/physical.rs", "ff:"),
    // OfflineQueue's own private RocksDB (and src/sync is not compiled).
    ("src/sync/offline_queue.rs", "queue:"),
    // --- Unit-test fixtures written to scratch stores, never by the engine ---
    // `test_extract_table_from_key_system_keys`.
    ("src/storage/engine.rs", "system:config"),
    // The TDE read-tolerance tests.
    ("src/storage/engine.rs", "meta:plain:"),
    ("src/storage/engine.rs", "meta:tolerance:"),
    // The physical-standby open-mode test's probe key (never written by the
    // engine; the test proves the gate refuses it).
    ("src/storage/engine.rs", "meta:standby:probe"),
    ("src/storage/transaction.rs", "meta:some_key"),
    // The raw key-value adapter's tests.
    ("src/protocols/adapters/storage.rs", "other:"),
    ("src/protocols/adapters/storage.rs", "prefix:"),
];

/// Exact literals used only by cfg(test) items or the externally test-gated
/// point-write test module. Unlike ALLOW's historical prefix entries these do
/// not admit a new production key, or even a different fixture in the same file.
const TEST_LITERALS: &[(&str, &str)] = &[
    ("src/storage/point_write_cap_tests.rs", "proof:native_retry"),
    ("src/storage/point_write_cap_tests.rs", "proof:role"),
    ("src/storage/point_write_cap_tests.rs", "proof:asref_small"),
    ("src/storage/transaction.rs", "meta:uncommitted"),
    ("src/storage/transaction.rs", "a:"),
    ("src/storage/wal_recovery.rs", "wal:entries_extra"),
    ("src/storage/write_gate.rs", "proof:refused"),
    ("src/storage/write_gate.rs", "proof:applied"),
    ("src/storage/write_gate.rs", "proof:unknown"),
    // Deliberately malformed relative record names / apply_records inputs.
    ("src/vector/persistent.rs", "row_image:"),
    ("src/vector/persistent.rs", "row_image:1"),
    ("src/vector/persistent.rs", "row_image:2"),
];

/// These are reader fragments, not top-level RocksDB keys. The third field is
/// the real containing family (or an actual key selected by a namespace test).
/// Admission requires the exact literal AND its starts_with/strip_prefix use;
/// a put of the same bare fragment still fails. The real constructors remain
/// subject to PREFIXES/FIXED_KEYS and the missing-family control below.
const READER_FRAGMENTS: &[(&str, &str, &[u8])] = &[
    ("src/storage/layout_epochs.rs", "table:", b"bmeta:"),
    ("src/storage/layout_epochs.rs", "missing:", b"bmeta:"),
    ("src/storage/write_gate.rs", "meta:delta:", b"meta:delta:last_id"),
    ("src/vector/persistent.rs", "row_image:", b"__vidx:"),
    ("src/vector/persistent.rs", "rmap:", b"__vidx:"),
];

/// Whole keys that the literal scan cannot see by shape (no `:` and no `__`,
/// or not in a key context), restated here so dropping a `FIXED_KEYS` row
/// fails. The spec's list minus `system:config` (only a test fixture writes it)
/// and with `compression:config` corrected: the real key is
/// `compression:config:{table}` (`Catalog::compression_config_key`), which the
/// scan does see.
const FULL_KEYS: &[&str] = &[
    "wal:last_lsn",
    "wal:checkpoint",
    "wal:pruned_through",
    "vgc:low_watermark",
    "w3_2_elide_used",
    "__nano_physical_replica_cursor_v1",
    "meta:tde:keycheck",
    "meta:role_oid_next",
    "meta:delta:last_id",
    "__view_metadata__",
    "branch:registry",
    "branch:pending_gc",
    "git:config",
    "repl:applied_lsn",
    // Spec P5.1: the stored replication epoch (`replication::epoch::EPOCH_KEY`).
    "repl:epoch",
];

// ---------------------------------------------------------------------------
// Lexer
// ---------------------------------------------------------------------------

/// One string literal: its source line, whether it is a byte string or a raw
/// string, its text between the quotes (escapes left as written), and the
/// byte span of the whole token including any `b`/`r`/`#` decoration.
#[derive(Debug)]
struct Lexed {
    line: usize,
    byte: bool,
    raw: bool,
    text: String,
    start: usize,
    end: usize,
}

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

/// Every string literal in `src`, in order.
fn lex_string_literals(src: &str) -> Vec<Lexed> {
    let b = src.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    let mut line = 1;
    while i < b.len() {
        let c = b[i];
        if c == b'\n' {
            line += 1;
            i += 1;
        } else if b[i..].starts_with(b"//") {
            // Line and doc comments: their quotes are prose.
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
        } else if b[i..].starts_with(b"/*") {
            let mut depth = 0usize;
            while i < b.len() {
                if b[i..].starts_with(b"/*") {
                    depth += 1;
                    i += 2;
                } else if b[i..].starts_with(b"*/") {
                    depth -= 1;
                    i += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    if b[i] == b'\n' {
                        line += 1;
                    }
                    i += 1;
                }
            }
        } else if c == b'\'' {
            i = skip_char_or_lifetime(b, i);
        } else if c == b'"' {
            i = push_literal(src, &mut out, &mut line, i, i, false, false, 0);
        } else if is_ident_start(c) {
            let start = i;
            while i < b.len() && is_ident_continue(b[i]) {
                i += 1;
            }
            let ident = &b[start..i];
            let next = b.get(i).copied();
            let raw = matches!(ident, b"r" | b"br" | b"cr");
            let byte = matches!(ident, b"b" | b"br");
            if raw && matches!(next, Some(b'"' | b'#')) {
                let mut quote = i;
                while quote < b.len() && b[quote] == b'#' {
                    quote += 1;
                }
                // `r#ident` is a raw identifier, not a string.
                if b.get(quote) == Some(&b'"') {
                    i = push_literal(src, &mut out, &mut line, start, quote, byte, true, quote - i);
                }
            } else if matches!(ident, b"b" | b"c") && next == Some(b'"') {
                i = push_literal(src, &mut out, &mut line, start, i, byte, false, 0);
            }
        } else {
            i += 1;
        }
    }
    out
}

/// Past a char literal (`'x'`, `'\''`, `'\u{1F600}'`, `'é'`) or past the quote
/// of a lifetime or label (`'a`, `'outer:`).
fn skip_char_or_lifetime(b: &[u8], i: usize) -> usize {
    match b.get(i + 1) {
        Some(b'\\') => {
            // Escaped char: the closing quote follows the escape.
            let mut j = i + 3;
            while j < b.len() && b[j] != b'\'' {
                j += 1;
            }
            j + 1
        }
        Some(&lead) => {
            let after = i + 1 + utf8_len(lead);
            if b.get(after) == Some(&b'\'') {
                after + 1
            } else {
                i + 1
            }
        }
        None => i + 1,
    }
}

/// Lex one string literal whose opening quote is at `quote` (the token starts
/// at `start`, before any `b`/`r`/`#`), record it and return the index after it.
#[allow(clippy::too_many_arguments)]
fn push_literal(
    src: &str,
    out: &mut Vec<Lexed>,
    line: &mut usize,
    start: usize,
    quote: usize,
    byte: bool,
    raw: bool,
    hashes: usize,
) -> usize {
    let b = src.as_bytes();
    let body = quote + 1;
    let (close, end) = if raw {
        let fence: Vec<u8> = std::iter::once(b'"')
            .chain(std::iter::repeat(b'#').take(hashes))
            .collect();
        let close = (body..b.len()).find(|&j| b[j..].starts_with(&fence)).unwrap_or(b.len());
        (close, (close + fence.len()).min(b.len()))
    } else {
        let mut j = body;
        while j < b.len() && b[j] != b'"' {
            j += if b[j] == b'\\' { 2 } else { 1 };
        }
        let close = j.min(b.len());
        (close, (close + 1).min(b.len()))
    };
    out.push(Lexed {
        line: *line,
        byte,
        raw,
        text: String::from_utf8_lossy(&b[body..close]).into_owned(),
        start,
        end,
    });
    *line += b[start..end].iter().filter(|&&c| c == b'\n').count();
    end
}

// ---------------------------------------------------------------------------
// Candidate selection
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Context {
    Byte,
    Format,
    AsBytes,
    PrefixLiteral,
}

fn is_key_ident_char(c: u8) -> bool {
    c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_'
}

/// `<ident>:` followed by the end, an identifier character or `{` — or
/// `__<ident>`.
fn looks_like_key(text: &str) -> bool {
    let t = text.as_bytes();
    if let Some(rest) = t.strip_prefix(b"__") {
        return rest.first().is_some_and(u8::is_ascii_lowercase);
    }
    let ident = t.iter().take_while(|&&c| is_key_ident_char(c)).count();
    if ident == 0 || t[0].is_ascii_digit() || t.get(ident) != Some(&b':') {
        return false;
    }
    match t.get(ident + 1) {
        None => true,
        Some(&c) => is_key_ident_char(c) || c == b'{',
    }
}

/// A chain of `<ident>:` segments ending in `:`, e.g. `"meta:index:"`.
fn is_prefix_literal(text: &str) -> bool {
    let Some(body) = text.strip_suffix(':') else {
        return false;
    };
    body.split(':').all(|segment| {
        let s = segment.as_bytes();
        !s.is_empty() && !s[0].is_ascii_digit() && s.iter().all(|&c| is_key_ident_char(c))
    })
}

/// The context that makes a lexed literal a candidate, if any.
fn candidate_context(src: &str, lit: &Lexed) -> Option<Context> {
    if lit.raw {
        return None;
    }
    let keyed = if lit.byte {
        Some(Context::Byte)
    } else if src[..lit.start].trim_end().ends_with("format!(") {
        Some(Context::Format)
    } else if src[lit.end..].trim_start().starts_with(".as_bytes()") {
        Some(Context::AsBytes)
    } else {
        None
    };
    match keyed {
        Some(context) if looks_like_key(&lit.text) => Some(context),
        _ if is_prefix_literal(&lit.text) => Some(Context::PrefixLiteral),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// The scan
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct Candidate {
    file: String,
    line: usize,
    context: Context,
    text: String,
    test_only: bool,
    reader_fragment: bool,
}

struct Scan {
    files: usize,
    candidates: Vec<Candidate>,
    /// Every non-raw literal's text, for the "every table row is backed by
    /// source" check.
    all_texts: Vec<String>,
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("read_dir {}: {e}", dir.display()))
        .map(|entry| entry.expect("directory entry").path())
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

fn checked_regex(expression: &str) -> Regex {
    match Regex::new(expression) {
        Ok(regex) => regex,
        Err(error) => panic!("invalid keyspace-tripwire expression {expression:?}: {error}"),
    }
}

/// Preserve byte positions while blanking literals, comments and char tokens.
/// This is only used to prove test-item ownership; the key lexer still scans
/// every test literal instead of silently omitting test code.
fn ownership_code(src: &str) -> String {
    let mut code = src.as_bytes().to_vec();
    for literal in lex_string_literals(src) {
        for byte in &mut code[literal.start..literal.end] {
            if *byte != b'\n' {
                *byte = b' ';
            }
        }
    }
    let mut i = 0;
    while i < code.len() {
        let start = i;
        if code[i..].starts_with(b"//") {
            while i < code.len() && code[i] != b'\n' {
                i += 1;
            }
        } else if code[i..].starts_with(b"/*") {
            let mut depth = 1;
            i += 2;
            while i < code.len() && depth != 0 {
                if code[i..].starts_with(b"/*") {
                    depth += 1;
                    i += 2;
                } else if code[i..].starts_with(b"*/") {
                    depth -= 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
        } else if code[i] == b'\'' {
            i = skip_char_or_lifetime(&code, i);
        } else {
            i += 1;
            continue;
        }
        for byte in &mut code[start..i] {
            if *byte != b'\n' {
                *byte = b' ';
            }
        }
    }
    match String::from_utf8(code) {
        Ok(code) => code,
        Err(error) => panic!("ownership lexer broke UTF-8 source: {error}"),
    }
}

fn test_item_mask(code: &str) -> Vec<bool> {
    let attribute = checked_regex(r"#\s*\[\s*(?:test\s*\]|cfg\s*\(\s*(?:test\s*\)|all\s*\(\s*test\b))");
    let bytes = code.as_bytes();
    let mut mask = vec![false; bytes.len()];
    for found in attribute.find_iter(code) {
        let Some(bracket) = code[found.start()..].find('[') else {
            panic!("matched test attribute has no opening bracket at {}", found.start());
        };
        let mut end = found.start() + bracket;
        let mut brackets = 0;
        while end < bytes.len() {
            if bytes[end] == b'[' {
                brackets += 1;
            }
            if bytes[end] == b']' {
                brackets -= 1;
                if brackets == 0 {
                    end += 1;
                    break;
                }
            }
            end += 1;
        }
        let mut braces = 0;
        while end < bytes.len() {
            match bytes[end] {
                b'{' => braces += 1,
                b'}' => {
                    braces -= 1;
                    if braces == 0 {
                        end += 1;
                        break;
                    }
                }
                b';' if braces == 0 => {
                    end += 1;
                    break;
                }
                _ => {}
            }
            end += 1;
        }
        mask[found.start()..end].fill(true);
    }
    mask
}

fn external_test_module(root: &Path, file: &str) -> bool {
    if file != "src/storage/point_write_cap_tests.rs" {
        return false;
    }
    let owner = root.join("src/storage/write_gate.rs");
    let source = match std::fs::read_to_string(&owner) {
        Ok(source) => source,
        Err(error) => panic!("read external test owner {}: {error}", owner.display()),
    };
    let code = ownership_code(&source);
    let mask = test_item_mask(&code);
    let declaration = checked_regex(r"\bmod\s+point_write_cap_tests\s*;");
    let declarations: Vec<_> = declaration.find_iter(&code).collect();
    let attributed = checked_regex(r"#\s*\[\s*path\s*=\s*[^\]]*\]\s*mod\s+point_write_cap_tests\s*;");
    // Removing the cfg or adding a production declaration revokes fixture
    // admission. The explicit source path is checked too, rather than inferred
    // from a filename ending in _tests.rs.
    !declarations.is_empty()
        && declarations.iter().all(|item| mask[item.start()])
        && declarations.iter().all(|item| {
            attributed.find_iter(&code).any(|attribute| {
                attribute.start() < item.start()
                    && attribute.end() == item.end()
                    && lex_string_literals(&source[attribute.start()..attribute.end()])
                        .iter()
                        .map(|literal| literal.text.as_str())
                        .collect::<Vec<_>>()
                        == ["point_write_cap_tests.rs"]
            })
        })
}

fn scan_src() -> Scan {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    rust_files(&root.join("src"), &mut files);
    let skipped = root.join("src/storage/keyspace.rs");
    let mut scan = Scan {
        files: 0,
        candidates: Vec::new(),
        all_texts: Vec::new(),
    };
    for path in files.into_iter().filter(|p| *p != skipped) {
        let src = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        let file = path.strip_prefix(root).unwrap_or(&path).display().to_string();
        scan.files += 1;
        let code = ownership_code(&src);
        let mask = test_item_mask(&code);
        let external_test = external_test_module(root, &file);
        let reader = checked_regex(r"\.\s*(?:starts_with|strip_prefix)\s*\(\s*$");
        for lit in lex_string_literals(&src) {
            if let Some(context) = candidate_context(&src, &lit) {
                scan.candidates.push(Candidate {
                    file: file.clone(),
                    line: lit.line,
                    context,
                    text: lit.text.clone(),
                    test_only: external_test || mask[lit.start],
                    reader_fragment: reader.is_match(&code[..lit.start]),
                });
            }
            if !lit.raw {
                scan.all_texts.push(lit.text);
            }
        }
    }
    scan
}

fn allowed(candidate: &Candidate) -> bool {
    (candidate.test_only
        && TEST_LITERALS
            .iter()
            .any(|(file, text)| candidate.file == *file && candidate.text == *text))
        || ALLOW
            .iter()
            .any(|(path, prefix)| candidate.file.starts_with(path) && candidate.text.starts_with(prefix))
}

/// `file:line: "literal" [context]` for every candidate the classifier calls
/// `Unknown` and `ALLOW` does not cover.
fn offenders(candidates: &[Candidate], class_of: impl Fn(&[u8]) -> KeyClass) -> Vec<String> {
    candidates
        .iter()
        .filter(|c| {
            let key = READER_FRAGMENTS
                .iter()
                .find(|(file, text, _)| c.reader_fragment && c.file == *file && c.text == *text)
                .map_or(c.text.as_bytes(), |(_, _, parent)| *parent);
            !allowed(c) && class_of(key) == KeyClass::Unknown
        })
        .map(|c| format!("  {}:{}: {:?} [{:?}]", c.file, c.line, c.text, c.context))
        .collect()
}

fn show(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Positive controls: literals that certainly exist in `src/`, one per
/// context the scan relies on. A lexer or walk that silently sees nothing
/// fails here rather than passing the real assertion vacuously.
fn assert_scan_sees_known_literals(scan: &Scan) {
    assert!(
        scan.files > 100,
        "the walk found only {} .rs files under src/",
        scan.files
    );
    assert!(
        scan.candidates.len() > 200,
        "only {} key-shaped literals found — the lexer or the candidate rules are broken",
        scan.candidates.len()
    );
    for (file, context, text) in [
        ("src/storage/wal.rs", Context::Byte, "wal:last_lsn"),
        ("src/vector/persistent.rs", Context::Format, "__vidx:{index_id}:"),
        ("src/storage/catalog.rs", Context::Format, "meta:table:{}"),
        ("src/lib.rs", Context::PrefixLiteral, "meta:function:"),
        ("src/storage/time_travel.rs", Context::Byte, "w3_2_elide_used"),
    ] {
        // `w3_2_elide_used` has no ':' — it must NOT be a candidate; it is
        // covered by FULL_KEYS instead. Every other control must be found.
        let found = scan
            .candidates
            .iter()
            .any(|c| c.file == file && c.context == context && c.text == text);
        let expected = text.contains(':') || text.starts_with("__");
        assert_eq!(
            found, expected,
            "positive control: {text:?} [{context:?}] in {file} — found {found}, expected {expected}"
        );
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn every_key_literal_in_src_is_classified() {
    let scan = scan_src();
    assert_scan_sees_known_literals(&scan);
    let unknown = offenders(&scan.candidates, |key| classify(key).class);
    assert!(
        unknown.is_empty(),
        "{} key-shaped literal(s) in src/ classify as Unknown. Add a PREFIXES or FIXED_KEYS row to \
         src/storage/keyspace.rs citing the constructor, or — only if the literal is provably not a \
         key of the engine's keyspace — an ALLOW entry here with its reason:\n{}",
        unknown.len(),
        unknown.join("\n")
    );
}

/// The red proof, run every time: the same scan with one prefix removed from
/// the classifier must report that prefix's constructors by file.
#[test]
fn a_missing_prefix_is_reported_by_file_and_line() {
    let scan = scan_src();
    let without_colpm = |key: &[u8]| {
        if key.starts_with(b"colpm:") {
            KeyClass::Unknown
        } else {
            classify(key).class
        }
    };
    let reported = offenders(&scan.candidates, without_colpm);
    for (file, text) in [
        ("src/storage/columnar.rs", "\"colpm:{}\" [Format]"),
        ("src/storage/transaction.rs", "\"colpm:\" [Byte]"),
    ] {
        assert!(
            reported.iter().any(|line| line.contains(file) && line.ends_with(text)),
            "dropping `colpm:` must report {text} in {file}; the tripwire reported:\n{}",
            reported.join("\n")
        );
    }
    assert!(
        reported.iter().all(|line| line.contains("colpm:")),
        "only `colpm:` literals may be reported when only `colpm:` is missing:\n{}",
        reported.join("\n")
    );
}

#[test]
fn every_full_key_is_classified() {
    let unknown: Vec<&str> = FULL_KEYS
        .iter()
        .copied()
        .filter(|key| classify(key.as_bytes()).class == KeyClass::Unknown)
        .collect();
    assert!(unknown.is_empty(), "fixed keys classify as Unknown: {unknown:?}");
}

/// No guessed rows: each table entry must appear at the start of some string
/// literal in `src/` (keyspace.rs itself excluded), i.e. it was copied from a
/// constructor or reader that exists.
#[test]
fn every_table_entry_is_backed_by_a_literal_in_src() {
    let scan = scan_src();
    let unbacked: Vec<String> = PREFIXES
        .iter()
        .chain(FIXED_KEYS)
        .filter(|(entry, _)| !scan.all_texts.iter().any(|text| text.as_bytes().starts_with(entry)))
        .map(|(entry, class)| format!("  {:?} ({class:?})", show(entry)))
        .collect();
    assert!(
        unbacked.is_empty(),
        "keyspace entries with no matching literal anywhere in src/ (guessed, or the constructor was \
         removed — delete the row):\n{}",
        unbacked.join("\n")
    );
}

/// `ALLOW` stays honest: every entry is still needed, is not itself a
/// classified key, and does not cover a classified family.
#[test]
fn every_allow_entry_is_needed_and_masks_nothing() {
    let scan = scan_src();
    for &(path, allowed_prefix) in ALLOW {
        assert!(
            scan.candidates
                .iter()
                .any(|c| c.file.starts_with(path) && c.text.starts_with(allowed_prefix)),
            "ALLOW entry ({path:?}, {allowed_prefix:?}) matches no candidate literal any more — remove it"
        );
        assert_eq!(
            classify(allowed_prefix.as_bytes()).class,
            KeyClass::Unknown,
            "ALLOW entry {allowed_prefix:?} is a classified key prefix; it must not be allowed"
        );
        for (prefix, class) in PREFIXES.iter().chain(FIXED_KEYS) {
            assert!(
                !prefix.starts_with(allowed_prefix.as_bytes()),
                "ALLOW entry {allowed_prefix:?} would hide the classified family {:?} ({class:?})",
                show(prefix)
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Lexer and candidate-rule controls on crafted source
// ---------------------------------------------------------------------------

fn texts(src: &str) -> Vec<(usize, String)> {
    lex_string_literals(src).into_iter().map(|l| (l.line, l.text)).collect()
}

#[test]
fn the_lexer_ignores_quotes_in_comments_chars_and_lifetimes() {
    let src = concat!(
        "// a \"comment\" quote\n",
        "/* block \"quote\" /* nested \" */ still comment */\n",
        "fn f<'a>(x: &'a str) -> char { let q = '\"'; let e = '\\''; let u = '\\u{1F600}'; 'x' }\n",
        "let s = \"real:{}\";\n",
        "/// doc \"comment\"\n",
        "let t = b\"bytes:\";\n",
    );
    assert_eq!(texts(src), vec![(4, "real:{}".to_string()), (6, "bytes:".to_string())]);
}

#[test]
fn the_lexer_handles_raw_strings_escapes_and_multiline_literals() {
    let src = concat!(
        "let a = r#\"raw \"quoted\" body\"#;\n",
        "let b = \"esc \\\" quote\";\n",
        "let c = format!(\n",
        "    \"data:{}:{}\",\n",
        "    t, r);\n",
        "let d = \"two\n",
        "lines\";\n",
        "let e = \"after:\";\n",
        "let r#type = 1;\n",
    );
    let lexed = lex_string_literals(src);
    let got: Vec<(usize, bool, &str)> = lexed.iter().map(|l| (l.line, l.raw, l.text.as_str())).collect();
    assert_eq!(
        got,
        vec![
            (1, true, "raw \"quoted\" body"),
            (2, false, "esc \\\" quote"),
            (4, false, "data:{}:{}"),
            (6, false, "two\nlines"),
            (8, false, "after:"),
        ]
    );
    let format_arg = &lexed[2];
    assert_eq!(candidate_context(src, format_arg), Some(Context::Format));
}

#[test]
fn candidate_rules_separate_keys_from_prose_urls_and_paths() {
    for key in [
        "data:{}:{}",
        "meta:table:{}",
        "v:",
        "wal:last_lsn",
        "__vidx:{index_id}:",
        "__view_metadata__",
    ] {
        assert!(looks_like_key(key), "{key:?} must look like a key");
    }
    for not_key in [
        "error: {}",
        "http://host",
        "nodb::tool",
        "Data:x",
        "{}:{}",
        "9x:",
        "_",
        "__",
        "__Upper",
    ] {
        assert!(!looks_like_key(not_key), "{not_key:?} must not look like a key");
    }
    for prefix in ["meta:index:", "data:", "git:schema_snapshot:"] {
        assert!(is_prefix_literal(prefix), "{prefix:?} is a prefix literal");
    }
    for not_prefix in ["meta:index", "a::", ":", "", "host:5433", "9a:"] {
        assert!(!is_prefix_literal(not_prefix), "{not_prefix:?} is not a prefix literal");
    }
}

#[test]
fn scoped_literals_never_admit_a_production_write_or_another_family() {
    let scan = scan_src();
    for &(file, text) in TEST_LITERALS {
        assert!(
            scan.candidates
                .iter()
                .any(|c| c.file == file && c.text == text && c.test_only),
            "stale or no longer test-only fixture: {file} {text}"
        );
        let production = Candidate {
            file: file.into(),
            line: 1,
            context: Context::Byte,
            text: text.into(),
            test_only: false,
            reader_fragment: false,
        };
        assert!(!offenders(&[production], |key| classify(key).class).is_empty());
    }
    for &(file, text, family) in READER_FRAGMENTS {
        assert_eq!(classify(text.as_bytes()).class, KeyClass::Unknown);
        assert_ne!(classify(family).class, KeyClass::Unknown);
        assert!(
            scan.candidates
                .iter()
                .any(|c| c.file == file && c.text == text && c.reader_fragment),
            "stale relative-family reader: {file} {text}"
        );
        let write = Candidate {
            file: file.into(),
            line: 1,
            context: Context::Byte,
            text: text.into(),
            test_only: false,
            reader_fragment: false,
        };
        assert!(!offenders(&[write], |key| classify(key).class).is_empty());
        let other_file = Candidate {
            file: "src/new_writer.rs".into(),
            line: 1,
            context: Context::Byte,
            text: text.into(),
            test_only: false,
            reader_fragment: true,
        };
        assert!(!offenders(&[other_file], |key| classify(key).class).is_empty());
        let reader = Candidate {
            file: file.into(),
            line: 1,
            context: Context::Byte,
            text: text.into(),
            test_only: false,
            reader_fragment: true,
        };
        assert!(
            !offenders(&[reader], |key| if key == family {
                KeyClass::Unknown
            } else {
                classify(key).class
            })
            .is_empty(),
            "lost containing-family red proof"
        );
    }
}

#[test]
fn fixture_ownership_ignores_comments_and_literals_and_ends_with_the_item() {
    let source = r##"
// #[cfg(test)] mod pretend { }
const TEXT: &str = "#[test] fn pretend() { }";
#[cfg(test)]
mod tests { fn fixture() { put(b"proof:fixture"); } }
fn shipping() { put(b"proof:shipping"); }
#[test]
fn individual() { put(b"proof:individual"); }
"##;
    let mask = test_item_mask(&ownership_code(source));
    let literals = lex_string_literals(source);
    let ownership: Vec<_> = literals
        .iter()
        .filter(|l| l.text.starts_with("proof:"))
        .map(|l| (l.text.as_str(), mask[l.start]))
        .collect();
    assert_eq!(
        ownership,
        [
            ("proof:fixture", true),
            ("proof:shipping", false),
            ("proof:individual", true)
        ]
    );
}

#[test]
fn external_fixture_admission_requires_its_real_test_gated_path() -> std::io::Result<()> {
    let root = tempfile::tempdir()?;
    std::fs::create_dir_all(root.path().join("src/storage"))?;
    let owner = root.path().join("src/storage/write_gate.rs");
    let path = "src/storage/point_write_cap_tests.rs";
    for (source, expected) in [
        ("#[cfg(test)]\n#[path=\"point_write_cap_tests.rs\"]\nmod point_write_cap_tests;", true),
        ("#[path=\"point_write_cap_tests.rs\"]\nmod point_write_cap_tests;", false),
        ("// #[path=\"point_write_cap_tests.rs\"]\n#[cfg(test)]\nmod point_write_cap_tests;", false),
        ("#[cfg(test)]\n#[path=\"different.rs\"]\nmod point_write_cap_tests;", false),
        ("#[cfg(test)]\n#[path=\"point_write_cap_tests.rs\"]\nmod point_write_cap_tests;\nmod point_write_cap_tests;", false),
    ] {
        std::fs::write(&owner, source)?;
        assert_eq!(external_test_module(root.path(), path), expected, "{source}");
    }
    Ok(())
}
