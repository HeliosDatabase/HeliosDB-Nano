//! The storage boundary's value codec — ONE rule, shared by every module that
//! reads or writes a stored value.
//!
//! Two directions, one policy:
//!
//! * [`seal`] / [`seal_row_value`] turn an in-memory value into the bytes that
//!   go to RocksDB.
//! * [`open_ref`] / [`open_owned`] / [`open_opt`] turn stored bytes back into
//!   an in-memory value.
//!
//! Both live here rather than on `StorageEngine` because the engine is not the
//! only writer: `Transaction` (the commit `WriteBatch`) and `SnapshotManager`
//! (the version chain and the fast autocommit `data:` write) build their own
//! batches against the same `Arc<DB>`. A rule that lived on only one of them
//! would be a per-route opt-in, and the answer to "what does this stored byte
//! string mean?" must depend on the KEY, never on which function wrote it.
//!
//! ZERO COST WHEN ENCRYPTION IS OFF (the default configuration). Every entry
//! point takes `Option<&KeyManager>` and returns early on `None`, borrowing the
//! caller's bytes. No allocation, no copy, and exactly the one `Option` check
//! the surrounding code already performed. On the write side the caller passes
//! `Cow::Borrowed` straight to `batch.put`, which is byte-for-byte the call it
//! made before.

use crate::crypto::{self, KeyManager};
use crate::{Error, Result};
use rocksdb::DB;
use std::borrow::Cow;
use std::sync::atomic::{AtomicU64, Ordering};
use tracing::warn;

/// Count of stored values accepted as plaintext on an encryption-enabled
/// database (see [`note_plaintext_passthrough`]).
static PLAINTEXT_PASSTHROUGH_COUNT: AtomicU64 = AtomicU64::new(0);

/// Keyspaces whose VALUES are user row data and are therefore sealed at the
/// storage boundary on an encryption-enabled database:
///
/// * `data:{table}:{row_id}` — the live row image.
/// * `v:{table}:{row_id}:{ts}` — the MVCC version chain, which holds a
///   byte-identical copy of that row image.
/// * `counter:{table}` — the row-id high-water mark (already sealed by
///   `StorageEngine::put_internal`; listing it here makes the transaction
///   commit batch agree with the engine instead of disagreeing by route).
/// * `wal:entries:{lsn}` — the logical WAL. Its `Insert` / `Update` operations
///   carry the row tuple itself, so a WAL entry is a second full copy of a row
///   living in the same RocksDB store as the `data:` key it describes.
///   `WriteAheadLog` owns every write of this keyspace and every read of it
///   (replay, integrity verification, truncation, cleanup, metrics), so it
///   seals and opens through this module the same way the engine does. Only the
///   `wal:entries:` values are covered: `wal:last_lsn` is an 8-byte
///   little-endian marker parsed by `WriteAheadLog::recover_last_lsn` with a
///   fixed-width `try_into`, it holds no user data, and a 28-byte AEAD frame
///   there would change a counter encoding to protect nothing.
/// * `bdata:{branch_id}:{table}:{row_id}` — branch row overlays. A row inserted
///   or updated on a non-`main` branch is a full user row image, so it is sealed
///   by the same rule as `data:` and by every route that writes it:
///   `StorageEngine::put` / `put_internal` (branch INSERT via
///   `insert_tuple_branch_aware_with_schema`, branch UPDATE via
///   `update_tuples_branch_aware`), the transaction commit batch (through
///   [`seal_row_value`], which is why this prefix is listed here),
///   `BranchManager::copy_key_to_branch`, and `StorageEngine::merge_branch`'s
///   branch-to-branch arm.
///
///   Its readers all decode through this module, so the tolerant open applies to
///   the keyspace exactly as it does to `data:`: `Transaction::get`'s
///   `read_raw_decoded` fallback (which `BranchTransaction::get` is built on),
///   `BranchTransaction::get`'s parent-chain arm, `BranchManager::
///   get_key_at_snapshot` / `get_latest_key_value`, and the engine's
///   `get` / `get_internal` / `decrypt_value` — the last of which is what
///   `scan_table_branch_aware` and the branch-aware point lookups use.
///
///   The two readers that COMPARE two `bdata:` values — the merge conflict
///   detector, through `get_key_at_snapshot` and `get_latest_key_value` — must
///   compare the DECODED form, because a fresh nonce per seal makes two AES-GCM
///   frames of one plaintext differ and a byte comparison would report a
///   conflict between two identical rows.
///
///   `bdel:` (branch delete markers) is deliberately absent: its values are
///   empty and every reader tests only for the key's presence.
/// * `delta:{table}:{delta_id}` — the materialized-view delta log written by
///   `mv_delta::DeltaTracker`. A `Delta`'s operation carries whole `Tuple`s
///   (`Insert { tuple }`, `Delete { tuple }`, `Update { old_tuple, new_tuple }`),
///   so one record is a complete row image — including the contents of a row a
///   `DELETE` has just removed. Recorded by autocommit UPDATE/DELETE
///   (`update_tuples_branch_aware` / `delete_tuples_branch_aware`) and by every
///   `StorageEngine::insert_tuple` caller (the REST insert handler, dump
///   restore, the audit log, MV incremental maintenance and the protocol
///   adapter executor), and never compacted, so the records are as long-lived as
///   the database. `DeltaTracker` therefore carries the engine's key manager and
///   seals in `record_delta`, and its three readers (`get_deltas_since`,
///   `count_deltas_since`, `purge_deltas_before`) open through this module.
///   Only the `delta:` values are covered: `meta:delta:last_id` is an 8-byte
///   little-endian id counter parsed with a fixed-width `try_into`, the same
///   reasoning as `wal:last_lsn`.
///
/// Deliberately NOT listed, each for a stated reason:
///
/// * `v_idx:` — an 8-byte big-endian commit timestamp with the W3.2 elision
///   flag in its high bit. It carries no user data, and it is decoded in
///   `time_travel.rs` iterator loops that inspect the raw bytes directly; a
///   16-byte AEAD expansion there would change an index encoding, not protect
///   a secret.
/// * `snapshot:` / `vmeta:` — snapshot metadata and COPY range markers
///   (timestamps and row-id bounds). Same reasoning.
/// * The columnar / dictionary / content-addressed sidecars (`col:` / `colz:` /
///   `colp:` / `cas:` / `dict:`) — written AND read raw inside `columnar.rs` /
///   `content_addr.rs` / `dictionary.rs`, which own no key manager, and some of
///   them are written straight to the `DB` outside any transaction. Sealing
///   their values here without converting those readers in the same change
///   would turn a self-consistent subsystem into a broken one.
///
///   THE CONSEQUENCE WHILE THEY STAY EXCLUDED, stated precisely rather than
///   left to be inferred: for a column declared with a non-default `STORAGE`
///   mode the `data:` row image holds only a REFERENCE to the value, so sealing
///   `data:` seals the reference and not the payload. Concretely, on an
///   encryption-enabled database:
///     - `STORAGE CONTENT_ADDRESSED` — a String/Bytes value of at least
///       `content_addr::CAS_MIN_SIZE` (1 KiB) is written verbatim to
///       `cas:{blake3}` and the row holds a `Value::CasRef`. The payload is
///       stored in the clear. (A value below that threshold stays inline in the
///       row and IS sealed with it.)
///     - `STORAGE DICTIONARY` — every distinct value of the column is written
///       verbatim inside the serialized dictionary at `dict:{table}:{column}`,
///       and the row holds a `Value::DictRef` code.
///     - `STORAGE COLUMNAR` — the column's values are written verbatim into the
///       `col:` / `colz:` / `colp:` batches and the row holds
///       `Value::ColumnarRef`.
///   The `v:` twin still holds the full logical row SEALED while time travel is
///   on, but with `storage.time_travel_enabled = false` there is no sealed copy
///   of such a value anywhere on disk. This is pinned by an executable test
///   rather than left as an assumption — see
///   `tests/encryption_at_rest_tests.rs::a_content_addressed_column_stores_its_payload_outside_the_seal`,
///   which asserts today's ACTUAL behaviour and will fail the moment sealing the
///   sidecars changes it.
#[inline]
pub(crate) fn is_row_value_key(key: &[u8]) -> bool {
    key.starts_with(b"data:")
        || key.starts_with(b"v:")
        || key.starts_with(b"counter:")
        || key.starts_with(b"wal:entries:")
        || key.starts_with(b"bdata:")
        || key.starts_with(b"delta:")
}

/// Seal a value the caller has already decided must be sealed, whatever its
/// key: either the key is a compile-time-known member of the sealed set at the
/// call site, or the caller seals unconditionally by design.
/// `StorageEngine::put` / `put_internal` are the second kind — they encrypt
/// every key they are handed, which is deliberately BROADER than
/// [`is_row_value_key`] (it is how `meta:` catalog blobs are sealed) — so they
/// use this rather than [`seal_row_value`].
///
/// `Cow::Borrowed` — no allocation, no copy — when encryption is disabled.
#[inline]
pub(crate) fn seal<'a>(key_manager: Option<&KeyManager>, value: &'a [u8]) -> Result<Cow<'a, [u8]>> {
    match key_manager {
        None => Ok(Cow::Borrowed(value)),
        Some(km) => Ok(Cow::Owned(crypto::encrypt(km.key(), value)?)),
    }
}

/// Seal a value the caller already owns, for a key it has already established
/// belongs to a sealed keyspace.
///
/// The owned mirror of [`seal`], and the exact counterpart of [`open_owned`]:
/// when encryption is disabled the input `Vec` is returned UNMOVED, so a caller
/// that must hand owned bytes to `batch.put` pays no copy on the default
/// configuration. Callers that already hold a borrow should use [`seal`].
#[inline]
pub(crate) fn seal_owned(key_manager: Option<&KeyManager>, value: Vec<u8>) -> Result<Vec<u8>> {
    match key_manager {
        None => Ok(value),
        Some(km) => crypto::encrypt(km.key(), &value),
    }
}

/// Seal a value whose key is only known at run time — the transaction commit
/// batch, which stages `data:`, `bdata:`, `col:`, `meta:` and more through the
/// same buffer. Applies [`is_row_value_key`], so the decision is a property of
/// the key alone.
#[inline]
pub(crate) fn seal_row_value<'a>(
    key_manager: Option<&KeyManager>,
    storage_key: &[u8],
    value: &'a [u8],
) -> Result<Cow<'a, [u8]>> {
    let Some(km) = key_manager else {
        return Ok(Cow::Borrowed(value));
    };
    if !is_row_value_key(storage_key) {
        return Ok(Cow::Borrowed(value));
    }
    Ok(Cow::Owned(crypto::encrypt(km.key(), value)?))
}

// ==================== Stored-value decode ====================
//
// WHY TOLERANCE IS REQUIRED, NOT A CONVENIENCE. A database opened with
// encryption enabled can legitimately hold a MIXTURE of ciphertext and
// plaintext under the same key prefixes. Stored values are untagged, so
// nothing on disk distinguishes the two, and a reader that assumes "key
// manager present ⇒ every value is ciphertext" cannot read data that is
// already in the field.
//
// SAFETY ARGUMENT. `crypto::encrypt` emits nonce(12) ‖ ciphertext ‖ tag(16).
// The passthrough branch is reached ONLY when the buffer is too short to be
// such a frame at all, or when the GCM tag check fails against the configured
// key. Accepting a genuine ciphertext as plaintext would require forging a
// tag: probability 2^-128. And only that authentication failure falls through
// — `crypto::try_decrypt` has no error case, so a missing key or a
// key-source/configuration failure cannot be swallowed here; those surface
// earlier, where the key manager is built.
//
// THE COST, STATED PLAINLY. Genuine corruption of a real ciphertext value no
// longer surfaces immediately as an `aead` error. It surfaces one step later,
// as a bincode/decode failure in the caller, because the corrupt bytes are
// handed on as if they were plaintext. That is the price of being able to read
// existing data at all. To keep it diagnosable, every passthrough is counted
// and logged at WARN with the storage key.

/// The one rule. `Ok(None)` means "the raw bytes are already plaintext, use
/// them as they are"; `Ok(Some(pt))` means "decrypted to `pt`".
///
/// Returns `Result` so that every call site keeps its `?` and so a future
/// key-level failure has somewhere to propagate from.
#[inline]
fn open_inner(key_manager: Option<&KeyManager>, storage_key: &[u8], raw: &[u8]) -> Result<Option<Vec<u8>>> {
    let Some(km) = key_manager else {
        return Ok(None);
    };
    match crypto::try_decrypt(km.key(), raw) {
        crypto::DecryptAttempt::Authenticated(plaintext) => Ok(Some(plaintext)),
        crypto::DecryptAttempt::Unauthenticated => {
            // Counted and logged ONLY for the keyspaces THIS rule seals. The
            // keyspaces in `is_row_value_key`'s exclusion list are stored
            // verbatim by design, so a `col:` or `snapshot:` value arriving here
            // as plaintext is the expected outcome, not an observation.
            // `read_raw_decoded` routes every non-`data:` key a transaction
            // touches through here, so without this gate a columnar workload
            // would drive the number almost entirely with non-events and
            // `plaintext_passthrough_count` could no longer answer what it
            // exists to answer.
            //
            // The trade-off, stated rather than hidden: `is_row_value_key` is
            // this module's sealed set, and it is deliberately narrower than
            // "every key `StorageEngine::put`/`put_internal` seals" — those two
            // encrypt whatever key they are handed, including `meta:`. A `meta:`
            // value read back as plaintext is therefore NOT counted here. That
            // is the price of having ONE predicate decide both directions
            // instead of a second, drifting list of keyspaces maintained only
            // for the diagnostic.
            if is_row_value_key(storage_key) {
                note_plaintext_passthrough(storage_key, raw.len());
            }
            Ok(None)
        }
    }
}

/// Cold path: record and (sparsely) log that a stored value was accepted as
/// plaintext on an encryption-enabled database.
///
/// Logged for the first occurrence and then at exponentially spaced counts
/// (1, 2, 4, 8, …) so that a wholly plaintext table stays diagnosable without
/// drowning the log or costing a WARN per row.
///
/// Never called when the key manager is absent, and never called for a key
/// outside `is_row_value_key` — see the gate in `open_inner` for why the
/// diagnostic is scoped to the keyspaces this rule seals.
#[cold]
#[inline(never)]
fn note_plaintext_passthrough(storage_key: &[u8], len: usize) {
    let seen = PLAINTEXT_PASSTHROUGH_COUNT
        .fetch_add(1, Ordering::Relaxed)
        .saturating_add(1);
    if seen.is_power_of_two() {
        warn!(
            "stored value at key '{}' ({} bytes) is not ciphertext under the configured key and was read as \
             plaintext; a corrupt value would now surface as a decode error rather than an aead error \
             (occurrence {})",
            String::from_utf8_lossy(storage_key),
            len,
            seen
        );
    }
}

/// How many values in one of THIS module's sealed keyspaces
/// ([`is_row_value_key`]) have been read as plaintext on an encryption-enabled
/// database in this process. `0` on a database with encryption disabled, and on
/// a uniformly encrypted one.
///
/// Scope, precisely: the keyspaces `is_row_value_key` lists (`data:`, `v:`,
/// `counter:`, `wal:entries:`, `bdata:`, `delta:`). Its exclusion list
/// is plaintext by design and is not counted; keys sealed only by
/// `StorageEngine::put` / `put_internal` (`meta:` and friends, which those two
/// encrypt whatever key they are given) are outside this number as well.
pub(crate) fn plaintext_passthrough_count() -> u64 {
    PLAINTEXT_PASSTHROUGH_COUNT.load(Ordering::Relaxed)
}

/// Borrowed decode: returns `Cow::Borrowed(raw)` when the bytes are used as
/// they are, so the encryption-disabled path allocates nothing.
#[inline]
pub(crate) fn open_ref<'a>(
    key_manager: Option<&KeyManager>,
    storage_key: &[u8],
    raw: &'a [u8],
) -> Result<Cow<'a, [u8]>> {
    match open_inner(key_manager, storage_key, raw)? {
        Some(plaintext) => Ok(Cow::Owned(plaintext)),
        None => Ok(Cow::Borrowed(raw)),
    }
}

/// Owned decode: consumes `raw` and returns it UNMOVED when the bytes are used
/// as they are, so the passthrough costs no copy.
#[inline]
pub(crate) fn open_owned(key_manager: Option<&KeyManager>, storage_key: &[u8], raw: Vec<u8>) -> Result<Vec<u8>> {
    match open_inner(key_manager, storage_key, &raw)? {
        Some(plaintext) => Ok(plaintext),
        None => Ok(raw),
    }
}

/// Owned decode of a `db.get`-shaped result. `None` (key absent) is passed
/// through untouched — it is not a decode failure.
#[inline]
pub(crate) fn open_opt(
    key_manager: Option<&KeyManager>,
    storage_key: &[u8],
    raw: Option<Vec<u8>>,
) -> Result<Option<Vec<u8>>> {
    match raw {
        None => Ok(None),
        Some(bytes) => Ok(Some(open_owned(key_manager, storage_key, bytes)?)),
    }
}

// ==================== Key verification ====================

/// The database's key-verification sentinel: a known constant, sealed under the
/// configured key, stored at a dedicated key.
///
/// WHY THIS SHIPS WITH THE TOLERANT READ, NOT AFTER IT. The two are one
/// mechanism. `open_inner` deliberately cannot tell "this stored value predates
/// sealing" from "this value was sealed under a DIFFERENT key" — both are an
/// AEAD tag failure, and no bytes on disk distinguish them. Tolerance is
/// required to read a mixed-format database at all, but on its own it would
/// also absorb a wrong or rotated key: every value would fail the tag check,
/// every value would be handed on as plaintext, and a session would go on to
/// write NEW rows sealed under that key beside rows sealed under the other one,
/// with nothing raised at any point. The sentinel restores the distinction at
/// the layer that can actually make it: ONE strict check, at open, before any
/// value is read — so "the operator supplied the wrong key" is a single loud
/// failure, and per-value tolerance is left to mean only what it says.
///
/// Verified with the STRICT [`crypto::decrypt`], never through the tolerant
/// path — the whole point is that this one value must be ciphertext under this
/// exact key.
const KEY_CHECK_KEY: &[u8] = b"meta:tde:keycheck";

/// The sealed constant. Its content is not secret; what matters is that it is
/// FIXED, so decrypting it under the configured key proves that key sealed this
/// database. Never change it: an existing database's sentinel was sealed from
/// these exact bytes, and a different constant would make every such database
/// refuse to open.
const KEY_CHECK_PLAINTEXT: &[u8] = b"heliosdb-nano tde key check v1";

/// How many stored values [`probe_key_against_existing_data`] examines before it
/// stops. The probe runs once per open and stops early the moment the key is
/// proven, so the budget only bounds the two outcomes that must read the whole
/// sample: "nothing here is sealed" and "something here is sealed and this key
/// does not open it".
const KEY_PROBE_BUDGET: usize = 128;

/// The keyspaces the probe reads, in the order it reads them, chosen because
/// every one of them has a plaintext form this build can RECOGNISE (see
/// [`value_is_recognisable_plaintext`]) — which is what lets the probe tell "the
/// values here predate sealing" apart from "the values here are sealed under
/// another key". Ordered cheapest-and-most-decisive first: `counter:` holds one
/// small fixed-width value per table, `meta:table:` one schema per table, and
/// `data:` the rows themselves.
const KEY_PROBE_PREFIXES: [&[u8]; 3] = [b"counter:".as_slice(), b"meta:table:".as_slice(), b"data:".as_slice()];

/// What the values already on disk say about the configured key.
enum KeyEvidence {
    /// The probed keyspaces are empty: a new database, or one whose tables have
    /// not been created yet. Nothing on disk can contradict the key.
    NoStoredValues,
    /// At least one stored value AUTHENTICATED under the configured key. This is
    /// positive proof: forging it would mean forging a GCM tag.
    KeyProven,
    /// Values are present and every one examined is structurally impossible as
    /// an AEAD frame this build would have written — the database's stored
    /// values predate sealing.
    PredatesSealing,
    /// Values are present, at least one of them is shaped like an AEAD frame,
    /// and none authenticated under the configured key.
    Contradicted,
}

/// True when `value` cannot be a frame [`crypto::encrypt`] produced, for the
/// keyspace `storage_key` belongs to.
///
/// This is a POSITIVE test for plaintext, not the absence of a decrypt. The
/// tolerant read cannot make this distinction — an AEAD tag failure is all it
/// sees — but the probe can, because it knows which keyspace it is looking at
/// and therefore what the plaintext there is supposed to decode to:
///
/// * anything shorter than [`crypto::MIN_CIPHERTEXT_LEN`] is not a frame at all,
///   since `encrypt` emits nonce(12) ‖ ciphertext ‖ tag(16) even for empty input
///   (pinned by `crypto::min_ciphertext_len_matches_what_encrypt_emits`). A
///   plaintext `counter:` value is a bincode `u64`, i.e. 8 bytes, so this alone
///   settles the row-id counters;
/// * a `meta:table:` value that deserializes as a [`crate::Schema`] and a
///   `data:` / `v:` / `bdata:` value that deserializes as a [`crate::Tuple`] are
///   plaintext for the same reason the engine's own readers treat them as such.
///
/// A ciphertext under some other key is indistinguishable from random bytes, so
/// for it to be misread as plaintext here the random bytes would have to form a
/// valid bincode encoding of the exact type the keyspace holds — a length prefix
/// small enough not to run off the end followed by in-range enum discriminants
/// for every element. The probe additionally requires this to hold for EVERY
/// value it samples, so a single sealed value anywhere in the sample is enough
/// to reach [`KeyEvidence::Contradicted`].
fn value_is_recognisable_plaintext(storage_key: &[u8], value: &[u8]) -> bool {
    use bincode::Options;

    if value.len() < crypto::MIN_CIPHERTEXT_LEN {
        return true;
    }
    if storage_key.starts_with(b"counter:") {
        // A bincode `u64` is 8 bytes, which the length rule above has already
        // accepted. Nothing longer under this prefix is a row-id counter.
        return false;
    }
    // The configuration `bincode::serialize` writes, WITHOUT its reader's
    // tolerance for trailing bytes: a stored value must account for its whole
    // buffer or it is not the value this keyspace holds. That tolerance would
    // matter here — it would let a long random buffer qualify on a short valid
    // prefix.
    let strict = bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .with_little_endian();
    if storage_key.starts_with(b"meta:table:") {
        return strict.deserialize::<crate::Schema>(value).is_ok();
    }
    strict.deserialize::<crate::Tuple>(value).is_ok()
}

/// Decide, from the values already on disk, whether the configured key may be
/// recorded as this database's key.
///
/// WHY THE PROBE EXISTS. The sentinel is permanent: once written, every later
/// open is checked against it and a database sealed under key A that gained a
/// sentinel under key B can never be opened under A again. So the ONE open that
/// installs it is the only chance to be wrong, and it is exactly the open where
/// the tolerant read is at its most forgiving — every value fails its tag check
/// under a wrong key and is handed on as plaintext, so nothing else in the open
/// path would object. Installing on faith would therefore convert a mistyped
/// `key_source` into permanent, unrecoverable loss of the data. This function is
/// the evidence that stops that.
///
/// The scan is bounded (`KEY_PROBE_BUDGET`) and short-circuits on proof, so on a
/// correctly configured database it reads one value. It runs only on the open
/// that finds no sentinel, i.e. once in a database's lifetime.
///
/// `total_order_seek` is required: the column family uses a fixed 5-byte prefix
/// extractor, so a seek without it can stop inside the wrong prefix block.
fn probe_key_against_existing_data(db: &DB, km: &KeyManager) -> Result<KeyEvidence> {
    let mut examined = 0usize;
    let mut saw_opaque_value = false;

    for prefix in KEY_PROBE_PREFIXES {
        if examined >= KEY_PROBE_BUDGET {
            break;
        }
        let mut read_opts = rocksdb::ReadOptions::default();
        read_opts.set_total_order_seek(true);
        let iter = db.iterator_opt(
            rocksdb::IteratorMode::From(prefix, rocksdb::Direction::Forward),
            read_opts,
        );
        for item in iter {
            let (key, value) = item.map_err(|e| {
                Error::storage(format!(
                    "Failed to scan the database to verify the encryption key: {}",
                    e
                ))
            })?;
            if !key.starts_with(prefix) {
                break;
            }
            if matches!(
                crypto::try_decrypt(km.key(), &value),
                crypto::DecryptAttempt::Authenticated(_)
            ) {
                return Ok(KeyEvidence::KeyProven);
            }
            if !value_is_recognisable_plaintext(&key, &value) {
                saw_opaque_value = true;
            }
            examined += 1;
            if examined >= KEY_PROBE_BUDGET {
                break;
            }
        }
    }

    Ok(if examined == 0 {
        KeyEvidence::NoStoredValues
    } else if saw_opaque_value {
        KeyEvidence::Contradicted
    } else {
        KeyEvidence::PredatesSealing
    })
}

/// Check the configured key against this database, and install the sentinel on
/// a database that does not have one yet.
///
/// Called once per open, before anything reads a stored value. The four cases,
/// each deliberate:
///
/// 1. **Encryption off, no sentinel** — the default configuration. Nothing to
///    do beyond the single `db.get` that established it (an open-time cost, not
///    a per-row one; no other work is performed and no key is touched).
/// 2. **Encryption off, sentinel present** — this data directory was written
///    with encryption ENABLED. Its stored values are ciphertext and no key is
///    configured to open them, so opening would hand ciphertext to the row
///    decoders. Refuse, and say which knob is missing.
/// 3. **Encryption on, sentinel present** — the load-bearing case. Strict
///    decrypt; a failure means the configured key is not the key this database
///    was sealed with. Refuse to open, naming the key SOURCE (never key
///    material), so a wrong or rotated key is one clear error at open instead
///    of an unbounded number of unreadable values later.
/// 4. **Encryption on, no sentinel** — a database created before the sentinel
///    existed, or one being encrypted for the first time. The sentinel is
///    written only when its correctness can be justified from what is already on
///    disk, which is what [`probe_key_against_existing_data`] establishes: a
///    sentinel is a PERMANENT statement about which key this database belongs
///    to, so installing one under an unverified key would turn a corrected
///    typo in `key_source` into a database that can never be opened again. The
///    three admissible outcomes and the one refusal are enumerated on that
///    function. A read-only handle must not write, so it verifies when it can
///    and installs nothing.
pub(crate) fn verify_or_install_key_check(db: &DB, key_manager: Option<&KeyManager>, read_only: bool) -> Result<()> {
    let stored = db
        .get(KEY_CHECK_KEY)
        .map_err(|e| Error::storage(format!("Failed to read the encryption key-check sentinel: {}", e)))?;

    match (key_manager, stored) {
        // (1) The default configuration.
        (None, None) => Ok(()),

        // (2) An encrypted database opened with no key configured.
        (None, Some(_)) => Err(Error::encryption(
            "this data directory was written with encryption enabled, but [encryption] enabled is false in this \
             configuration; its stored values cannot be read without the key. Set [encryption] enabled = true and \
             point key_source at the key this database was created with.",
        )),

        // (3) Verify the configured key against the database.
        (Some(km), Some(sealed)) => {
            let plaintext = crypto::decrypt(km.key(), &sealed).map_err(|_| {
                Error::encryption(format!(
                    "the configured encryption key does not match this database (key source: {:?}). Refusing to \
                     open: continuing would read every stored value as if it were unencrypted and write new values \
                     under a key the existing ones were not sealed with.",
                    km.source()
                ))
            })?;
            if plaintext == KEY_CHECK_PLAINTEXT {
                Ok(())
            } else {
                Err(Error::encryption(format!(
                    "the encryption key-check sentinel at '{}' decrypted to unexpected contents; this data \
                     directory is not in a state this build can safely open (key source: {:?}).",
                    String::from_utf8_lossy(KEY_CHECK_KEY),
                    km.source()
                )))
            }
        }

        // (4) No sentinel yet: install one only where the evidence on disk
        //     justifies it.
        (Some(km), None) => {
            if read_only {
                warn!(
                    "no encryption key-check sentinel in this database and this handle is read-only, so the \
                     configured key cannot be verified against it; open a writable handle once to install one"
                );
                return Ok(());
            }
            match probe_key_against_existing_data(db, km)? {
                KeyEvidence::NoStoredValues | KeyEvidence::KeyProven | KeyEvidence::PredatesSealing => {}
                KeyEvidence::Contradicted => {
                    return Err(Error::encryption(format!(
                        "this database holds stored values that the configured encryption key does not open and that \
                         are not in this build's unencrypted form either (key source: {:?}). Refusing to open: \
                         recording this key as the database's key is permanent, and doing it on a database sealed \
                         with a different key would lock that key out for good. Point [encryption] key_source at the \
                         key this database was created with.",
                        km.source()
                    )));
                }
            }
            let sealed = crypto::encrypt(km.key(), KEY_CHECK_PLAINTEXT)?;
            db.put(KEY_CHECK_KEY, &sealed)
                .map_err(|e| Error::storage(format!("Failed to install the encryption key-check sentinel: {}", e)))?;
            Ok(())
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn test_key_manager() -> KeyManager {
        KeyManager::generate_random()
    }

    #[test]
    fn disabled_codec_is_a_passthrough_in_both_directions() {
        let value = b"plain row bytes".to_vec();

        // The DEFAULT configuration. `Cow::Borrowed` is the zero-cost claim
        // made in this module's header, asserted rather than asserted-by-comment.
        let sealed = seal(None, &value).expect("seal");
        assert!(matches!(sealed, Cow::Borrowed(_)), "no allocation when disabled");
        assert_eq!(sealed.as_ref(), &value[..]);

        let borrowed = open_ref(None, b"data:t:1", &value).expect("open_ref");
        assert!(matches!(borrowed, Cow::Borrowed(_)), "no allocation when disabled");

        // The counter must not move on the disabled path: it exists to report
        // plaintext found on an ENCRYPTED database, and a false count there
        // would send an operator hunting a problem that does not exist.
        let before = plaintext_passthrough_count();
        let opened = open_owned(None, b"data:t:1", value.clone()).expect("open");
        assert_eq!(opened, value);
        assert_eq!(
            plaintext_passthrough_count(),
            before,
            "the disabled path must never touch the passthrough counter"
        );
    }

    #[test]
    fn sealed_value_round_trips_and_is_not_the_plaintext() {
        let km = test_key_manager();
        let value = b"secret row bytes".to_vec();

        let sealed = seal(Some(&km), &value).expect("seal");
        assert_ne!(sealed.as_ref(), &value[..], "stored bytes must not be the plaintext");
        assert!(sealed.len() >= value.len() + crate::crypto::MIN_CIPHERTEXT_LEN);

        let opened = open_owned(Some(&km), b"data:t:1", sealed.into_owned()).expect("open");
        assert_eq!(opened, value);
    }

    #[test]
    fn legacy_plaintext_value_still_reads_on_an_encrypted_database() {
        let km = test_key_manager();
        let legacy = b"a value written before the boundary sealed it".to_vec();
        let opened = open_owned(Some(&km), b"data:t:7", legacy.clone()).expect("open");
        assert_eq!(opened, legacy, "a mixed-format database must stay readable");
    }

    #[test]
    fn row_value_keys_are_sealed_and_foreign_keyspaces_are_not() {
        let km = test_key_manager();
        let value = b"row bytes".to_vec();

        for key in [
            b"data:users:1".as_slice(),
            b"v:users:1:42".as_slice(),
            b"counter:users".as_slice(),
            b"wal:entries:00000000000000000042".as_slice(),
            // Branch row overlays and MV delta records are full row images and
            // are sealed by the same rule as `data:`.
            b"bdata:2:data:users:1".as_slice(),
            b"delta:users:00000000000000000007".as_slice(),
        ] {
            let sealed = seal_row_value(Some(&km), key, &value).expect("seal");
            assert_ne!(sealed.as_ref(), &value[..], "{:?} must be sealed", key);
        }

        // `wal:last_lsn` must NOT be caught by the WAL prefix and
        // `meta:delta:last_id` must NOT be caught by the delta prefix (both are
        // fixed-width counters), and the sidecars stay verbatim so their raw
        // readers keep working. `bdel:` markers carry no value at all.
        for key in [
            b"col:users:name:0".as_slice(),
            b"v_idx:users:1:00000000000000000042".as_slice(),
            b"meta:table:users".as_slice(),
            b"wal:last_lsn".as_slice(),
            b"meta:delta:last_id".as_slice(),
            b"bdel:2:users:1".as_slice(),
        ] {
            let sealed = seal_row_value(Some(&km), key, &value).expect("seal");
            assert_eq!(sealed.as_ref(), &value[..], "{:?} must stay verbatim", key);
        }
    }

    #[test]
    fn seal_owned_returns_the_input_buffer_unmoved_when_disabled() {
        let value = b"plain row bytes".to_vec();
        let before = value.as_ptr();
        let sealed = seal_owned(None, value).expect("seal_owned");
        assert_eq!(
            sealed.as_ptr(),
            before,
            "the disabled path must hand back the SAME allocation, not a copy"
        );
        assert_eq!(sealed, b"plain row bytes".to_vec());

        let km = test_key_manager();
        let sealed = seal_owned(Some(&km), b"secret".to_vec()).expect("seal_owned");
        assert_ne!(sealed, b"secret".to_vec());
        assert_eq!(
            open_owned(Some(&km), b"wal:entries:1", sealed).expect("open"),
            b"secret"
        );
    }

    #[test]
    fn the_passthrough_counter_ignores_keyspaces_that_are_stored_verbatim() {
        let km = test_key_manager();
        let verbatim = b"a sidecar batch, stored as it is by design".to_vec();

        // `PLAINTEXT_PASSTHROUGH_COUNT` is a process-global that sibling tests
        // in this binary also bump, so this measures a DELTA over enough
        // iterations that the gate's effect cannot be confused with that noise:
        // without the gate the delta would be exactly `ROUNDS * keys`, and the
        // only other tests here that can bump it do so at most once each.
        const ROUNDS: usize = 200;
        let excluded: [&[u8]; 5] = [
            b"col:users:name:0",
            b"colz:users:name:0",
            b"cas:0123456789abcdef",
            b"dict:users:name",
            b"snapshot:42",
        ];

        let before = plaintext_passthrough_count();
        for _ in 0..ROUNDS {
            for key in excluded {
                assert_eq!(
                    open_owned(Some(&km), key, verbatim.clone()).expect("open"),
                    verbatim,
                    "an excluded keyspace must still READ as plaintext — only the count is gated"
                );
            }
        }
        let drift = plaintext_passthrough_count() - before;
        assert!(
            drift < 20,
            "{} of {} reads in verbatim keyspaces were counted as plaintext passthroughs; the \
             diagnostic must not be driven by keyspaces that are plaintext by design",
            drift,
            ROUNDS * excluded.len()
        );

        // …and a value that IS in a sealed keyspace still counts, or the gate
        // would have silenced the thing it exists to report.
        let before = plaintext_passthrough_count();
        assert_eq!(
            open_owned(Some(&km), b"data:users:1", verbatim.clone()).expect("open"),
            verbatim
        );
        assert!(
            plaintext_passthrough_count() > before,
            "a plaintext value under a SEALED key must still be counted"
        );
    }

    // ---- key-verification sentinel ----------------------------------------

    fn scratch_db(name: &str) -> (tempfile::TempDir, DB) {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let mut opts = rocksdb::Options::default();
        opts.create_if_missing(true);
        let db = DB::open(&opts, dir.path().join(name)).expect("open rocksdb");
        (dir, db)
    }

    #[test]
    fn the_default_configuration_needs_no_sentinel() {
        let (_dir, db) = scratch_db("plain");
        verify_or_install_key_check(&db, None, false).expect("a plain database must open");
        assert!(
            db.get(KEY_CHECK_KEY).expect("get").is_none(),
            "no sentinel may be written when encryption is off"
        );
    }

    #[test]
    fn the_right_key_installs_a_sentinel_and_then_verifies_against_it() {
        let (_dir, db) = scratch_db("keyed");
        let km = test_key_manager();

        // First open: nothing to check against, so it opens AND gains a sentinel.
        verify_or_install_key_check(&db, Some(&km), false).expect("first open installs");
        let sealed = db.get(KEY_CHECK_KEY).expect("get").expect("sentinel installed");
        assert_ne!(sealed, KEY_CHECK_PLAINTEXT, "the sentinel must be stored sealed");

        // Every later open verifies, and does not rewrite it.
        verify_or_install_key_check(&db, Some(&km), false).expect("second open verifies");
        assert_eq!(db.get(KEY_CHECK_KEY).expect("get").expect("still there"), sealed);
    }

    #[test]
    fn a_different_key_is_refused_at_open_instead_of_reading_garbage() {
        let (_dir, db) = scratch_db("wrongkey");
        let right = test_key_manager();
        verify_or_install_key_check(&db, Some(&right), false).expect("install");

        let wrong = test_key_manager();
        let err = verify_or_install_key_check(&db, Some(&wrong), false)
            .expect_err("a database must not open under a key it was not sealed with");
        let msg = err.to_string();
        assert!(
            msg.contains("does not match this database"),
            "the error must name the cause, got: {msg}"
        );

        // And it must be the STRICT check that rejected it — the tolerant read
        // classifies the very same bytes as "not ciphertext under this key" and
        // would have passed them through.
        assert!(matches!(
            crypto::try_decrypt(wrong.key(), &db.get(KEY_CHECK_KEY).expect("get").expect("sentinel")),
            crypto::DecryptAttempt::Unauthenticated
        ));
    }

    /// A row image as the engine stores it: a bincode `Tuple`.
    fn plaintext_row(note: &str) -> Vec<u8> {
        bincode::serialize(&crate::Tuple::new(vec![
            crate::Value::Int4(1),
            crate::Value::String(note.to_string()),
        ]))
        .expect("serialize tuple")
    }

    /// ★ THE SENTINEL MUST NOT BE INSTALLABLE ON FAITH.
    ///
    /// A database sealed under one key, opened under another, before it ever
    /// gained a sentinel — an encrypted data directory written by a build that
    /// predates the sentinel, opened with a mistyped `key_source`. Installing a
    /// sentinel here would record the WRONG key permanently and lock the real
    /// one out forever, which is worse than the misconfiguration it started as.
    #[test]
    fn a_sealed_database_with_no_sentinel_refuses_a_key_that_opens_nothing() {
        let (_dir, db) = scratch_db("sealed_no_sentinel");
        let right = test_key_manager();
        let wrong = test_key_manager();

        // Data sealed under `right`, and deliberately NO sentinel.
        db.put(
            b"data:users:1",
            crypto::encrypt(right.key(), &plaintext_row("row one")).expect("seal"),
        )
        .expect("put");
        db.put(
            b"counter:users",
            crypto::encrypt(right.key(), &bincode::serialize(&1u64).expect("ser")).expect("seal"),
        )
        .expect("put");

        let err = verify_or_install_key_check(&db, Some(&wrong), false)
            .expect_err("a key that opens nothing must not be recorded as this database's key");
        assert!(
            err.to_string().contains("does not open"),
            "the error must say the key opened nothing, got: {err}"
        );
        assert!(
            db.get(KEY_CHECK_KEY).expect("get").is_none(),
            "*** IRREVERSIBLE *** a refused open must leave no sentinel behind, or the correct key \
             can never open this database again"
        );

        // …and the RIGHT key still opens it and installs the sentinel, which is
        // what makes the assertion above a discrimination rather than a blanket
        // refusal.
        verify_or_install_key_check(&db, Some(&right), false).expect("the key that seals this data must open it");
        assert!(db.get(KEY_CHECK_KEY).expect("get").is_some(), "the proven key installs");
    }

    /// The upgrade case the tolerant read exists for: a database whose stored
    /// values are all pre-encryption plaintext must still open and gain a
    /// sentinel. "No sealed value exists to check against" is a different state
    /// from "a sealed value exists and this key does not open it".
    #[test]
    fn a_wholly_plaintext_database_still_opens_and_gains_a_sentinel() {
        let (_dir, db) = scratch_db("plaintext_upgrade");
        let km = test_key_manager();

        db.put(b"data:users:1", plaintext_row("legacy row one")).expect("put");
        db.put(b"data:users:2", plaintext_row("legacy row two")).expect("put");
        db.put(b"counter:users", bincode::serialize(&2u64).expect("ser"))
            .expect("put");

        verify_or_install_key_check(&db, Some(&km), false)
            .expect("a database whose values predate sealing must open and gain a sentinel");
        assert!(
            db.get(KEY_CHECK_KEY).expect("get").is_some(),
            "the upgrade path must install a sentinel so every later open is verified"
        );
    }

    /// A database with nothing in the probed keyspaces is new: there is nothing
    /// on disk that could contradict the key, so it installs.
    #[test]
    fn a_new_database_installs_without_needing_anything_to_check_against() {
        let (_dir, db) = scratch_db("brand_new");
        let km = test_key_manager();
        verify_or_install_key_check(&db, Some(&km), false).expect("a new database must open");
        assert!(db.get(KEY_CHECK_KEY).expect("get").is_some());
    }

    /// The discriminator itself, on the two shapes that matter: a sealed value
    /// must NOT be mistaken for plaintext, and each keyspace's real plaintext
    /// form must be recognised.
    #[test]
    fn the_plaintext_discriminator_separates_sealed_bytes_from_stored_values() {
        let km = test_key_manager();
        let row = plaintext_row("a row");

        assert!(value_is_recognisable_plaintext(b"data:users:1", &row));
        assert!(value_is_recognisable_plaintext(
            b"counter:users",
            &bincode::serialize(&7u64).expect("ser")
        ));
        assert!(
            value_is_recognisable_plaintext(b"data:users:1", b""),
            "a value too short to be an AEAD frame is plaintext by construction"
        );

        for key in [b"data:users:1".as_slice(), b"counter:users".as_slice()] {
            let sealed = crypto::encrypt(km.key(), &row).expect("seal");
            assert!(
                !value_is_recognisable_plaintext(key, &sealed),
                "a sealed value under {:?} must not be read as plaintext, or a wrong key would \
                 install a sentinel",
                key
            );
        }
    }

    #[test]
    fn an_encrypted_database_is_refused_when_no_key_is_configured() {
        let (_dir, db) = scratch_db("nokey");
        let km = test_key_manager();
        verify_or_install_key_check(&db, Some(&km), false).expect("install");

        let err = verify_or_install_key_check(&db, None, false)
            .expect_err("an encrypted database must not open with encryption disabled");
        assert!(err.to_string().contains("written with encryption enabled"), "{err}");
    }

    #[test]
    fn a_read_only_handle_verifies_but_never_installs() {
        let (_dir, db) = scratch_db("readonly");
        let km = test_key_manager();

        // No sentinel and no write permission: open, install nothing.
        verify_or_install_key_check(&db, Some(&km), true).expect("read-only open must succeed");
        assert!(
            db.get(KEY_CHECK_KEY).expect("get").is_none(),
            "a read-only handle must not write the sentinel"
        );

        // Once one exists, a read-only handle still verifies it.
        verify_or_install_key_check(&db, Some(&km), false).expect("install");
        let wrong = test_key_manager();
        verify_or_install_key_check(&db, Some(&wrong), true)
            .expect_err("a read-only handle must still refuse the wrong key");
    }
}
