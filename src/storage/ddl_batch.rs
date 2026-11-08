//! Spec R5.3 (`docs/plans/specs/R5_3_ddl_batch.md`, parent R5 decision 3): one catalog
//! DDL statement, one durable write group.
//!
//! A [`DdlBatch`] is an explicit handle a schema statement creates at its start and
//! commits once at its end. Everything durable the statement does goes into it: the
//! catalog records and data keys it stages (the `stage_*` funnels of `Catalog` take
//! the batch), the logical-WAL entries it logs ([`DdlBatch::log`], sealed into the
//! same batch at commit, spec R1 §4) and, through `WriteGate`, the copies B4's freeze
//! hook adds for live child branches. `DdlBatch::commit` writes it with ONE
//! `WriteGate::write`, then publishes the in-memory state the statement changed
//! (caches, registries, ART and HNSW structures) in the order the statement queued it
//! ([`DdlBatch::on_commit`]), and hands back the `DdlWrite` whose replication wait
//! the caller makes last. A batch that fails to write, or that is dropped without a
//! commit, publishes nothing and runs what [`DdlBatch::on_abort`] queued, in reverse:
//! the undo of the few eager in-memory registrations a statement needs while it runs
//! (a new table's PRIMARY KEY tree, a new index registered hidden).
//!
//! # The detector
//!
//! Decision 3: no hidden capture. A thread-local marker exists ONLY to catch a write
//! that escapes the batch. While a batch is open on a thread, every `WriteGate` write
//! of the same gate that is neither the batch's own commit nor inside an
//! [`allow_side_write`] scope (with its reason) is a finding, and so is a logical-WAL
//! entry appended there instead of [`DdlBatch::log`]ged. It is counted in every build;
//! in test builds (`debug_assertions` or `internal-tests`) it also panics, naming the
//! writer, unless the thread is unwinding already (a second panic aborts the process,
//! R5.1's rule). In release it warns once per writing site. The detector looks at
//! nothing inside RocksDB's batch callback: a write is judged by who makes it, not by
//! its keys. With no batch open anywhere in the process the gate pays one relaxed
//! load.
//!
//! What a statement legitimately writes apart from its batch, and why it may, is the
//! allow-list ([`allow_side_write`]'s callers):
//!
//! * the ART / HNSW snapshot invalidation (`StorageEngine::delete_prefix_raw`): the
//!   first index mutation after a checkpoint deletes the derived snapshot markers so a
//!   crash never trusts a stale snapshot; it changes no catalog and no row;
//! * a dropped or renamed-away name's version history (`Catalog::
//!   purge_version_history_locked`): written in bounded chunks under a name that is no
//!   table, recorded by the statement's own batch (`meta:history_purge:`);
//! * persistent HNSW (decision 8): its own batch, BEFORE the definition that makes it
//!   reachable; a dangling id is filtered on read;
//! * what a volatile `ADD COLUMN` default does while it is evaluated, once per row
//!   ([`VOLATILE_DEFAULT_EFFECTS`], spec R5.5): `nextval` makes its sequence's
//!   high-water durable before it serves a value from it (R5 design §2.3: a value
//!   served from a block whose high-water rode an unwritten group would be served
//!   again after a crash). The values themselves ride the statement's group.

use std::cell::Cell;
use std::marker::PhantomData;
use std::panic::Location;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use rocksdb::WriteBatch;

use super::engine::{DdlWrite, StorageEngine};
use super::wal::WalOperation;
use super::write_gate::WriteGate;
use super::write_outcome::{WriteAttempt, WriteOutcome};
use crate::{Error, Result};

/// The allow-list reason of a columnar read's lazy backfill (`storage::columnar`).
pub const COLUMNAR_LAZY_BACKFILL: &str =
    "a columnar read's lazy backfill of derived side data (presence, zone stats, a v2 re-encoding)";

/// The allow-list reason of a volatile default's evaluation inside `ALTER TABLE` (spec
/// R5.5): a sequence's high-water is durable before the value it serves.
pub const VOLATILE_DEFAULT_EFFECTS: &str =
    "a volatile default's own effects while it is evaluated (a sequence's high-water, before the values it serves)";

/// Batches open anywhere in this process: the gate looks at its thread-local marker
/// only while this is not zero.
static OPEN_BATCHES: AtomicUsize = AtomicUsize::new(0);

/// Writes the detector found outside a batch, in every build.
static SIDE_WRITES: AtomicU64 = AtomicU64::new(0);

/// Sites the release detector has warned about.
#[cfg(not(any(debug_assertions, feature = "internal-tests")))]
static WARNED_SITES: std::sync::LazyLock<parking_lot::Mutex<std::collections::HashSet<(&'static str, u32)>>> =
    std::sync::LazyLock::new(|| parking_lot::Mutex::new(std::collections::HashSet::new()));

thread_local! {
    /// The gate of the batch open on this thread (its address), if any.
    static OPEN: Cell<Option<usize>> = const { Cell::new(None) };
    /// This thread is writing its batch.
    static COMMITTING: Cell<bool> = const { Cell::new(false) };
    /// Why this thread's writes apart from its batch are allowed (the allow-list).
    static SIDE_WRITE_ALLOWED: Cell<Option<&'static str>> = const { Cell::new(None) };
    /// `internal-tests`: the next commit on this thread fails before it writes.
    #[cfg(feature = "internal-tests")]
    static FAIL_NEXT_COMMIT: Cell<bool> = const { Cell::new(false) };
    /// A real group lands, then its result reports an uncertain failure.
    #[cfg(feature = "internal-tests")]
    static FAIL_NEXT_APPLIED_COMMIT: Cell<bool> = const { Cell::new(false) };
    #[cfg(feature = "internal-tests")]
    static AFTER_APPLIED_COMMIT: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
}

/// The identity of a gate in the marker.
fn gate_id(gate: &WriteGate) -> usize {
    std::ptr::from_ref(gate) as usize
}

/// A batch open on this thread, until it drops.
struct OpenMarker {
    previous: Option<usize>,
    _not_send: PhantomData<*const ()>,
}

impl OpenMarker {
    fn enter(gate: &WriteGate) -> Self {
        OPEN_BATCHES.fetch_add(1, Ordering::Relaxed);
        Self {
            previous: OPEN.with(|open| open.replace(Some(gate_id(gate)))),
            _not_send: PhantomData,
        }
    }
}

impl Drop for OpenMarker {
    fn drop(&mut self) {
        OPEN.with(|open| open.set(self.previous));
        OPEN_BATCHES.fetch_sub(1, Ordering::Relaxed);
    }
}

/// The batch's own write, while it lasts.
struct Committing {
    previous: bool,
}

impl Committing {
    fn enter() -> Self {
        Self {
            previous: COMMITTING.with(|committing| committing.replace(true)),
        }
    }
}

impl Drop for Committing {
    fn drop(&mut self) {
        COMMITTING.with(|committing| committing.set(self.previous));
    }
}

/// This thread's writes apart from its open batch are allowed, for the reason given,
/// until it drops (the detector's allow-list; see the module documentation).
#[must_use]
pub struct SideWriteAllowed {
    previous: Option<&'static str>,
    _not_send: PhantomData<*const ()>,
}

impl Drop for SideWriteAllowed {
    fn drop(&mut self) {
        SIDE_WRITE_ALLOWED.with(|allowed| allowed.set(self.previous));
    }
}

/// See [`SideWriteAllowed`].
pub fn allow_side_write(reason: &'static str) -> SideWriteAllowed {
    SideWriteAllowed {
        previous: SIDE_WRITE_ALLOWED.with(|allowed| allowed.replace(Some(reason))),
        _not_send: PhantomData,
    }
}

/// Writes the detector found outside a statement's batch.
pub fn side_writes() -> u64 {
    SIDE_WRITES.load(Ordering::Relaxed)
}

/// Whether a batch is open on this thread for `gate`.
pub fn batch_open_for(gate: &WriteGate) -> bool {
    OPEN_BATCHES.load(Ordering::Relaxed) != 0 && OPEN.with(Cell::get) == Some(gate_id(gate))
}

/// From `WriteGate::{write, put, delete}`: report the write if a batch is open on this
/// thread for `gate` and this is neither its commit nor allowed.
#[inline]
pub fn check_gate_write(gate: &WriteGate, caller: &'static Location<'static>) {
    if OPEN_BATCHES.load(Ordering::Relaxed) == 0 {
        return;
    }
    check_side_write(gate, "a write", caller);
}

/// From `WriteAheadLog::append*`: a logical-WAL entry appended while a batch is open
/// on this thread lands in another write group (its own, or the statement's
/// transaction's commit) instead of the batch's.
#[inline]
pub fn check_wal_append(gate: &WriteGate, caller: &'static Location<'static>) {
    if OPEN_BATCHES.load(Ordering::Relaxed) == 0 {
        return;
    }
    check_side_write(gate, "a logical-WAL entry appended", caller);
}

#[cold]
fn check_side_write(gate: &WriteGate, what: &str, caller: &'static Location<'static>) {
    if OPEN.with(Cell::get) != Some(gate_id(gate)) {
        return;
    }
    if COMMITTING.with(Cell::get) || SIDE_WRITE_ALLOWED.with(Cell::get).is_some() {
        return;
    }
    report_side_write(what, caller);
}

// The panic is the detector's point in a test build (debug_assertions or internal-tests),
// where a write that escapes its statement's batch must fail the test that did it; a
// release build counts and warns instead.
#[allow(clippy::panic)]
fn report_side_write(what: &str, caller: &'static Location<'static>) {
    SIDE_WRITES.fetch_add(1, Ordering::Relaxed);
    #[cfg(any(debug_assertions, feature = "internal-tests"))]
    {
        // A panic while unwinding aborts the process and the rest of the test binary
        // with it: the first panic fails the test, this one is counted and logged.
        if std::thread::panicking() {
            tracing::error!(
                "R5.3 DDL batch: {what} by {caller} outside its schema statement's write group, while this \
                 thread is unwinding"
            );
            return;
        }
        panic!(
            "R5.3 DDL batch: {what} by {caller} outside its schema statement's write group. Stage it in the \
             statement's DdlBatch, or, if it may be a group of its own, say why with ddl_batch::allow_side_write"
        );
    }
    #[cfg(not(any(debug_assertions, feature = "internal-tests")))]
    {
        if WARNED_SITES.lock().insert((caller.file(), caller.line())) {
            tracing::warn!(
                "{what} by {caller} was made outside its schema statement's write group (reported once per site)"
            );
        }
    }
}

/// `internal-tests` builds only: the next [`DdlBatch::commit`] on this thread fails
/// before it writes anything (spec R5.3,
/// `ddl_commit_failure_publishes_nothing`).
#[cfg(feature = "internal-tests")]
#[doc(hidden)]
pub fn debug_fail_next_commit() {
    FAIL_NEXT_COMMIT.with(|fail| fail.set(true));
}

/// Model a native write/durability error whose group reached storage, before
/// normal in-memory publication. Unlike `debug_fail_next_commit`, this is not
/// evidence that undoing eager state is safe.
#[cfg(feature = "internal-tests")]
#[doc(hidden)]
pub fn debug_fail_next_applied_commit() {
    FAIL_NEXT_APPLIED_COMMIT.with(|fail| fail.set(true));
}

#[cfg(feature = "internal-tests")]
thread_local! {
    static NEXT_BEFORE_NATIVE_COMMIT: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        std::cell::RefCell::new(None);
}

/// Arm a native fault at this DDL's owning write, after derived graph builds.
#[cfg(feature = "internal-tests")]
#[doc(hidden)]
pub fn debug_next_before_native_commit(hook: impl FnOnce() + 'static) {
    NEXT_BEFORE_NATIVE_COMMIT.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
}

/// Observe the actual native-OK owning group without injecting a failure.
#[cfg(feature = "internal-tests")]
#[doc(hidden)]
pub fn debug_next_after_native_commit(hook: impl FnOnce() + 'static) {
    AFTER_APPLIED_COMMIT.with(|next| *next.borrow_mut() = Some(Box::new(hook)));
}

/// As above, with a handshake at the real applied boundary for corruption and
/// publication-order controls. The hook never runs if the write is refused.
#[cfg(feature = "internal-tests")]
#[doc(hidden)]
pub fn debug_fail_next_applied_commit_with(hook: impl FnOnce() + 'static) {
    debug_fail_next_applied_commit();
    AFTER_APPLIED_COMMIT.with(|next| *next.borrow_mut() = Some(Box::new(hook)));
}

#[cfg(feature = "internal-tests")]
fn take_applied_hook() -> Option<Box<dyn FnOnce()>> {
    AFTER_APPLIED_COMMIT.with(|next| next.borrow_mut().take())
}

#[cfg(not(feature = "internal-tests"))]
fn take_applied_hook() -> Option<Box<dyn FnOnce()>> {
    None
}

#[cfg(feature = "internal-tests")]
fn take_applied_failure() -> bool {
    FAIL_NEXT_APPLIED_COMMIT.with(Cell::take)
}

#[cfg(not(feature = "internal-tests"))]
const fn take_applied_failure() -> bool {
    false
}

#[cfg(feature = "internal-tests")]
fn take_injected_failure() -> bool {
    FAIL_NEXT_COMMIT.with(Cell::take)
}

#[cfg(not(feature = "internal-tests"))]
const fn take_injected_failure() -> bool {
    false
}

/// What a batch runs after its write, or in place of it.
type Deferred<'e> = Box<dyn FnOnce() + 'e>;

/// Anything a batch keeps alive until it is written (a guard).
trait Held {}

impl<T> Held for T {}

/// One schema statement's durable writes, written as ONE write group by
/// [`Self::commit`]. See the module documentation.
#[must_use = "a DDL batch writes nothing until it is committed"]
pub struct DdlBatch<'e> {
    engine: &'e StorageEngine,
    batch: WriteBatch,
    operations: Vec<WalOperation>,
    on_commit: Vec<Deferred<'e>>,
    on_abort: Vec<Deferred<'e>>,
    /// Settle ALL owned write resources before any registry publication/undo.
    after_write: Vec<Box<dyn FnOnce(bool) + 'e>>,
    persistent_handles: Option<super::vector_index::PersistentRetirementHandles>,
    persistent_retirements: std::collections::HashSet<String>,
    /// A caller's strict durable witness, for a native error that may have applied.
    error_application: Option<Box<dyn FnOnce() -> Result<bool> + 'e>>,
    affected_tables: std::collections::BTreeSet<String>,
    caller_reconciles_unknown: bool,
    /// `internal-tests` crash points of spec R1 §4, fired right after the write.
    exits: Vec<(&'static str, String)>,
    /// Guards the statement's funnels took that must outlive the write (a
    /// history-purge stripe, the zone-stats lock), released last first right after it.
    held_for_write: Vec<Box<dyn Held + 'e>>,
    /// Guards that must outlive the publication too (a catalog write's table locks),
    /// released last first after it.
    held_for_publication: Vec<Box<dyn Held + 'e>>,
    marker: Option<OpenMarker>,
}

impl<'e> DdlBatch<'e> {
    /// Open a batch on `engine` for the statement running on this thread.
    pub fn new(engine: &'e StorageEngine) -> Self {
        Self {
            engine,
            batch: WriteBatch::default(),
            operations: Vec::new(),
            on_commit: Vec::new(),
            on_abort: Vec::new(),
            after_write: Vec::new(),
            persistent_handles: None,
            persistent_retirements: std::collections::HashSet::new(),
            error_application: None,
            affected_tables: std::collections::BTreeSet::new(),
            caller_reconciles_unknown: false,
            exits: Vec::new(),
            held_for_write: Vec::new(),
            held_for_publication: Vec::new(),
            marker: Some(OpenMarker::enter(engine.write_gate())),
        }
    }

    /// The engine the batch writes to.
    pub fn engine(&self) -> &'e StorageEngine {
        self.engine
    }

    /// Stage a put of `value`, sealed as `StorageEngine::put` seals it.
    pub fn put(&mut self, key: impl AsRef<[u8]>, value: &[u8]) -> Result<()> {
        self.note_layout_key(key.as_ref());
        let stored = super::tde::seal(self.engine.key_manager_ref(), value)?;
        self.batch.put(key, stored.as_ref());
        Ok(())
    }

    /// Stage a put of a value already in its stored form (read raw, or sealed by the
    /// caller for this very key).
    pub fn put_stored(&mut self, key: impl AsRef<[u8]>, stored: &[u8]) {
        self.note_layout_key(key.as_ref());
        self.batch.put(key, stored);
    }

    /// Stage a delete.
    pub fn delete(&mut self, key: impl AsRef<[u8]>) {
        self.note_layout_key(key.as_ref());
        self.batch.delete(key);
    }

    pub(crate) fn affect_table(&mut self, table: &str) {
        self.affected_tables.insert(table.to_string());
    }

    fn note_layout_key(&mut self, key: &[u8]) {
        if let Some(super::layout_epochs::LayoutKey::Tables(table, other)) = super::layout_epochs::layout_key(key) {
            self.affect_table(table);
            if let Some(other) = other {
                self.affect_table(other);
            }
        }
    }

    /// The batch itself, for the funnels that stage into a `WriteBatch`.
    pub fn batch_mut(&mut self) -> &mut WriteBatch {
        &mut self.batch
    }

    /// Log `operation` with the change: sealed into the batch at commit, under LSNs
    /// reserved then (nothing during the logical WAL's replay, or with no logical WAL).
    pub fn log(&mut self, operation: WalOperation) {
        match &operation {
            WalOperation::Insert { table, .. }
            | WalOperation::Update { table, .. }
            | WalOperation::Delete { table, .. }
            | WalOperation::Truncate { table }
            | WalOperation::CreateTable { table, .. }
            | WalOperation::DropTable { table }
            | WalOperation::AlterColumnStorage { table, .. }
            | WalOperation::CreateIndex { table, .. }
            | WalOperation::CreateTrigger { table, .. }
            | WalOperation::AddConstraint { table, .. }
            | WalOperation::DropConstraint { table, .. }
            | WalOperation::AlterTableSchema { table, .. }
            | WalOperation::SetColumnMissing { table, .. } => self.affect_table(table),
            WalOperation::RenameTable { old_table, new_table } => {
                self.affect_table(old_table);
                self.affect_table(new_table);
            }
            WalOperation::CreateMaterializedView { name, .. }
            | WalOperation::DropMaterializedView { name }
            | WalOperation::RefreshMaterializedView { name, .. } => {
                self.affect_table(name);
                self.affect_table(&super::materialized_view::MaterializedViewCatalog::mv_data_table_name(
                    name,
                ));
            }
            WalOperation::DropTrigger { table: Some(table), .. } => self.affect_table(table),
            _ => {}
        }
        self.operations.push(operation);
    }

    /// Run `publish` in order after success or strictly proven application.
    pub fn on_commit(&mut self, publish: impl FnOnce() + 'e) {
        self.on_commit.push(Box::new(publish));
    }

    /// Run `undo` only for a proven unwritten group, or an unwritten drop.
    /// A native error alone cannot justify reversing eager derived state.
    pub fn on_abort(&mut self, undo: impl FnOnce() + 'e) {
        self.on_abort.push(Box::new(undo));
    }

    /// Acquire topology planning before dictionary/zone-stats guards when a
    /// later stage will purge persistent indexes. Does not mark any purge staged.
    pub(crate) fn prepare_persistent_retirements(&mut self) {
        let engine = self.engine;
        self.persistent_handles
            .get_or_insert_with(|| engine.vector_indexes().snapshot_persistent_retirement_handles());
    }

    pub(crate) fn begin_persistent_retirement(&mut self, name: &str) -> bool {
        // Capture registry state before acquiring the first lease, then reuse
        // only these owned handles throughout a multi-table/cascading batch.
        self.prepare_persistent_retirements();
        self.persistent_retirements.insert(name.to_string())
    }

    #[cfg(feature = "vector-persist")]
    pub(crate) fn persistent_retirement_handles(&self) -> Result<&super::vector_index::PersistentRetirementHandles> {
        self.persistent_handles
            .as_ref()
            .ok_or_else(|| Error::internal("persistent retirement was not prepared"))
    }

    /// Release/settle an owned write resource before ANY normal publication
    /// or abort callback. `true` means the write reported success; `false`
    /// includes uncertain native errors and an unwritten drop. Used by leased
    /// persistent graph purges: no callback may take a registry while another
    /// table in this batch still holds a graph write lease.
    pub(crate) fn after_write_before_publication(&mut self, settle: impl FnOnce(bool) + 'e) {
        self.after_write.push(Box::new(settle));
    }

    /// On a reported write error, determine whether this atomic group applied.
    /// A true result requires a unique same-group stamp or an AFTER value whose
    /// BEFORE value differed, observed under the caller's retained exclusive lock.
    /// False requires the exact BEFORE value. An unreadable/unexpected state must
    /// fence and retire unusable derived state before returning Err. Such an Err
    /// runs neither publication nor blind abort undo. The original write error is
    /// still returned after a proven application and its normal publication.
    pub(crate) fn on_write_error(&mut self, witness: impl FnOnce() -> Result<bool> + 'e) {
        assert!(
            self.error_application.is_none(),
            "one application witness per DDL group"
        );
        self.error_application = Some(Box::new(witness));
    }

    /// A changed catalog record is a same-group outcome witness under the
    /// statement's retained exclusive table lock. Values are plaintext, as
    /// returned by StorageEngine::get; no row payload is copied.
    pub(crate) fn watch_catalog_change(
        &mut self,
        table: &str,
        key: Vec<u8>,
        before: Option<Vec<u8>>,
        after: Option<Vec<u8>>,
    ) {
        self.affect_table(table);
        let table = table.to_string();
        let engine = self.engine;
        self.on_write_error(move || {
            let observed = engine.get(&key);
            match observed {
                Ok(value) if before != after && value == after => Ok(true),
                Ok(value) if before != after && value == before => Ok(false),
                outcome => {
                    let reason = match outcome {
                        Ok(_) => "DDL catalog witness is neither a distinct BEFORE nor AFTER value".to_string(),
                        Err(error) => format!("Cannot read DDL catalog witness: {error}"),
                    };
                    Err(engine.quarantine_generic_indexes(&table, &reason))
                }
            }
        });
    }

    /// MV population retains its claim/census and reconciles actual durable
    /// rows immediately after commit returns Err. It must not be fenced before
    /// that owner can prove that an uncertain group left the old image intact.
    pub(crate) fn reconcile_unknown_in_caller(&mut self) {
        self.caller_reconciles_unknown = true;
    }

    /// `internal-tests`: end the process right after the write, before anything is
    /// published (spec R1 §4's `exit_after_ddl_write`, `<point>:<table>`).
    pub fn exit_after_write(&mut self, point: &'static str, table: &str) {
        self.exits.push((point, table.to_string()));
    }

    /// Keep `guard` until the batch is written (or abandoned), and release it then,
    /// before the publication: what only orders the write against other writers (a
    /// history-purge stripe; the process-global zone-stats lock, decision 9).
    /// Released in the reverse of the order handed over.
    pub fn hold_until_written<T: 'e>(&mut self, guard: T) {
        self.held_for_write.push(Box::new(guard));
    }

    /// Keep `guard` until the batch is written AND published (or abandoned and
    /// undone): a catalog write's table locks, which keep writers out until the
    /// caches say what the store says. Released in the reverse of the order handed
    /// over, as the thread-local scopes some of them restore require.
    pub fn hold_until_published<T: 'e>(&mut self, guard: T) {
        self.held_for_publication.push(Box::new(guard));
    }

    fn release_for_write(&mut self) {
        while let Some(guard) = self.held_for_write.pop() {
            drop(guard);
        }
    }

    fn release_held(&mut self) {
        self.release_for_write();
        while let Some(guard) = self.held_for_publication.pop() {
            drop(guard);
        }
    }

    /// Nothing staged, nothing logged.
    pub fn is_empty(&self) -> bool {
        self.batch.is_empty() && self.operations.is_empty()
    }

    /// The bytes staged so far (the WAL entries are added at commit).
    pub fn size_in_bytes(&self) -> usize {
        self.batch.size_in_bytes()
    }

    /// A lower-bound preflight before a persistent graph's separate derived
    /// builds. The final gate still caps the complete sealed/WAL/B4 group.
    pub(crate) fn check_staged_budget(&self) -> Result<()> {
        self.engine.write_gate().refuse_oversized(&self.batch)
    }

    /// Write the group, settle resources, then publish or undo. An optional
    /// strict application witness handles a possibly-applied native error before
    /// choosing callbacks; the original write error is always retained.
    pub(crate) fn commit(mut self) -> Result<DdlWrite> {
        let batch = std::mem::take(&mut self.batch);
        let operations = std::mem::take(&mut self.operations);
        let on_commit = std::mem::take(&mut self.on_commit);
        let on_abort = std::mem::take(&mut self.on_abort);
        let after_write = std::mem::take(&mut self.after_write);
        let error_application = self.error_application.take();
        let exits = std::mem::take(&mut self.exits);
        // Spec R5.6: the layout change the write makes ends after the publication
        // below, not at the write: until then a reader's epoch is in flight.
        let publication = super::layout_epochs::PublicationScope::enter();
        let attempt = WriteAttempt::default();
        let written = if take_injected_failure() {
            Err(Error::storage(
                "the schema statement's write group was not written (internal-tests: debug_fail_next_commit)",
            ))
        } else if batch.is_empty() && (operations.is_empty() || !self.engine.logs_ddl_operations()) {
            Ok(DdlWrite::nothing())
        } else {
            let _committing = Committing::enter();
            let applied_failure = take_applied_failure();
            let applied_hook = take_applied_hook();
            #[cfg(feature = "internal-tests")]
            if let Some(hook) = NEXT_BEFORE_NATIVE_COMMIT.with(|slot| slot.borrow_mut().take()) {
                hook();
            }
            self.engine
                .write_ddl_batch_ops_observed(operations, batch, &attempt)
                .and_then(|written| {
                    if let Some(hook) = applied_hook {
                        hook();
                    }
                    if applied_failure {
                        Err(Error::storage(
                            "injected schema durability failure after the group applied",
                        ))
                    } else {
                        Ok(written)
                    }
                })
        };
        // Preserve the strongest crash boundary: the native group succeeded,
        // but no guard/resource or in-memory publication has been settled yet.
        if written.is_ok() {
            for (point, table) in &exits {
                super::engine::exit_after_ddl_write(point, table);
            }
        }
        // The statement's durable write is made (or failed): what runs from here is
        // no longer inside it.
        drop(self.marker.take());
        self.release_for_write();
        for settle in after_write {
            settle(written.is_ok());
        }
        // Release the lifecycle planning lease only after every graph lease,
        // and before any existing callback takes a manager registry.
        self.persistent_handles = None;
        let outcome = match written {
            Ok(written) => {
                for publish in on_commit {
                    publish();
                }
                Ok(written)
            }
            Err(error) => {
                let applied = match attempt.outcome() {
                    WriteOutcome::NotWritten => Ok(false),
                    outcome => match error_application {
                        Some(witness) => witness(),
                        None if outcome == WriteOutcome::Applied => Ok(true),
                        None => {
                            if !self.caller_reconciles_unknown {
                                // Fence EVERY member before retirement of
                                // the first can wait on a backend lease.
                                for table in &self.affected_tables {
                                    self.engine.layout_epochs().quarantine(table);
                                    self.engine.write_gate().quarantine_table(
                                        table,
                                        "DDL native write outcome is unknown; reopen to reconcile",
                                    );
                                    self.engine.data_write_attempt_publication(table.clone())();
                                }
                                for table in &self.affected_tables {
                                    self.engine.quarantine_generic_indexes(
                                        table,
                                        "DDL native write outcome is unknown; reopen to reconcile",
                                    );
                                }
                            }
                            Err(Error::storage(
                                "DDL native write outcome requires actual-state reconciliation",
                            ))
                        }
                    },
                };
                match applied {
                    Ok(true) => {
                        for publish in on_commit {
                            publish();
                        }
                    }
                    Ok(false) => {
                        for undo in on_abort.into_iter().rev() {
                            undo();
                        }
                    }
                    Err(unresolved) => {
                        tracing::error!("DDL write outcome could not be reconciled: {unresolved}");
                    }
                }
                Err(error)
            }
        };
        drop(publication);
        self.release_held();
        outcome
    }
}

impl Drop for DdlBatch<'_> {
    fn drop(&mut self) {
        // After a commit every list is empty: this runs for a batch dropped unwritten.
        self.release_for_write();
        for settle in std::mem::take(&mut self.after_write) {
            settle(false);
        }
        self.persistent_handles = None;
        self.on_commit.clear();
        drop(self.marker.take());
        for undo in std::mem::take(&mut self.on_abort).into_iter().rev() {
            undo();
        }
        self.release_held();
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn scratch_db() -> (tempfile::TempDir, rocksdb::DB) {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut opts = rocksdb::Options::default();
        opts.create_if_missing(true);
        let db = rocksdb::DB::open(&opts, dir.path()).expect("open");
        (dir, db)
    }

    #[test]
    fn resources_settle_before_publication_or_undo_on_success_refusal_and_unwritten_drop() {
        use std::cell::RefCell;
        use std::rc::Rc;
        struct HeldEvent(Rc<RefCell<Vec<&'static str>>>, &'static str);
        impl Drop for HeldEvent {
            fn drop(&mut self) {
                self.0.borrow_mut().push(self.1);
            }
        }
        for mode in ["success", "refused", "unwritten"] {
            let db = crate::EmbeddedDatabase::new_in_memory().unwrap();
            let events = Rc::new(RefCell::new(Vec::new()));
            let mut ddl = DdlBatch::new(&db.storage);
            ddl.put_stored(b"idxsnapv:resource-phase", &[1; 128]);
            ddl.hold_until_written(HeldEvent(Rc::clone(&events), "write-guard"));
            ddl.hold_until_published(HeldEvent(Rc::clone(&events), "publication-guard"));
            for label in ["resource-one", "resource-two"] {
                let events = Rc::clone(&events);
                ddl.after_write_before_publication(move |success| {
                    assert_eq!(success, mode == "success");
                    events.borrow_mut().push(label);
                });
            }
            let committed = Rc::clone(&events);
            ddl.on_commit(move || committed.borrow_mut().push("commit"));
            let aborted = Rc::clone(&events);
            ddl.on_abort(move || aborted.borrow_mut().push("abort"));
            let before = db.storage.db().latest_sequence_number();
            match mode {
                "success" => {
                    ddl.commit().unwrap().wait_replicated();
                }
                "refused" => {
                    db.storage.debug_set_write_group_ceiling(Some(32));
                    assert!(ddl.commit().is_err());
                    db.storage.debug_set_write_group_ceiling(None);
                }
                "unwritten" => drop(ddl),
                _ => unreachable!(),
            }
            assert_eq!(
                &*events.borrow(),
                &[
                    "write-guard",
                    "resource-one",
                    "resource-two",
                    if mode == "success" { "commit" } else { "abort" },
                    "publication-guard"
                ]
            );
            assert_eq!(
                db.storage.db().get(b"idxsnapv:resource-phase").unwrap().is_some(),
                mode == "success"
            );
            if mode != "success" {
                assert_eq!(db.storage.db().latest_sequence_number(), before);
            }
        }
    }

    /// The detector fires on a write of the marked gate that is not the batch's own
    /// commit, and not on the commit, an allowed write, another gate's write, or a
    /// write once the batch is gone.
    #[cfg(any(debug_assertions, feature = "internal-tests"))]
    #[test]
    fn a_write_outside_the_open_batch_is_a_finding() {
        let (_dir, db) = scratch_db();
        let gate = WriteGate::open();
        let other = WriteGate::open();
        let before = side_writes();
        {
            let _open = OpenMarker::enter(&gate);
            assert!(batch_open_for(&gate));
            assert!(!batch_open_for(&other));

            let escaped = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                gate.put(&db, b"meta:table:t", b"v", None).unwrap();
            }));
            assert!(escaped.is_err(), "a put outside the batch panics in a test build");
            assert!(side_writes() > before, "and is counted");

            let counted = side_writes();
            {
                let _commit = Committing::enter();
                let mut batch = WriteBatch::default();
                batch.put(b"meta:table:t", b"v");
                gate.write(&db, batch, None).unwrap();
            }
            {
                let _allowed = allow_side_write("a test's derived write");
                gate.delete(&db, b"idxsnapv:t", None).unwrap();
            }
            other.put(&db, b"meta:table:u", b"v", None).unwrap();
            assert_eq!(
                side_writes(),
                counted,
                "the commit, an allowed write and another gate pass"
            );
        }
        assert!(!batch_open_for(&gate));
        let counted = side_writes();
        gate.put(&db, b"meta:table:t", b"w", None).unwrap();
        assert_eq!(side_writes(), counted, "nothing is open any more");
    }

    /// A finding while unwinding is counted, never a second panic.
    #[cfg(any(debug_assertions, feature = "internal-tests"))]
    #[test]
    fn a_finding_while_unwinding_does_not_abort() {
        struct WritesOnTheWayOut<'a>(&'a WriteGate, &'a rocksdb::DB);
        impl Drop for WritesOnTheWayOut<'_> {
            fn drop(&mut self) {
                let _ = self.0.put(self.1, b"meta:table:t", b"v", None);
            }
        }
        let (_dir, db) = scratch_db();
        let gate = WriteGate::open();
        let before = side_writes();
        let payload = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _open = OpenMarker::enter(&gate);
            let _on_the_way_out = WritesOnTheWayOut(&gate, &db);
            panic!("the first finding");
        }))
        .expect_err("it unwound");
        assert_eq!(payload.downcast_ref::<&str>().copied(), Some("the first finding"));
        assert!(side_writes() > before, "the write on the way out is counted");
    }
}
