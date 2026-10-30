//! Per-connection state the storage-less expression evaluator must be able to
//! READ AND WRITE in the middle of a statement — sprinter 6dc0cc115db9
//! (`LASTVAL()`) and sprinter f4f5d450e816 (`pg_backend_pid()` /
//! `application_name`).
//!
//! # Why this is not a field on [`Session`](super::Session)
//!
//! Every other piece of session state is *read* by the engine before the
//! statement runs (`Session::search_path`, `Session::login_identity`, the GH#28
//! timeout overrides) and reaches SQL through a per-statement thread-local the
//! engine installs. That shape does not work for these three:
//!
//! * `lastval` is **written** from inside statement execution — by the
//!   `nextval()` evaluator arm AND by the SERIAL/IDENTITY fill deep in the
//!   insert funnels — and the write must land on the *session*, not on a
//!   snapshot the guard took.
//! * `application_name` is written by `SET` on one statement and read by
//!   `SHOW` / `current_setting()` on the next, from an evaluator that holds no
//!   session handle.
//!
//! So the session owns an `Arc<SessionScopedState>` and the per-statement guard
//! installs a *clone of the handle* (one atomic increment) rather than a copy
//! of the values. Writes through it reach the session directly.
//!
//! # Why it is not a process global
//!
//! This is the single most important design constraint on sprinter
//! 6dc0cc115db9. A process-global "last nextval" would answer connection B with
//! connection A's row id — a `cursor.lastrowid` that silently names someone
//! else's row is strictly worse than the `None` it replaces. The same reasoning
//! rules out putting `application_name` in the process-global
//! `sql::settings::SessionSettings` registry, exactly as GH#28 kept the
//! connection-lifetime timeouts off it.
//!
//! Two later items moved more state in here for exactly the same reason:
//!
//! * sprinter 7903b7111cb4 — `currval('s')`. The sequence STORE stays process-
//!   wide (that is a durability decision, see `crate::sql::sequences`), but
//!   `currval`'s OBSERVABLE value is "what *this session's* last `nextval` on
//!   that sequence returned", so the per-sequence value is recorded here at the
//!   same point `lastval` is. One write serves both.
//! * sprinter a3077a3f68d8 — the USER-SETTABLE GUC overlay (`statement_timeout`,
//!   `work_mem`, `bulk_load_mode`, the planner switches …). `SET` on those used
//!   to write the ONE process-global `sql::settings::SessionSettings` registry,
//!   so `SET statement_timeout = 1` on any connection cancelled every other
//!   connection's queries. They are per-session in PostgreSQL and they are
//!   per-session here now; the registry keeps only genuine SERVER-level
//!   parameters (`sql::settings::is_server_level`).
//! * sprinter d03de7fc3b22 (the KEYSTONE) — the session's TENANT. `max_qps`
//!   metering and every RLS gate resolved their tenant from
//!   `TenantManager::current_context`, ONE `RwLock<Option<TenantContext>>`
//!   shared by every connection and thread in the process, whose only
//!   production writer was the REPL's `\tenant use`. So on a wire path the
//!   answer was always "no tenant" (nothing metered, nothing gated), and on the
//!   paths that DID set it the answer was one value for every concurrent
//!   connection. A tenant is a property of the CONNECTION — PostgreSQL resolves
//!   the database in `InitPostgres`, once, per backend — so it belongs here,
//!   next to `application_name` and the GUC overlay, for exactly the reasons
//!   above. See [`SessionScopedState::bind_tenant`].
//! * sprinter 6bc4a4040eb7 (phase B1) — the session's ACTIVE BRANCH. It was
//!   `StorageEngine.current_branch`, ONE `Mutex<Option<String>>` for the whole
//!   process, so one connection's `USE BRANCH` moved every other connection,
//!   the REST branch route ran on whatever branch some other caller had last
//!   selected, and an MCP call's switch-and-restore raced every concurrent
//!   call. A branch is selected per connection exactly like a database is, so
//!   it lives here, behind the same lock-plus-mirror shape as the tenant. See
//!   [`SessionScopedState::bind_branch`] and `StorageEngine::active_branch`,
//!   the one accessor every reader goes through.
//!
//! The one deliberately process-wide structure here is [`live_backends`] — the
//! registry `pg_stat_activity` scans. Listing every live connection IS that
//! view's purpose, and each entry is a `Weak` handle on the very state the
//! session holds, so the catalog can never disagree with what the connection
//! itself reports — and a closed connection drops out of it automatically.
//!
//! It is process-wide but it is NOT unscoped — sprinter 32ed4b9e0002. Every
//! entry carries the [`SessionScopedState::engine_instance`] it was minted for,
//! and [`snapshot_live_backends`] takes the scanning engine's id and returns
//! only ITS backends. Without that, the registry read like the module doc it
//! used to carry ("Nano has one database"): two open `EmbeddedDatabase`
//! handles in one process are two SERVERS, and database A's
//! `pg_stat_activity` listed database B's `usename`, `application_name`,
//! `client_addr` and `client_port` to a principal with no connection to B at
//! all. The id is `StorageEngine::instance_id()` — a per-construction counter,
//! never reused, deliberately not a pointer address (a recycled address would
//! let a new engine inherit a dead one's backends).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicI64, AtomicI8, AtomicU64, Ordering};
use std::sync::{Arc, Weak};

use dashmap::DashMap;

use crate::sql::SettingValue;

/// Sentinel stored in [`SessionScopedState::lastval`] for "no `nextval` has run
/// in this session". `i64::MIN` is not a value any sequence can serve (a
/// sequence's `min_value` is clamped well above it), so it cannot collide with
/// a legitimate id — and, unlike `0`, it can never be mistaken for one.
const LASTVAL_UNDEFINED: i64 = i64::MIN;

/// PostgreSQL's own message for `lastval()` before any `nextval()`
/// (`commands/sequence.c`), reported under SQLSTATE 55000
/// `object_not_in_prerequisite_state`.
///
/// Owned here, by the single emitter, so the wire layer's SQLSTATE mapping
/// (`protocol::postgres::handler::sqlstate_for_query_execution_message`) and
/// the message cannot drift — the same marker-const discipline GH#28 used for
/// `SET_LOCAL_TIMEOUT_GUC_UNSUPPORTED`.
pub const LASTVAL_UNDEFINED_MESSAGE: &str = "lastval is not yet defined in this session";

/// PostgreSQL's own message for `currval('s')` before this session has advanced
/// `s` (`commands/sequence.c`), reported under the same SQLSTATE 55000
/// `object_not_in_prerequisite_state` as [`LASTVAL_UNDEFINED_MESSAGE`]
/// — sprinter 7903b7111cb4.
///
/// The sequence NAME is interpolated, so the wire layer's SQLSTATE mapping
/// anchors on [`CURRVAL_UNDEFINED_PREFIX`] rather than on the whole string; the
/// message itself is still owned by the single emitter, so wording and code
/// cannot drift.
pub fn currval_undefined_message(name: &str) -> String {
    format!("{CURRVAL_UNDEFINED_PREFIX} \"{name}\" is not yet defined in this session")
}

/// The invariant prefix of [`currval_undefined_message`] — the marker the
/// SQLSTATE classifiers match on.
pub const CURRVAL_UNDEFINED_PREFIX: &str = "currval of sequence";

/// Sentinel stored in the `statement_timeout_ms` mirror for "this session has
/// not set `statement_timeout`". `0` cannot be the sentinel: `SET
/// statement_timeout = 0` is PostgreSQL for *unlimited*, and it must override a
/// configured server default rather than fall through to it.
const GUC_TIMEOUT_UNSET: i64 = -1;

/// Sentinel for the `bulk_load_mode` mirror: `-1` no override, `0` off, `1` on.
const GUC_TRISTATE_UNSET: i8 = -1;

/// Allocate the next backend pid. Starts at 1 and never repeats within the
/// process, which is all `pg_backend_pid()` promises: unique among LIVE
/// sessions and stable within one. It is deliberately NOT the OS pid — every
/// connection in this process would then report the same number, which is the
/// one thing a pool's affinity check must not see.
fn next_backend_pid() -> i32 {
    static COUNTER: AtomicI32 = AtomicI32::new(1);
    // Wrap back to 1 rather than overflowing (a 2-billion-connection process
    // would have recycled every earlier pid long ago).
    let pid = COUNTER.fetch_add(1, Ordering::Relaxed);
    if pid <= 0 {
        COUNTER.store(1, Ordering::Relaxed);
        return 1;
    }
    pid
}

/// `engine_instance` for a backend that belongs to no open database — sprinter
/// 32ed4b9e0002.
///
/// `StorageEngine::instance_id()` is minted from a counter that starts at 1, so
/// `0` can never name a real engine. It is what [`SessionScopedState::new`]
/// records, and it is reachable only from a [`super::SessionManager`] that was
/// built with [`super::SessionManager::new`] rather than
/// `new_for_engine` — i.e. from unit tests that exercise the manager without an
/// engine at all. Such a backend is visible to NOBODY's `pg_stat_activity`
/// (`0` matches no scanning engine), which is the fail-closed direction: a
/// backend whose database cannot be established must not be attributed to an
/// arbitrary one.
pub const UNATTACHED_ENGINE: u64 = 0;

/// The process-wide registry of LIVE backends, keyed by backend pid.
///
/// Populated by [`SessionScopedState::new`] and emptied by its `Drop`, so a
/// destroyed session leaves the view the moment its `Session` is dropped from
/// the `SessionManager` map. Entries are **`Weak`** on purpose: a strong `Arc`
/// here would be a reference the session's own `Drop` waits on, so the refcount
/// would never reach zero, `Drop` would never run, and every backend that ever
/// connected would stay in `pg_stat_activity` forever. The `Weak` also makes
/// the registry self-healing — an upgrade that fails is a backend that is gone.
///
/// A live entry still shares the SAME state the session holds, so
/// `pg_stat_activity.application_name` is the live value, never a stale copy.
pub fn live_backends() -> &'static DashMap<i32, Weak<SessionScopedState>> {
    static BACKENDS: std::sync::OnceLock<DashMap<i32, Weak<SessionScopedState>>> = std::sync::OnceLock::new();
    BACKENDS.get_or_init(DashMap::new)
}

/// A snapshot of one live backend, in `pg_stat_activity` column order.
#[derive(Debug, Clone)]
pub struct BackendSnapshot {
    pub pid: i32,
    pub username: String,
    pub application_name: String,
    pub client_addr: Option<String>,
    pub client_port: Option<i32>,
    pub backend_start: i64,
}

/// Snapshot the live backends of ONE open database, ordered by pid so the view
/// is deterministic — sprinter 32ed4b9e0002.
///
/// `engine_instance` is the scanning engine's `StorageEngine::instance_id()`.
/// Backends minted for any other engine are not rows: they belong to a
/// different open database, which in this process is a different SERVER, not a
/// different database of one cluster. PostgreSQL's `pg_stat_activity` is
/// cluster-wide precisely because one postmaster owns one shared-memory
/// backend array; it has no view into a second postmaster either, and neither
/// does this.
///
/// Within the returned set, every backend of the engine IS listed — PostgreSQL
/// does not filter the view by database, and hiding rows would break the
/// operator's whole use for it. What the caller does about the columns that
/// disclose another ROLE's activity is decided at the view
/// (`sql::phase3::system_views::execute_pg_stat_activity`), exactly as
/// `pg_stat_get_activity` decides it.
pub fn snapshot_live_backends(engine_instance: u64) -> Vec<BackendSnapshot> {
    let mut out: Vec<BackendSnapshot> = live_backends()
        .iter()
        .filter_map(|e| e.value().upgrade())
        .filter(|s| s.engine_instance() == engine_instance)
        .map(|s| s.snapshot())
        .collect();
    out.sort_by_key(|b| b.pid);
    out
}

/// sprinter 6bc4a4040eb7 (phase B1): the branch a session is bound to.
///
/// Built ONCE, when the binding is made (`USE BRANCH`, `SET heliosdb.branch`,
/// the PostgreSQL startup `database=<db>@<branch>` / `options=-c
/// heliosdb.branch=…`, a REST branch route, an MCP call that names a branch),
/// from a branch the registry reported ACTIVE at that moment
/// (`StorageEngine::resolve_active_branch`). Every reader shares it through an
/// `Arc`, so the per-row readers (`branch_aware_data_key`, the overlay scans)
/// cost one refcount and never re-resolve the name — the pre-B1 accessor
/// resolved `name -> id` through `get_branch_by_name` (a RocksDB read) on
/// EVERY call.
///
/// The ancestor `chain` is immutable for the life of a branch (a branch's
/// parent is fixed at creation and a branch with children cannot be dropped),
/// so caching it here is exact, not an approximation. Later phases extend each
/// level (B4 may add the level's fork anchor) without changing who builds or
/// reads the binding.
#[derive(Debug)]
pub struct BranchBinding {
    /// `StorageEngine::instance_id()` of the engine whose registry resolved
    /// the branch. A branch id names a branch of ONE database; a binding made
    /// against another open database in the same process is not a binding on
    /// this one (`StorageEngine::active_branch` compares it).
    pub engine_instance: u64,
    /// The branch name as the registry stores it.
    pub name: String,
    /// The branch id — what the overlay keys (`bdata:{id}:…`, `bdel:{id}:…`,
    /// `bv:{id}:…`) are written under. Never reused, so a binding to a
    /// dropped branch can never alias a newer branch that took its name.
    pub branch_id: u64,
    /// The overlay chain the branch reads through, OLDEST FIRST and ENDING
    /// WITH `branch_id` itself — exactly the order the overlay merge applies
    /// it (each level overrides the one before it). Main's `data:` keyspace is
    /// read before the chain and is not a level of it.
    pub chain: Vec<u64>,
    /// The engine's branch-registry generation this binding was last checked
    /// against (`StorageEngine::branch_registry_generation`). A DROP or MERGE
    /// of any branch bumps the generation; the next statement of a bound
    /// session then re-checks that ITS branch is still active, and refuses
    /// (3D000) when it is not. Unchanged generation = one atomic compare.
    pub validated_generation: AtomicU64,
}

impl BranchBinding {
    /// A binding to `name` (`branch_id`, reading through `chain`) on the
    /// engine `engine_instance`, validated at registry generation `generation`.
    pub fn new(engine_instance: u64, name: String, branch_id: u64, chain: Vec<u64>, generation: u64) -> Self {
        Self {
            engine_instance,
            name,
            branch_id,
            chain,
            validated_generation: AtomicU64::new(generation),
        }
    }

    /// The registry generation this binding was last validated at.
    #[inline]
    pub fn validated_generation(&self) -> u64 {
        self.validated_generation.load(Ordering::Acquire)
    }

    /// Record that the binding was re-validated at `generation`.
    #[inline]
    pub fn note_validated(&self, generation: u64) {
        self.validated_generation.store(generation, Ordering::Release);
    }

    /// The same binding — same engine, name, id and chain — stamped as never
    /// validated, so its first use re-checks the registry. What a reset to the
    /// connection's home branch installs: by ID, so a home branch that was
    /// dropped (and perhaps re-created under the same name, with a new id)
    /// fails closed instead of silently binding a different branch.
    pub fn unvalidated_copy(&self) -> Self {
        // Generations start at 1, so 0 never matches.
        Self::new(
            self.engine_instance,
            self.name.clone(),
            self.branch_id,
            self.chain.clone(),
            0,
        )
    }
}

/// Session-scoped state shared between the session and the statement currently
/// running on its behalf. See the module docs for why it is an `Arc` and not a
/// set of plain `Session` fields.
#[derive(Debug)]
pub struct SessionScopedState {
    /// `pg_backend_pid()`. Immutable for the life of the connection.
    backend_pid: i32,
    /// sprinter 32ed4b9e0002: the OPEN DATABASE this backend belongs to —
    /// `StorageEngine::instance_id()` of the engine whose `EmbeddedDatabase`
    /// minted it, or [`UNATTACHED_ENGINE`].
    ///
    /// Immutable, and set at construction rather than on first use on purpose:
    /// a backend that has authenticated but not yet run a statement is exactly
    /// the one `pg_stat_activity` is read to find, so it must already know
    /// which database it is on. It is a `u64` counter and NOT the engine's
    /// address — an `Arc` address is subject to ABA reuse, and a recycled one
    /// would let a new engine inherit a dead engine's backends.
    engine_instance: u64,
    /// Unix seconds at which this backend was created (`pg_stat_activity.backend_start`).
    backend_start: i64,
    /// The value most recently RETURNED by `nextval` in this session — whether
    /// the caller spelled it `nextval('s')` or let a SERIAL/IDENTITY column
    /// generate it. [`LASTVAL_UNDEFINED`] until the first one.
    ///
    /// Relaxed ordering throughout: a session is single-threaded with respect to
    /// its own statements (the wire handler runs one at a time), so there is no
    /// happens-before to establish; the atomic exists only so the evaluator can
    /// write through a shared `&`.
    lastval: AtomicI64,
    /// The session's `application_name` GUC. Empty string = PostgreSQL's
    /// default, which is a VALUE, not "unset" — `SHOW application_name` on a
    /// stock server prints an empty line, it does not error.
    application_name: parking_lot::RwLock<String>,
    /// `SET LOCAL application_name` support: the value to restore when the
    /// current transaction block ends, armed on the first `SET LOCAL` of the
    /// block and disarmed by [`Self::end_transaction`]. `None` = no `SET LOCAL`
    /// is in effect.
    application_name_saved: parking_lot::Mutex<Option<String>>,
    /// The login identity (`pg_stat_activity.usename`), republished by
    /// `Session::set_login` after authentication.
    username: parking_lot::RwLock<String>,
    /// Peer address, when the listener knows one (`None` on the embedded
    /// funnels and over an in-memory duplex stream).
    client_addr: parking_lot::RwLock<Option<String>>,
    client_port: AtomicI32,
    /// sprinter 7903b7111cb4: `sequence name -> the value THIS session's last
    /// `nextval` on it returned`. Absent = `currval('s')` raises 55000, exactly
    /// as PostgreSQL does, instead of answering another connection's id.
    ///
    /// The map — not a single slot — because `currval` names a sequence and
    /// must follow THAT one, while `lastval` (the slot above) follows whichever
    /// sequence moved last. One `nextval` writes both (see [`Self::note_nextval`]).
    seq_currval: parking_lot::RwLock<HashMap<String, i64>>,
    /// Lock-free mirror of `!seq_currval.is_empty()`, so a session that has
    /// never touched a sequence answers `currval` without taking the lock.
    seq_currval_present: AtomicBool,
    /// sprinter a3077a3f68d8: this session's USER-SETTABLE GUC overrides — the
    /// values `SET` installs and `SHOW` / `current_setting()` / the executor
    /// read back. Empty for every session that never issued one.
    ///
    /// This is the map that used to be `EmbeddedDatabase::session_settings`, a
    /// single process-global registry whose name was a lie: one connection's
    /// `SET statement_timeout = 1` cancelled every other connection's queries
    /// and one connection's `SET bulk_load_mode = on` flipped the storage
    /// engine's flag for the whole process.
    gucs: parking_lot::RwLock<HashMap<String, SettingValue>>,
    /// Lock-free mirror of `!gucs.is_empty()`. The long-tail GUC readers gate on
    /// this so an unmodified session never pays the map lock.
    guc_overrides_present: AtomicBool,
    /// Lock-free mirror of the `statement_timeout` override in milliseconds
    /// ([`GUC_TIMEOUT_UNSET`] = none). `EmbeddedDatabase::effective_statement_timeout_ms`
    /// runs on EVERY executor construction, so it must not lowercase a name into
    /// a fresh `String` and take a lock the way the old registry read did — this
    /// mirror makes that read one relaxed atomic load, which is strictly cheaper
    /// than what it replaces.
    statement_timeout_ms: AtomicI64,
    /// Lock-free mirror of the `bulk_load_mode` override
    /// ([`GUC_TRISTATE_UNSET`] = none, `0` off, `1` on). Read by
    /// `StorageEngine::is_bulk_load_mode` on per-row insert paths, so it has to
    /// be exactly this cheap.
    bulk_load_mode: AtomicI8,
    /// sprinter d03de7fc3b22 (the KEYSTONE): the tenant THIS connection is
    /// bound to, resolved once from the database name the client asked for (see
    /// `EmbeddedDatabase::bind_session_tenant`).
    ///
    /// `None` is "this session made no tenant decision", NOT "this session has
    /// no tenant": the reader (`EmbeddedDatabase::effective_tenant_context`)
    /// falls back to the process-global `TenantManager::current_context` on
    /// `None`, which is what keeps the embedded library API and the REPL — both
    /// of which have no session at all and set the global directly — working
    /// exactly as they did. Only a WIRE connection ever binds one.
    tenant: parking_lot::RwLock<Option<crate::tenant::TenantContext>>,
    /// Lock-free mirror of `tenant.is_some()`. Every RLS gate in the engine and
    /// the QPS meter read the binding on the per-statement hot path, and the
    /// overwhelming majority of sessions never bind one, so the common answer
    /// has to cost one relaxed load rather than an `RwLock` read — the same
    /// discipline `guc_overrides_present` follows above.
    tenant_bound: AtomicBool,
    /// sprinter 6bc4a4040eb7 (phase B1): the branch THIS session is bound to,
    /// or `None` for main. The ONLY branch selector in the process — the
    /// engine-wide `StorageEngine.current_branch` it replaces is gone. Written
    /// by [`Self::bind_branch`] / [`Self::unbind_branch`] /
    /// [`Self::replace_branch`]; read by `StorageEngine::active_branch`.
    branch: parking_lot::RwLock<Option<Arc<BranchBinding>>>,
    /// Lock-free mirror of `branch.is_some()`. `StorageEngine::is_branch_active`
    /// is read per statement and on per-row fast-path gates, and nearly every
    /// session is on main, so the common answer has to be one atomic load —
    /// the discipline `tenant_bound` follows above.
    on_branch: AtomicBool,
    /// Lock-free mirror of the bound branch's `engine_instance` (`0` =
    /// unbound; `0` is [`UNATTACHED_ENGINE`], which names no real engine). Lets
    /// `is_branch_active` answer "on a branch OF THIS ENGINE" without the lock.
    branch_engine: AtomicU64,
    /// The branch this connection was OPENED on — the PostgreSQL startup
    /// packet's `database=<db>@<branch>` / `options=-c heliosdb.branch=…`,
    /// `None` for main. It is what `RESET heliosdb.branch`, `RESET ALL` and
    /// `DISCARD ALL` return to (a GUC set at startup is PostgreSQL's reset
    /// value), so a pooled connection keyed on `<db>@<branch>` goes back to its
    /// own branch between clients instead of keeping the previous client's
    /// `USE BRANCH`. Kept as the BINDING (by id), never by name.
    home_branch: parking_lot::RwLock<Option<Arc<BranchBinding>>>,
    /// sprinter 1703dba8e82d: has THIS connection created a `CREATE TEMPORARY
    /// TABLE`? Set by the catalog when a `pg_temp_<backend_pid>` key is
    /// registered, and never cleared for the life of the connection.
    ///
    /// It is not a convenience. Two things key off it, and both would be
    /// WRONG if it were engine-wide rather than per-connection:
    ///
    /// * `Planner::resolve_table_ref`'s implicit `pg_temp`-first probe — a
    ///   session with no temp table of its own must not pay a catalog probe on
    ///   every bare name just because some other connection has one;
    /// * the bypass of the SHARED, SQL-text-keyed plan and result caches
    ///   (`EmbeddedDatabase::current_schema_is_set`). Those caches are keyed by
    ///   statement text and shared across every session, so a plan built for
    ///   `SELECT * FROM t` by a session whose `t` is `pg_temp_7.t` must never
    ///   be served to a session whose `t` is `public.t` — the exact leak this
    ///   item closes, reached through the cache instead of the catalog. The
    ///   `search_path` overlay bypasses them for the identical reason, and this
    ///   flag is what keeps the bypass narrow: only the connections that
    ///   actually own a temp table pay it.
    ///
    /// Relaxed ordering throughout, like every other mirror on this type: a
    /// session is single-threaded with respect to its own statements.
    holds_temp_tables: AtomicBool,
    /// `SET LOCAL` support for the overlay: `name -> the value to restore when
    /// the current transaction block ends` (`None` = the name had no override,
    /// so restoring means REMOVING it). Armed on the first `SET LOCAL` of a
    /// block per name and disarmed by [`Self::end_transaction`], the same shape
    /// `application_name_saved` above uses.
    gucs_saved: parking_lot::Mutex<Option<HashMap<String, Option<SettingValue>>>>,
}

impl SessionScopedState {
    /// Mint a fresh backend that belongs to NO open database and register it in
    /// [`live_backends`].
    ///
    /// Kept for the manager's engine-less unit tests. Every production caller
    /// goes through [`Self::new_for_engine`]: a backend that cannot name its
    /// database appears in nobody's `pg_stat_activity` (see
    /// [`UNATTACHED_ENGINE`]).
    pub fn new() -> Arc<Self> {
        Self::new_for_engine(UNATTACHED_ENGINE)
    }

    /// Mint a fresh backend OWNED BY `engine_instance` and register it in
    /// [`live_backends`] — sprinter 32ed4b9e0002.
    ///
    /// `engine_instance` is `StorageEngine::instance_id()` of the engine this
    /// connection (or, for the handle's own session-less funnel backend, this
    /// `EmbeddedDatabase`) is served by.
    pub fn new_for_engine(engine_instance: u64) -> Arc<Self> {
        let state = Arc::new(Self {
            backend_pid: next_backend_pid(),
            engine_instance,
            backend_start: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64,
            lastval: AtomicI64::new(LASTVAL_UNDEFINED),
            application_name: parking_lot::RwLock::new(String::new()),
            application_name_saved: parking_lot::Mutex::new(None),
            username: parking_lot::RwLock::new(String::new()),
            client_addr: parking_lot::RwLock::new(None),
            client_port: AtomicI32::new(0),
            seq_currval: parking_lot::RwLock::new(HashMap::new()),
            seq_currval_present: AtomicBool::new(false),
            gucs: parking_lot::RwLock::new(HashMap::new()),
            guc_overrides_present: AtomicBool::new(false),
            statement_timeout_ms: AtomicI64::new(GUC_TIMEOUT_UNSET),
            bulk_load_mode: AtomicI8::new(GUC_TRISTATE_UNSET),
            gucs_saved: parking_lot::Mutex::new(None),
            tenant: parking_lot::RwLock::new(None),
            tenant_bound: AtomicBool::new(false),
            branch: parking_lot::RwLock::new(None),
            on_branch: AtomicBool::new(false),
            branch_engine: AtomicU64::new(UNATTACHED_ENGINE),
            home_branch: parking_lot::RwLock::new(None),
            holds_temp_tables: AtomicBool::new(false),
        });
        live_backends().insert(state.backend_pid, Arc::downgrade(&state));
        state
    }

    /// sprinter 6bc4a4040eb7 (phase B1): a backend for ONE request or call
    /// that has no connection of its own — a REST request under
    /// `/v1/branches/:name/…`, an MCP tool call.
    ///
    /// Same state as [`Self::new_for_engine`], but deliberately NOT listed in
    /// [`live_backends`]: a REST request is not a connection, and listing it in
    /// `pg_stat_activity` for the milliseconds it runs would show an operator a
    /// nameless backend that is gone before they can act on it. Its `Drop`
    /// still runs the registry removal, which is a no-op for an unlisted pid.
    pub fn new_request_scoped(engine_instance: u64) -> Arc<Self> {
        Arc::new(Self {
            backend_pid: next_backend_pid(),
            engine_instance,
            backend_start: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64,
            lastval: AtomicI64::new(LASTVAL_UNDEFINED),
            application_name: parking_lot::RwLock::new(String::new()),
            application_name_saved: parking_lot::Mutex::new(None),
            username: parking_lot::RwLock::new(String::new()),
            client_addr: parking_lot::RwLock::new(None),
            client_port: AtomicI32::new(0),
            seq_currval: parking_lot::RwLock::new(HashMap::new()),
            seq_currval_present: AtomicBool::new(false),
            gucs: parking_lot::RwLock::new(HashMap::new()),
            guc_overrides_present: AtomicBool::new(false),
            statement_timeout_ms: AtomicI64::new(GUC_TIMEOUT_UNSET),
            bulk_load_mode: AtomicI8::new(GUC_TRISTATE_UNSET),
            gucs_saved: parking_lot::Mutex::new(None),
            tenant: parking_lot::RwLock::new(None),
            tenant_bound: AtomicBool::new(false),
            branch: parking_lot::RwLock::new(None),
            on_branch: AtomicBool::new(false),
            branch_engine: AtomicU64::new(UNATTACHED_ENGINE),
            home_branch: parking_lot::RwLock::new(None),
            holds_temp_tables: AtomicBool::new(false),
        })
    }

    /// `pg_backend_pid()`.
    pub fn backend_pid(&self) -> i32 {
        self.backend_pid
    }

    /// sprinter 1703dba8e82d: record that this connection has created a
    /// session-private temp table. Called once per `CREATE TEMPORARY TABLE`
    /// from `storage::Catalog::create_table`, the single registration site.
    ///
    /// Deliberately never cleared, not even when the session drops its last
    /// temp table: the flag guards CORRECTNESS decisions (which plan cache a
    /// statement may use), and re-enabling a shared cache mid-session on the
    /// strength of a count would be one off-by-one away from serving this
    /// session's private plan to another. Paying an extra probe for the rest of
    /// a connection that used a temp table once is the cheap side of that
    /// trade.
    pub fn note_temp_table(&self) {
        self.holds_temp_tables.store(true, Ordering::Relaxed);
    }

    /// Has this connection created a temp table? One relaxed load — this is
    /// read per bare table reference and per cache probe.
    #[inline]
    pub fn holds_temp_tables(&self) -> bool {
        self.holds_temp_tables.load(Ordering::Relaxed)
    }

    /// The open database this backend belongs to — `StorageEngine::instance_id()`
    /// of the engine that minted it, or [`UNATTACHED_ENGINE`].
    pub fn engine_instance(&self) -> u64 {
        self.engine_instance
    }

    /// The login identity this backend published (`pg_stat_activity.usename`),
    /// empty for a connection that has published none.
    ///
    /// This is the principal `pg_stat_activity`'s column masking compares — the
    /// role, authenticated by the listener, never the client-chosen database
    /// name. See `sql::phase3::system_views::execute_pg_stat_activity`.
    pub fn username(&self) -> String {
        self.username.read().clone()
    }

    /// Record the value `nextval` (or a SERIAL/IDENTITY fill) just produced.
    ///
    /// Deliberately unconditional: PostgreSQL's `lastval()` follows whichever
    /// sequence was advanced LAST, which is exactly what makes it usable by a
    /// driver that does not know the sequence name.
    pub fn note_lastval(&self, value: i64) {
        self.lastval.store(value, Ordering::Relaxed);
    }

    /// `lastval()` — `None` when no `nextval` has run in this session.
    ///
    /// Fails closed: a `0` here would be indistinguishable from a real row id to
    /// `cursor.lastrowid`. sprinter 7903b7111cb4 brought `currval` into line —
    /// it used to be the odd one out, answering `0` for a sequence this session
    /// had never advanced (and another connection's value when it had).
    pub fn lastval(&self) -> Option<i64> {
        match self.lastval.load(Ordering::Relaxed) {
            LASTVAL_UNDEFINED => None,
            v => Some(v),
        }
    }

    /// sprinter 7903b7111cb4: record one `nextval('<name>')` return — BOTH the
    /// session's `lastval()` and its `currval('<name>')`.
    ///
    /// One call from the one evaluator arm that produces such a value, so the
    /// two functions can never disagree about what "this session" means (they
    /// did: `LASTVAL()` shipped session-scoped in v4.38.0 while `currval` was
    /// still reading the process-wide sequence runtime).
    ///
    /// `name` is stored exactly as the caller spelled it, because that is how
    /// `crate::sql::sequences`' per-database store keys the runtime too — `nextval('s')` and
    /// `nextval('public.s')` are already two different sequences to this engine,
    /// and a normalization here would make `currval` disagree with `nextval`.
    pub fn note_nextval(&self, name: &str, value: i64) {
        self.note_lastval(value);
        self.note_currval(name, value);
    }

    /// Record a `currval` value for `name` WITHOUT touching `lastval`.
    ///
    /// The `setval('s', n)` arm calls this: PostgreSQL documents `setval` as
    /// setting `currval` for the calling session, but it is not a value
    /// `nextval` returned, so it must not move `lastval()`.
    pub fn note_currval(&self, name: &str, value: i64) {
        let mut map = self.seq_currval.write();
        map.insert(name.to_string(), value);
        drop(map);
        self.seq_currval_present.store(true, Ordering::Relaxed);
    }

    /// `currval('<name>')` — `None` when THIS session has never advanced (or
    /// `setval`'d) that sequence, which the caller must report as SQLSTATE 55000
    /// with [`currval_undefined_message`].
    pub fn currval(&self, name: &str) -> Option<i64> {
        // Hot gate: a session that has never touched a sequence — every read-only
        // connection — answers without taking the lock.
        if !self.seq_currval_present.load(Ordering::Relaxed) {
            return None;
        }
        self.seq_currval.read().get(name).copied()
    }

    // ---- sprinter a3077a3f68d8: the per-session USER-SETTABLE GUC overlay ----

    /// `SET <name> = <value>` (session scope) for a user-settable GUC.
    ///
    /// The caller has already validated the value
    /// (`sql::settings::SessionSettings::validate_setting`) and established that
    /// `name` is user-settable rather than server-level — this type stores, it
    /// does not police.
    pub fn set_guc(&self, name: &str, value: SettingValue) {
        let mut map = self.gucs.write();
        map.insert(name.to_string(), value.clone());
        let present = !map.is_empty();
        drop(map);
        self.guc_overrides_present.store(present, Ordering::Relaxed);
        self.refresh_guc_mirror(name, Some(&value));
    }

    /// `SET LOCAL <name> = <value>` inside an open transaction block.
    ///
    /// Arms the restore slot for `name` on the FIRST `SET LOCAL` of the block
    /// only, so two `SET LOCAL`s of the same name in one transaction both revert
    /// to the value the block started with (PostgreSQL semantics), not to each
    /// other — the rule [`Self::set_local_application_name`] already follows.
    ///
    /// GH#28 REFUSED `SET LOCAL` for the three connection-lifetime timeout GUCs
    /// because `Session` had nowhere to keep transaction-scoped state. This
    /// overlay is that place, so the generic GUCs do not need the refusal; the
    /// GH#28 names keep theirs (they live on `Session`, not here).
    pub fn set_local_guc(&self, name: &str, value: SettingValue) {
        {
            let mut saved = self.gucs_saved.lock();
            let slot = saved.get_or_insert_with(HashMap::new);
            if !slot.contains_key(name) {
                let previous = self.gucs.read().get(name).cloned();
                slot.insert(name.to_string(), previous);
            }
        }
        self.set_guc(name, value);
    }

    /// `RESET <name>` / `SET <name> TO DEFAULT` — drop this session's override so
    /// the name reads back as the server default again.
    pub fn reset_guc(&self, name: &str) {
        let mut map = self.gucs.write();
        map.remove(name);
        let present = !map.is_empty();
        drop(map);
        self.guc_overrides_present.store(present, Ordering::Relaxed);
        self.refresh_guc_mirror(name, None);
    }

    /// This session's override for `name`, if it has one.
    pub fn guc(&self, name: &str) -> Option<SettingValue> {
        if !self.guc_overrides_present.load(Ordering::Relaxed) {
            return None;
        }
        self.gucs.read().get(name).cloned()
    }

    /// This session's `statement_timeout` in milliseconds, or `None` when it has
    /// not set one (`Some(0)` is PostgreSQL's *unlimited*, and is NOT the same
    /// answer as `None` — it overrides a configured server default).
    ///
    /// One relaxed atomic load: this is read for every executor.
    pub fn statement_timeout_ms(&self) -> Option<u64> {
        match self.statement_timeout_ms.load(Ordering::Relaxed) {
            GUC_TIMEOUT_UNSET => None,
            ms => Some(ms.max(0) as u64),
        }
    }

    /// This session's `bulk_load_mode`, or `None` when it has not set one (the
    /// storage engine then uses its own server-level flag).
    pub fn bulk_load_mode(&self) -> Option<bool> {
        match self.bulk_load_mode.load(Ordering::Relaxed) {
            GUC_TRISTATE_UNSET => None,
            v => Some(v != 0),
        }
    }

    /// Keep the two lock-free mirrors in step with the map. `value == None` is a
    /// reset (back to the sentinel).
    ///
    /// Only the two hot-path names have a mirror; every other GUC is read
    /// through [`Self::guc`], which is gated on `guc_overrides_present`.
    fn refresh_guc_mirror(&self, name: &str, value: Option<&SettingValue>) {
        match name {
            "statement_timeout" => {
                let ms = value
                    .and_then(|v| v.as_duration_ms())
                    .map_or(GUC_TIMEOUT_UNSET, |ms| ms.min(i64::MAX as u64) as i64);
                self.statement_timeout_ms.store(ms, Ordering::Relaxed);
            }
            "bulk_load_mode" => {
                let flag = value
                    .and_then(|v| v.as_bool())
                    .map_or(GUC_TRISTATE_UNSET, |on| i8::from(on));
                self.bulk_load_mode.store(flag, Ordering::Relaxed);
            }
            _ => {}
        }
    }

    /// The session's `application_name`.
    pub fn application_name(&self) -> String {
        self.application_name.read().clone()
    }

    /// `SET application_name = '…'` (session scope) — also the startup-packet
    /// path, which is where every driver actually sends it.
    ///
    /// Truncated to PostgreSQL's `NAMEDATALEN - 1` for the same reason
    /// `Session::set_login` truncates the login name: on a trust listener the
    /// value is chosen by the client, and it is copied into every
    /// `pg_stat_activity` scan.
    pub fn set_application_name(&self, value: &str) {
        let mut slot = self.application_name.write();
        slot.clear();
        slot.push_str(truncate_on_char_boundary(value, APPLICATION_NAME_MAX_BYTES));
    }

    /// `SET LOCAL application_name = '…'` inside an open transaction block.
    ///
    /// Arms the restore slot on the FIRST `SET LOCAL` of the block only, so two
    /// `SET LOCAL`s in one transaction both revert to the value the block
    /// started with (PostgreSQL semantics), not to each other.
    pub fn set_local_application_name(&self, value: &str) {
        let mut saved = self.application_name_saved.lock();
        if saved.is_none() {
            *saved = Some(self.application_name.read().clone());
        }
        drop(saved);
        self.set_application_name(value);
    }

    /// End of a transaction block — COMMIT *and* ROLLBACK alike. PostgreSQL
    /// reverts a `SET LOCAL` in both cases, so this is called from both.
    ///
    /// A no-op (two uncontended mutex probes) when no `SET LOCAL` is armed,
    /// which is every transaction in practice.
    pub fn end_transaction(&self) {
        let restore = self.application_name_saved.lock().take();
        if let Some(previous) = restore {
            let mut slot = self.application_name.write();
            *slot = previous;
        }
        // sprinter a3077a3f68d8: the generic GUC overlay reverts on the same
        // boundary and for the same reason — PostgreSQL reverts `SET LOCAL` on
        // COMMIT *and* ROLLBACK alike, and both callers already reach here.
        let restore_gucs = self.gucs_saved.lock().take();
        if let Some(previous) = restore_gucs {
            for (name, value) in previous {
                match value {
                    Some(v) => self.set_guc(&name, v),
                    None => self.reset_guc(&name),
                }
            }
        }
    }

    /// Republish the login identity (`pg_stat_activity.usename`). Called by
    /// `Session::set_login`, the single writer of the session's identity, so
    /// the view and `current_user` can never disagree.
    pub fn set_username(&self, name: &str) {
        let mut slot = self.username.write();
        slot.clear();
        slot.push_str(name);
    }

    /// Stamp the peer address a listener accepted the connection from.
    pub fn set_client_address(&self, addr: Option<&str>, port: i32) {
        *self.client_addr.write() = addr.map(str::to_string);
        self.client_port.store(port, Ordering::Relaxed);
    }

    // ---- sprinter d03de7fc3b22: the per-session TENANT binding ----

    /// Bind this connection to `context`, returning the tenant it was bound to
    /// BEFORE (so the caller can release that tenant's connection slot).
    ///
    /// Called once per connection, from the point the requested database name
    /// is resolved post-authentication — `EmbeddedDatabase::bind_session_tenant`
    /// is the only caller, and it is what keeps "database name == tenant name"
    /// (`EmbeddedDatabase::database_name_is_valid`) the single definition of
    /// which tenant a connection belongs to. MySQL's `COM_INIT_DB` can call it a
    /// second time, which is why it reports the displaced binding rather than
    /// asserting there was none.
    pub fn bind_tenant(&self, context: crate::tenant::TenantContext) -> Option<crate::tenant::TenantId> {
        let mut slot = self.tenant.write();
        let previous = slot.as_ref().map(|c| c.tenant_id);
        *slot = Some(context);
        drop(slot);
        self.tenant_bound.store(true, Ordering::Relaxed);
        previous
    }

    /// Drop this connection's tenant binding, returning the tenant it held.
    ///
    /// The mirror is cleared BEFORE the slot, never after: a reader that sees
    /// the mirror still set only takes the lock and finds `None`, which is the
    /// same answer; the reverse order would let a reader skip the lock while the
    /// slot still held a binding it was entitled to see.
    pub fn unbind_tenant(&self) -> Option<crate::tenant::TenantId> {
        if !self.tenant_bound.load(Ordering::Relaxed) {
            return None;
        }
        self.tenant_bound.store(false, Ordering::Relaxed);
        self.tenant.write().take().map(|c| c.tenant_id)
    }

    /// This connection's bound tenant context, or `None` when it made no tenant
    /// decision (every embedded caller, and every wire connection to a reserved
    /// database name).
    pub fn tenant_context(&self) -> Option<crate::tenant::TenantContext> {
        if !self.tenant_bound.load(Ordering::Relaxed) {
            return None;
        }
        self.tenant.read().clone()
    }

    /// This connection's bound tenant id, without the `TenantContext` clone
    /// [`Self::tenant_context`] pays (a `String` `user_id` plus a `Vec<String>`
    /// of roles) — the spelling the QPS meter uses, on every statement of every
    /// execution family.
    pub fn tenant_id(&self) -> Option<crate::tenant::TenantId> {
        if !self.tenant_bound.load(Ordering::Relaxed) {
            return None;
        }
        self.tenant.read().as_ref().map(|c| c.tenant_id)
    }

    /// Is this connection bound to a tenant? One relaxed atomic load — the
    /// spelling the RLS fast-path gates use.
    #[inline]
    pub fn has_tenant(&self) -> bool {
        self.tenant_bound.load(Ordering::Relaxed)
    }

    // ---- sprinter 6bc4a4040eb7 (phase B1): the per-session BRANCH binding ----

    /// Bind this session to `binding`, returning the binding it replaces.
    ///
    /// The caller has already resolved the branch as ACTIVE
    /// (`StorageEngine::resolve_active_branch`); this type stores, it does not
    /// police — the same split [`Self::bind_tenant`] follows. The mirrors are
    /// published AFTER the slot, so a reader that sees them set always finds
    /// the binding behind the lock.
    pub fn bind_branch(&self, binding: BranchBinding) -> Option<Arc<BranchBinding>> {
        self.replace_branch(Some(Arc::new(binding)))
    }

    /// Return this session to main, returning the binding it held.
    pub fn unbind_branch(&self) -> Option<Arc<BranchBinding>> {
        self.replace_branch(None)
    }

    /// Install `binding` (or main, for `None`) and return the previous one —
    /// the primitive a SCOPED binding is built from: the code-graph `ON BRANCH`
    /// guard installs its target for the duration of one statement and puts the
    /// exact previous binding back on `Drop`.
    ///
    /// Mirror ordering mirrors [`Self::unbind_tenant`]: on the way to main the
    /// mirrors are cleared BEFORE the slot (a reader that still sees them set
    /// takes the lock and finds `None`, the same answer); on the way to a
    /// branch they are set AFTER it.
    pub fn replace_branch(&self, binding: Option<Arc<BranchBinding>>) -> Option<Arc<BranchBinding>> {
        match binding {
            Some(binding) => {
                let engine = binding.engine_instance;
                let previous = self.branch.write().replace(binding);
                self.branch_engine.store(engine, Ordering::Release);
                self.on_branch.store(true, Ordering::Release);
                previous
            }
            None => {
                self.on_branch.store(false, Ordering::Release);
                self.branch_engine.store(UNATTACHED_ENGINE, Ordering::Release);
                self.branch.write().take()
            }
        }
    }

    /// This session's branch binding, or `None` for main. One atomic load on
    /// the (overwhelmingly common) main answer; a read lock and an `Arc`
    /// refcount otherwise.
    #[inline]
    pub fn branch_binding(&self) -> Option<Arc<BranchBinding>> {
        if !self.on_branch.load(Ordering::Acquire) {
            return None;
        }
        self.branch.read().clone()
    }

    /// Record the branch this connection was opened on (`None` = main).
    pub fn set_home_branch(&self, branch: Option<Arc<BranchBinding>>) {
        *self.home_branch.write() = branch;
    }

    /// The branch this connection was opened on, `None` for main.
    pub fn home_branch(&self) -> Option<Arc<BranchBinding>> {
        self.home_branch.read().clone()
    }

    /// Return this session to its home branch — main when it was opened on
    /// none. Never fails: a home branch that has since been dropped is bound
    /// anyway (unvalidated), so the session fails CLOSED on its next statement
    /// until it chooses a branch, rather than keeping whatever the previous
    /// client of a pooled connection had bound.
    pub fn reset_branch_to_home(&self) {
        let home = self.home_branch().map(|binding| Arc::new(binding.unvalidated_copy()));
        self.replace_branch(home);
    }

    /// Seed this session's USER-SETTABLE GUC overlay from `other`'s — what a
    /// request-scoped backend (a REST request, an MCP call) inherits from the
    /// handle it runs on, so a handle-level `SET statement_timeout` or
    /// `SET bulk_load_mode` still applies to it (sprinter 6bc4a4040eb7, B1).
    pub fn copy_gucs_from(&self, other: &SessionScopedState) {
        if !other.guc_overrides_present.load(Ordering::Relaxed) {
            return;
        }
        let overlay = other.gucs.read().clone();
        for (name, value) in overlay {
            self.set_guc(&name, value);
        }
    }

    /// Is this session bound to a branch of the engine `engine_instance`?
    /// Two atomic loads, no lock — the spelling the per-statement and per-row
    /// fast-path gates use.
    #[inline]
    pub fn on_branch_of(&self, engine_instance: u64) -> bool {
        self.on_branch.load(Ordering::Acquire) && self.branch_engine.load(Ordering::Acquire) == engine_instance
    }

    /// One row of `pg_stat_activity`.
    pub fn snapshot(&self) -> BackendSnapshot {
        let port = self.client_port.load(Ordering::Relaxed);
        BackendSnapshot {
            pid: self.backend_pid,
            username: self.username.read().clone(),
            application_name: self.application_name.read().clone(),
            client_addr: self.client_addr.read().clone(),
            client_port: (port != 0).then_some(port),
            backend_start: self.backend_start,
        }
    }
}

impl Drop for SessionScopedState {
    fn drop(&mut self) {
        // The registry holds only a `Weak`, so this runs as soon as the
        // `Session` (and every clone of it) is gone. Removing here is what keeps
        // `pg_stat_activity` free of pids a client would try to join against
        // forever.
        live_backends().remove(&self.backend_pid);
    }
}

/// PostgreSQL's `NAMEDATALEN - 1`: where a stock server truncates
/// `application_name`.
const APPLICATION_NAME_MAX_BYTES: usize = 63;

/// Truncate to at most `max` bytes without splitting a UTF-8 sequence.
fn truncate_on_char_boundary(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    // `get` rather than `s[..end]`: the loop has already proved `end` is a char
    // boundary, and the fallback keeps this panic-free by construction rather
    // than by argument.
    s.get(..end).unwrap_or(s)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn lastval_is_undefined_until_a_nextval_lands() {
        let s = SessionScopedState::new();
        assert!(s.lastval().is_none());
        s.note_lastval(7);
        assert_eq!(s.lastval(), Some(7));
        // It follows the LAST value, whichever sequence produced it.
        s.note_lastval(500);
        assert_eq!(s.lastval(), Some(500));
    }

    #[test]
    fn two_backends_never_share_a_pid_or_a_lastval() {
        let a = SessionScopedState::new();
        let b = SessionScopedState::new();
        assert_ne!(a.backend_pid(), b.backend_pid());
        a.note_lastval(1);
        assert!(b.lastval().is_none(), "backend B saw backend A's lastval");
    }

    #[test]
    fn set_local_reverts_to_the_value_the_block_started_with() {
        let s = SessionScopedState::new();
        s.set_application_name("outer");
        s.set_local_application_name("inner");
        // A second SET LOCAL in the same block still reverts to `outer`.
        s.set_local_application_name("inner2");
        assert_eq!(s.application_name(), "inner2");
        s.end_transaction();
        assert_eq!(s.application_name(), "outer");
        // Disarmed: a later transaction end must not re-restore.
        s.set_application_name("later");
        s.end_transaction();
        assert_eq!(s.application_name(), "later");
    }

    #[test]
    fn a_dropped_backend_leaves_the_live_registry() {
        let pid = {
            let s = SessionScopedState::new();
            let pid = s.backend_pid();
            assert!(live_backends().get(&pid).is_some_and(|w| w.upgrade().is_some()));
            pid
        };
        assert!(!live_backends().contains_key(&pid), "a dropped backend stayed listed");
    }

    /// sprinter 7903b7111cb4: `currval` is per SEQUENCE and per SESSION, and one
    /// `nextval` write serves both it and `lastval()`.
    #[test]
    fn currval_is_per_sequence_and_per_session() {
        let a = SessionScopedState::new();
        let b = SessionScopedState::new();
        assert!(a.currval("s").is_none(), "a fresh session has no currval");

        a.note_nextval("s", 7);
        assert_eq!(a.currval("s"), Some(7));
        assert_eq!(a.lastval(), Some(7), "one write must serve both");
        assert!(b.currval("s").is_none(), "session B saw session A's currval");
        assert!(a.currval("other").is_none(), "currval leaked across sequence names");

        // `lastval` follows the LAST sequence; `currval` follows the NAMED one.
        a.note_nextval("other", 500);
        assert_eq!(a.lastval(), Some(500));
        assert_eq!(a.currval("s"), Some(7));

        // `setval` defines currval without moving lastval (PostgreSQL).
        a.note_currval("s", 4242);
        assert_eq!(a.currval("s"), Some(4242));
        assert_eq!(a.lastval(), Some(500));
    }

    /// sprinter a3077a3f68d8: the GUC overlay is per session, the hot mirrors
    /// track the map, and `RESET` really drops the override (rather than writing
    /// a default over it, which would pin a session to a stale value).
    #[test]
    fn the_guc_overlay_is_per_session_and_its_mirrors_track_it() {
        let a = SessionScopedState::new();
        let b = SessionScopedState::new();
        assert!(a.statement_timeout_ms().is_none());
        assert!(a.bulk_load_mode().is_none());

        a.set_guc("statement_timeout", SettingValue::Duration(250));
        a.set_guc("bulk_load_mode", SettingValue::Boolean(true));
        assert_eq!(a.statement_timeout_ms(), Some(250));
        assert_eq!(a.bulk_load_mode(), Some(true));
        assert!(b.statement_timeout_ms().is_none(), "session B saw session A's GUC");
        assert!(b.bulk_load_mode().is_none(), "session B saw session A's GUC");

        // `0` is PostgreSQL's *unlimited* — a VALUE, distinct from "unset",
        // because it must beat a configured server default.
        a.set_guc("statement_timeout", SettingValue::Duration(0));
        assert_eq!(a.statement_timeout_ms(), Some(0));

        a.reset_guc("statement_timeout");
        assert!(a.statement_timeout_ms().is_none());
        assert!(a.guc("statement_timeout").is_none());
        // The other override survives its neighbour's reset.
        assert_eq!(a.bulk_load_mode(), Some(true));
        a.reset_guc("bulk_load_mode");
        assert!(a.bulk_load_mode().is_none());
    }

    /// `SET LOCAL` reverts to the value the BLOCK started with, for every name it
    /// touched — including back to "no override at all".
    #[test]
    fn set_local_guc_reverts_at_the_end_of_the_block() {
        let s = SessionScopedState::new();
        s.set_guc("statement_timeout", SettingValue::Duration(5_000));

        s.set_local_guc("statement_timeout", SettingValue::Duration(250));
        s.set_local_guc("statement_timeout", SettingValue::Duration(750));
        // A name with NO prior override must go back to having none.
        s.set_local_guc("work_mem", SettingValue::Integer(65_536));
        assert_eq!(s.statement_timeout_ms(), Some(750));

        s.end_transaction();
        assert_eq!(s.statement_timeout_ms(), Some(5_000), "reverted to the wrong value");
        assert!(
            s.guc("work_mem").is_none(),
            "a SET LOCAL on a fresh name must leave none"
        );

        // Disarmed: a later block end must not re-restore.
        s.set_guc("statement_timeout", SettingValue::Duration(1));
        s.end_transaction();
        assert_eq!(s.statement_timeout_ms(), Some(1));
    }

    /// sprinter d03de7fc3b22: the KEYSTONE invariant — a tenant binding is
    /// PER CONNECTION. On the pre-fix tree there was nowhere to put one at all:
    /// every reader went to `TenantManager::current_context`, one slot for the
    /// whole process.
    #[test]
    fn the_tenant_binding_never_crosses_sessions() {
        use crate::tenant::{IsolationMode, TenantContext};
        let a = SessionScopedState::new();
        let b = SessionScopedState::new();
        assert!(!a.has_tenant(), "a fresh session must make no tenant decision");
        assert!(a.tenant_context().is_none());
        assert!(a.tenant_id().is_none());

        let tenant_id = uuid::Uuid::new_v4();
        let previous = a.bind_tenant(TenantContext {
            tenant_id,
            user_id: "alice".to_string(),
            roles: vec![],
            isolation_mode: IsolationMode::SharedSchema,
        });
        assert!(previous.is_none(), "a first bind displaces nothing");
        assert!(a.has_tenant());
        assert_eq!(a.tenant_id(), Some(tenant_id));
        assert_eq!(a.tenant_context().map(|c| c.user_id), Some("alice".to_string()));
        assert!(!b.has_tenant(), "session B saw session A's tenant");
        assert!(b.tenant_id().is_none(), "session B saw session A's tenant");

        // Rebinding (MySQL `COM_INIT_DB`) reports the displaced tenant, so the
        // caller can release its connection slot.
        let other = uuid::Uuid::new_v4();
        let displaced = a.bind_tenant(TenantContext {
            tenant_id: other,
            user_id: "alice".to_string(),
            roles: vec![],
            isolation_mode: IsolationMode::DatabasePerTenant,
        });
        assert_eq!(displaced, Some(tenant_id));
        assert_eq!(a.tenant_id(), Some(other));

        assert_eq!(a.unbind_tenant(), Some(other));
        assert!(!a.has_tenant());
        assert!(a.tenant_context().is_none());
        // Idempotent: a second unbind has nothing to release, so a disconnect
        // path that runs twice cannot double-decrement the tenant's connection
        // count.
        assert_eq!(a.unbind_tenant(), None);
    }

    /// sprinter 6bc4a4040eb7 (phase B1): the branch binding is PER SESSION and
    /// per ENGINE, its mirrors track the slot, and `replace_branch` hands back
    /// the exact previous binding (what a scoped binding restores). On the
    /// pre-B1 tree there was nowhere to put one: the only selector was the
    /// engine-wide `StorageEngine.current_branch`.
    #[test]
    fn the_branch_binding_never_crosses_sessions() {
        let a = SessionScopedState::new_for_engine(7);
        let b = SessionScopedState::new_for_engine(7);
        assert!(a.branch_binding().is_none(), "a fresh session is on main");
        assert!(!a.on_branch_of(7));

        let previous = a.bind_branch(BranchBinding::new(7, "dev".to_string(), 3, vec![3], 1));
        assert!(previous.is_none(), "a first bind displaces nothing");
        assert!(a.on_branch_of(7));
        assert!(!a.on_branch_of(8), "a binding names a branch of ONE engine");
        assert_eq!(a.branch_binding().map(|bb| bb.branch_id), Some(3));
        assert!(b.branch_binding().is_none(), "session B saw session A's branch");
        assert!(!b.on_branch_of(7), "session B saw session A's branch");

        // A scoped replacement returns the exact binding it displaced.
        let displaced = a.replace_branch(Some(Arc::new(BranchBinding::new(
            7,
            "other".to_string(),
            9,
            vec![9],
            1,
        ))));
        assert_eq!(displaced.as_ref().map(|bb| bb.name.as_str()), Some("dev"));
        assert_eq!(a.branch_binding().map(|bb| bb.branch_id), Some(9));
        a.replace_branch(displaced);
        assert_eq!(a.branch_binding().map(|bb| bb.branch_id), Some(3));

        assert_eq!(a.unbind_branch().map(|bb| bb.branch_id), Some(3));
        assert!(a.branch_binding().is_none());
        assert!(!a.on_branch_of(7));
        assert!(a.unbind_branch().is_none(), "unbinding main is a no-op");
    }

    #[test]
    fn a_request_scoped_backend_is_not_listed() {
        let s = SessionScopedState::new_request_scoped(7);
        assert!(
            !live_backends().contains_key(&s.backend_pid()),
            "a REST/MCP request backend must not appear in pg_stat_activity"
        );
        assert_eq!(s.engine_instance(), 7);
    }

    #[test]
    fn application_name_is_truncated_on_a_char_boundary() {
        let s = SessionScopedState::new();
        let long = "é".repeat(100); // 200 bytes
        s.set_application_name(&long);
        let got = s.application_name();
        assert!(got.len() <= APPLICATION_NAME_MAX_BYTES);
        assert!(got.chars().all(|c| c == 'é'), "truncation split a UTF-8 sequence");
    }
}
