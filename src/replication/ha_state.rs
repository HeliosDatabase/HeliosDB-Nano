//! HA State Registry
//!
//! Process-global state of this node's replication role:
//! - its HA configuration and role (the system views read it);
//! - on a primary, the **standby registry**: every receiver session its
//!   replication port serves, with the physical sequences each standby has
//!   applied and made durable (spec P2.1 §2);
//! - the synchronous-replication wait a primary's commit path runs on the
//!   physical sequence of its write ([`HAStateRegistry::wait_for_sequence`]).
//!
//! # Why the registry is the commit path's
//!
//! Sync and semi-sync replication are only real if a commit waits for a
//! standby to hold the PHYSICAL sequence that covers it. The registry is
//! written by the replication sessions (`physical_source`, from each
//! standby's `Hello` and heartbeats) and read by the commit path, so both
//! name the same number: the RocksDB sequence of the primary's write. Both
//! recorded sequences only move forward (`fetch_max`): a late or stale
//! heartbeat can never regress the watermark a commit is waiting on — the
//! native v3 channel's heartbeats reset it to 0 every few seconds.

use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    Arc, RwLock,
};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use uuid::Uuid;

/// Global HA state registry instance
static HA_STATE: once_cell::sync::Lazy<HAStateRegistry> = once_cell::sync::Lazy::new(HAStateRegistry::new);

/// Get the global HA state registry
pub fn ha_state() -> &'static HAStateRegistry {
    &HA_STATE
}

/// HA Node Role
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HARole {
    Standalone,
    Primary,
    Standby,
}

impl HARole {
    pub fn as_str(&self) -> &'static str {
        match self {
            HARole::Standalone => "standalone",
            HARole::Primary => "primary",
            HARole::Standby => "standby",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "primary" => HARole::Primary,
            "standby" => HARole::Standby,
            _ => HARole::Standalone,
        }
    }
}

/// Sync mode for replication
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncMode {
    Async,
    SemiSync,
    Sync,
}

impl SyncMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            SyncMode::Async => "async",
            SyncMode::SemiSync => "semi-sync",
            SyncMode::Sync => "sync",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "semi-sync" | "semisync" => SyncMode::SemiSync,
            "sync" => SyncMode::Sync,
            _ => SyncMode::Async,
        }
    }
}

/// What a commit does when its standbys did not acknowledge it within
/// `[replication] sync_timeout_ms` (`sync_timeout_action`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncTimeoutAction {
    /// Commit with a WARN, then commit asynchronously until enough standbys
    /// acknowledge again (the default).
    Warn,
    /// Report the commit as failed to its client — it is already durable on
    /// this primary, like a cancelled synchronous commit in PostgreSQL.
    Fail,
}

impl SyncTimeoutAction {
    /// `warn` or `fail` (validated by `ReplicationConfig::validate`).
    pub fn from_str(value: &str) -> Self {
        if value.trim().eq_ignore_ascii_case("fail") {
            Self::Fail
        } else {
            Self::Warn
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Warn => "warn",
            Self::Fail => "fail",
        }
    }
}

/// Node configuration
#[derive(Debug, Clone)]
pub struct NodeConfig {
    pub node_id: Uuid,
    pub role: HARole,
    pub listen_addr: String,
    pub port: u16,
    pub replication_port: u16,
    pub sync_mode: SyncMode,
    pub primary_host: Option<String>,
    pub started_at: i64,
    /// `[replication] min_acks`: how many standbys must acknowledge a commit.
    pub min_acks: usize,
    /// `[replication] sync_timeout_ms`.
    pub sync_timeout: Duration,
    /// `[replication] sync_timeout_action`.
    pub sync_timeout_action: SyncTimeoutAction,
}

impl Default for NodeConfig {
    fn default() -> Self {
        Self {
            node_id: Uuid::new_v4(),
            role: HARole::Standalone,
            listen_addr: "127.0.0.1".to_string(),
            port: 5432,
            replication_port: 5433,
            sync_mode: SyncMode::Async,
            primary_host: None,
            started_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64,
            min_acks: 1,
            sync_timeout: Duration::from_secs(10),
            sync_timeout_action: SyncTimeoutAction::Warn,
        }
    }
}

/// A standby as the primary's registry reports it (a snapshot).
#[derive(Debug, Clone)]
pub struct StandbyInfo {
    pub node_id: Uuid,
    /// The peer address of its replication session.
    pub address: String,
    /// The dataset history its `Hello` named; `None` while it bootstraps.
    pub history: Option<Uuid>,
    /// Its PostgreSQL port (0 when it serves none).
    pub pg_port: u16,
    /// The sync mode it was configured with (informational: this primary's
    /// own sync mode decides what a commit waits for).
    pub sync_mode: SyncMode,
    /// The highest physical source sequence it reported applied.
    pub applied_seq: u64,
    /// The highest physical source sequence it reported durable.
    pub durable_seq: u64,
    /// Unix seconds when its (first live) session was admitted.
    pub connected_at: i64,
    /// Unix seconds of its last `Hello` or heartbeat.
    pub last_seen: i64,
    /// This primary's physical sequence minus `applied_seq`: write operations behind.
    pub lag_bytes: u64,
    /// Milliseconds since it last reported holding everything this primary
    /// had written at that moment (0 while it does).
    pub lag_ms: u64,
    pub state: StandbyState,
    /// Whether it counts toward `min_acks`: a serving standby following this
    /// primary, not a bootstrap copy or an offline `resync` receiver.
    pub counts_for_sync: bool,
    /// Spec P3.2 §1: where its last poll was served from — `"wal"` (this
    /// node's RocksDB WAL) or `"archive"` (its WAL archive, filling a gap the
    /// WAL no longer holds).
    pub source: &'static str,
}

/// Standby connection state
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StandbyState {
    /// Downloading its first copy (a snapshot session).
    Bootstrapping,
    /// Following this primary (a resumed session).
    Streaming,
}

impl StandbyState {
    pub fn as_str(&self) -> &'static str {
        match self {
            StandbyState::Bootstrapping => "bootstrapping",
            StandbyState::Streaming => "streaming",
        }
    }
}

/// What a replication session registers when it admits a standby's `Hello`.
#[derive(Debug, Clone)]
pub struct StandbyRegistration {
    pub node_id: Uuid,
    pub address: String,
    pub history: Option<Uuid>,
    pub pg_port: u16,
    pub sync_mode: SyncMode,
    /// The durable cursor the standby opened its session at.
    pub cursor: Option<u64>,
    pub state: StandbyState,
    pub counts_for_sync: bool,
}

/// One registered standby. The sequences are atomics so a heartbeat records
/// without the registry's write lock, and only ever move forward.
#[derive(Debug)]
struct StandbyEntry {
    node_id: Uuid,
    address: String,
    history: Option<Uuid>,
    pg_port: u16,
    sync_mode: SyncMode,
    state: StandbyState,
    counts_for_sync: bool,
    connected_at: i64,
    applied: AtomicU64,
    durable: AtomicU64,
    /// Unix milliseconds.
    last_seen: AtomicU64,
    /// Unix milliseconds of the last ack at or past the primary's sequence.
    caught_up_at: AtomicU64,
    /// Live sessions of this entry (a reconnect can overlap the session it replaces).
    sessions: AtomicUsize,
    /// Spec P3.2 §1: its last poll was served from the WAL archive.
    from_archive: AtomicBool,
}

impl StandbyEntry {
    fn applied(&self) -> u64 {
        self.applied.load(Ordering::Acquire)
    }

    fn durable(&self) -> u64 {
        self.durable.load(Ordering::Acquire)
    }
}

/// The primary's sequence source, for lag and the views.
pub type SequenceSource = Arc<dyn Fn() -> u64 + Send + Sync>;

/// The standby registry and the notification every waiting commit sleeps
/// on. Shared by its [`HAStateRegistry`] and every [`StandbyHandle`], so a
/// handle reports to the registry that issued it.
struct StandbyRegistry {
    standbys: parking_lot::RwLock<HashMap<Uuid, Arc<StandbyEntry>>>,
    sequence_source: parking_lot::RwLock<Option<SequenceSource>>,
    /// Bumped on every acknowledgement and registry change, under this lock,
    /// so a waiter cannot miss the notification it waits for.
    generation: parking_lot::Mutex<u64>,
    changed: parking_lot::Condvar,
}

impl StandbyRegistry {
    fn primary_sequence(&self) -> u64 {
        self.sequence_source.read().as_ref().map_or(0, |source| source())
    }

    fn record_ack(&self, entry: &StandbyEntry, applied: u64, durable: u64) {
        let now = now_millis();
        entry.applied.fetch_max(applied, Ordering::AcqRel);
        entry.durable.fetch_max(durable, Ordering::AcqRel);
        entry.last_seen.fetch_max(now, Ordering::AcqRel);
        if entry.applied() >= self.primary_sequence() {
            entry.caught_up_at.fetch_max(now, Ordering::AcqRel);
        }
        self.notify();
    }

    fn end_session(&self, entry: &Arc<StandbyEntry>) {
        {
            // Under the lock `register_standby` joins entries under, so a
            // session joining this entry cannot race its removal.
            let mut standbys = self.standbys.write();
            if entry.sessions.fetch_sub(1, Ordering::AcqRel) == 1
                && standbys
                    .get(&entry.node_id)
                    .is_some_and(|registered| Arc::ptr_eq(registered, entry))
            {
                standbys.remove(&entry.node_id);
            }
        }
        self.notify();
    }

    fn notify(&self) {
        *self.generation.lock() += 1;
        self.changed.notify_all();
    }
}

/// A registered standby session. Dropping it ends the session's
/// registration; the standby leaves the registry with its last session.
pub struct StandbyHandle {
    registry: Arc<StandbyRegistry>,
    entry: Arc<StandbyEntry>,
}

impl std::fmt::Debug for StandbyHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StandbyHandle")
            .field("entry", &self.entry)
            .finish_non_exhaustive()
    }
}

impl StandbyHandle {
    pub fn node_id(&self) -> Uuid {
        self.entry.node_id
    }

    /// Record an acknowledgement (a heartbeat). Monotonic: values below what
    /// was already recorded change nothing, and the durable sequence is never
    /// recorded above the applied one.
    pub fn record_ack(&self, applied: u64, durable: u64) {
        self.registry.record_ack(&self.entry, applied, durable.min(applied));
    }

    /// Spec P3.2 §1: the session's last poll was served from the WAL archive
    /// (`true`) or from this node's own WAL (`false`).
    pub fn note_source(&self, from_archive: bool) {
        self.entry.from_archive.store(from_archive, Ordering::Release);
    }
}

impl Drop for StandbyHandle {
    fn drop(&mut self) {
        self.registry.end_session(&self.entry);
    }
}

/// Primary connection information (for standbys)
#[derive(Debug, Clone)]
pub struct PrimaryInfo {
    pub node_id: Uuid,
    pub address: String,
    pub connected_at: i64,
    pub last_heartbeat: i64,
    pub primary_lsn: u64,
    pub local_lsn: u64,
    pub lag_bytes: u64,
    pub lag_ms: u64,
    pub fencing_token: u64,
    pub state: PrimaryConnectionState,
}

/// Primary connection state
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrimaryConnectionState {
    Disconnected,
    Connecting,
    Connected,
    Streaming,
    Error,
}

impl PrimaryConnectionState {
    pub fn as_str(&self) -> &'static str {
        match self {
            PrimaryConnectionState::Disconnected => "disconnected",
            PrimaryConnectionState::Connecting => "connecting",
            PrimaryConnectionState::Connected => "connected",
            PrimaryConnectionState::Streaming => "streaming",
            PrimaryConnectionState::Error => "error",
        }
    }
}

/// Replication metrics
#[derive(Debug, Clone, Default)]
pub struct ReplicationMetrics {
    pub wal_writes: u64,
    pub wal_bytes_written: u64,
    pub records_replicated: u64,
    pub bytes_replicated: u64,
    pub heartbeats_sent: u64,
    pub heartbeats_received: u64,
    pub reconnect_count: u64,
    pub last_wal_write: Option<i64>,
    pub last_replication: Option<i64>,
}

/// A commit its standbys did not acknowledge within the timeout, under
/// `sync_timeout_action = "fail"`. The commit is durable on this primary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncUnconfirmed {
    pub sequence: u64,
    pub acknowledged: usize,
    pub min_acks: usize,
    pub mode: SyncMode,
    pub timeout: Duration,
}

impl std::fmt::Display for SyncUnconfirmed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}: {} of {} required standbys acknowledged physical sequence {} within {} ms ({}); the commit is \
             durable on this primary but may not be on any standby",
            crate::error::SYNC_REPLICATION_UNCONFIRMED,
            self.acknowledged,
            self.min_acks,
            self.sequence,
            self.timeout.as_millis(),
            self.mode.as_str()
        )
    }
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
}

/// HA State Registry
pub struct HAStateRegistry {
    config: RwLock<NodeConfig>,
    registry: Arc<StandbyRegistry>,
    primary: RwLock<Option<PrimaryInfo>>,
    metrics: RwLock<ReplicationMetrics>,
    current_lsn: RwLock<u64>,
    is_read_only: RwLock<bool>,
    /// Set for a guarded physical standby process ([`Self::disable_forwarding`]).
    forwarding_disabled: AtomicBool,
    /// This primary's dataset history: semi-sync counts only standbys of it.
    history: parking_lot::RwLock<Option<Uuid>>,
    /// Set once this primary serves its replication port: a primary that
    /// serves none has no standby to wait for, whatever its sync mode says.
    serving: AtomicBool,
    /// `warn` mode only: the sequence whose wait timed out, while commits
    /// run asynchronously until enough standbys acknowledge it (0 = none).
    degraded_at: AtomicU64,
}

impl HAStateRegistry {
    /// Create a new HA state registry
    pub fn new() -> Self {
        Self {
            config: RwLock::new(NodeConfig::default()),
            registry: Arc::new(StandbyRegistry {
                standbys: parking_lot::RwLock::new(HashMap::new()),
                sequence_source: parking_lot::RwLock::new(None),
                generation: parking_lot::Mutex::new(0),
                changed: parking_lot::Condvar::new(),
            }),
            primary: RwLock::new(None),
            metrics: RwLock::new(ReplicationMetrics::default()),
            current_lsn: RwLock::new(0),
            is_read_only: RwLock::new(false),
            forwarding_disabled: AtomicBool::new(false),
            history: parking_lot::RwLock::new(None),
            serving: AtomicBool::new(false),
            degraded_at: AtomicU64::new(0),
        }
    }

    // ========== Configuration ==========

    /// Set node configuration
    pub fn set_config(&self, config: NodeConfig) {
        // Standbys should be read-only by default
        let read_only = config.role == HARole::Standby;

        if let Ok(mut cfg) = self.config.write() {
            *cfg = config;
        }
        if let Ok(mut ro) = self.is_read_only.write() {
            *ro = read_only;
        }
        self.degraded_at.store(0, Ordering::Release);
    }

    /// Mark this process a guarded physical standby, permanently: every
    /// PostgreSQL session is a standby session whose writes are refused
    /// (25006) in every posture. Role/read-only changes must not silently undo
    /// this restriction. (The name predates spec P2.1, which removed the write
    /// forwarding it used to switch off; no write is ever forwarded now.)
    /// The one sanctioned undo is [`Self::enable_forwarding_after_promotion`].
    pub fn disable_forwarding(&self) {
        self.forwarding_disabled.store(true, Ordering::Release);
    }

    /// Spec P5.2 §3 step 7: THE one sanctioned undo of
    /// [`Self::disable_forwarding`] — called once, by the online promotion,
    /// after this process's store was converted and reopened as the
    /// primary's and its replication port serves it. From here on its
    /// PostgreSQL sessions are a primary's. Nothing else may lift the guard:
    /// a role or read-only change never does. Logged.
    pub fn enable_forwarding_after_promotion(&self) {
        if self.forwarding_disabled.swap(false, Ordering::AcqRel) {
            tracing::warn!(
                "replication: this process was promoted to primary: the standby guard of its PostgreSQL sessions \
                 is lifted (the one sanctioned undo of the standby's write guard)"
            );
        }
    }

    pub fn forwarding_disabled(&self) -> bool {
        self.forwarding_disabled.load(Ordering::Acquire)
    }

    /// Get node configuration
    pub fn get_config(&self) -> Option<NodeConfig> {
        self.config.read().ok().map(|c| c.clone())
    }

    /// Get current role
    pub fn get_role(&self) -> HARole {
        self.config.read().map(|c| c.role).unwrap_or(HARole::Standalone)
    }

    /// Check if node is read-only (standbys are read-only by default)
    pub fn is_read_only(&self) -> bool {
        self.is_read_only.read().map(|r| *r).unwrap_or(false)
    }

    /// Set read-only mode explicitly
    pub fn set_read_only(&self, read_only: bool) {
        if let Ok(mut ro) = self.is_read_only.write() {
            *ro = read_only;
        }
    }

    /// This primary's dataset history (semi-sync counts only its standbys).
    pub fn set_history(&self, history: Option<Uuid>) {
        *self.history.write() = history;
    }

    pub fn history(&self) -> Option<Uuid> {
        *self.history.read()
    }

    /// Where this primary's physical sequence is read (lag, views).
    pub fn set_sequence_source(&self, source: Option<SequenceSource>) {
        *self.registry.sequence_source.write() = source;
    }

    /// Whether this primary serves its replication port. Only then does a
    /// commit wait for standbys: one that serves none has nobody to wait for.
    pub fn set_replication_serving(&self, serving: bool) {
        self.serving.store(serving, Ordering::Release);
        self.registry.notify();
    }

    /// Whether a commit on this node may have to wait for standbys: a
    /// primary that serves its replication port in sync or semi-sync mode.
    /// The commit path asks this before reading its physical sequence.
    pub fn sync_wait_enabled(&self) -> bool {
        self.serving.load(Ordering::Acquire)
            && self
                .config
                .read()
                .map(|config| config.role == HARole::Primary && config.sync_mode != SyncMode::Async)
                .unwrap_or(false)
    }

    // ========== Standby registry (primary role) ==========

    /// Register a standby session (its `Hello` was admitted). A session of a
    /// standby already registered under the same node id and history joins
    /// its entry — the recorded sequences only move forward; a session with
    /// another history (a rebuilt standby) replaces it.
    pub fn register_standby(&self, registration: StandbyRegistration) -> StandbyHandle {
        let now = now_millis();
        let cursor = registration.cursor.unwrap_or(0);
        let entry = {
            let mut standbys = self.registry.standbys.write();
            match standbys.get(&registration.node_id) {
                Some(existing)
                    if existing.history == registration.history
                        && existing.state == registration.state
                        && existing.counts_for_sync == registration.counts_for_sync =>
                {
                    existing.sessions.fetch_add(1, Ordering::AcqRel);
                    Arc::clone(existing)
                }
                _ => {
                    let entry = Arc::new(StandbyEntry {
                        node_id: registration.node_id,
                        address: registration.address,
                        history: registration.history,
                        pg_port: registration.pg_port,
                        sync_mode: registration.sync_mode,
                        state: registration.state,
                        counts_for_sync: registration.counts_for_sync,
                        connected_at: (now / 1000) as i64,
                        applied: AtomicU64::new(0),
                        durable: AtomicU64::new(0),
                        last_seen: AtomicU64::new(now),
                        caught_up_at: AtomicU64::new(now),
                        sessions: AtomicUsize::new(1),
                        from_archive: AtomicBool::new(false),
                    });
                    standbys.insert(registration.node_id, Arc::clone(&entry));
                    entry
                }
            }
        };
        // The cursor a standby opens at is durable on it: its first ack.
        self.registry.record_ack(&entry, cursor, cursor);
        StandbyHandle {
            registry: Arc::clone(&self.registry),
            entry,
        }
    }

    /// Get all standbys
    pub fn get_standbys(&self) -> Vec<StandbyInfo> {
        let primary = self.registry.primary_sequence();
        let now = now_millis();
        let mut standbys: Vec<StandbyInfo> = self
            .registry
            .standbys
            .read()
            .values()
            .map(|entry| {
                let applied = entry.applied();
                StandbyInfo {
                    node_id: entry.node_id,
                    address: entry.address.clone(),
                    history: entry.history,
                    pg_port: entry.pg_port,
                    sync_mode: entry.sync_mode,
                    applied_seq: applied,
                    durable_seq: entry.durable(),
                    connected_at: entry.connected_at,
                    last_seen: (entry.last_seen.load(Ordering::Acquire) / 1000) as i64,
                    lag_bytes: primary.saturating_sub(applied),
                    lag_ms: if applied >= primary {
                        0
                    } else {
                        now.saturating_sub(entry.caught_up_at.load(Ordering::Acquire))
                    },
                    state: entry.state,
                    counts_for_sync: entry.counts_for_sync,
                    source: if entry.from_archive.load(Ordering::Acquire) {
                        "archive"
                    } else {
                        "wal"
                    },
                }
            })
            .collect();
        standbys.sort_by_key(|standby| standby.node_id);
        standbys
    }

    /// Get standby count
    pub fn standby_count(&self) -> usize {
        self.registry.standbys.read().len()
    }

    /// How many standbys acknowledged `sequence` the way `mode` requires.
    fn acknowledgements(&self, mode: SyncMode, sequence: u64) -> usize {
        let history = self.history();
        self.registry
            .standbys
            .read()
            .values()
            .filter(|entry| {
                // Only a serving standby that follows THIS dataset counts: a
                // bootstrap copy or an offline receiver serves nobody.
                let ours = match history {
                    Some(history) => entry.history == Some(history),
                    None => entry.history.is_some(),
                };
                entry.counts_for_sync
                    && ours
                    && match mode {
                        SyncMode::SemiSync => entry.durable() >= sequence,
                        SyncMode::Sync => entry.applied() >= sequence,
                        SyncMode::Async => true,
                    }
            })
            .count()
    }

    /// Spec P2.1 §2: block (bounded) until enough standbys hold `sequence`,
    /// the physical sequence the caller read right after its write.
    ///
    /// * `semi-sync`: `min_acks` standbys of this dataset report it durable;
    /// * `sync`: `min_acks` standbys report it applied (visible to their readers);
    /// * `async`, a node that is not a primary, or a primary that serves no
    ///   replication port: returns at once.
    ///
    /// After `sync_timeout_ms` the commit returns. `warn`: Ok, with a WARN,
    /// and later commits run asynchronously until `min_acks` standbys
    /// acknowledge the sequence that timed out (so a primary whose standby is
    /// gone pays the timeout once, not on every commit). `fail`: the
    /// [`SyncUnconfirmed`] error, for a commit that is durable on this primary.
    pub fn wait_for_sequence(&self, sequence: u64) -> std::result::Result<(), SyncUnconfirmed> {
        if !self.serving.load(Ordering::Acquire) {
            return Ok(());
        }
        let Ok(config) = self.config.read().map(|config| config.clone()) else {
            return Ok(());
        };
        if config.role != HARole::Primary || config.sync_mode == SyncMode::Async {
            return Ok(());
        }
        let min_acks = config.min_acks.max(1);
        if config.sync_timeout_action == SyncTimeoutAction::Warn {
            let degraded_at = self.degraded_at.load(Ordering::Acquire);
            if degraded_at != 0 {
                if self.acknowledgements(config.sync_mode, degraded_at) < min_acks {
                    return Ok(());
                }
                if self
                    .degraded_at
                    .compare_exchange(degraded_at, 0, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
                {
                    tracing::info!(
                        "synchronous replication resumed: {min_acks} standby(s) acknowledged physical sequence \
                         {degraded_at} again"
                    );
                }
            }
        }
        let deadline = Instant::now() + config.sync_timeout;
        let mut generation = self.registry.generation.lock();
        let acknowledged = loop {
            let acknowledged = self.acknowledgements(config.sync_mode, sequence);
            if acknowledged >= min_acks {
                return Ok(());
            }
            if Instant::now() >= deadline || !self.serving.load(Ordering::Acquire) {
                break acknowledged;
            }
            // Re-checked under the lock every acknowledgement takes.
            let _ = self.registry.changed.wait_until(&mut generation, deadline);
        };
        drop(generation);
        if !self.serving.load(Ordering::Acquire) {
            return Ok(());
        }
        let unconfirmed = SyncUnconfirmed {
            sequence,
            acknowledged,
            min_acks,
            mode: config.sync_mode,
            timeout: config.sync_timeout,
        };
        match config.sync_timeout_action {
            SyncTimeoutAction::Warn => {
                if self
                    .degraded_at
                    .compare_exchange(0, sequence.max(1), Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
                {
                    tracing::warn!(
                        "{unconfirmed}; sync_timeout_action = \"warn\": the commit returns, and commits run \
                         asynchronously until {min_acks} standby(s) acknowledge physical sequence {sequence}"
                    );
                }
                Ok(())
            }
            SyncTimeoutAction::Fail => Err(unconfirmed),
        }
    }

    // ========== Primary Connection (Standby Role) ==========

    /// Set primary connection info
    pub fn set_primary(&self, info: PrimaryInfo) {
        if let Ok(mut primary) = self.primary.write() {
            *primary = Some(info);
        }
    }

    /// Update primary connection
    pub fn update_primary(&self, update: impl FnOnce(&mut PrimaryInfo)) {
        if let Ok(mut primary) = self.primary.write() {
            if let Some(ref mut p) = *primary {
                update(p);
            }
        }
    }

    /// Clear primary connection
    pub fn clear_primary(&self) {
        if let Ok(mut primary) = self.primary.write() {
            *primary = None;
        }
    }

    /// Get primary connection info
    pub fn get_primary(&self) -> Option<PrimaryInfo> {
        self.primary.read().ok().and_then(|p| p.clone())
    }

    // ========== LSN Management ==========

    /// Set current LSN
    pub fn set_lsn(&self, lsn: u64) {
        if let Ok(mut current) = self.current_lsn.write() {
            *current = lsn;
        }
    }

    /// The node's position: on a primary serving its replication port, its
    /// physical sequence; otherwise the value last set.
    pub fn get_lsn(&self) -> u64 {
        if let Some(source) = self.registry.sequence_source.read().as_ref() {
            return source();
        }
        self.current_lsn.read().map(|l| *l).unwrap_or(0)
    }

    /// Increment LSN and return new value
    pub fn increment_lsn(&self) -> u64 {
        if let Ok(mut current) = self.current_lsn.write() {
            *current += 1;
            *current
        } else {
            0
        }
    }

    // ========== Metrics ==========

    /// Get replication metrics
    pub fn get_metrics(&self) -> ReplicationMetrics {
        self.metrics.read().map(|m| m.clone()).unwrap_or_default()
    }

    /// Update metrics
    pub fn update_metrics(&self, update: impl FnOnce(&mut ReplicationMetrics)) {
        if let Ok(mut metrics) = self.metrics.write() {
            update(&mut metrics);
        }
    }

    /// Record WAL write
    pub fn record_wal_write(&self, bytes: u64) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        self.update_metrics(|m| {
            m.wal_writes += 1;
            m.wal_bytes_written += bytes;
            m.last_wal_write = Some(now);
        });
    }

    /// Record replication
    pub fn record_replication(&self, records: u64, bytes: u64) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        self.update_metrics(|m| {
            m.records_replicated += records;
            m.bytes_replicated += bytes;
            m.last_replication = Some(now);
        });
    }

    /// Record heartbeat sent
    pub fn record_heartbeat_sent(&self) {
        self.update_metrics(|m| m.heartbeats_sent += 1);
    }

    /// Record heartbeat received
    pub fn record_heartbeat_received(&self) {
        self.update_metrics(|m| m.heartbeats_received += 1);
    }

    /// Record reconnect
    pub fn record_reconnect(&self) {
        self.update_metrics(|m| m.reconnect_count += 1);
    }
}

impl Default for HAStateRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    // Every test builds its own registry: a test that armed the process-global
    // `ha_state()` as a primary would make every commit of the tests running
    // beside it wait for standbys (see `tests/txn_replication_sync.rs`).

    fn registration(node_id: Uuid, history: Option<Uuid>, cursor: Option<u64>) -> StandbyRegistration {
        StandbyRegistration {
            node_id,
            address: "127.0.0.1:1".to_string(),
            history,
            pg_port: 5432,
            sync_mode: SyncMode::Async,
            cursor,
            state: StandbyState::Streaming,
            counts_for_sync: true,
        }
    }

    fn standby(registry: &HAStateRegistry, node_id: Uuid) -> Option<StandbyInfo> {
        registry
            .get_standbys()
            .into_iter()
            .find(|standby| standby.node_id == node_id)
    }

    fn armed(
        mode: SyncMode,
        min_acks: usize,
        timeout_ms: u64,
        action: SyncTimeoutAction,
        history: Uuid,
    ) -> HAStateRegistry {
        let registry = HAStateRegistry::new();
        rearm(&registry, mode, min_acks, timeout_ms, action);
        registry.set_history(Some(history));
        registry.set_replication_serving(true);
        registry
    }

    fn rearm(registry: &HAStateRegistry, mode: SyncMode, min_acks: usize, timeout_ms: u64, action: SyncTimeoutAction) {
        registry.set_config(NodeConfig {
            role: HARole::Primary,
            sync_mode: mode,
            min_acks,
            sync_timeout: Duration::from_millis(timeout_ms),
            sync_timeout_action: action,
            ..Default::default()
        });
    }

    #[test]
    fn test_ha_state_registry() {
        let registry = HAStateRegistry::new();

        // Test default config
        let config = registry.get_config().unwrap();
        assert_eq!(config.role, HARole::Standalone);
        assert_eq!(config.min_acks, 1);
        assert_eq!(config.sync_timeout, Duration::from_secs(10));
        assert_eq!(config.sync_timeout_action, SyncTimeoutAction::Warn);

        // Test setting config
        registry.set_config(NodeConfig {
            role: HARole::Primary,
            ..Default::default()
        });
        assert_eq!(registry.get_role(), HARole::Primary);

        // Test LSN
        assert_eq!(registry.get_lsn(), 0);
        registry.set_lsn(100);
        assert_eq!(registry.get_lsn(), 100);
        assert_eq!(registry.increment_lsn(), 101);
        registry.set_sequence_source(Some(Arc::new(|| 7)));
        assert_eq!(registry.get_lsn(), 7, "a serving primary reports its physical sequence");
        assert_eq!(SyncTimeoutAction::from_str(" FAIL "), SyncTimeoutAction::Fail);
        assert_eq!(SyncTimeoutAction::from_str("warn"), SyncTimeoutAction::Warn);
    }

    /// Spec P2.1: the registry's sequences only move forward. A late or stale
    /// heartbeat — the native v3 channel's reported 0 every few seconds — can
    /// never regress what a commit is waiting on.
    #[test]
    fn registry_sequences_are_monotonic_and_durable_never_exceeds_applied() {
        let registry = HAStateRegistry::new();
        let primary = Arc::new(AtomicU64::new(30));
        let source = Arc::clone(&primary);
        registry.set_sequence_source(Some(Arc::new(move || source.load(Ordering::Acquire))));
        let node = Uuid::new_v4();
        let history = Some(Uuid::new_v4());
        let handle = registry.register_standby(registration(node, history, Some(10)));
        let info = standby(&registry, node).unwrap();
        assert_eq!(
            (info.applied_seq, info.durable_seq),
            (10, 10),
            "the session's cursor is its first ack"
        );
        assert_eq!(info.lag_bytes, 20);
        handle.record_ack(20, 15);
        handle.record_ack(0, 0);
        handle.record_ack(12, 12);
        let info = standby(&registry, node).unwrap();
        assert_eq!((info.applied_seq, info.durable_seq), (20, 15));
        handle.record_ack(25, 99);
        let info = standby(&registry, node).unwrap();
        assert_eq!(
            (info.applied_seq, info.durable_seq),
            (25, 25),
            "durable is recorded at most at applied"
        );
        handle.record_ack(30, 30);
        assert_eq!(standby(&registry, node).unwrap().lag_bytes, 0);
        assert_eq!(standby(&registry, node).unwrap().lag_ms, 0);
        // A second session of the same standby (a reconnect overlapping the
        // first) joins the entry; its older cursor moves nothing back.
        let second = registry.register_standby(registration(node, history, Some(5)));
        assert_eq!(standby(&registry, node).unwrap().applied_seq, 30);
        assert_eq!(registry.standby_count(), 1);
        drop(handle);
        assert_eq!(
            standby(&registry, node).unwrap().applied_seq,
            30,
            "the entry lives while a session does"
        );
        drop(second);
        assert!(standby(&registry, node).is_none());
        // A rebuilt standby (another history) under the same id starts over.
        let old = registry.register_standby(registration(node, history, Some(50)));
        let rebuilt = registry.register_standby(registration(node, Some(Uuid::new_v4()), Some(3)));
        assert_eq!(standby(&registry, node).unwrap().applied_seq, 3);
        drop(old);
        assert_eq!(
            standby(&registry, node).unwrap().applied_seq,
            3,
            "the replaced entry's last session leaves the new one in place"
        );
        drop(rebuilt);
        assert_eq!(registry.standby_count(), 0);
    }

    #[test]
    fn semi_sync_waits_for_durable_and_sync_for_applied_from_min_acks_standbys_of_this_dataset() {
        let history = Uuid::new_v4();
        let registry = Arc::new(armed(SyncMode::SemiSync, 1, 200, SyncTimeoutAction::Fail, history));
        assert!(registry.sync_wait_enabled());
        let other = registry.register_standby(registration(Uuid::new_v4(), Some(Uuid::new_v4()), Some(100)));
        let bootstrap = registry.register_standby(StandbyRegistration {
            state: StandbyState::Bootstrapping,
            counts_for_sync: false,
            ..registration(Uuid::new_v4(), Some(history), Some(100))
        });
        let ours = registry.register_standby(registration(Uuid::new_v4(), Some(history), Some(5)));
        ours.record_ack(30, 20);
        assert!(registry.wait_for_sequence(20).is_ok());
        let failed = registry.wait_for_sequence(21).unwrap_err();
        assert_eq!(
            (failed.acknowledged, failed.min_acks),
            (0, 1),
            "another dataset's standby and a bootstrap copy never count"
        );
        rearm(&registry, SyncMode::Sync, 1, 200, SyncTimeoutAction::Fail);
        assert!(registry.wait_for_sequence(30).is_ok(), "sync counts applied");
        assert!(registry.wait_for_sequence(31).is_err());
        // An acknowledgement that arrives while the commit waits releases it.
        rearm(&registry, SyncMode::SemiSync, 1, 5_000, SyncTimeoutAction::Fail);
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            ours.record_ack(40, 40);
            ours
        });
        let started = Instant::now();
        assert!(registry.wait_for_sequence(40).is_ok());
        assert!(started.elapsed() < Duration::from_secs(4), "{:?}", started.elapsed());
        let ours = releaser.join().unwrap();
        drop((ours, other, bootstrap));
        registry.set_replication_serving(false);
        assert!(!registry.sync_wait_enabled());
        assert!(
            registry.wait_for_sequence(u64::MAX).is_ok(),
            "a primary that serves no replication port never waits"
        );
        registry.set_replication_serving(true);
        rearm(&registry, SyncMode::Async, 1, 5_000, SyncTimeoutAction::Fail);
        assert!(registry.wait_for_sequence(u64::MAX).is_ok(), "async never waits");
    }

    #[test]
    fn min_acks_shortfall_warns_once_then_runs_async_until_acknowledged_or_fails_every_time() {
        let history = Uuid::new_v4();
        let registry = armed(SyncMode::SemiSync, 2, 100, SyncTimeoutAction::Warn, history);
        let one = registry.register_standby(registration(Uuid::new_v4(), Some(history), Some(10)));
        let started = Instant::now();
        assert!(registry.wait_for_sequence(10).is_ok(), "warn commits");
        assert!(started.elapsed() >= Duration::from_millis(90), "it waited the timeout");
        let started = Instant::now();
        assert!(registry.wait_for_sequence(11).is_ok());
        assert!(
            started.elapsed() < Duration::from_millis(90),
            "degraded: later commits do not pay the timeout again"
        );
        // Two standbys hold the sequence that timed out: waits resume.
        let two = registry.register_standby(registration(Uuid::new_v4(), Some(history), Some(10)));
        let started = Instant::now();
        assert!(registry.wait_for_sequence(12).is_ok());
        assert!(started.elapsed() >= Duration::from_millis(90), "synchronous again");
        rearm(&registry, SyncMode::SemiSync, 2, 100, SyncTimeoutAction::Fail);
        drop(two);
        // `fail` never degrades: every short commit pays the timeout and fails.
        for (sequence, acknowledged) in [(10, 1), (10, 1), (11, 0)] {
            let failed = registry.wait_for_sequence(sequence).unwrap_err();
            assert_eq!((failed.acknowledged, failed.min_acks), (acknowledged, 2));
            assert!(failed.to_string().contains(crate::error::SYNC_REPLICATION_UNCONFIRMED));
        }
        drop(one);
    }

    #[test]
    fn guarded_forwarding_refusal_survives_role_and_read_only_changes() {
        let registry = HAStateRegistry::new();
        assert!(!registry.forwarding_disabled());
        registry.disable_forwarding();
        for role in [HARole::Standby, HARole::Primary, HARole::Standalone] {
            registry.set_config(NodeConfig {
                role,
                ..Default::default()
            });
            registry.set_read_only(false);
            assert!(registry.forwarding_disabled());
        }
        // Spec P5.2 §3 step 7: the promotion, and only it, lifts the guard.
        registry.enable_forwarding_after_promotion();
        assert!(!registry.forwarding_disabled());
        registry.enable_forwarding_after_promotion();
        assert!(!registry.forwarding_disabled(), "idempotent");
    }

    #[test]
    fn test_read_only_mode() {
        let registry = HAStateRegistry::new();

        // Standalone is not read-only by default
        registry.set_config(NodeConfig {
            role: HARole::Standalone,
            ..Default::default()
        });
        assert!(!registry.is_read_only());

        // Standby is read-only by default
        registry.set_config(NodeConfig {
            role: HARole::Standby,
            ..Default::default()
        });
        assert!(registry.is_read_only());

        // Primary is not read-only
        registry.set_config(NodeConfig {
            role: HARole::Primary,
            ..Default::default()
        });
        assert!(!registry.is_read_only());
    }
}
