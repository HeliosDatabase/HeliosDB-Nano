//! Actual pinned RocksDB WAL rotation, without synthesizing a storage error.

use super::*;
use rocksdb::checkpoint::Checkpoint;
use rocksdb::ErrorKind;
use std::cell::RefCell;
use tempfile::TempDir;

type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;
type AfterOpen = Box<dyn FnOnce(&DB) -> super::Result<()>>;

#[derive(Default)]
struct Hooks {
    after_open: Option<AfterOpen>,
    errors: Vec<ErrorKind>,
    active: bool,
}

thread_local! {
    static HOOKS: RefCell<Hooks> = const { RefCell::new(Hooks { after_open: None, errors: Vec::new(), active: false }) };
}

pub(super) fn after_iterator_open(db: &DB) -> super::Result<()> {
    let hook = HOOKS.with(|slot| slot.borrow_mut().after_open.take());
    if let Some(hook) = hook {
        hook(db)?;
    }
    Ok(())
}

pub(super) fn observe_status(status: &std::result::Result<(), rocksdb::Error>) {
    if let Err(error) = status {
        HOOKS.with(|slot| {
            let mut hooks = slot.borrow_mut();
            if hooks.active {
                hooks.errors.push(error.kind());
            }
        });
    }
}

struct HookGuard;

impl Drop for HookGuard {
    fn drop(&mut self) {
        HOOKS.with(|slot| *slot.borrow_mut() = Hooks::default());
    }
}

fn rotate_after_open(key: &'static [u8]) -> HookGuard {
    HOOKS.with(|slot| {
        *slot.borrow_mut() = Hooks {
            after_open: Some(Box::new(move |db| {
                // Flush rolls the nonempty memtable's WAL. The iterator has
                // already captured the old file list, as in upstream RocksDB
                // TransactionLogIteratorNewFileWhileScanning.
                db.flush()?;
                db.put(key, b"new-tail")?;
                Ok(())
            })),
            errors: Vec::new(),
            active: true,
        };
    });
    HookGuard
}

fn assert_real_try_again() {
    HOOKS.with(|slot| {
        let hooks = slot.borrow();
        assert!(hooks.after_open.is_none(), "the real iterator boundary was reached");
        assert!(
            hooks.errors.contains(&ErrorKind::TryAgain),
            "the native iterator, rather than a synthetic error, must report TryAgain: {:?}",
            hooks.errors
        );
        assert!(hooks.errors.iter().all(|kind| *kind == ErrorKind::TryAgain));
    });
}

fn open(path: &Path) -> TestResult<DB> {
    let mut options = Options::default();
    options.create_if_missing(true);
    options.set_wal_ttl_seconds(3600);
    Ok(DB::open(&options, path)?)
}

fn batch(db: &DB, key: &[u8]) -> TestResult {
    let mut group = WriteBatch::default();
    group.put(key, b"value");
    group.put([key, b":peer"].concat(), b"peer-value");
    db.write(group)?;
    Ok(())
}

fn source_and_receiver(root: &Path, history: Uuid) -> TestResult<(DB, StagedPhysicalReplica, u64)> {
    let source = open(&root.join("source"))?;
    source.put(b"seed", b"keep")?;
    let cut = source.latest_sequence_number();
    let receiver_path = root.join("receiver");
    Checkpoint::new(&source)?.create_checkpoint(&receiver_path)?;
    let receiver = StagedPhysicalReplica::from_checkpoint(open(&receiver_path)?, history, cut)?;
    Ok((source, receiver, cut))
}

fn finish_and_prove(
    source: &DB,
    receiver: &StagedPhysicalReplica,
    history: Uuid,
    cut: u64,
    first: PhysicalPoll,
    memo: &FlushMemo,
) -> TestResult {
    let through = first.through;
    let mut imported = Vec::new();
    let mut expected = cut + 1;
    for group in &first.batches {
        assert_eq!(group.first_sequence(), expected);
        expected = group.last_sequence() + 1;
        assert!(receiver.apply(group)?);
        imported.push(group.data().to_vec());
    }
    assert_eq!(receiver.cursor().source_sequence(), through);
    assert_eq!(expected, through + 1);

    // The next ordinary poll opens a fresh native iterator at exactly the
    // validated prefix; it must neither skip nor reapply its final group.
    let rest = poll_source(source, history, through, MAX_BATCH_BYTES, memo)?;
    assert!(!rest.batches.is_empty(), "the new WAL tail remains to be served");
    assert_eq!(rest.through, source.latest_sequence_number());
    assert_eq!(rest.through, rest.durable_barrier);
    for group in &rest.batches {
        assert_eq!(group.first_sequence(), expected);
        expected = group.last_sequence() + 1;
        assert!(receiver.apply(group)?);
        assert!(!receiver.apply(group)?, "duplicate ACK retry is idempotent");
        imported.push(group.data().to_vec());
    }
    assert_eq!(receiver.cursor().source_sequence(), rest.through);
    assert_eq!(read_cursor(receiver.writer().db())?, receiver.cursor());
    assert_eq!(
        range_digest(source, None, b"")?,
        range_digest(receiver.writer().db(), None, b"")?,
        "the complete source default family is copied byte for byte"
    );

    let mut native = source.get_updates_since(cut)?;
    let raw: Vec<_> = native
        .by_ref()
        .map(|entry| entry.map(|(_, batch)| batch.data().to_vec()))
        .collect::<std::result::Result<_, _>>()?;
    native.status()?;
    assert_eq!(imported, raw, "all native owning groups and bytes arrive exactly once");
    Ok(())
}

#[test]
fn a_real_native_wal_rotation_requests_a_fresh_iterator() -> TestResult {
    let root = TempDir::new()?;
    let db = open(root.path())?;
    batch(&db, b"first")?;
    let old_barrier = db.latest_sequence_number();
    let mut old = db.get_updates_since(0)?;
    db.flush()?;
    db.put(b"later", b"tail")?;

    let (first, group) = old.next().transpose()?.ok_or("the initial native group is missing")?;
    assert_eq!(first, 1);
    assert_eq!(group.len() as u64, old_barrier);
    assert!(old.next().is_none());
    let error = old
        .status()
        .err()
        .ok_or("the native tail status is unexpectedly successful")?;
    assert_eq!(error.kind(), ErrorKind::TryAgain);
    assert!(error
        .to_string()
        .contains("Create a new iterator to fetch the new tail."));
    let mut fresh = db.get_updates_since(old_barrier)?;
    assert_eq!(
        fresh.next().transpose()?.ok_or("the new native tail is missing")?.0,
        old_barrier + 1
    );
    assert!(fresh.next().is_none());
    fresh.status()?;
    eprintln!("physical-wal-tail-cell native_rotation");
    Ok(())
}

#[test]
fn a_completed_poll_keeps_its_sampled_barrier_after_real_wal_rotation() -> TestResult {
    let root = TempDir::new()?;
    let history = Uuid::new_v4();
    let (source, receiver, cut) = source_and_receiver(root.path(), history)?;
    batch(&source, b"first")?;
    let sampled = source.latest_sequence_number();
    let memo = FlushMemo::new();
    let guard = rotate_after_open(b"later");
    let result = poll_source(&source, history, cut, MAX_BATCH_BYTES, &memo);
    assert_real_try_again();
    drop(guard);
    let first = result?;
    assert_eq!(first.batches.len(), 1);
    assert_eq!((first.through, first.durable_barrier), (sampled, sampled));
    assert!(first.through < source.latest_sequence_number());
    assert_eq!(
        memo.last_flushed(),
        sampled,
        "the later tail was not falsely declared fsynced"
    );
    finish_and_prove(&source, &receiver, history, cut, first, &memo)?;
    eprintln!("physical-wal-tail-cell completed_barrier");
    Ok(())
}

#[test]
fn a_byte_limited_prefix_survives_real_wal_rotation_without_skipping_a_group() -> TestResult {
    let root = TempDir::new()?;
    let history = Uuid::new_v4();
    let (source, receiver, cut) = source_and_receiver(root.path(), history)?;
    batch(&source, b"first")?;
    let prefix = source.latest_sequence_number();
    batch(&source, b"second")?;
    let sampled = source.latest_sequence_number();
    let memo = FlushMemo::new();
    let guard = rotate_after_open(b"later");
    let result = poll_source(&source, history, cut, 1, &memo);
    assert_real_try_again();
    drop(guard);
    let first = result?;
    assert_eq!(first.batches.len(), 1);
    assert_eq!((first.through, first.durable_barrier), (prefix, sampled));
    assert!(first.through < first.durable_barrier, "partial does not mean caught up");
    finish_and_prove(&source, &receiver, history, cut, first, &memo)?;
    eprintln!("physical-wal-tail-cell bounded_prefix");
    Ok(())
}

#[test]
fn no_validated_prefix_preserves_real_try_again_and_the_interior_cursor_refusal() -> TestResult {
    let root = TempDir::new()?;
    let source = open(root.path())?;
    source.put(b"seed", b"keep")?;
    batch(&source, b"group")?;
    let interior = 2;
    let history = Uuid::new_v4();
    let guard = rotate_after_open(b"later");
    let result = poll_source(&source, history, interior, MAX_BATCH_BYTES, &FlushMemo::new());
    assert_real_try_again();
    drop(guard);
    let error = result.err().ok_or("the no-prefix poll was unexpectedly successful")?;
    let PhysicalError::Storage(error) = error else {
        return Err(format!("no-progress native tail failure must stay a storage refusal: {error}").into());
    };
    assert_eq!(error.kind(), ErrorKind::TryAgain);
    assert!(error
        .to_string()
        .contains("Create a new iterator to fetch the new tail."));

    // A stable fresh iterator now exposes the actual interior-group cursor:
    // no empty success, fabricated barrier advance, or skipped operation.
    assert!(matches!(
        poll_source(&source, history, interior, MAX_BATCH_BYTES, &FlushMemo::new()),
        Err(PhysicalError::Gap {
            expected: 3,
            observed: 4
        })
    ));
    eprintln!("physical-wal-tail-cell no_prefix_refusal");
    Ok(())
}
