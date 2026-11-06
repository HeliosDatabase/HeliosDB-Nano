//! Branch storage implementation
//!
//! Implements copy-on-write branch storage for database branching.
//! Enables instant branch creation with minimal storage overhead.

#![allow(unused_variables)]

use super::branch_freeze::BranchFreeze;
use super::write_gate::WriteGate;
use super::{Key, Transaction};
use crate::{Error, Result};
use parking_lot::RwLock;
use rocksdb::DB;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock};

/// Snapshot identifier (timestamp-based versioning)
pub type SnapshotId = u64;

/// Branch identifier (globally unique)
pub type BranchId = u64;

/// sprinter 6bc4a4040eb7 (B1): how long a DROP or MERGE BRANCH waits for the
/// statements still running on the branch to finish before it gives up with
/// SQLSTATE 55006. Bounded so that two sessions dropping each other's branch
/// cannot wait on each other forever.
pub const BRANCH_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// sprinter 6bc4a4040eb7 (B1): the per-branch DRAIN.
///
/// A statement bound to branch `b` holds a SHARED hold on `b` for its whole
/// duration ([`BranchDrains::enter`]); DROP and MERGE of `b` take it
/// EXCLUSIVELY ([`BranchDrains::close`]): they wait for the in-flight
/// statements to finish, take `b` out of `Active`, and only then let new
/// statements in — which re-validate their binding and are refused. So once a
/// DROP returns, nothing bound to the dropped branch writes again. Checking
/// the binding only at statement entry could not give that: a statement that
/// had already passed the check kept writing after the drop.
///
/// Transactions do NOT hold it across statements (a long-open transaction must
/// not block a DROP); a transaction's COMMIT re-validates instead.
///
/// One instance per engine, shared with the [`BranchManager`] (whose
/// `drop_branch` / `merge_branch` close it) the same way the registry
/// generation is.
#[derive(Default)]
pub struct BranchDrains {
    branches: dashmap::DashMap<BranchId, Arc<BranchDrain>>,
}

#[derive(Default)]
struct BranchDrain {
    state: parking_lot::Mutex<DrainState>,
    changed: parking_lot::Condvar,
}

#[derive(Default)]
struct DrainState {
    /// Statements running on the branch right now.
    in_flight: usize,
    /// A DROP or MERGE is taking the branch out of `Active`.
    closing: bool,
    /// Further DROPs / MERGEs waiting for the current close to finish.
    waiting_closers: usize,
    /// sprinter 6bc4a4040eb7 (B3 fix pass): a schema statement runs on the
    /// branch alone ([`BranchDrains::exclusive`]). New statements wait for it.
    exclusive: bool,
    /// Statements waiting for that section to end.
    waiting_statements: usize,
}

/// A point-in-time view of one branch's drain — for diagnostics and the
/// deterministic concurrency tests (`StorageEngine::branch_drain_state`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BranchDrainState {
    /// Statements running on the branch.
    pub in_flight: usize,
    /// A DROP or MERGE is closing it.
    pub closing: bool,
    /// DROPs / MERGEs queued behind that close.
    pub waiting_closers: usize,
    /// A schema statement or a merge runs on it alone, or is waiting to.
    pub exclusive: bool,
    /// Statements waiting for that section to end.
    pub waiting_statements: usize,
}

thread_local! {
    /// The drains THIS thread holds, as `(drains instance, branch id)`. A
    /// statement that re-enters the engine on the same thread (a UDF body, a
    /// trigger, a `_for_session` entry delegating to the session-less funnel)
    /// is covered by the outer statement's hold and must not take a second one
    /// — a closer waiting for the first would then wait for itself.
    static HELD_BRANCHES: std::cell::RefCell<Vec<(usize, BranchId)>> = const { std::cell::RefCell::new(Vec::new()) };
}

impl BranchDrains {
    fn drain(&self, branch_id: BranchId) -> Arc<BranchDrain> {
        Arc::clone(&self.branches.entry(branch_id).or_default())
    }

    fn instance_key(self: &Arc<Self>) -> usize {
        Arc::as_ptr(self) as usize
    }

    /// Does the calling thread already hold `branch_id`?
    pub fn held_by_this_thread(self: &Arc<Self>, branch_id: BranchId) -> bool {
        let key = self.instance_key();
        HELD_BRANCHES.with(|held| held.borrow().contains(&(key, branch_id)))
    }

    /// A statement bound to `branch_id` (named `name`) starts: take a shared
    /// hold for its duration. Free — an inert hold — when this thread already
    /// holds the branch.
    ///
    /// Refused IMMEDIATELY (SQLSTATE 55006) while a DROP or MERGE is closing
    /// the branch (B1 second fix pass): once that close succeeds the branch is
    /// no longer active and every waiter would be refused anyway, so waiting
    /// only pinned the caller's thread for the length of a merge copy.
    ///
    /// WAITS — at most [`BRANCH_DRAIN_TIMEOUT`], then 55006 — while a schema
    /// statement runs on the branch alone ([`Self::exclusive`], sprinter
    /// 6bc4a4040eb7, B3 fix pass): it is short, and the statement that waited
    /// then sees the new definition.
    pub fn enter(self: &Arc<Self>, branch_id: BranchId, name: &str) -> Result<BranchHold> {
        if self.held_by_this_thread(branch_id) {
            return Ok(BranchHold::none());
        }
        let drain = self.drain(branch_id);
        {
            let mut state = drain.state.lock();
            if state.exclusive && !state.closing {
                let deadline = std::time::Instant::now() + BRANCH_DRAIN_TIMEOUT;
                state.waiting_statements += 1;
                while state.exclusive && !state.closing {
                    if drain.changed.wait_until(&mut state, deadline).timed_out() {
                        state.waiting_statements -= 1;
                        return Err(Error::branch_statement_waited(name));
                    }
                }
                state.waiting_statements -= 1;
            }
            if state.closing {
                return Err(Error::branch_closing(name));
            }
            state.in_flight += 1;
        }
        let key = self.instance_key();
        HELD_BRANCHES.with(|held| held.borrow_mut().push((key, branch_id)));
        Ok(BranchHold {
            held: Some((key, branch_id, drain)),
            _not_send: std::marker::PhantomData,
        })
    }

    /// The drain of `branch_id` right now (see [`BranchDrainState`]).
    pub fn state(&self, branch_id: BranchId) -> BranchDrainState {
        let Some(drain) = self.branches.get(&branch_id).map(|entry| Arc::clone(entry.value())) else {
            return BranchDrainState::default();
        };
        let state = drain.state.lock();
        BranchDrainState {
            in_flight: state.in_flight,
            closing: state.closing,
            waiting_closers: state.waiting_closers,
            exclusive: state.exclusive,
            waiting_statements: state.waiting_statements,
        }
    }

    /// A DROP or MERGE of `branch_id` (named `name`, for the refusal) is about
    /// to take it out of `Active`: wait — at most `timeout` — for the
    /// statements running on it to finish, and keep new ones out until the
    /// returned guard drops. Refused with SQLSTATE 55006 when the calling
    /// thread's own statement runs on the branch, or when the wait times out.
    pub fn close(
        self: &Arc<Self>,
        branch_id: BranchId,
        name: &str,
        timeout: std::time::Duration,
    ) -> Result<BranchCloseGuard> {
        if self.held_by_this_thread(branch_id) {
            return Err(Error::branch_in_use(name));
        }
        let drain = self.drain(branch_id);
        let deadline = std::time::Instant::now() + timeout;
        let mut state = drain.state.lock();
        if state.closing {
            // Another DROP / MERGE is closing it: queue behind it. The caller
            // RE-READS the branch once this returns — the close it waited for
            // has usually taken the branch out of `Active` already.
            state.waiting_closers += 1;
            while state.closing {
                if drain.changed.wait_until(&mut state, deadline).timed_out() {
                    state.waiting_closers -= 1;
                    return Err(Error::branch_busy(name));
                }
            }
            state.waiting_closers -= 1;
        }
        state.closing = true;
        // A schema statement waiting for its exclusive section gives up now
        // (B3 fix pass): it holds the branch, so this close would wait for it.
        drain.changed.notify_all();
        while state.in_flight > 0 {
            if drain.changed.wait_until(&mut state, deadline).timed_out() {
                state.closing = false;
                drain.changed.notify_all();
                return Err(Error::branch_busy(name));
            }
        }
        drop(state);
        Ok(BranchCloseGuard { drain })
    }

    /// sprinter 6bc4a4040eb7 (B3 fix pass): a schema statement on `branch_id`
    /// (named `name`, for the refusal) runs on the branch ALONE: wait — at most
    /// `timeout` — for the OTHER statements running on it to finish, and hold
    /// new ones back until the guard drops. Otherwise another session's write
    /// racing the statement is lost (its row re-put from a scan that read
    /// before it), or lands under the definition it was not validated against.
    ///
    /// Refused at once (55006) while a DROP or MERGE is closing the branch, and
    /// while another schema statement holds the section: two of them must not
    /// wait for each other's shared hold. The calling thread's own statement
    /// hold is not "another statement", and its nested statements (a `CREATE
    /// TABLE AS`'s population) re-enter through `held_by_this_thread`.
    pub fn exclusive(
        self: &Arc<Self>,
        branch_id: BranchId,
        name: &str,
        timeout: std::time::Duration,
    ) -> Result<BranchExclusiveGuard> {
        // The guard's own shared hold when the caller's statement has none:
        // the nested statements then find the branch held by this thread and
        // do not wait for this section.
        let hold = self.enter(branch_id, name)?;
        let drain = self.drain(branch_id);
        let deadline = std::time::Instant::now() + timeout;
        let mut state = drain.state.lock();
        if state.closing {
            return Err(Error::branch_closing(name));
        }
        if state.exclusive {
            return Err(Error::branch_schema_busy(name));
        }
        state.exclusive = true;
        while state.in_flight > 1 {
            let timed_out = drain.changed.wait_until(&mut state, deadline).timed_out();
            if state.closing || timed_out {
                let closing = state.closing;
                state.exclusive = false;
                drain.changed.notify_all();
                return Err(if closing {
                    Error::branch_closing(name)
                } else {
                    Error::branch_schema_busy(name)
                });
            }
        }
        drop(state);
        Ok(BranchExclusiveGuard { drain, _hold: hold })
    }
}

/// A schema statement's exclusive section on a branch (see
/// [`BranchDrains::exclusive`]). Dropping it lets the waiting statements in.
pub struct BranchExclusiveGuard {
    drain: Arc<BranchDrain>,
    _hold: BranchHold,
}

impl Drop for BranchExclusiveGuard {
    fn drop(&mut self) {
        let mut state = self.drain.state.lock();
        state.exclusive = false;
        self.drain.changed.notify_all();
    }
}

/// A statement's shared hold on its branch (see [`BranchDrains`]). Released on
/// `Drop`. Not `Send` (the `PhantomData`): it is recorded in the taking
/// thread's `HELD_BRANCHES`, so it must be released on that thread.
pub struct BranchHold {
    held: Option<(usize, BranchId, Arc<BranchDrain>)>,
    _not_send: std::marker::PhantomData<*const ()>,
}

impl BranchHold {
    /// A hold on nothing — main, or a statement already covered by its outer
    /// statement's hold.
    pub fn none() -> Self {
        Self {
            held: None,
            _not_send: std::marker::PhantomData,
        }
    }
}

impl Drop for BranchHold {
    fn drop(&mut self) {
        let Some((key, branch_id, drain)) = self.held.take() else {
            return;
        };
        {
            let mut state = drain.state.lock();
            state.in_flight = state.in_flight.saturating_sub(1);
            // A closer waits for 0, a schema statement's exclusive section for
            // its own hold alone (B3 fix pass).
            if state.in_flight <= 1 {
                drain.changed.notify_all();
            }
        }
        HELD_BRANCHES.with(|held| {
            let mut held = held.borrow_mut();
            if let Some(position) = held.iter().rposition(|entry| *entry == (key, branch_id)) {
                held.remove(position);
            }
        });
    }
}

/// A DROP's or MERGE's exclusive hold on a branch (see [`BranchDrains::close`]).
/// Dropping it lets new statements in again; by then the branch is no longer
/// active, so they are refused.
pub struct BranchCloseGuard {
    drain: Arc<BranchDrain>,
}

impl Drop for BranchCloseGuard {
    fn drop(&mut self) {
        let mut state = self.drain.state.lock();
        state.closing = false;
        self.drain.changed.notify_all();
    }
}

/// Key prefixes for branch metadata
const BRANCH_META_PREFIX: &[u8] = b"branch:meta:";
const BRANCH_REGISTRY_KEY: &[u8] = b"branch:registry";
const BRANCH_CHILDREN_PREFIX: &[u8] = b"branch:children:";
const BRANCH_DATA_PREFIX: &[u8] = b"bdata:";

/// Key prefixes for Git integration
pub const GIT_CONFIG_KEY: &[u8] = b"git:config";
pub const GIT_LINK_PREFIX: &[u8] = b"git:link:";
pub const GIT_COMMIT_PREFIX: &[u8] = b"git:commit:";
pub const GIT_DDL_HISTORY_PREFIX: &[u8] = b"git:ddl:";
pub const GIT_SCHEMA_SNAPSHOT_PREFIX: &[u8] = b"git:schema_snapshot:";
pub const GIT_PR_PREFIX: &[u8] = b"git:pr:";

/// Branch state
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BranchState {
    /// Branch is active
    Active,

    /// Branch has been merged
    Merged {
        /// Target branch ID
        into_branch: BranchId,
        /// Merge timestamp
        at_timestamp: u64,
    },

    /// Branch has been dropped
    Dropped {
        /// Drop timestamp
        at_timestamp: u64,
    },
}

/// Merge strategy for resolving conflicts
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MergeStrategy {
    /// Automatically resolve conflicts (prefer source)
    Auto,

    /// Fail on any conflict
    Manual,

    /// Always prefer source branch changes
    Theirs,

    /// Always prefer target branch changes
    Ours,
}

impl Default for MergeStrategy {
    fn default() -> Self {
        Self::Auto
    }
}

/// Conflict information for a key
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergeConflict {
    /// Key that has conflict
    pub key: String,

    /// Value in merge base (common ancestor)
    pub base_value: Option<Vec<u8>>,

    /// Value in source branch
    pub source_value: Option<Vec<u8>>,

    /// Value in target branch
    pub target_value: Option<Vec<u8>>,

    /// Timestamp in source
    pub source_timestamp: u64,

    /// Timestamp in target
    pub target_timestamp: u64,
}

/// Result of merge operation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergeResult {
    /// Merge commit timestamp
    pub merge_timestamp: u64,

    /// Number of keys merged
    pub merged_keys: usize,

    /// Conflicts detected
    pub conflicts: Vec<MergeConflict>,

    /// Whether merge was completed
    pub completed: bool,
}

/// Git integration metadata for linking database branches to Git branches
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitLinkMetadata {
    /// Git branch name (e.g., "feature/user-auth")
    pub git_branch: String,

    /// Last synced Git commit SHA
    pub last_commit: Option<String>,

    /// Auto-sync enabled (switch DB branch on git checkout)
    pub auto_sync: bool,

    /// Git provider (e.g., "github", "gitlab", "generic")
    pub provider: Option<String>,

    /// Associated PR/MR number (for preview branches)
    pub pr_number: Option<u64>,

    /// Repository path (for local detection)
    pub repo_path: Option<String>,

    /// Link creation timestamp
    pub linked_at: u64,
}

impl Default for GitLinkMetadata {
    fn default() -> Self {
        Self {
            git_branch: String::new(),
            last_commit: None,
            auto_sync: true,
            provider: None,
            pr_number: None,
            repo_path: None,
            linked_at: 0,
        }
    }
}

/// Branch creation/merge options
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BranchOptions {
    /// Replication factor (for distributed mode)
    pub replication_factor: Option<usize>,

    /// Region hint (for distributed mode)
    pub region: Option<String>,

    /// Custom metadata
    pub metadata: HashMap<String, String>,

    /// Git integration link (links this DB branch to a Git branch)
    pub git_link: Option<GitLinkMetadata>,
}

/// Branch statistics
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BranchStats {
    /// Number of modified keys in this branch
    pub modified_keys: u64,

    /// Approximate storage size (bytes)
    pub storage_bytes: u64,

    /// Number of commits in this branch
    pub commit_count: u64,

    /// Last activity timestamp
    pub last_modified: u64,
}

/// Branch metadata
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BranchMetadata {
    /// Branch name (user-visible)
    pub name: String,

    /// Unique branch ID
    pub branch_id: BranchId,

    /// Parent branch ID (None for root)
    pub parent_id: Option<BranchId>,

    /// Creation timestamp
    pub created_at: u64,

    /// Snapshot at branch point
    pub created_from_snapshot: SnapshotId,

    /// Current state
    pub state: BranchState,

    /// Last merge base (for three-way merge)
    pub merge_base: Option<SnapshotId>,

    /// Additional metadata
    pub options: BranchOptions,

    /// Statistics
    pub stats: BranchStats,
}

/// Branch registry
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BranchRegistry {
    /// Map of branch ID -> branch name
    pub branches: HashMap<BranchId, String>,

    /// Main branch ID
    pub main_branch: BranchId,

    /// Next available branch ID
    pub next_branch_id: u64,
}

impl BranchRegistry {
    /// Create a new registry with the main branch
    pub fn new() -> Self {
        let mut branches = HashMap::new();
        branches.insert(1, "main".to_string());

        Self {
            branches,
            main_branch: 1,
            next_branch_id: 2,
        }
    }

    /// Get next branch ID (auto-increment)
    pub fn next_id(&mut self) -> BranchId {
        let id = self.next_branch_id;
        self.next_branch_id += 1;
        id
    }

    /// Add a branch
    pub fn add_branch(&mut self, branch_id: BranchId, name: String) {
        self.branches.insert(branch_id, name);
    }

    /// Remove a branch
    pub fn remove_branch(&mut self, branch_id: BranchId) {
        self.branches.remove(&branch_id);
    }

    /// Check if branch exists
    pub fn contains(&self, branch_id: BranchId) -> bool {
        self.branches.contains_key(&branch_id)
    }

    /// Get branch name
    pub fn get_name(&self, branch_id: BranchId) -> Option<&str> {
        self.branches.get(&branch_id).map(|s| s.as_str())
    }
}

impl Default for BranchRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Branch manager - coordinates all branch operations
pub struct BranchManager {
    /// Storage engine reference. Written only through `gate`.
    db: Arc<DB>,

    /// The write gate of the engine that owns `db`.
    gate: Arc<WriteGate>,

    /// Branch registry (cached)
    registry: Arc<RwLock<BranchRegistry>>,

    /// Branch metadata cache
    metadata_cache: Arc<RwLock<HashMap<BranchId, BranchMetadata>>>,

    /// Current timestamp generator
    timestamp: Arc<RwLock<u64>>,

    /// The storage boundary's key manager, or `None` when encryption is
    /// disabled. Set by the engine right after construction; see
    /// [`BranchManager::set_key_manager`].
    key_manager: Option<Arc<crate::crypto::KeyManager>>,

    /// sprinter 6bc4a4040eb7 (phase B1): the branch-registry generation the
    /// engine's session bindings validate against, bumped by every transition
    /// that takes a branch out of `Active` (drop, merge). Shared with the
    /// engine through [`BranchManager::set_registry_generation`]; a manager
    /// built on its own (unit tests) keeps a private counter.
    registry_generation: Arc<std::sync::atomic::AtomicU64>,

    /// sprinter 6bc4a4040eb7 (phase B1): the per-branch drains DROP and MERGE
    /// close before taking a branch out of `Active`. Shared with the engine
    /// through [`BranchManager::set_branch_drains`].
    drains: Arc<BranchDrains>,

    /// Sprinter 6bc4a4040eb7 (phase B4): the write gate's freeze hook, which is told
    /// of every level that comes and goes. Set once, at a writable open.
    freeze: OnceLock<Arc<BranchFreeze>>,

    /// See [`Self::debug_catalog_listings`].
    catalog_listings: std::sync::atomic::AtomicU64,
}

/// One batch of a level's reclaim, in keys.
const RECLAIM_CHUNK_KEYS: usize = 10_000;

/// Every family a level's keys live in. The id follows in decimal, then `:`, so
/// `bdata:1:` never matches `bdata:12:`.
const LEVEL_FAMILIES: [&str; 6] = ["bdata:", "bdel:", "bfrz:", "bv:", "bmeta:", "bkey:"];

/// Written by the deferred binary-format GC this phase replaced, which never ran. An
/// open deletes it; nothing reads it.
const LEGACY_PENDING_GC_KEY: &[u8] = b"branch:pending_gc";

impl BranchManager {
    /// Create new branch manager
    pub fn new(db: Arc<DB>, gate: Arc<WriteGate>, timestamp: Arc<RwLock<u64>>) -> Result<Self> {
        let registry = Self::load_or_create_registry(&db, &gate)?;

        Ok(Self {
            db,
            gate,
            registry: Arc::new(RwLock::new(registry)),
            metadata_cache: Arc::new(RwLock::new(HashMap::new())),
            timestamp,
            key_manager: None,
            registry_generation: Arc::new(std::sync::atomic::AtomicU64::new(1)),
            drains: Arc::new(BranchDrains::default()),
            freeze: OnceLock::new(),
            catalog_listings: std::sync::atomic::AtomicU64::new(0),
        })
    }

    /// Load a complete existing branch registry without creating metadata.
    pub(crate) fn open_existing(db: Arc<DB>, gate: Arc<WriteGate>, timestamp: Arc<RwLock<u64>>) -> Result<Self> {
        let bytes = db
            .get(BRANCH_REGISTRY_KEY)
            .map_err(|e| Error::storage(format!("branch registry read failed: {e}")))?
            .ok_or_else(|| Error::storage("physical snapshot has no branch registry"))?;
        let registry: BranchRegistry =
            bincode::deserialize(&bytes).map_err(|e| Error::storage(format!("branch registry decode failed: {e}")))?;
        if registry.branches.get(&registry.main_branch).map(String::as_str) != Some("main") {
            return Err(Error::storage("physical snapshot has no registered main branch"));
        }
        let mut names = std::collections::HashSet::new();
        let mut metadata_cache = HashMap::new();
        for (id, name) in &registry.branches {
            if *id == 0 || *id >= registry.next_branch_id || !names.insert(name.clone()) {
                return Err(Error::storage("invalid branch registry identity"));
            }
            let bytes = db
                .get(encode_branch_meta_key(name))
                .map_err(|e| Error::storage(format!("branch metadata read failed: {e}")))?
                .ok_or_else(|| Error::storage(format!("missing branch metadata for {name}")))?;
            let metadata: BranchMetadata = bincode::deserialize(&bytes)
                .map_err(|e| Error::storage(format!("branch metadata decode failed: {e}")))?;
            if metadata.branch_id != *id || metadata.name != *name {
                return Err(Error::storage("branch metadata identity mismatch"));
            }
            metadata_cache.insert(*id, metadata);
        }
        Ok(Self {
            db,
            gate,
            registry: Arc::new(RwLock::new(registry)),
            metadata_cache: Arc::new(RwLock::new(metadata_cache)),
            timestamp,
            key_manager: None,
            registry_generation: Arc::new(std::sync::atomic::AtomicU64::new(1)),
            drains: Arc::new(BranchDrains::default()),
            freeze: OnceLock::new(),
            catalog_listings: std::sync::atomic::AtomicU64::new(0),
        })
    }

    /// Give this manager the engine's key manager.
    ///
    /// It builds writes against the same `Arc<DB>` the engine does, so it must
    /// apply the same rule: a value read out of one key and written under
    /// another is decoded for the source key and sealed for the destination
    /// key. Called before the manager is published, so no reader can observe it
    /// unset.
    pub fn set_key_manager(&mut self, key_manager: Option<Arc<crate::crypto::KeyManager>>) {
        self.key_manager = key_manager;
    }

    /// Share the engine's branch-registry generation (sprinter 6bc4a4040eb7,
    /// B1). Called before the manager is published, like `set_key_manager`.
    pub fn set_registry_generation(&mut self, generation: Arc<std::sync::atomic::AtomicU64>) {
        self.registry_generation = generation;
    }

    /// Share the engine's per-branch drains (sprinter 6bc4a4040eb7, B1).
    pub fn set_branch_drains(&mut self, drains: Arc<BranchDrains>) {
        self.drains = drains;
    }

    /// The per-branch drains (see [`BranchDrains`]).
    pub fn branch_drains(&self) -> &Arc<BranchDrains> {
        &self.drains
    }

    /// Sprinter 6bc4a4040eb7 (B4): tell this manager of the engine's freeze hook.
    pub(crate) fn install_freeze(&self, freeze: Arc<BranchFreeze>) {
        let _ = self.freeze.set(freeze);
    }

    /// Sprinter 6bc4a4040eb7 (B4): every level the freeze hook must know at open, as
    /// `(id, parent, live)`: Active, or merged with an unfinished merge, which an
    /// `ABORT` puts back, are live; a merged one stays in its descendants' chains.
    pub(crate) fn levels_for_freeze(&self) -> Result<Vec<(BranchId, BranchId, bool)>> {
        let main_id = self.registry.read().main_branch;
        let unfinished = self.unfinished_merge_sources()?;
        Ok(self
            .list_branches_for_catalog()?
            .into_iter()
            .filter(|branch| branch.branch_id != main_id)
            .map(|branch| {
                let live = branch.state == BranchState::Active || unfinished.contains(&branch.branch_id);
                (branch.branch_id, branch.parent_id.unwrap_or(main_id), live)
            })
            .collect())
    }

    /// A branch just left `Active` (dropped, or merged by a writer outside
    /// this manager, e.g. the engine's MERGE): sessions bound to it must
    /// re-validate on their next statement. Called AFTER the metadata write.
    pub fn note_branch_left_active(&self) {
        self.registry_generation
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    }

    /// Load registry from storage or create new one (through `gate`)
    fn load_or_create_registry(db: &DB, gate: &WriteGate) -> Result<BranchRegistry> {
        match db.get(BRANCH_REGISTRY_KEY) {
            Ok(Some(data)) => bincode::deserialize(&data)
                .map_err(|e| Error::storage(format!("Failed to deserialize registry: {}", e))),
            Ok(None) => {
                // Create new registry with main branch
                let registry = BranchRegistry::new();

                // Create main branch metadata
                let main_metadata = BranchMetadata {
                    name: "main".to_string(),
                    branch_id: 1,
                    parent_id: None,
                    created_at: 0,
                    created_from_snapshot: 0,
                    state: BranchState::Active,
                    merge_base: None,
                    options: BranchOptions::default(),
                    stats: BranchStats::default(),
                };

                // Save main branch metadata
                let meta_key = encode_branch_meta_key("main");
                let meta_value = bincode::serialize(&main_metadata)
                    .map_err(|e| Error::storage(format!("Failed to serialize metadata: {}", e)))?;
                gate.put(db, &meta_key, &meta_value, None)
                    .map_err(|e| Error::storage(format!("Failed to save main branch metadata: {}", e)))?;

                // Save registry
                let registry_value = bincode::serialize(&registry)
                    .map_err(|e| Error::storage(format!("Failed to serialize registry: {}", e)))?;
                gate.put(db, BRANCH_REGISTRY_KEY, &registry_value, None)
                    .map_err(|e| Error::storage(format!("Failed to save registry: {}", e)))?;

                Ok(registry)
            }
            Err(e) => Err(Error::storage(format!("Failed to load registry: {}", e))),
        }
    }

    /// Save registry to storage
    fn save_registry(&self) -> Result<()> {
        let registry = self.registry.read();
        let value = bincode::serialize(&*registry)
            .map_err(|e| Error::storage(format!("Failed to serialize registry: {}", e)))?;
        self.gate
            .put(&self.db, BRANCH_REGISTRY_KEY, &value, None)
            .map_err(|e| Error::storage(format!("Failed to save registry: {}", e)))
    }

    /// Create a new branch
    pub fn create_branch(
        &self,
        name: &str,
        parent_name: Option<&str>,
        snapshot_id: SnapshotId,
        options: BranchOptions,
    ) -> Result<BranchId> {
        self.create_branch_with(name, parent_name, snapshot_id, options, |_, _| Ok(()))
    }

    /// Sprinter 6bc4a4040eb7 (B4): an id for a branch being created, durable before
    /// anything can be written under it: the keys of a branch that never came to be
    /// must not alias the next one's. A crash leaves them to the next open
    /// ([`Self::reclaim_dead_levels`]).
    pub(crate) fn reserve_branch_id(&self) -> Result<BranchId> {
        let mut registry = self.registry.write();
        let mut next = registry.clone();
        let id = next.next_id();
        let value =
            bincode::serialize(&next).map_err(|e| Error::storage(format!("Failed to serialize registry: {}", e)))?;
        self.gate
            .put(&self.db, BRANCH_REGISTRY_KEY, &value, None)
            .map_err(|e| Error::storage(format!("Failed to save registry: {}", e)))?;
        *registry = next;
        Ok(id)
    }

    /// Refuse a name an Active branch holds, before an id is spent on it. The create
    /// asks again, under the registry's lock.
    pub(crate) fn refuse_name_in_use(&self, name: &str) -> Result<()> {
        match self.get_branch_by_name(name) {
            Ok(previous) if previous.state == BranchState::Active => {
                Err(Error::storage(format!("Branch '{}' already exists", name)))
            }
            _ => Ok(()),
        }
    }

    /// [`Self::create_branch_with`], for the id [`Self::reserve_branch_id`] gave.
    pub(crate) fn create_reserved_branch(
        &self,
        id: BranchId,
        name: &str,
        parent_name: Option<&str>,
        snapshot_id: SnapshotId,
        options: BranchOptions,
        extra: impl FnOnce(BranchId, &mut rocksdb::WriteBatch) -> Result<()>,
    ) -> Result<BranchId> {
        self.create_branch_inner(Some(id), name, parent_name, snapshot_id, options, extra)
    }

    /// A branch being created will not be: the hook forgets it, and whatever it froze
    /// into it goes. What cannot go now goes at the next open.
    pub(crate) fn abandon_reserved_branch(&self, id: BranchId) {
        if let Some(freeze) = self.freeze.get() {
            freeze.remove(id);
        }
        if let Err(e) = self.reclaim_branch_keys(id) {
            tracing::warn!(
                "The keys of an abandoned branch (id {}) stay until the next open: {}",
                id,
                e
            );
        }
    }

    /// [`Self::create_branch`], with `extra` adding keys to the creation batch.
    /// sprinter 6bc4a4040eb7 (B3 fix pass): the fork record of table
    /// incarnations is born with the branch, in one WriteBatch — a branch never
    /// exists without it. `extra` runs with the registry locked and must not
    /// touch this manager.
    pub fn create_branch_with(
        &self,
        name: &str,
        parent_name: Option<&str>,
        snapshot_id: SnapshotId,
        options: BranchOptions,
        extra: impl FnOnce(BranchId, &mut rocksdb::WriteBatch) -> Result<()>,
    ) -> Result<BranchId> {
        self.create_branch_inner(None, name, parent_name, snapshot_id, options, extra)
    }

    fn create_branch_inner(
        &self,
        reserved: Option<BranchId>,
        name: &str,
        parent_name: Option<&str>,
        snapshot_id: SnapshotId,
        options: BranchOptions,
        extra: impl FnOnce(BranchId, &mut rocksdb::WriteBatch) -> Result<()>,
    ) -> Result<BranchId> {
        // Resolve parent branch — it must be ACTIVE (B1): a dropped or merged
        // branch is not a branch anybody can fork from. B1 second fix pass:
        // and it is HELD (a statement's shared drain hold) until the child is
        // recorded in its children list, then re-read under that hold. A DROP
        // or MERGE of the parent closing right now refuses the create (55006);
        // one that starts after the hold waits for it and then re-reads the
        // parent's children, so it sees the new child and refuses. Without
        // the hold a DROP could pass its child check, wait out the statement
        // creating the child, and then drop the parent of a live branch. Free
        // when this thread already holds the parent (a session bound to it).
        let main_id = self.registry.read().main_branch;
        let (parent_id, _parent_hold) = match parent_name {
            Some(parent) => {
                let meta = match self.get_branch_by_name(parent) {
                    Ok(meta) if meta.state == BranchState::Active => meta,
                    _ => return Err(Error::branch_not_active(parent)),
                };
                if meta.branch_id == main_id {
                    (Some(main_id), BranchHold::none())
                } else {
                    let hold = self.drains.enter(meta.branch_id, parent)?;
                    match self.get_branch_by_name(parent) {
                        Ok(fresh) if fresh.branch_id == meta.branch_id && fresh.state == BranchState::Active => {}
                        _ => return Err(Error::branch_not_active(parent)),
                    }
                    (Some(meta.branch_id), hold)
                }
            }
            // Default to main branch
            None => (Some(main_id), BranchHold::none()),
        };

        // sprinter 6bc4a4040eb7 (B3 third fix pass): not from the target of an
        // unfinished merge: a child would read through a level part way through
        // being rewritten, and a resume would then rewrite it under the child.
        if let Some(parent_id) = parent_id {
            self.refuse_if_in_unfinished_merge(parent_id, parent_name.unwrap_or("main"))?;
        }

        // B1 second fix pass: the registry is locked for the whole create and
        // changed only AFTER the WriteBatch below succeeded — on a CLONE first
        // — so a failed write leaves neither a phantom entry nor a consumed id
        // in memory, and two concurrent creates of one name cannot both pass
        // the name check.
        let mut registry = self.registry.write();

        // sprinter 6bc4a4040eb7 (phase B1): only an ACTIVE branch holds its
        // name. Branch metadata is keyed by NAME, so a dropped (or merged)
        // branch's record used to sit on the name forever — `CREATE BRANCH x`
        // after `DROP BRANCH x` failed "already exists" while every other
        // surface said `x` was gone. The new branch gets a NEW id from the
        // monotonic registry counter (persisted with the registry, so never
        // reused across a restart either), and the old overlay keys are under
        // the old id, so none of the old branch's rows is visible through it.
        let previous = self.get_branch_by_name(name).ok();
        let mut next_registry = registry.clone();
        // Children lists this create rewrites, by parent id.
        let mut children_updates: HashMap<BranchId, Vec<BranchId>> = HashMap::new();
        let mut merged_predecessor = None;
        if let Some(previous) = previous.as_ref() {
            match previous.state {
                BranchState::Active => {
                    return Err(Error::storage(format!("Branch '{}' already exists", name)));
                }
                // A merged branch is still REGISTERED (system views list merge
                // history from the registry). Its name is taken over, so its
                // registry entry and parent link go; a merged branch that still
                // has children keeps its name, because the children resolve
                // their parent through the registry by id.
                BranchState::Merged { .. } => {
                    // B3 fix pass: an unfinished merge is resumed by this name.
                    self.refuse_if_in_unfinished_merge(previous.branch_id, name)?;
                    let children = self.get_child_branches(previous.branch_id)?;
                    if !children.is_empty() {
                        return Err(Error::storage(format!(
                            "Cannot reuse the name of merged branch '{}': it still has {} child branch(es)",
                            name,
                            children.len()
                        )));
                    }
                    if let Some(old_parent) = previous.parent_id {
                        let mut siblings = self.get_child_branches(old_parent)?;
                        siblings.retain(|&id| id != previous.branch_id);
                        children_updates.insert(old_parent, siblings);
                    }
                    next_registry.remove_branch(previous.branch_id);
                    merged_predecessor = Some(previous.branch_id);
                }
                BranchState::Dropped { .. } => {}
            }
        }

        // Allocate branch ID, unless the engine reserved it already.
        let branch_id = match reserved {
            Some(id) => id,
            None => next_registry.next_id(),
        };
        next_registry.add_branch(branch_id, name.to_string());
        if let Some(parent_id) = parent_id {
            let mut children = match children_updates.remove(&parent_id) {
                Some(children) => children,
                None => self.get_child_branches(parent_id)?,
            };
            children.push(branch_id);
            children_updates.insert(parent_id, children);
        }

        // Get current timestamp
        let current_ts = self.next_timestamp();

        // Create metadata
        let metadata = BranchMetadata {
            name: name.to_string(),
            branch_id,
            parent_id,
            created_at: current_ts,
            created_from_snapshot: snapshot_id,
            state: BranchState::Active,
            merge_base: None,
            options,
            stats: BranchStats::default(),
        };

        // sprinter 6bc4a4040eb7 (B1): the metadata (id N, Active), the registry
        // (which carries `next_branch_id`) and the children lists go in ONE
        // WriteBatch. Written separately, a crash between the metadata and the
        // registry left a branch with id N on disk under a registry whose
        // `next_branch_id` was still N — and the next CREATE BRANCH reused N,
        // so its overlay keys aliased the crashed branch's.
        let meta_key = encode_branch_meta_key(name);
        let meta_value = bincode::serialize(&metadata)
            .map_err(|e| Error::storage(format!("Failed to serialize metadata: {}", e)))?;
        let registry_value = bincode::serialize(&next_registry)
            .map_err(|e| Error::storage(format!("Failed to serialize registry: {}", e)))?;
        let mut batch = rocksdb::WriteBatch::default();
        batch.put(&meta_key, &meta_value);
        batch.put(BRANCH_REGISTRY_KEY, &registry_value);
        for (parent, children) in &children_updates {
            let children_value = bincode::serialize(children)
                .map_err(|e| Error::storage(format!("Failed to serialize children: {}", e)))?;
            batch.put(encode_branch_children_key(*parent), &children_value);
        }
        extra(branch_id, &mut batch)?;
        self.gate
            .write(&self.db, batch, None)
            .map_err(|e| Error::storage(format!("Failed to save branch metadata: {}", e)))?;

        // The batch is durable: now the in-memory registry and cache follow.
        *registry = next_registry;
        drop(registry);
        let mut cache = self.metadata_cache.write();
        if let Some(predecessor) = merged_predecessor {
            cache.remove(&predecessor);
        }
        cache.insert(branch_id, metadata);
        drop(cache);
        // Sprinter 6bc4a4040eb7 (B4): a create with no engine in front of it (a
        // webhook with none attached); the engine's registered it first.
        if let (Some(freeze), Some(parent)) = (self.freeze.get(), parent_id) {
            freeze.note_published(branch_id, parent);
        }

        Ok(branch_id)
    }

    /// sprinter 6bc4a4040eb7 (B3 fix pass): refuse (55006) what would take away
    /// a branch an unfinished MERGE BRANCH still needs: its source holds the rows
    /// the resume copies, its target receives them. Here, in the manager, so
    /// every surface is covered (SQL, the REST routes, the webhooks).
    pub(crate) fn refuse_if_in_unfinished_merge(&self, branch_id: BranchId, name: &str) -> Result<()> {
        let prefix = super::branch_catalog::BRANCH_MERGE_INTENT_PREFIX.as_bytes();
        let mut read_opts = rocksdb::ReadOptions::default();
        read_opts.set_total_order_seek(true);
        let iter = self.db.iterator_opt(
            rocksdb::IteratorMode::From(prefix, rocksdb::Direction::Forward),
            read_opts,
        );
        for item in iter {
            let (key, value) = item.map_err(|e| Error::storage(format!("Iterator error: {}", e)))?;
            if !key.starts_with(prefix) {
                break;
            }
            let plain = super::tde::open_ref(self.key_manager.as_deref(), &key, &value)?;
            let intent = super::branch_catalog::MergeIntent::decode(&plain)?;
            if intent.source_id == branch_id || intent.target_id == branch_id {
                return Err(Error::branch_merge_pending(
                    &format!("branch {name}"),
                    &intent.source_name,
                    &intent.target_name,
                ));
            }
        }
        Ok(())
    }

    /// The sources of the unfinished merges of this store.
    fn unfinished_merge_sources(&self) -> Result<HashSet<BranchId>> {
        let prefix = super::branch_catalog::BRANCH_MERGE_INTENT_PREFIX.as_bytes();
        let mut read_opts = rocksdb::ReadOptions::default();
        read_opts.set_total_order_seek(true);
        let iter = self.db.iterator_opt(
            rocksdb::IteratorMode::From(prefix, rocksdb::Direction::Forward),
            read_opts,
        );
        let mut sources = HashSet::new();
        for item in iter {
            let (key, value) = item.map_err(|e| Error::storage(format!("Iterator error: {}", e)))?;
            if !key.starts_with(prefix) {
                break;
            }
            let plain = super::tde::open_ref(self.key_manager.as_deref(), &key, &value)?;
            sources.insert(super::branch_catalog::MergeIntent::decode(&plain)?.source_id);
        }
        Ok(sources)
    }

    /// Drop a branch
    pub fn drop_branch(&self, name: &str, if_exists: bool) -> Result<()> {
        // Get branch metadata. sprinter 6bc4a4040eb7 (B1): an already-DROPPED
        // branch is not found (its record only keeps the name until reuse);
        // a merged branch can still be dropped — `MERGE … WITH
        // (delete_branch_after)` relies on that.
        let metadata = match self.get_branch_by_name(name) {
            Ok(meta) if !matches!(meta.state, BranchState::Dropped { .. }) => meta,
            Ok(_) if if_exists => return Ok(()),
            Ok(_) => return Err(Error::storage(format!("Branch '{}' not found", name))),
            Err(_) if if_exists => return Ok(()),
            Err(e) => return Err(e),
        };

        // Prevent dropping main branch
        if metadata.branch_id == self.registry.read().main_branch {
            return Err(Error::storage("Cannot drop main branch"));
        }

        // Check for child branches
        self.refuse_if_it_has_a_child(metadata.branch_id, name)?;

        // sprinter 6bc4a4040eb7 (B1): wait for the statements running on the
        // branch, and keep new ones out, until it is marked dropped below — so
        // nothing bound to it writes after this returns.
        let drained = self.drains.close(metadata.branch_id, name, BRANCH_DRAIN_TIMEOUT)?;

        // B1 second fix pass: everything read above may be stale by now — the
        // close waited for running statements and, possibly, for another DROP
        // or MERGE closing the branch first. RE-READ it: it must still be the
        // branch whose drain was closed (same id: not a newer branch that
        // took the name meanwhile) and not already dropped (a merged branch
        // may still be dropped, as before), and the child check is repeated —
        // a statement running on the branch while the close waited may have
        // been a `CREATE BRANCH` forking from it. The Dropped record is built
        // from this fresh read, never from the pre-wait one (which would
        // overwrite a concurrent merge's record).
        let metadata = match self.get_branch_by_name(name) {
            Ok(fresh)
                if fresh.branch_id == metadata.branch_id && !matches!(fresh.state, BranchState::Dropped { .. }) =>
            {
                fresh
            }
            _ if if_exists => return Ok(()),
            _ => return Err(Error::storage(format!("Branch '{}' not found", name))),
        };
        self.refuse_if_in_unfinished_merge(metadata.branch_id, name)?;
        self.refuse_if_it_has_a_child(metadata.branch_id, name)?;

        // Mark as dropped (soft delete)
        let mut updated_meta = metadata.clone();
        updated_meta.state = BranchState::Dropped {
            at_timestamp: self.current_timestamp(),
        };

        // Save updated metadata
        let meta_key = encode_branch_meta_key(name);
        let meta_value = bincode::serialize(&updated_meta)
            .map_err(|e| Error::storage(format!("Failed to serialize metadata: {}", e)))?;
        self.gate
            .put(&self.db, &meta_key, &meta_value, None)
            .map_err(|e| Error::storage(format!("Failed to save updated metadata: {}", e)))?;

        // Release this branch from its parent's children list. Without it the
        // parent keeps a dead child id and can never be dropped itself — see
        // `remove_child_branch`. Done after the metadata write (the point of no
        // return) so the index cannot be pruned for a drop that then fails.
        if let Some(parent_id) = metadata.parent_id {
            self.remove_child_branch(parent_id, metadata.branch_id)?;
        }

        // Remove from registry
        {
            let mut registry = self.registry.write();
            registry.remove_branch(metadata.branch_id);
        }
        self.save_registry()?;

        // Remove from cache
        self.metadata_cache.write().remove(&metadata.branch_id);

        // sprinter 6bc4a4040eb7 (B1): the branch is no longer active — every
        // session bound to it fails closed from its next statement on.
        self.note_branch_left_active();
        drop(drained);

        // Sprinter 6bc4a4040eb7 (B4): its keys go, now that it is out of service. No
        // write freezes into it once the hook has forgotten it, and that waits for the
        // ones that still might. The drop is done whatever happens here: what a crash
        // or an error leaves is finished by the next open.
        if let Some(freeze) = self.freeze.get() {
            freeze.remove(metadata.branch_id);
        }
        if let Err(e) = self.reclaim_branch_keys(metadata.branch_id) {
            tracing::warn!(
                "The keys of dropped branch '{}' (id {}) stay until the next open: {}",
                name,
                metadata.branch_id,
                e
            );
        }
        // It may have been the last live branch reading through a merged parent.
        self.reclaim_merged_chain(metadata.parent_id);

        Ok(())
    }

    /// Sprinter 6bc4a4040eb7 (B4): delete every key of level `id`, in batches of
    /// [`RECLAIM_CHUNK_KEYS`]. Idempotent: run again, it deletes what is left. The
    /// caller knows nothing reads or writes the level any more. A prefix match on the
    /// decimal id and its `:`. Returns the keys deleted.
    pub(crate) fn reclaim_branch_keys(&self, id: BranchId) -> Result<u64> {
        let mut deleted = 0u64;
        for family in LEVEL_FAMILIES {
            deleted += self.delete_prefix_in_batches(format!("{family}{id}:").as_bytes())?;
        }
        if let Some(freeze) = self.freeze.get() {
            super::branch_view::forget_level(freeze.engine_id(), id);
            super::StorageEngine::forget_level_records(freeze.engine_id(), id);
        }
        Ok(deleted)
    }

    fn delete_prefix_in_batches(&self, prefix: &[u8]) -> Result<u64> {
        let mut deleted = 0u64;
        // Sprinter 6bc4a4040eb7 (B4 fix pass): each batch starts after the last key of
        // the one before, and stops at the prefix's end: starting over at the prefix
        // stepped through every key already deleted, once per batch.
        let mut from = prefix.to_vec();
        loop {
            // A fresh iterator per batch: it pins no more than one batch's worth.
            let iter = self.db.iterator_opt(
                rocksdb::IteratorMode::From(&from, rocksdb::Direction::Forward),
                super::time_travel::bounded_read_options(prefix),
            );
            let mut batch = rocksdb::WriteBatch::default();
            let mut keys = 0usize;
            for item in iter {
                let (key, _) = item.map_err(|e| Error::storage(format!("Iterator error: {}", e)))?;
                if !key.starts_with(prefix) {
                    break;
                }
                batch.delete(&key);
                keys += 1;
                if keys >= RECLAIM_CHUNK_KEYS {
                    from = key.to_vec();
                    from.push(0);
                    break;
                }
            }
            if keys == 0 {
                return Ok(deleted);
            }
            self.gate
                .write(&self.db, batch, None)
                .map_err(|e| Error::storage(format!("Failed to delete branch keys: {}", e)))?;
            deleted += keys as u64;
            if let Some(freeze) = self.freeze.get() {
                super::branch_catalog::engine_failpoint_tick(
                    freeze.engine_id(),
                    super::branch_catalog::MergeFailSite::Reclaim,
                )?;
            }
        }
    }

    /// The levels a live branch still reads through: every ancestor of a branch that
    /// is Active or the source of an unfinished merge. Asked of the metadata: a merged
    /// level stays in its descendants' chains. Linear in the branches listed.
    fn levels_with_live_descendants(branches: &[BranchMetadata], unfinished: &HashSet<BranchId>) -> HashSet<BranchId> {
        let parents: HashMap<BranchId, Option<BranchId>> = branches
            .iter()
            .map(|branch| (branch.branch_id, branch.parent_id))
            .collect();
        let mut read_through: HashSet<BranchId> = HashSet::new();
        for branch in branches {
            if branch.state != BranchState::Active && !unfinished.contains(&branch.branch_id) {
                continue;
            }
            let mut cursor = branch.parent_id;
            // An ancestor already in the set has its own ancestors there too.
            while let Some(level) = cursor {
                if !read_through.insert(level) {
                    break;
                }
                cursor = parents.get(&level).copied().flatten();
            }
        }
        read_through
    }

    /// Sprinter 6bc4a4040eb7 (B4): from `start` up the parents: reclaim every merged
    /// level with nothing left to read it or finish it. Stops at the first that is
    /// Active (a `MERGE … ABORT` returns a source there), has an unfinished merge, or
    /// still has a live descendant. Never fails: what stays is finished at an open.
    pub(crate) fn reclaim_merged_chain(&self, start: Option<BranchId>) {
        let main_id = self.registry.read().main_branch;
        let mut cursor = start;
        while let Some(id) = cursor.filter(|id| *id != main_id) {
            let Some(name) = self.get_branch_name(id) else {
                return;
            };
            let Ok(meta) = self.get_branch_by_name(&name) else {
                return;
            };
            if meta.branch_id != id || !matches!(meta.state, BranchState::Merged { .. }) {
                return;
            }
            let reclaimed = self.unfinished_merge_sources().and_then(|unfinished| {
                let branches = self.list_branches_for_catalog()?;
                if unfinished.contains(&id) || Self::levels_with_live_descendants(&branches, &unfinished).contains(&id)
                {
                    return Ok(false);
                }
                self.reclaim_branch_keys(id).map(|_| true)
            });
            match reclaimed {
                Ok(true) => cursor = meta.parent_id,
                Ok(false) => return,
                Err(e) => {
                    tracing::warn!("The keys of merged branch '{}' stay until the next open: {}", name, e);
                    return;
                }
            }
        }
    }

    /// Sprinter 6bc4a4040eb7 (B4): at a writable open, after every unfinished merge
    /// had its chance: delete the keys of every level that is gone: a drop or a merge
    /// the crash stopped half way, a merged level whose last descendant went, a branch
    /// whose creation never finished (its id has no metadata). A level with a live
    /// state, an unfinished merge or a live descendant is left alone. Walks the ids
    /// present in each family, a seek per id, whatever the volume of the keys. Returns
    /// the keys deleted.
    pub(crate) fn reclaim_dead_levels(&self) -> Result<u64> {
        let unfinished = self.unfinished_merge_sources()?;
        let branches = self.list_branches_for_catalog()?;
        let mut keep = Self::levels_with_live_descendants(&branches, &unfinished);
        keep.insert(self.registry.read().main_branch);
        for branch in &branches {
            if branch.state == BranchState::Active || unfinished.contains(&branch.branch_id) {
                keep.insert(branch.branch_id);
            }
        }
        let mut deleted = 0u64;
        for family in LEVEL_FAMILIES {
            let mut cursor = family.as_bytes().to_vec();
            loop {
                let mut iter = self.db.iterator_opt(
                    rocksdb::IteratorMode::From(&cursor, rocksdb::Direction::Forward),
                    super::time_travel::bounded_read_options(family.as_bytes()),
                );
                let Some(item) = iter.next() else {
                    break;
                };
                let (key, _) = item.map_err(|e| Error::storage(format!("Iterator error: {}", e)))?;
                let Some(rest) = key.strip_prefix(family.as_bytes()) else {
                    break;
                };
                match super::branch_freeze::level_and_rest(rest) {
                    Some((id, _)) => {
                        if !keep.contains(&id) {
                            deleted += self.delete_prefix_in_batches(format!("{family}{id}:").as_bytes())?;
                        }
                        // `;` is the byte after `:`: the next id.
                        cursor = format!("{family}{id};").into_bytes();
                    }
                    // Not a level's key: the binary format of the `BranchTransaction`
                    // API, which no SQL path writes. Left alone.
                    None => {
                        cursor = key.to_vec();
                        cursor.push(0);
                    }
                }
            }
        }
        if self
            .db
            .get_pinned(LEGACY_PENDING_GC_KEY)
            .map_err(|e| Error::storage(format!("Get failed: {}", e)))?
            .is_some()
        {
            self.gate
                .delete(&self.db, LEGACY_PENDING_GC_KEY, None)
                .map_err(|e| Error::storage(format!("Failed to delete a stale key: {}", e)))?;
        }
        Ok(deleted)
    }

    /// A branch with a child cannot be dropped: 2BP01, naming the child and what to do.
    /// A merged child counts until it is dropped: its record names this as its parent.
    fn refuse_if_it_has_a_child(&self, id: BranchId, name: &str) -> Result<()> {
        let children = self.get_child_branches(id)?;
        if children.is_empty() {
            return Ok(());
        }
        // One that still reads through this first: it is the one to act on.
        let mut named: Option<(String, bool)> = None;
        for child in &children {
            let Some(child_name) = self.get_branch_name(*child) else {
                continue;
            };
            let merged = matches!(
                self.get_branch_by_name(&child_name).map(|meta| meta.state),
                Ok(BranchState::Merged { .. })
            );
            if !merged {
                named = Some((child_name, false));
                break;
            }
            if named.is_none() {
                named = Some((child_name, true));
            }
        }
        let (child, merged) =
            named.unwrap_or_else(|| (children.first().map_or_else(String::new, ToString::to_string), false));
        Err(Error::branch_has_child(name, &child, children.len(), merged))
    }

    /// See [`Self::refuse_if_it_has_a_child`], by name; nothing to refuse for a branch
    /// there is none of.
    pub(crate) fn refuse_drop_of_branch_with_child(&self, name: &str) -> Result<()> {
        match self.get_branch_by_name(name) {
            Ok(meta) if !matches!(meta.state, BranchState::Dropped { .. }) => {
                self.refuse_if_it_has_a_child(meta.branch_id, name)
            }
            _ => Ok(()),
        }
    }

    /// Get branch metadata by name
    pub fn get_branch_by_name(&self, name: &str) -> Result<BranchMetadata> {
        let meta_key = encode_branch_meta_key(name);
        let data = self
            .db
            .get(&meta_key)
            .map_err(|e| Error::storage(format!("Failed to read branch metadata: {}", e)))?
            .ok_or_else(|| Error::storage(format!("Branch '{}' not found", name)))?;

        bincode::deserialize(&data).map_err(|e| Error::storage(format!("Failed to deserialize metadata: {}", e)))
    }

    /// Persist `metadata` under its name, refresh the id-keyed cache, and —
    /// when the branch is no longer `Active` — bump the registry generation so
    /// sessions bound to it re-validate (sprinter 6bc4a4040eb7, B1). Also used
    /// to put a branch BACK to its previous state when a merge fails.
    pub fn write_branch_metadata(&self, metadata: &BranchMetadata) -> Result<()> {
        let meta_key = encode_branch_meta_key(&metadata.name);
        let meta_value =
            bincode::serialize(metadata).map_err(|e| Error::storage(format!("Failed to serialize metadata: {}", e)))?;
        self.gate
            .put(&self.db, &meta_key, &meta_value, None)
            .map_err(|e| Error::storage(format!("Failed to save branch metadata: {}", e)))?;
        self.metadata_cache.write().insert(metadata.branch_id, metadata.clone());
        // Bumped on every state write, `Active` included: a session whose
        // binding was refused while the branch was briefly marked (a failed
        // merge being rolled back) re-validates and finds it active again.
        self.note_branch_left_active();
        Ok(())
    }

    /// Evict `branch_id` from the id-keyed metadata cache, after a writer
    /// outside this manager (the engine's MERGE, which marks the source
    /// `Merged`) rewrote its record.
    pub fn evict_cached_metadata(&self, branch_id: BranchId) {
        self.metadata_cache.write().remove(&branch_id);
    }

    /// Get branch metadata by ID
    pub fn get_branch_by_id(&self, branch_id: BranchId) -> Result<BranchMetadata> {
        // Check cache first
        if let Some(metadata) = self.metadata_cache.read().get(&branch_id) {
            return Ok(metadata.clone());
        }

        // Get branch name from registry
        let name = self
            .registry
            .read()
            .get_name(branch_id)
            .ok_or_else(|| Error::storage(format!("Branch ID {} not found", branch_id)))?
            .to_string();

        // Load metadata
        let metadata = self.get_branch_by_name(&name)?;

        // Cache it
        self.metadata_cache.write().insert(branch_id, metadata.clone());

        Ok(metadata)
    }

    /// List all active branches
    pub fn list_branches(&self) -> Result<Vec<BranchMetadata>> {
        let registry = self.registry.read();
        let mut branches = Vec::new();

        for (branch_id, name) in &registry.branches {
            match self.get_branch_by_name(name) {
                Ok(metadata) if metadata.state == BranchState::Active => {
                    branches.push(metadata);
                }
                _ => continue,
            }
        }

        Ok(branches)
    }

    /// List branches for catalog/history views: `Active` **and** `Merged`.
    ///
    /// `list_branches` returns only `Active` branches, which is what operational
    /// callers want — branch resolution, "which branches can I use".
    ///
    /// The catalog views are different: they project a state/status column, and
    /// `handle_show_branches` even carries a formatting arm for
    /// `Merged { into_branch, at_timestamp }` that could never run, because every
    /// non-Active branch was filtered out upstream. A merged branch vanished from
    /// `pg_database_branches()` entirely, so merge history was unreachable even
    /// though `BranchState::Merged` is recorded faithfully in storage — the
    /// status column could only ever read "Active".
    ///
    /// `Dropped` stays hidden here: a drop is a delete, not history.
    pub fn list_branches_for_catalog(&self) -> Result<Vec<BranchMetadata>> {
        self.catalog_listings.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let registry = self.registry.read();
        let mut branches = Vec::new();

        for name in registry.branches.values() {
            match self.get_branch_by_name(name) {
                Ok(metadata) if !matches!(metadata.state, BranchState::Dropped { .. }) => {
                    branches.push(metadata);
                }
                _ => continue,
            }
        }

        Ok(branches)
    }

    /// Return true when any registered branch other than main exists.
    pub fn has_user_branches_registered(&self) -> bool {
        let registry = self.registry.read();
        registry
            .branches
            .iter()
            .any(|(branch_id, name)| *branch_id != registry.main_branch && name != "main")
    }

    /// How many times the catalog's branches were listed. For the tests: an open must
    /// not do that per branch.
    #[doc(hidden)]
    pub fn debug_catalog_listings(&self) -> u64 {
        self.catalog_listings.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Get branch name by ID
    pub fn get_branch_name(&self, branch_id: BranchId) -> Option<String> {
        let registry = self.registry.read();
        registry.branches.get(&branch_id).cloned()
    }

    /// Remove a child branch from its parent's children list.
    ///
    /// The mirror of the children-list append in `create_branch`, which had
    /// none: the list only ever grew. A dropped child stayed in its parent's list forever, so
    /// `drop_branch`'s "has N child branch(es)" guard kept firing and any branch
    /// that had EVER had a child became permanently undroppable — an unbounded
    /// leak with no workaround, since nothing else writes this key.
    ///
    /// Writes the pruned list back rather than deleting the key when it empties:
    /// `get_child_branches` treats a missing key and an empty list identically,
    /// so this needs no delete primitive on the store handle.
    fn remove_child_branch(&self, parent_id: BranchId, child_id: BranchId) -> Result<()> {
        let key = encode_branch_children_key(parent_id);

        let mut children: Vec<BranchId> = match self.db.get(&key) {
            Ok(Some(data)) => bincode::deserialize(&data)
                .map_err(|e| Error::storage(format!("Failed to deserialize children: {}", e)))?,
            // No list at all: nothing to prune.
            Ok(None) => return Ok(()),
            Err(e) => return Err(Error::storage(format!("Failed to read children: {}", e))),
        };

        let before = children.len();
        children.retain(|&id| id != child_id);
        if children.len() == before {
            // Not a child of this parent — leave the key untouched rather than
            // rewriting it, so a bad call cannot churn unrelated state.
            return Ok(());
        }

        let value = bincode::serialize(&children)
            .map_err(|e| Error::storage(format!("Failed to serialize children: {}", e)))?;
        self.gate
            .put(&self.db, &key, &value, None)
            .map_err(|e| Error::storage(format!("Failed to save children: {}", e)))
    }

    /// Get child branches
    fn get_child_branches(&self, parent_id: BranchId) -> Result<Vec<BranchId>> {
        let key = encode_branch_children_key(parent_id);

        match self.db.get(&key) {
            Ok(Some(data)) => bincode::deserialize(&data)
                .map_err(|e| Error::storage(format!("Failed to deserialize children: {}", e))),
            Ok(None) => Ok(Vec::new()),
            Err(e) => Err(Error::storage(format!("Failed to read children: {}", e))),
        }
    }

    /// Get current timestamp
    pub fn current_timestamp(&self) -> u64 {
        *self.timestamp.read()
    }

    /// Get next timestamp (increment)
    fn next_timestamp(&self) -> u64 {
        let mut ts = self.timestamp.write();
        *ts += 1;
        *ts
    }

    /// Build parent chain for a branch
    pub fn build_parent_chain(&self, branch_id: BranchId) -> Result<Vec<(BranchId, SnapshotId)>> {
        let mut chain = Vec::new();
        let mut current_id = branch_id;

        loop {
            let metadata = self.get_branch_by_id(current_id)?;

            match metadata.parent_id {
                Some(parent_id) => {
                    chain.push((parent_id, metadata.created_from_snapshot));
                    current_id = parent_id;
                }
                None => break, // Reached root
            }
        }

        Ok(chain)
    }
}

/// Branch-aware transaction
pub struct BranchTransaction {
    /// Underlying transaction
    tx: Transaction,

    /// Branch ID
    branch_id: BranchId,

    /// Branch metadata (cached)
    branch_meta: BranchMetadata,

    /// Parent chain (cached for reads)
    parent_chain: Vec<(BranchId, SnapshotId)>,

    /// Database reference for parent lookups
    db: Arc<DB>,
}

impl BranchTransaction {
    /// Create a new branch transaction. `gate` is the write gate of the engine
    /// that owns `db`; the commit of the inner transaction goes through it.
    pub fn new(
        db: Arc<DB>,
        gate: Arc<WriteGate>,
        branch_id: BranchId,
        branch_meta: BranchMetadata,
        parent_chain: Vec<(BranchId, SnapshotId)>,
        snapshot_id: SnapshotId,
        snapshot_manager: Arc<super::time_travel::SnapshotManager>,
        key_manager: Option<Arc<crate::crypto::KeyManager>>,
    ) -> Result<Self> {
        let mut tx = Transaction::new(Arc::clone(&db), gate, snapshot_id, snapshot_manager)?;
        // The parent-chain fallback in `get` below reads MAIN-branch `data:`
        // keys, which the storage boundary now seals. Carry the engine's key
        // manager so those reads decode by the same rule.
        tx.set_key_manager(key_manager);

        Ok(Self {
            tx,
            branch_id,
            branch_meta,
            parent_chain,
            db,
        })
    }

    /// Read from branch with parent fallback
    pub fn get(&self, key: &Key) -> Result<Option<Vec<u8>>> {
        // Convert user key to string for branch key encoding
        let user_key = String::from_utf8_lossy(key);

        // Try current branch first
        let branch_key = encode_branch_data_key(self.branch_id, &user_key, self.tx.snapshot_id());

        // Check if exists in current branch
        if let Some(value) = self.tx.get(&branch_key)? {
            return Ok(Some(value));
        }

        // Walk parent chain
        for (parent_id, _parent_snapshot) in &self.parent_chain {
            // For the main branch (parent_id == 1), use the original key directly
            // For other branches, use the branch-aware key format
            let parent_key = if *parent_id == 1 {
                // Main branch data: use the original key as-is
                key.to_vec()
            } else {
                // Other branch data: bdata:<parent_id>:<user_key>
                encode_branch_data_key(*parent_id, &user_key, 0) // timestamp unused
            };

            if let Some(value) = self
                .db
                .get(&parent_key)
                .map_err(|e| Error::storage(format!("Parent read failed: {}", e)))?
            {
                // For `parent_id == 1` this is a main-branch `data:` row, i.e.
                // a sealed keyspace read raw. Decode through the transaction's
                // copy of the shared rule.
                return Ok(Some(self.tx.decode_stored(&parent_key, value)?));
            }
        }

        Ok(None)
    }

    /// Write to branch (copy-on-write)
    pub fn put(&mut self, key: Key, value: Vec<u8>) -> Result<()> {
        let user_key = String::from_utf8_lossy(&key);
        let branch_key = encode_branch_data_key(self.branch_id, &user_key, self.tx.snapshot_id());

        self.tx.put(branch_key, value)
    }

    /// Delete from branch
    pub fn delete(&mut self, key: Key) -> Result<()> {
        let user_key = String::from_utf8_lossy(&key);
        let branch_key = encode_branch_data_key(self.branch_id, &user_key, self.tx.snapshot_id());

        self.tx.delete(branch_key)
    }

    /// Commit transaction
    pub fn commit(self) -> Result<()> {
        self.tx.commit()
    }

    /// Rollback transaction
    pub fn rollback(self) -> Result<()> {
        self.tx.rollback()
    }

    /// Get snapshot ID
    pub fn snapshot_id(&self) -> SnapshotId {
        self.tx.snapshot_id()
    }
}

/// Encode a branch metadata key
fn encode_branch_meta_key(branch_name: &str) -> Vec<u8> {
    let mut key = Vec::new();
    key.extend_from_slice(BRANCH_META_PREFIX);
    key.extend_from_slice(branch_name.as_bytes());
    key
}

/// Encode a branch children key
fn encode_branch_children_key(parent_id: BranchId) -> Vec<u8> {
    let mut key = Vec::new();
    key.extend_from_slice(BRANCH_CHILDREN_PREFIX);
    key.extend_from_slice(&parent_id.to_be_bytes());
    key
}

/// Encode a branch data key
///
/// Format: bdata:<branch_id>:<user_key>
/// Note: timestamp parameter is kept for backward compatibility but not used in the key
/// The key must NOT include the timestamp because each transaction has its own snapshot ID,
/// which would make reads from subsequent transactions unable to find previous writes
pub fn encode_branch_data_key(branch_id: BranchId, user_key: &str, _timestamp: u64) -> Vec<u8> {
    let mut key = Vec::new();
    key.extend_from_slice(BRANCH_DATA_PREFIX);
    key.extend_from_slice(&branch_id.to_be_bytes());
    key.push(b':');
    key.extend_from_slice(user_key.as_bytes());
    key
}

/// Decode a branch data key
///
/// Returns (branch_id, user_key)
/// Note: timestamp is no longer part of the key
pub fn decode_branch_data_key(key: &[u8]) -> Option<(BranchId, String)> {
    if !key.starts_with(BRANCH_DATA_PREFIX) {
        return None;
    }

    let remaining = key.get(BRANCH_DATA_PREFIX.len()..)?;

    // Parse branch ID (8 bytes)
    if remaining.len() < 8 {
        return None;
    }
    let branch_id = u64::from_be_bytes(remaining.get(..8)?.try_into().ok()?);

    // Find separator
    if remaining.get(8)? != &b':' {
        return None;
    }

    let remaining = remaining.get(9..)?;

    // Remaining is the user key
    let user_key = String::from_utf8(remaining.to_vec()).ok()?;

    Some((branch_id, user_key))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::storage::StorageEngine;

    /// sprinter 6bc4a4040eb7 (B3 fix pass): a schema statement's exclusive
    /// section waits for the OTHER statements on the branch (times out 55006),
    /// a second one is refused at once, its own nested statements pass, and a
    /// new statement waits for it and then proceeds.
    #[test]
    fn the_exclusive_section_runs_alone() {
        let drains = Arc::new(BranchDrains::default());
        let short = std::time::Duration::from_millis(50);

        // Another thread holds the branch: the section times out, and clears.
        {
            let (held_tx, held_rx) = std::sync::mpsc::channel();
            let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
            let other = Arc::clone(&drains);
            let holder = std::thread::spawn(move || {
                let _hold = other.enter(9, "b").unwrap();
                held_tx.send(()).unwrap();
                done_rx.recv().unwrap();
            });
            held_rx.recv().unwrap();
            let err = drains
                .exclusive(9, "b", short)
                .err()
                .expect("another statement is running");
            assert!(err.to_string().contains(crate::error::BRANCH_BUSY_MARKER), "{err}");
            done_tx.send(()).unwrap();
            holder.join().unwrap();
        }

        // Alone: granted, and the caller's nested statements pass.
        let section = drains.exclusive(9, "b", short).unwrap();
        drop(drains.enter(9, "b").unwrap());
        drop(section);

        // Two statements already on the branch both want the section: the
        // second is refused at once (neither could end waiting for the other's
        // hold), and the first gets it when the second's statement ends.
        {
            let (held_tx, held_rx) = std::sync::mpsc::channel();
            let (asked_tx, asked_rx) = std::sync::mpsc::channel::<()>();
            let other = Arc::clone(&drains);
            let second = std::thread::spawn(move || {
                let _hold = other.enter(9, "b").unwrap();
                held_tx.send(()).unwrap();
                asked_rx.recv().unwrap();
                // The first is waiting by now, and this hold keeps it waiting.
                while !other.state(9).exclusive {
                    std::thread::yield_now();
                }
                let started = std::time::Instant::now();
                let refused = other.exclusive(9, "b", std::time::Duration::from_secs(20)).err();
                (refused.map(|e| e.to_string()), started.elapsed())
            });
            held_rx.recv().unwrap();
            asked_tx.send(()).unwrap();
            let granted = drains.exclusive(9, "b", std::time::Duration::from_secs(20));
            assert!(granted.is_ok(), "the first gets the section once the second ended");
            let (refused, took) = second.join().unwrap();
            let refused = refused.expect("the second section is refused");
            assert!(refused.contains(crate::error::BRANCH_BUSY_MARKER), "{refused}");
            assert!(
                took < std::time::Duration::from_secs(5),
                "refused at once, took {took:?}"
            );
        }

        // A new statement waits for the section and then proceeds.
        let section = drains.exclusive(9, "b", short).unwrap();
        let entered = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let waiter = {
            let drains = Arc::clone(&drains);
            let entered = Arc::clone(&entered);
            std::thread::spawn(move || {
                let hold = drains.enter(9, "b");
                entered.store(true, std::sync::atomic::Ordering::SeqCst);
                assert!(hold.is_ok(), "the statement proceeds once the section ends");
            })
        };
        while drains.state(9).waiting_statements == 0 {
            std::thread::yield_now();
        }
        assert!(
            !entered.load(std::sync::atomic::Ordering::SeqCst),
            "the statement waits for the section"
        );
        drop(section);
        waiter.join().unwrap();
        assert!(entered.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(drains.state(9), BranchDrainState::default());
    }

    /// sprinter 6bc4a4040eb7 (B1 fix pass): the drain. A close waits for the
    /// statements holding the branch, refuses new ones IMMEDIATELY (55006)
    /// until it is done (B1 second fix pass), and refuses (instead of waiting
    /// for itself) when its own thread holds it. A second close queues.
    #[test]
    fn a_branch_close_waits_for_holders_and_keeps_new_ones_out() {
        let drains = Arc::new(BranchDrains::default());
        let hold = drains.enter(7, "b").unwrap();
        assert!(drains.held_by_this_thread(7));
        assert_eq!(drains.state(7).in_flight, 1);
        // Nested entry on the same thread is free (no self-wait).
        let nested = drains.enter(7, "b").unwrap();
        drop(nested);
        assert!(drains.held_by_this_thread(7), "the outer hold is still held");
        assert!(
            drains.close(7, "b", BRANCH_DRAIN_TIMEOUT).is_err(),
            "own hold: refused, not a self-wait"
        );

        let (closed_tx, closed_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let closer = {
            let drains = Arc::clone(&drains);
            std::thread::spawn(move || {
                let guard = drains.close(7, "b", BRANCH_DRAIN_TIMEOUT).unwrap();
                closed_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                drop(guard);
            })
        };
        // The closer is waiting for the hold: `closing` is set, nothing closed.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !drains.state(7).closing {
            assert!(std::time::Instant::now() < deadline, "the close never started");
            std::thread::yield_now();
        }
        assert!(
            closed_rx.try_recv().is_err(),
            "the close must wait while a statement holds the branch"
        );
        drop(hold);
        assert!(!drains.held_by_this_thread(7));
        closed_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("the close proceeds once the holder is gone");
        // A new statement arriving while the close is in progress is refused
        // at once, with the closing marker — not parked behind the close.
        let late = drains.enter(7, "b").err().expect("refused while closing");
        assert!(late.to_string().contains(crate::error::BRANCH_CLOSING_MARKER), "{late}");
        assert!(!drains.held_by_this_thread(7));
        // A second close queues behind the first (and is counted).
        let second = {
            let drains = Arc::clone(&drains);
            std::thread::spawn(move || drains.close(7, "b", BRANCH_DRAIN_TIMEOUT).map(drop).is_ok())
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while drains.state(7).waiting_closers != 1 {
            assert!(std::time::Instant::now() < deadline, "the second close never queued");
            std::thread::yield_now();
        }
        release_tx.send(()).unwrap();
        closer.join().unwrap();
        assert!(second.join().unwrap(), "the queued close runs after the first");
        assert_eq!(drains.state(7), BranchDrainState::default());
        drop(drains.enter(7, "b").unwrap());

        // A close that cannot drain in time gives up with 55006's marker.
        let stuck = drains.enter(9, "s").unwrap();
        let waiter = {
            let drains = Arc::clone(&drains);
            std::thread::spawn(move || drains.close(9, "s", std::time::Duration::from_millis(50)).err())
        };
        let refusal = waiter.join().unwrap().expect("timed out");
        assert!(
            refusal.to_string().contains(crate::error::BRANCH_BUSY_MARKER),
            "{refusal}"
        );
        drop(stuck);
        assert!(
            drains.close(9, "s", BRANCH_DRAIN_TIMEOUT).is_ok(),
            "a timed-out close leaves the branch open"
        );
    }
    use crate::Config;

    #[test]
    fn test_branch_key_encoding() {
        let key = encode_branch_data_key(1, "users:123", 100);
        let (branch_id, user_key) = decode_branch_data_key(&key).unwrap();

        assert_eq!(branch_id, 1);
        assert_eq!(user_key, "users:123");
    }

    #[test]
    fn test_create_branch_manager() {
        let config = Config::in_memory();
        let engine = StorageEngine::open_in_memory(&config).unwrap();

        let manager = BranchManager::new(
            Arc::clone(&engine.db),
            Arc::clone(engine.write_gate()),
            Arc::clone(&engine.timestamp),
        );

        assert!(manager.is_ok());
    }

    #[test]
    fn test_create_branch() {
        let config = Config::in_memory();
        let engine = StorageEngine::open_in_memory(&config).unwrap();

        let manager = BranchManager::new(
            Arc::clone(&engine.db),
            Arc::clone(engine.write_gate()),
            Arc::clone(&engine.timestamp),
        )
        .unwrap();

        let branch_id = manager
            .create_branch("dev", Some("main"), 100, BranchOptions::default())
            .unwrap();

        assert!(branch_id > 1); // main is 1

        // Verify metadata
        let metadata = manager.get_branch_by_name("dev").unwrap();
        assert_eq!(metadata.name, "dev");
        assert_eq!(metadata.branch_id, branch_id);
        assert_eq!(metadata.parent_id, Some(1));
        assert_eq!(metadata.state, BranchState::Active);
    }

    #[test]
    fn test_drop_branch() {
        let config = Config::in_memory();
        let engine = StorageEngine::open_in_memory(&config).unwrap();

        let manager = BranchManager::new(
            Arc::clone(&engine.db),
            Arc::clone(engine.write_gate()),
            Arc::clone(&engine.timestamp),
        )
        .unwrap();

        // Create and drop branch
        manager
            .create_branch("temp", Some("main"), 100, BranchOptions::default())
            .unwrap();

        manager.drop_branch("temp", false).unwrap();

        // Verify branch is dropped
        let result = manager.get_branch_by_name("temp");
        assert!(result.is_ok()); // Metadata still exists
        let metadata = result.unwrap();
        assert!(matches!(metadata.state, BranchState::Dropped { .. }));
    }

    #[test]
    fn test_cannot_drop_main() {
        let config = Config::in_memory();
        let engine = StorageEngine::open_in_memory(&config).unwrap();

        let manager = BranchManager::new(
            Arc::clone(&engine.db),
            Arc::clone(engine.write_gate()),
            Arc::clone(&engine.timestamp),
        )
        .unwrap();

        let result = manager.drop_branch("main", false);
        assert!(result.is_err());
    }

    #[test]
    fn test_cannot_drop_with_children() {
        let config = Config::in_memory();
        let engine = StorageEngine::open_in_memory(&config).unwrap();

        let manager = BranchManager::new(
            Arc::clone(&engine.db),
            Arc::clone(engine.write_gate()),
            Arc::clone(&engine.timestamp),
        )
        .unwrap();

        // Create parent and child
        manager
            .create_branch("parent", Some("main"), 100, BranchOptions::default())
            .unwrap();

        manager
            .create_branch("child", Some("parent"), 200, BranchOptions::default())
            .unwrap();

        // Try to drop parent
        let result = manager.drop_branch("parent", false);
        assert!(result.is_err());
    }

    #[test]
    fn test_list_branches() {
        let config = Config::in_memory();
        let engine = StorageEngine::open_in_memory(&config).unwrap();

        let manager = BranchManager::new(
            Arc::clone(&engine.db),
            Arc::clone(engine.write_gate()),
            Arc::clone(&engine.timestamp),
        )
        .unwrap();

        // Create branches
        manager
            .create_branch("dev", Some("main"), 100, BranchOptions::default())
            .unwrap();
        manager
            .create_branch("staging", Some("main"), 200, BranchOptions::default())
            .unwrap();

        let branches = manager.list_branches().unwrap();
        assert_eq!(branches.len(), 3); // main, dev, staging

        let names: Vec<_> = branches.iter().map(|b| b.name.as_str()).collect();
        assert!(names.contains(&"main"));
        assert!(names.contains(&"dev"));
        assert!(names.contains(&"staging"));
    }

    #[test]
    fn test_parent_chain() {
        let config = Config::in_memory();
        let engine = StorageEngine::open_in_memory(&config).unwrap();

        let manager = BranchManager::new(
            Arc::clone(&engine.db),
            Arc::clone(engine.write_gate()),
            Arc::clone(&engine.timestamp),
        )
        .unwrap();

        // Create hierarchy: main -> dev -> feature
        let dev_id = manager
            .create_branch("dev", Some("main"), 100, BranchOptions::default())
            .unwrap();

        let feature_id = manager
            .create_branch("feature", Some("dev"), 200, BranchOptions::default())
            .unwrap();

        // Build parent chain for feature
        let chain = manager.build_parent_chain(feature_id).unwrap();

        assert_eq!(chain.len(), 2); // dev, main
        assert_eq!(chain[0].0, dev_id);
        assert_eq!(chain[1].0, 1); // main
    }

    /// Sprinter 6bc4a4040eb7 (B4): a reclaim takes the level's keys in every family and
    /// no other level's: `1:` does not match `12:`. The open's walk does the same for
    /// a level that is gone, leaves a live one and a key that is no level's, and takes
    /// the stale queue of the GC this replaced.
    #[test]
    fn a_reclaim_takes_exactly_a_levels_keys() {
        let config = Config::in_memory();
        let engine = StorageEngine::open_in_memory(&config).unwrap();
        let db = Arc::clone(&engine.db);
        let manager = BranchManager::new(
            Arc::clone(&db),
            Arc::clone(engine.write_gate()),
            Arc::clone(&engine.timestamp),
        )
        .unwrap();
        let live = manager
            .create_branch("live", Some("main"), 100, BranchOptions::default())
            .unwrap();
        let keys = |id: u64| -> Vec<String> {
            LEVEL_FAMILIES
                .iter()
                .map(|family| format!("{family}{id}:table:{id}"))
                .collect()
        };
        for id in [live, 7, 70] {
            for key in keys(id) {
                db.put(key.as_bytes(), b"x").unwrap();
            }
        }
        let binary = encode_branch_data_key(7, "user_key", 0);
        db.put(&binary, b"x").unwrap();
        db.put(LEGACY_PENDING_GC_KEY, b"x").unwrap();

        assert_eq!(manager.reclaim_branch_keys(7).unwrap(), LEVEL_FAMILIES.len() as u64);
        for key in keys(7) {
            assert!(db.get(key.as_bytes()).unwrap().is_none(), "{key}");
        }
        for key in keys(70).into_iter().chain(keys(live)) {
            assert!(db.get(key.as_bytes()).unwrap().is_some(), "{key}");
        }

        assert_eq!(manager.reclaim_dead_levels().unwrap(), LEVEL_FAMILIES.len() as u64);
        for key in keys(70) {
            assert!(db.get(key.as_bytes()).unwrap().is_none(), "{key}: no such branch");
        }
        for key in keys(live) {
            assert!(db.get(key.as_bytes()).unwrap().is_some(), "{key}: an active branch");
        }
        assert!(db.get(&binary).unwrap().is_some(), "not a level's key");
        assert!(db.get(LEGACY_PENDING_GC_KEY).unwrap().is_none());
    }

    /// The id is durable before the branch: a later manager over the same store does
    /// not hand it out again, and the create takes the id it was given.
    #[test]
    fn a_reserved_id_is_not_handed_out_again() {
        let config = Config::in_memory();
        let engine = StorageEngine::open_in_memory(&config).unwrap();
        let new_manager = || {
            BranchManager::new(
                Arc::clone(&engine.db),
                Arc::clone(engine.write_gate()),
                Arc::clone(&engine.timestamp),
            )
            .unwrap()
        };
        let first = new_manager();
        let reserved = first.reserve_branch_id().unwrap();
        let second = new_manager();
        let other = second.reserve_branch_id().unwrap();
        assert!(other > reserved);
        let created = first
            .create_reserved_branch(reserved, "r", Some("main"), 1, BranchOptions::default(), |_, _| Ok(()))
            .unwrap();
        assert_eq!(created, reserved);
    }
}
