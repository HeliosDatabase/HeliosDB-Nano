//! External primitive proof draft for pinned rocksdb 0.22 / RocksDB 8.10.
//! Private TempDir databases only; no Nano source changes, ports or global HA.
//! These tests prove raw storage primitives, NOT Nano derived-state publication,
//! network transfer safety, retention leases, process-crash durability or resync.
//! The unsupported/missing-history tests intentionally require rejection.

use rocksdb::{checkpoint::Checkpoint, IteratorMode, Options, WriteBatch, WriteOptions, DB};
use std::{
    collections::BTreeMap,
    fs,
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const CURSOR: &[u8] = b"__private_receiver_source_cursor";
const OPS: u64 = 4;

fn options() -> Options {
    let mut options = Options::default();
    options.create_if_missing(true);
    // Retain archived WAL for this bounded fixture. TTL is NOT a retention pin.
    options.set_wal_ttl_seconds(3600);
    options.set_wal_size_limit_mb(0);
    options
}

fn sync_write() -> WriteOptions {
    let mut options = WriteOptions::default();
    options.set_sync(true);
    options
}

fn batch(epoch: u64) -> WriteBatch {
    let mut batch = WriteBatch::default();
    batch.put(b"left", epoch.to_be_bytes());
    batch.put(b"right", epoch.to_be_bytes());
    batch.put(format!("history/{epoch:08}"), epoch.to_be_bytes());
    // A missing-key DELETE is still a counted source operation/sequence.
    batch.delete(format!("obsolete/{epoch:08}"));
    assert_eq!(batch.len(), OPS as usize);
    batch
}

fn number(value: Option<Vec<u8>>) -> Option<u64> {
    value.map(|value| u64::from_be_bytes(value.try_into().expect("fixture value must be eight bytes")))
}

fn rows(db: &DB) -> BTreeMap<Vec<u8>, Vec<u8>> {
    db.iterator(IteratorMode::Start)
        .map(|item| {
            let (key, value) = item.expect("row iterator must not conceal an error");
            (key.to_vec(), value.to_vec())
        })
        .collect()
}

fn assert_complete_cut(db: &DB, cut: u64) {
    assert!(cut >= OPS);
    assert_eq!(cut % OPS, 0, "checkpoint cut split a counted batch");
    let epoch = cut / OPS;
    let rows = rows(db);
    assert_eq!(rows.len(), epoch as usize + 2);
    assert_eq!(number(rows.get(b"left".as_slice()).cloned()), Some(epoch));
    assert_eq!(number(rows.get(b"right".as_slice()).cloned()), Some(epoch));
    for committed in 1..=epoch {
        assert_eq!(
            number(rows.get(format!("history/{committed:08}").as_bytes()).cloned()),
            Some(committed)
        );
    }
}

#[derive(Debug)]
struct ExportedBatch {
    first: u64,
    last: u64,
    count: usize,
    raw: Vec<u8>,
}

// This checked exporter is test scaffolding, not a production protocol. Call
// only after source writes stop and flush_wal(true) establishes its barrier.
fn export_checked(db: &DB, after: u64, barrier: u64) -> Result<Vec<ExportedBatch>, String> {
    if after > barrier {
        return Err("cursor exceeds barrier".into());
    }
    let mut iterator = db
        .get_updates_since(after)
        .map_err(|error| format!("WAL open: {error}"))?;
    let mut cursor = after;
    let mut batches = Vec::new();
    while let Some(result) = iterator.next() {
        let (first, batch) = result.map_err(|error| format!("WAL batch: {error}"))?;
        let count = batch.len();
        if count == 0 {
            return Err("zero-count batch needs an explicit protocol policy".into());
        }
        let expected = cursor.checked_add(1).ok_or("sequence overflow")?;
        let last = first.checked_add(count as u64 - 1).ok_or("sequence overflow")?;
        if first != expected || last > barrier {
            return Err(format!(
                "incomplete interval: expected {expected}, got {first}..={last}, barrier {barrier}"
            ));
        }
        batches.push(ExportedBatch {
            first,
            last,
            count,
            raw: batch.data().to_vec(),
        });
        cursor = last;
    }
    // next()==None can mean an invalid iterator. Never treat it alone as EOF.
    iterator
        .status()
        .map_err(|error| format!("WAL final status: {error}"))?;
    if cursor != barrier {
        return Err(format!("missing tail: stopped at {cursor}, barrier {barrier}"));
    }
    Ok(batches)
}

fn import_with_cursor(receiver: &DB, exported: &ExportedBatch) {
    let mut imported = WriteBatch::from_data(&exported.raw);
    assert_eq!(
        imported.data(),
        exported.raw.as_slice(),
        "raw round trip changed batch bytes"
    );
    assert_eq!(imported.len(), exported.count);
    assert_eq!(exported.last - exported.first + 1, exported.count as u64);
    imported.put(CURSOR, exported.last.to_be_bytes());
    assert_eq!(imported.len(), exported.count + 1);
    receiver.write_opt(imported, &sync_write()).unwrap();
}

// Ensure checkpoint assertion failures still stop/join the private writer
// before its TempDir is removed. No detached worker can outlive this fixture.
struct Writer {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<u64>>,
}
impl Writer {
    fn finish(mut self) -> u64 {
        self.stop.store(true, Ordering::Release);
        self.handle.take().unwrap().join().expect("writer failed")
    }
}
impl Drop for Writer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

struct Importer(Option<JoinHandle<()>>);
impl Importer {
    fn finish(mut self) {
        self.0.take().unwrap().join().expect("importer failed");
    }
}
impl Drop for Importer {
    fn drop(&mut self) {
        // Also join if a snapshot/precondition assertion panics. The worker's
        // only channel wait has a five-second timeout and its writes are finite.
        if let Some(handle) = self.0.take() {
            let _ = handle.join();
        }
    }
}

#[test]
fn concurrent_checkpoint_cut_and_raw_continuation_preserve_complete_batches() {
    let root = tempfile::tempdir().unwrap();
    let source = Arc::new(DB::open(&options(), root.path().join("source")).unwrap());
    source.write_opt(batch(1), &sync_write()).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let (started_tx, started_rx) = mpsc::channel();
    let writer = Writer {
        stop: stop.clone(),
        handle: Some(thread::spawn({
            let source = source.clone();
            move || {
                let mut last = 1;
                for epoch in 2..=4096 {
                    if stop.load(Ordering::Acquire) {
                        break;
                    }
                    source.write(batch(epoch)).unwrap();
                    last = epoch;
                    if epoch == 2 {
                        started_tx.send(()).unwrap();
                    }
                    thread::sleep(Duration::from_millis(1));
                }
                last
            }
        })),
    };
    started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let mut checkpoints = Vec::new();
    let mut observed_write_during_checkpoint = false;
    for index in 0..3 {
        let path = root.path().join(format!("checkpoint-{index}"));
        assert!(!path.exists());
        let before = source.latest_sequence_number();
        Checkpoint::new(source.as_ref())
            .unwrap()
            .create_checkpoint(&path)
            .unwrap();
        let after = source.latest_sequence_number();
        observed_write_during_checkpoint |= after > before;
        // Read the cut from the FINISHED checkpoint, never infer it from source.
        let checkpoint = DB::open_for_read_only(&Options::default(), &path, false).unwrap();
        let cut = checkpoint.latest_sequence_number();
        assert!(
            before <= cut && cut <= after,
            "cut={cut}, source bracket={before}..={after}"
        );
        assert_complete_cut(&checkpoint, cut);
        drop(checkpoint);
        checkpoints.push((path, cut));
    }
    let last = writer.finish();
    assert!(
        observed_write_during_checkpoint,
        "inconclusive schedule: no source commit overlapped any checkpoint call"
    );
    // Every checkpoint has at least one subsequent complete batch to replay.
    source.write_opt(batch(last + 1), &sync_write()).unwrap();
    source.flush_wal(true).unwrap();
    let barrier = source.latest_sequence_number();
    assert_eq!(barrier, (last + 1) * OPS);
    let expected = rows(&source);
    for (path, cut) in checkpoints {
        let exported = export_checked(&source, cut, barrier).unwrap();
        assert!(!exported.is_empty());
        let receiver = DB::open(&options(), &path).unwrap();
        assert_eq!(receiver.latest_sequence_number(), cut);
        for batch in &exported {
            import_with_cursor(&receiver, batch);
        }
        assert_eq!(number(receiver.get(CURSOR).unwrap()), Some(barrier));
        // Receiver-local cursor puts consume LOCAL sequences, not source ones.
        assert_eq!(receiver.latest_sequence_number(), barrier + exported.len() as u64);
        let mut actual = rows(&receiver);
        actual.remove(CURSOR);
        assert_eq!(actual, expected);
        drop(receiver);
        let reopened = DB::open(&options(), &path).unwrap();
        assert_eq!(number(reopened.get(CURSOR).unwrap()), Some(barrier));
        let mut persisted = rows(&reopened);
        persisted.remove(CURSOR);
        assert_eq!(persisted, expected);
    }
}

#[test]
fn interior_batch_cursor_is_unsupported_and_must_not_skip_operations() {
    let root = tempfile::tempdir().unwrap();
    let db = DB::open(&options(), root.path().join("source")).unwrap();
    db.write(batch(1)).unwrap();
    db.write_opt(batch(2), &sync_write()).unwrap();
    db.flush_wal(true).unwrap();
    let mut iterator = db.get_updates_since(2).unwrap();
    // Pinned Rust wrapper skips the entire containing batch (first=1 <= 2).
    let (first, batch) = iterator.next().unwrap().unwrap();
    assert_eq!((first, batch.len()), (5, OPS as usize));
    assert!(iterator.next().is_none());
    iterator.status().unwrap();
    assert!(export_checked(&db, 2, 8).is_err());
    let valid = export_checked(&db, 4, 8).unwrap();
    assert_eq!(valid.first().map(|batch| (batch.first, batch.last)), Some((5, 8)));
    assert!(db.get_updates_since(9).is_err(), "out-of-range request must fail");
}

#[test]
fn imported_cursor_and_multikey_state_are_atomic_to_snapshot_readers() {
    let root = tempfile::tempdir().unwrap();
    let source = DB::open(&options(), root.path().join("source")).unwrap();
    for epoch in 1..=48 {
        source.write(batch(epoch)).unwrap();
    }
    source.flush_wal(true).unwrap();
    let exported = export_checked(&source, 0, 48 * OPS).unwrap();
    assert_eq!(exported.len(), 48);
    let path = root.path().join("receiver");
    let receiver = Arc::new(DB::open(&options(), &path).unwrap());
    let (first_tx, first_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();
    let worker = Importer(Some(thread::spawn({
        let receiver = receiver.clone();
        move || {
            for (index, batch) in exported.iter().enumerate() {
                import_with_cursor(&receiver, batch);
                if index == 0 {
                    first_tx.send(()).unwrap();
                    release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                }
                thread::sleep(Duration::from_micros(100));
            }
            done_tx.send(()).unwrap();
        }
    })));
    first_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut first_release = Some(release_tx);
    let mut observed = 0;
    let mut errors = Vec::new();
    loop {
        let snapshot = receiver.snapshot();
        let cursor = number(snapshot.get(CURSOR).unwrap());
        let left = number(snapshot.get(b"left").unwrap());
        let right = number(snapshot.get(b"right").unwrap());
        if !matches!(cursor, Some(c) if c % OPS == 0 && left == Some(c / OPS) && right == left) {
            errors.push(format!(
                "mixed snapshot cursor={cursor:?} left={left:?} right={right:?}"
            ));
        }
        observed += 1;
        drop(snapshot);
        if let Some(release) = first_release.take() {
            release.send(()).unwrap();
        }
        match done_rx.try_recv() {
            Ok(()) => break,
            Err(mpsc::TryRecvError::Disconnected) => {
                errors.push("importer disconnected".into());
                break;
            }
            Err(mpsc::TryRecvError::Empty) => (),
        }
        if Instant::now() >= deadline {
            errors.push("importer deadline exceeded".into());
            break;
        }
        thread::yield_now();
    }
    // Join before assertions/TempDir drop. No test-created worker is detached.
    worker.finish();
    assert!(errors.is_empty(), "{}", errors.join("\n"));
    assert!(observed > 0);
    assert_eq!(number(receiver.get(CURSOR).unwrap()), Some(48 * OPS));
    assert_eq!(receiver.latest_sequence_number(), 48 * (OPS + 1));
    drop(receiver);
    let reopened = DB::open(&options(), &path).unwrap();
    assert_eq!(number(reopened.get(CURSOR).unwrap()), Some(48 * OPS));
    let mut actual = rows(&reopened);
    actual.remove(CURSOR);
    assert_eq!(actual, rows(&source));
}

fn quarantine_closed_fixture_wals(source: &Path, quarantine: &Path) -> usize {
    fs::create_dir(quarantine).unwrap();
    let mut moved = 0;
    for (label, directory) in [("root", source.to_path_buf()), ("archive", source.join("archive"))] {
        if !directory.exists() {
            continue;
        }
        for entry in fs::read_dir(directory).unwrap() {
            let entry = entry.unwrap();
            if entry.path().extension().is_some_and(|extension| extension == "log") {
                assert!(entry.file_type().unwrap().is_file());
                let target = quarantine.join(format!("{label}-{}", entry.file_name().to_string_lossy()));
                assert!(!target.exists());
                fs::rename(entry.path(), target).unwrap();
                moved += 1;
            }
        }
    }
    moved
}

#[test]
fn retained_wal_reopens_but_missing_private_wal_is_never_a_complete_interval() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("source");
    let source = DB::open(&options(), &path).unwrap();
    source.write_opt(batch(1), &sync_write()).unwrap();
    source.flush().unwrap();
    source.write_opt(batch(2), &sync_write()).unwrap();
    source.flush().unwrap();
    source.flush_wal(true).unwrap();
    assert_eq!(export_checked(&source, 0, 8).unwrap().len(), 2);
    let expected = rows(&source);
    drop(source);
    let reopened = DB::open(&options(), &path).unwrap();
    assert_eq!(rows(&reopened), expected);
    assert_eq!(
        export_checked(&reopened, 0, 8).unwrap().len(),
        2,
        "bounded TTL retention across restart"
    );
    reopened.flush().unwrap();
    drop(reopened);
    // Fault injection into CLOSED, FLUSHED, test-owned storage only. Preserve
    // removed WAL files in the private quarantine, never touch a live DB file.
    assert!(quarantine_closed_fixture_wals(&path, &root.path().join("quarantine")) > 0);
    let missing = DB::open(&options(), &path).unwrap();
    assert_eq!(missing.latest_sequence_number(), 8);
    assert_eq!(rows(&missing), expected, "SST data must survive the WAL-loss fixture");
    missing.write_opt(batch(3), &sync_write()).unwrap();
    missing.flush_wal(true).unwrap();
    let outcome = export_checked(&missing, 0, 12);
    assert!(outcome.is_err(), "missing history was falsely complete: {outcome:?}");
    assert_eq!(export_checked(&missing, 8, 12).unwrap().len(), 1);
}

#[test]
fn wal_disabled_source_mutation_is_an_explicit_continuity_gap() {
    let root = tempfile::tempdir().unwrap();
    let source = DB::open(&options(), root.path().join("source")).unwrap();
    source.write_opt(batch(1), &sync_write()).unwrap();
    let mut without_wal = WriteOptions::default();
    without_wal.disable_wal(true);
    source.write_opt(batch(2), &without_wal).unwrap();
    source.write_opt(batch(3), &sync_write()).unwrap();
    source.flush_wal(true).unwrap();
    assert_eq!(source.latest_sequence_number(), 12);
    assert_eq!(number(source.get(b"left").unwrap()), Some(3));
    assert!(
        export_checked(&source, 4, 12).is_err(),
        "WAL-disabled writes must not be silently skipped"
    );
}

#[test]
fn raw_batch_roundtrip_preserves_delete_range_without_put_delete_callbacks() {
    let root = tempfile::tempdir().unwrap();
    let source = DB::open(&options(), root.path().join("source")).unwrap();
    let receiver = DB::open(&options(), root.path().join("receiver")).unwrap();
    let mut seed = WriteBatch::default();
    seed.put(b"range/a", b"a");
    seed.put(b"range/b", b"b");
    seed.put(b"survivor", b"keep");
    receiver.write(WriteBatch::from_data(seed.data())).unwrap();
    source.write(seed).unwrap();
    assert_eq!(source.latest_sequence_number(), 3);
    let mut mutation = WriteBatch::default();
    mutation.delete_range(b"range/", b"range0");
    mutation.put(b"after", b"committed");
    assert_eq!(mutation.len(), 2);
    source.write_opt(mutation, &sync_write()).unwrap();
    source.flush_wal(true).unwrap();
    let exported = export_checked(&source, 3, 5).unwrap();
    assert_eq!(exported.len(), 1);
    if let Some(batch) = exported.first() {
        import_with_cursor(&receiver, batch);
    }
    assert_eq!(receiver.get(b"range/a").unwrap(), None);
    assert_eq!(receiver.get(b"range/b").unwrap(), None);
    assert_eq!(receiver.get(b"survivor").unwrap(), Some(b"keep".to_vec()));
    let mut actual = rows(&receiver);
    actual.remove(CURSOR);
    assert_eq!(actual, rows(&source));
    // The pinned WriteBatchIterator trait only exposes put/delete callbacks.
    // This test never reconstructs raw batches through that incomplete surface.
}
