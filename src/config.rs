//! Configuration for HeliosDB Lite
//!
//! This module provides comprehensive configuration options for the database.
//! Configuration can be loaded from TOML files or constructed programmatically.
//!
//! # Configuration Sections
//!
//! - [`StorageConfig`] - Database path, WAL, compression, caching
//! - [`EncryptionConfig`] - Encryption at rest with AES-256-GCM
//! - [`ServerConfig`] - Network settings, TLS, connection limits
//! - [`SessionConfig`] - Session timeouts and per-user limits
//! - [`LockConfig`] - Lock acquisition timeouts, deadlock detection
//! - [`DumpConfig`] - Backup scheduling and compression
//! - [`VectorConfig`] - Vector index settings (HNSW, PQ)
//!
//! # Example: TOML Configuration
//!
//! ```toml
//! [storage]
//! path = "./data"
//! memory_only = false
//! wal_enabled = true
//! compression = "Zstd"
//!
//! [server]
//! listen_addr = "0.0.0.0"
//! port = 5432
//! max_connections = 100
//!
//! [session]
//! timeout_secs = 3600
//! max_sessions_per_user = 10
//! ```
//!
//! # Example: Programmatic Configuration
//!
//! ```rust
//! use heliosdb_nano::Config;
//!
//! // In-memory database
//! let config = Config::in_memory();
//!
//! // Load from file
//! // let config = Config::from_file("heliosdb.toml")?;
//! ```

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Database configuration
///
/// The main configuration struct for HeliosDB Lite. All configuration
/// sections use sensible defaults and can be customized individually.
///
/// # Loading Configuration
///
/// ```rust,no_run
/// use heliosdb_nano::Config;
///
/// // Default configuration with file-based storage
/// let config = Config::default();
///
/// // In-memory database (for testing)
/// let config = Config::in_memory();
///
/// // Load from TOML file
/// let config = Config::from_file("heliosdb.toml")?;
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
///
/// # Validation
///
/// Call [`validate()`](Config::validate) to check all configuration values
/// before using the configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Named settings bundle: "safe" | "balanced" | "fast" | "agent" (D3/D6).
    ///
    /// Applied by [`Config::from_file`] / [`Config::from_toml_str`] after the
    /// file is parsed: the profile fills in the bundled storage fields
    /// (`wal_sync_mode`, `time_travel_enabled`, `durable_commit`) **only when
    /// the file does not set them explicitly** — an explicit per-field value
    /// in the file always wins over the profile.
    ///
    /// Must be the first field so TOML serialization emits the scalar before
    /// any `[section]` tables.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<ProfileConfig>,
    /// Storage configuration
    #[serde(default)]
    pub storage: StorageConfig,
    /// Encryption configuration
    #[serde(default)]
    pub encryption: EncryptionConfig,
    /// Server configuration
    #[serde(default)]
    pub server: ServerConfig,
    /// Performance configuration
    #[serde(default)]
    pub performance: PerformanceConfig,
    /// Audit configuration
    #[serde(default)]
    pub audit: crate::audit::AuditConfig,
    /// Optimizer configuration (v2.1)
    #[serde(default)]
    pub optimizer: OptimizerConfig,
    /// Authentication configuration (v2.1)
    #[serde(default)]
    pub authentication: AuthenticationConfig,
    /// Compression configuration (v2.1)
    #[serde(default)]
    pub compression: CompressionConfig,
    /// Materialized view configuration (v2.1)
    #[serde(default)]
    pub materialized_views: MaterializedViewConfig,
    /// Vector index configuration (v2.1)
    #[serde(default)]
    pub vector: VectorConfig,
    /// Sync configuration (v2.3 - Experimental)
    #[serde(default)]
    pub sync: SyncConfig,
    /// Session configuration (v3.1.0)
    #[serde(default)]
    pub session: SessionConfig,
    /// Lock configuration (v3.1.0)
    #[serde(default)]
    pub locks: LockConfig,
    /// Dump configuration (v3.1.0)
    #[serde(default)]
    pub dump: DumpConfig,
    /// Resource quota configuration (v3.1.0)
    #[serde(default)]
    pub resource_quotas: ResourceQuotaConfig,
    /// BaaS REST API configuration (v3.2.0)
    #[serde(default)]
    pub api: ApiConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            profile: None,
            storage: StorageConfig::default(),
            encryption: EncryptionConfig::default(),
            server: ServerConfig::default(),
            performance: PerformanceConfig::default(),
            audit: crate::audit::AuditConfig::default(),
            optimizer: OptimizerConfig::default(),
            authentication: AuthenticationConfig::default(),
            compression: CompressionConfig::default(),
            materialized_views: MaterializedViewConfig::default(),
            vector: VectorConfig::default(),
            sync: SyncConfig::default(),
            session: SessionConfig::default(),
            locks: LockConfig::default(),
            dump: DumpConfig::default(),
            resource_quotas: ResourceQuotaConfig::default(),
            api: ApiConfig::default(),
        }
    }
}

impl Config {
    /// Create in-memory configuration
    pub fn in_memory() -> Self {
        Self {
            storage: StorageConfig {
                path: None,
                memory_only: true,
                wal_enabled: false, // No WAL needed for in-memory mode
                ..Default::default()
            },
            audit: crate::audit::AuditConfig::default(),
            ..Default::default()
        }
    }

    /// Load configuration from file
    pub fn from_file(path: impl AsRef<std::path::Path>) -> crate::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        Self::from_toml_str(&content)
    }

    /// Parse configuration from a TOML string, applying the `profile` bundle.
    ///
    /// Precedence (lowest to highest):
    /// 1. struct defaults,
    /// 2. the `profile` bundle (if the file sets `profile = "..."`),
    /// 3. explicit per-field values in the file.
    ///
    /// i.e. the profile only fills in bundled fields the file did not set
    /// explicitly — an explicit `[storage]` key in the file always wins.
    pub fn from_toml_str(content: &str) -> crate::Result<Self> {
        let mut config: Config =
            toml::from_str(content).map_err(|e| crate::Error::config(format!("Failed to parse config: {}", e)))?;

        // Re-parse as a raw TOML document. Two consumers: the profile-precedence
        // check below, and the unknown-section warning. Hoisted out of the
        // `if let Some(profile)` so the warning runs for every config file, not
        // only profile-bearing ones.
        let raw: toml::Value =
            toml::from_str(content).map_err(|e| crate::Error::config(format!("Failed to parse config: {}", e)))?;

        Self::warn_unknown_sections(&raw);

        // `#[serde(skip)]` gives this `false` after deserialization regardless, so
        // derive it from the raw document: only it can say whether the operator
        // actually wrote `[api] jwt_secret`. A generated secret is otherwise
        // indistinguishable from a configured one.
        config.api.jwt_secret_is_ephemeral = raw.get("api").and_then(|api| api.get("jwt_secret")).is_none();

        if let Some(profile) = config.profile {
            // Which [storage] keys the file set EXPLICITLY — after serde
            // deserialization a default is indistinguishable from an explicit value.
            config.apply_profile_defaults(profile, |key| raw.get("storage").and_then(|s| s.get(key)).is_some());
        }
        Ok(config)
    }

    /// Warn about top-level sections this build does not recognise.
    ///
    /// serde ignores unknown fields by default and nothing here sets
    /// `deny_unknown_fields`, so a misspelled or obsolete section is silently
    /// discarded. That is how a user came to run a database they believed was
    /// authenticated: they wrote `[auth] method = "md5"`, the real section is
    /// `[authentication]`, and NOTHING said otherwise — while `[storage]` from the
    /// same file applied correctly, making the file look honoured.
    ///
    /// Warn rather than hard-fail: rejecting outright would stop existing configs
    /// from booting over a stray key, and a config that refuses to start is its own
    /// outage. A warning that names the likely intended section is enough to make
    /// the failure visible, which was the entire problem.
    ///
    /// NOTE this catches TOP-LEVEL sections only. An unknown KEY inside a known
    /// section (`[authentication] passwrod = ...`) is still silent; closing that
    /// needs per-section allowlists or `serde_ignored`, and is tracked separately.
    fn warn_unknown_sections(raw: &toml::Value) {
        const KNOWN: &[&str] = &[
            "profile",
            "storage",
            "encryption",
            "server",
            "performance",
            "audit",
            "optimizer",
            "authentication",
            "compression",
            "materialized_views",
            "vector",
            "sync",
            "session",
            "locks",
            "dump",
            "resource_quotas",
            "api",
        ];

        let Some(table) = raw.as_table() else { return };
        for key in table.keys() {
            if KNOWN.contains(&key.as_str()) {
                continue;
            }
            // Cheap nearest-match: prefix relation in either direction covers the
            // realistic confusions (auth/authentication, vec/vector, perf/performance).
            let suggestion = KNOWN
                .iter()
                .find(|k| k.starts_with(key.as_str()) || key.as_str().starts_with(*k));
            match suggestion {
                Some(k) => tracing::warn!(
                    "config: unknown section [{key}] is IGNORED — did you mean [{k}]? \
                     Settings under [{key}] have no effect."
                ),
                None => {
                    tracing::warn!("config: unknown section [{key}] is IGNORED — settings under it have no effect.")
                }
            }
        }
    }

    /// Default configuration with a named profile bundle applied.
    pub fn with_profile(profile: ProfileConfig) -> Self {
        let mut config = Config {
            profile: Some(profile),
            ..Config::default()
        };
        config.apply_profile_defaults(profile, |_| false);
        config
    }

    /// Apply a profile's bundled storage settings, skipping any field for
    /// which `explicitly_set(key)` returns true (explicit fields win).
    fn apply_profile_defaults(&mut self, profile: ProfileConfig, explicitly_set: impl Fn(&str) -> bool) {
        let bundle = profile.storage_bundle();
        if !explicitly_set("wal_sync_mode") {
            self.storage.wal_sync_mode = bundle.wal_sync_mode;
        }
        if !explicitly_set("time_travel_enabled") {
            self.storage.time_travel_enabled = bundle.time_travel_enabled;
        }
        if let Some(durable_commit) = bundle.durable_commit {
            if !explicitly_set("durable_commit") {
                self.storage.durable_commit = durable_commit;
            }
        }
        if let Some(compression) = bundle.compression {
            if !explicitly_set("compression") {
                self.storage.compression = compression;
            }
        }
        if let Some(cache_size) = bundle.cache_size {
            if !explicitly_set("cache_size") {
                self.storage.cache_size = cache_size;
            }
        }
    }

    /// Save configuration to file
    pub fn save_to_file(&self, path: impl AsRef<std::path::Path>) -> crate::Result<()> {
        let content = toml::to_string_pretty(self)
            .map_err(|e| crate::Error::config(format!("Failed to serialize config: {}", e)))?;
        std::fs::write(path, content)?;
        Ok(())
    }

    /// Validate all configuration sections
    pub fn validate(&self) -> crate::Result<()> {
        self.session.validate()?;
        self.locks.validate()?;
        self.dump.validate()?;
        self.resource_quotas.validate()?;
        Ok(())
    }
}

/// Storage configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct StorageConfig {
    /// Database path (None for in-memory)
    pub path: Option<PathBuf>,
    /// Memory-only mode
    pub memory_only: bool,
    /// Write-ahead log enabled
    pub wal_enabled: bool,
    /// WAL synchronization mode (sync, async, or group_commit)
    pub wal_sync_mode: WalSyncModeConfig,
    /// Maximum memory for cache (bytes)
    pub cache_size: usize,
    /// Compression enabled
    pub compression: CompressionType,
    /// Automatic time-travel versioning enabled (default: true)
    ///
    /// When enabled, all insert/update operations automatically create
    /// versioned snapshots for time-travel queries. This enables AS OF
    /// TIMESTAMP/TRANSACTION/SCN queries with zero configuration.
    ///
    /// Set to false to disable automatic versioning and reduce write overhead
    /// for workloads that don't require time-travel queries.
    pub time_travel_enabled: bool,
    /// W3.2: single-copy latest version. When true, a versioned main-branch
    /// INSERT elides the full `v:` row copy (~65% of INSERT byte volume on the
    /// measured shapes; `W3_2_DESIGN.md` §1) and writes only a flag-bearing
    /// `v_idx:` event; the value is served from `data:` until the row's first
    /// mutation materializes the real `v:`. Only meaningful with
    /// `time_travel_enabled = true` (no version chain is written otherwise).
    ///
    /// Default: false. Enabling it is a ONE-WAY, release-noted decision: a row
    /// written elided cannot be read by a binary that predates W3.2 (its `AS OF`
    /// at the insert timestamp of a never-mutated elided row mis-resolves — see
    /// `W3_2_DESIGN.md` §4). The downgrade escape hatch is dump/restore, which
    /// re-serializes through logical row values and is format-agnostic. The flag
    /// is per-row and durable, so toggling this off leaves already-elided rows
    /// elided (no migration) and stops eliding new inserts.
    #[serde(default)]
    pub elide_latest_version: bool,
    /// Query timeout in milliseconds (None for unlimited)
    ///
    /// If set, queries that exceed this duration will be automatically
    /// terminated to prevent resource exhaustion. Applies to the entire
    /// query execution from start to finish.
    ///
    /// Default: None (unlimited)
    /// Recommended: 30000 (30 seconds) for production environments
    pub query_timeout_ms: Option<u64>,
    /// Statement timeout in milliseconds (None for unlimited)
    ///
    /// If set, individual statement operations (e.g., a single scan or join)
    /// that exceed this duration will be terminated. This provides finer-grained
    /// timeout control than query_timeout_ms.
    ///
    /// Default: None (unlimited)
    /// Note: Currently not implemented, reserved for future use
    pub statement_timeout_ms: Option<u64>,
    /// Transaction isolation level
    ///
    /// Controls the visibility of concurrent transaction changes.
    /// Default: ReadCommitted (standard PostgreSQL default)
    ///
    /// NOTE: `SERIALIZABLE` is served as SNAPSHOT ISOLATION — see
    /// [`TransactionIsolation::Serializable`] and
    /// [`StorageConfig::serializable_policy`].
    pub transaction_isolation: TransactionIsolation,
    /// Task #103: what to do when a client asks for `SERIALIZABLE`.
    ///
    /// HeliosDB-Nano implements SNAPSHOT ISOLATION with first-committer-wins
    /// write-write conflict detection. It does NOT implement SSI: there is no
    /// read-set tracking anywhere in the engine, so `SERIALIZABLE` and
    /// `REPEATABLE READ` are the same level and **write skew is not
    /// prevented**. This knob decides how loudly that is said:
    ///
    /// * `"warn"` (default) — run the transaction as snapshot isolation and
    ///   tell the client: a `WARNING` on the PostgreSQL wire plus a server-log
    ///   `WARN` line naming write skew.
    /// * `"error"` — refuse `BEGIN … ISOLATION LEVEL SERIALIZABLE` outright,
    ///   for deployments that need the real guarantee or nothing.
    ///
    /// Default: `Warn`.
    pub serializable_policy: SerializablePolicy,
    /// Slow query log threshold in milliseconds (None to disable)
    ///
    /// Queries exceeding this threshold are logged at WARN level with their
    /// SQL text (truncated to 200 chars), duration, and row count.
    ///
    /// Default: Some(1000) (1 second)
    #[serde(default = "default_slow_query_threshold")]
    pub slow_query_threshold_ms: Option<u64>,
    /// Write and fsync per-statement logical WAL entries for fast DML.
    ///
    /// Default: false. Standalone fast DML relies on RocksDB's own WAL for local
    /// recovery and skips the extra logical-WAL RocksDB batch. HA primaries still
    /// append and broadcast logical WAL entries even when this is false. Setting
    /// this to true restores the legacy strict logical-WAL path for every fast
    /// INSERT/UPDATE/DELETE, including a per-statement fsync.
    #[serde(default)]
    pub logical_wal_per_statement: bool,
    /// R1.3: fsync the RocksDB WriteBatch at transaction COMMIT, making
    /// commits power-loss durable. RocksDB's leader/follower write groups
    /// amortize one fsync across concurrent committers, so durable-commit
    /// throughput scales with connection count. Default false (process-
    /// crash-safe commits, matching the historical contract); flip per
    /// deployment when power-loss durability is required.
    pub durable_commit: bool,
    /// W3.5: how a snapshot / AS OF / branch read decodes a stored base row
    /// written under an OLDER schema than the current catalog (fewer columns
    /// than the table now has, after `ALTER TABLE ... ADD COLUMN`). Homed here
    /// beside `time_travel_enabled` because it only affects version-resolved
    /// (`AS OF`, open-snapshot) and branch-overlay reads. See
    /// [`SnapshotSchemaEvolution`] and
    /// `docs/plans/PERF_STABILITY_2026_07/W3_5_DESIGN.md`.
    #[serde(default = "default_snapshot_schema_evolution")]
    pub snapshot_schema_evolution: SnapshotSchemaEvolution,
    /// R4.3: how long MVCC version history (`v:` / `v_idx:` keys written on
    /// every versioned INSERT/UPDATE/DELETE) is retained before the version
    /// garbage collector may reclaim it.
    ///
    /// Accepts a plain number of seconds (`"86400"`) or a humantime-style
    /// suffix: `"90s"`, `"15m"`, `"24h"`, `"7d"`, `"2w"`.
    ///
    /// Default: `None` = infinite retention = version GC fully disabled
    /// (exactly the pre-R4.3 behavior). Time-travel (`AS OF`) reads older
    /// than the retention window fail with a clear error once GC has
    /// advanced past them, instead of returning wrong data.
    #[serde(default)]
    pub version_retention: Option<String>,
    /// R4.3: background version-GC cycle interval in seconds.
    ///
    /// - `None` (default): automatic — no background GC when
    ///   `version_retention` is unset; every 300s when it is set.
    /// - `Some(0)`: background GC off even with retention set (history is
    ///   then only reclaimed by explicit `VACUUM VERSIONS`).
    /// - `Some(n)`: run a bounded GC cycle every `n` seconds.
    #[serde(default)]
    pub version_gc_interval_secs: Option<u64>,
    /// R4.3: maximum number of dead versions reclaimed per background GC
    /// cycle, bounding per-cycle work so the collector never causes latency
    /// spikes. `VACUUM VERSIONS` loops cycles until a full pass completes.
    ///
    /// Default: 50_000.
    #[serde(default = "default_version_gc_max_per_cycle")]
    pub version_gc_max_per_cycle: usize,
    /// R1.3 phase 2: group-commit accumulation window in microseconds.
    ///
    /// Durable commits write their batch unsynced and enqueue on the
    /// engine's group committer; the first committer of a generation leads,
    /// accumulates joiners for this window, then issues ONE WAL fsync for
    /// the whole cohort. `0` disables the accumulation wait (commits still
    /// group behind any in-flight fsync). Only meaningful with
    /// `durable_commit = true` (or sessions running
    /// `SET synchronous_commit = on`). Default: 1000 — measured 984-1718
    /// txn/s vs 890-1393 at w=200 on 24-32T durable workloads
    /// (perf/r1_3_p2_runs); at saturation the window hides behind the
    /// in-flight fsync, so the added latency only shows on idle-WAL commits.
    pub group_commit_window_us: u64,
    /// Item 8 (v3.58): direct RocksDB write-path tunables for bulk ingest.
    /// Every field is `None` by default = the built-in literal shown, so
    /// existing configs and the OLTP/`pg35` path are byte-unchanged. Override
    /// only to tune a bulk-load (e.g. the `fast_ingest` profile / a code-KB).
    ///
    /// `None` derives the memtable as 25% of `cache_size` (legacy split);
    /// `Some(n)` sets the write buffer directly AND gives the full `cache_size`
    /// to the block cache — so a write-only ingest gets a large memtable
    /// without over-allocating read cache.
    #[serde(default)]
    pub rocksdb_write_buffer_size: Option<usize>,
    /// `None` = 4.
    #[serde(default)]
    pub rocksdb_max_write_buffer_number: Option<i32>,
    /// `None` = 2.
    #[serde(default)]
    pub rocksdb_min_write_buffer_number_to_merge: Option<i32>,
    /// `None` = 4.
    #[serde(default)]
    pub rocksdb_level0_file_num_compaction_trigger: Option<i32>,
    /// `None` = 4.
    #[serde(default)]
    pub rocksdb_max_background_jobs: Option<i32>,
    /// `None` = 1 MiB (1048576).
    #[serde(default)]
    pub rocksdb_bytes_per_sync: Option<u64>,
}

fn default_slow_query_threshold() -> Option<u64> {
    Some(1000)
}

fn default_snapshot_schema_evolution() -> SnapshotSchemaEvolution {
    SnapshotSchemaEvolution::NullPad
}

/// W3.5: how a base-table scan shapes a stored row that was written under an
/// OLDER schema than the current catalog, when a version-resolved (`AS OF` /
/// open-snapshot) or branch-forked read surfaces the pre-ALTER tuple. The row
/// can be narrower than the current catalog (a later `ADD COLUMN`) or wider (a
/// later `DROP COLUMN`, which rewrites `data:` in place with no new version and
/// does not touch a branch's `bdata:`, so an old/forked wide row survives).
///
/// The `"versioned"` value (Stage 2: resolve the schema as-of the snapshot) is
/// reserved and rejected at config parse until implemented.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotSchemaEvolution {
    /// Decode at the row's stored arity. Projecting a column the row predates
    /// raises "Column index N out of bounds in tuple" (pre-W3.5 behavior).
    /// Retained as a rollback for any consumer keyed on that error.
    Strict,
    /// Stage 1: shape a base row to the current catalog width. A short row
    /// (post-`ADD COLUMN`) is padded with `NULL`, so a pre-ALTER snapshot
    /// projects the added column as NULL — never the post-snapshot DEFAULT
    /// backfill (isolation-preserving). A row WIDER than the catalog
    /// (post-`DROP COLUMN`: a resolved old version or an un-rewritten branch
    /// fork) is TRUNCATED to the current width, matching the pre-W3.5 projection
    /// — it is legitimate schema evolution, not corruption.
    NullPad,
}

impl<'de> Deserialize<'de> for SnapshotSchemaEvolution {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        // Hand-rolled (not derived) so the reserved Stage-2 value and any typo
        // fail config parse with an actionable message rather than a bare
        // "unknown variant" — the flip changes an error into rows, so a
        // mis-set knob must be loud.
        let raw = String::deserialize(deserializer)?;
        match raw.as_str() {
            "strict" => Ok(SnapshotSchemaEvolution::Strict),
            "null_pad" => Ok(SnapshotSchemaEvolution::NullPad),
            "versioned" => Err(serde::de::Error::custom(
                "storage.snapshot_schema_evolution = \"versioned\" is reserved for W3.5 Stage 2 \
                 (catalog time-travel) and is not implemented yet; use \"strict\" or \"null_pad\"",
            )),
            other => Err(serde::de::Error::custom(format!(
                "invalid storage.snapshot_schema_evolution '{}': expected \"strict\" or \"null_pad\"",
                other
            ))),
        }
    }
}

fn default_version_gc_max_per_cycle() -> usize {
    50_000
}

impl StorageConfig {
    /// R4.3: parse `version_retention` into seconds. `None` = infinite
    /// retention (version GC disabled). Errors on unparseable values so a
    /// misconfigured retention never silently disables (or enables) GC.
    pub fn version_retention_secs(&self) -> crate::Result<Option<u64>> {
        match &self.version_retention {
            None => Ok(None),
            Some(raw) => parse_retention_duration_secs(raw).map(Some),
        }
    }

    /// R4.3: effective background GC interval in seconds (0 = no worker).
    /// See `version_gc_interval_secs` field docs for the `None`/`Some(0)`
    /// semantics.
    pub fn effective_version_gc_interval_secs(&self) -> crate::Result<u64> {
        if self.version_retention_secs()?.is_none() {
            return Ok(0);
        }
        Ok(self.version_gc_interval_secs.unwrap_or(300))
    }
}

/// Parse a humantime-style duration into whole seconds: plain seconds
/// (`"900"`) or a single `s`/`m`/`h`/`d`/`w` suffix (`"90s"`, `"15m"`,
/// `"24h"`, `"7d"`, `"2w"`).
pub fn parse_retention_duration_secs(raw: &str) -> crate::Result<u64> {
    let s = raw.trim();
    if s.is_empty() {
        return Err(crate::Error::config(
            "storage.version_retention is empty; use e.g. \"24h\", \"7d\" or a number of seconds",
        ));
    }
    let (digits, multiplier) = match s.chars().last() {
        Some('s') | Some('S') => (&s[..s.len() - 1], 1u64),
        Some('m') | Some('M') => (&s[..s.len() - 1], 60u64),
        Some('h') | Some('H') => (&s[..s.len() - 1], 3_600u64),
        Some('d') | Some('D') => (&s[..s.len() - 1], 86_400u64),
        Some('w') | Some('W') => (&s[..s.len() - 1], 604_800u64),
        _ => (s, 1u64),
    };
    let value: u64 = digits.trim().parse().map_err(|_| {
        crate::Error::config(format!(
            "invalid storage.version_retention '{}': expected seconds or <n>[s|m|h|d|w]",
            raw
        ))
    })?;
    value
        .checked_mul(multiplier)
        .ok_or_else(|| crate::Error::config(format!("storage.version_retention '{}' overflows u64 seconds", raw)))
}

fn default_idle_timeout_secs() -> u64 {
    300 // 5 minutes
}

fn default_copy_max_buffered_rows() -> usize {
    10_000_000 // generous cap on rows buffered per COPY FROM STDIN; 0 disables
}

fn default_copy_max_record_bytes() -> usize {
    1_073_741_824 // 1 GiB per in-progress COPY record (PG's own per-field limit); 0 disables
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            path: Some(PathBuf::from("./heliosdb-data")),
            memory_only: false,
            wal_enabled: true,
            wal_sync_mode: WalSyncModeConfig::Sync, // Safest default for single-user workloads
            cache_size: 512 * 1024 * 1024,          // 512 MB
            compression: CompressionType::Zstd,
            time_travel_enabled: true,   // Enable by default for zero-config transparency
            elide_latest_version: false, // W3.2: single-copy latest version OFF by default (one-way door)
            query_timeout_ms: None,      // Unlimited by default
            statement_timeout_ms: None,  // Unlimited by default
            transaction_isolation: TransactionIsolation::ReadCommitted, // PostgreSQL default
            // #103: warn (never silently accept) that SERIALIZABLE is served
            // as snapshot isolation. `"error"` refuses it instead.
            serializable_policy: SerializablePolicy::Warn,
            slow_query_threshold_ms: Some(1000), // 1 second default
            logical_wal_per_statement: false,    // rely on RocksDB WAL at commit (see field docs)
            durable_commit: false,
            // W3.5 Stage 1 on by default: turn the pre-ALTER-snapshot arity
            // error into isolation-preserving NULL-padded rows. `"strict"`
            // restores the pre-W3.5 error as a rollback.
            snapshot_schema_evolution: SnapshotSchemaEvolution::NullPad,
            version_retention: None,        // infinite retention: version GC disabled (R4.3)
            version_gc_interval_secs: None, // auto: off without retention, 300s with it
            version_gc_max_per_cycle: default_version_gc_max_per_cycle(),
            group_commit_window_us: 1000,
            // Item 8: None = built-in RocksDB literals (no behavior change).
            rocksdb_write_buffer_size: None,
            rocksdb_max_write_buffer_number: None,
            rocksdb_min_write_buffer_number_to_merge: None,
            rocksdb_level0_file_num_compaction_trigger: None,
            rocksdb_max_background_jobs: None,
            rocksdb_bytes_per_sync: None,
        }
    }
}

/// Named configuration profile (D3): a one-key bundle of durability /
/// versioning settings. See [`Config::profile`] for precedence semantics.
///
/// What a profile trades is durability and history — never correctness.
/// PRIMARY KEY, UNIQUE, FOREIGN KEY and CHECK are enforced identically under
/// every profile. That invariant did not hold before the fix noted under
/// "Unreleased" in `CHANGELOG.md`: `Fast` and `FastIngest` set
/// `time_travel_enabled = false`, which selected a storage insert arm that
/// skipped the PK/UNIQUE check, so non-SQL insert entry points (REST, dump
/// RESTORE, protocol adapters, materialized-view refresh) could persist
/// duplicate primary keys. Any new profile MUST keep enforcement invariant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProfileConfig {
    /// `wal_sync_mode = "sync"`, `time_travel_enabled = true` (the current
    /// built-in defaults). Slowest writes; every operation fsyncs. Users who
    /// also need power-loss durable transaction commits should additionally
    /// set `storage.durable_commit = true` (off by default, v3.44.0+).
    Safe,
    /// `wal_sync_mode = "group_commit"`, `time_travel_enabled = true`.
    /// Batches WAL syncs across writers: a process crash keeps committed
    /// data (RocksDB WAL), a power loss may lose the last commit window.
    Balanced,
    /// `wal_sync_mode = "group_commit"`, `time_travel_enabled = false`,
    /// `durable_commit = false`. Fastest writes: no version snapshots (no
    /// AS OF time-travel queries) and commits are process-crash-safe only.
    Fast,
    /// AI-agent workload bundle (D6): `wal_sync_mode = "group_commit"`,
    /// `time_travel_enabled = true`. Guarantees the features a 10-minute
    /// agent session leans on stay enabled together: branchable sandboxes
    /// (branching is always on — no flag needed — but `AS OF` branch
    /// anchors require time-travel), `AS OF` time-travel reads, and HNSW
    /// vector search (on by default), with group-commit write throughput
    /// for chatty tool-call write patterns. Storage values match
    /// `Balanced`; the named profile exists so agent deployments opt into
    /// the documented bundle rather than individual knobs.
    Agent,
    /// Regenerable bulk-ingest bundle (v3.58): `wal_sync_mode = "async"`,
    /// `time_travel_enabled = false`, `durable_commit = false`,
    /// `compression = "lz4"`, larger `cache_size`, plus code-index overrides
    /// (skip refs / cross-file-resolve, bounded `chunk_size`). For throwaway /
    /// rebuildable indexes (e.g. a code-KB) where `AS OF` history, a fully
    /// resolved ref graph, and power-loss durability are not needed during the
    /// build. Opt-in only — never selected by default, so OLTP/`pg35` paths are
    /// untouched.
    #[serde(rename = "fast_ingest")]
    FastIngest,
}

/// Bundled storage values a profile applies (None = leave field untouched).
pub(crate) struct ProfileStorageBundle {
    pub wal_sync_mode: WalSyncModeConfig,
    pub time_travel_enabled: bool,
    pub durable_commit: Option<bool>,
    /// SST compression (None = leave the configured/default value).
    pub compression: Option<CompressionType>,
    /// RocksDB cache budget in bytes (None = leave the configured/default value).
    pub cache_size: Option<usize>,
}

/// Code-index overrides a profile recommends for the ingest path. Lives in
/// `config.rs` (always compiled, no `code-graph` dependency) so an external
/// indexer plugin can read the engine's recommended values instead of
/// re-hardcoding them, then map them onto its `CodeIndexOptions`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CodeIndexProfileDefaults {
    pub skip_symbol_refs: bool,
    pub skip_cross_file_resolve: bool,
    pub chunk_size: Option<usize>,
}

impl ProfileConfig {
    /// Lowercase name as written in TOML.
    pub fn as_str(&self) -> &'static str {
        match self {
            ProfileConfig::Safe => "safe",
            ProfileConfig::Balanced => "balanced",
            ProfileConfig::Fast => "fast",
            ProfileConfig::Agent => "agent",
            ProfileConfig::FastIngest => "fast_ingest",
        }
    }

    /// Code-index overrides recommended when ingesting under this profile.
    /// `None` for non-ingest profiles (no change to indexing behavior). An
    /// external indexer reads this and applies it to its `CodeIndexOptions`,
    /// keeping the values single-sourced in the engine.
    pub fn code_index_overrides(&self) -> Option<CodeIndexProfileDefaults> {
        match self {
            ProfileConfig::FastIngest => Some(CodeIndexProfileDefaults {
                skip_symbol_refs: true,
                skip_cross_file_resolve: true,
                chunk_size: Some(2000),
            }),
            _ => None,
        }
    }

    pub(crate) fn storage_bundle(&self) -> ProfileStorageBundle {
        match self {
            // Safe/Balanced leave durable_commit at its default/file value so
            // an operator's explicit `durable_commit = true` (power-loss
            // durability, v3.44.0+) is never silently downgraded.
            ProfileConfig::Safe => ProfileStorageBundle {
                wal_sync_mode: WalSyncModeConfig::Sync,
                time_travel_enabled: true,
                durable_commit: None,
                compression: None,
                cache_size: None,
            },
            ProfileConfig::Balanced => ProfileStorageBundle {
                wal_sync_mode: WalSyncModeConfig::GroupCommit,
                time_travel_enabled: true,
                durable_commit: None,
                compression: None,
                cache_size: None,
            },
            ProfileConfig::Fast => ProfileStorageBundle {
                wal_sync_mode: WalSyncModeConfig::GroupCommit,
                time_travel_enabled: false,
                durable_commit: Some(false),
                compression: None,
                cache_size: None,
            },
            // Regenerable bulk-ingest: async WAL, no version snapshots, no
            // power-loss durability, cheap Lz4 SST compression, and a larger
            // cache budget (the 25% write-buffer slice cuts L0 flushes during
            // the bulk write). Opt-in; all other profiles set the new fields to
            // None so their behavior — and pg35 — is unchanged.
            ProfileConfig::FastIngest => ProfileStorageBundle {
                wal_sync_mode: WalSyncModeConfig::Async,
                time_travel_enabled: false,
                durable_commit: Some(false),
                compression: Some(CompressionType::Lz4),
                cache_size: Some(2 * 1024 * 1024 * 1024),
            },
            // Agent: time-travel must stay on (AS OF reads + AS OF branch
            // anchors are part of the agent bundle), writes use group
            // commit. Branching and vector search need no storage knobs —
            // both are always available. durable_commit stays None so an
            // explicit operator opt-in is never silently downgraded.
            ProfileConfig::Agent => ProfileStorageBundle {
                wal_sync_mode: WalSyncModeConfig::GroupCommit,
                time_travel_enabled: true,
                durable_commit: None,
                compression: None,
                cache_size: None,
            },
        }
    }
}

/// WAL synchronization mode configuration
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WalSyncModeConfig {
    /// Synchronous - fsync on every write (safest, slowest)
    Sync,
    /// Asynchronous - OS-managed flush (faster, less safe)
    Async,
    /// Group commit - batch multiple operations (balanced)
    GroupCommit,
}

/// Compression type
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CompressionType {
    /// No compression
    None,
    /// Zstandard compression (recommended)
    Zstd,
    /// LZ4 compression (faster, lower ratio)
    Lz4,
}

/// Transaction isolation level
///
/// Controls how transactions see concurrent changes from other transactions.
/// HeliosDB-Nano implements SNAPSHOT ISOLATION. Read the per-variant docs
/// before choosing: `RepeatableRead` and `Serializable` are the SAME level
/// here, and `Serializable` is **not** serializable (task #103).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TransactionIsolation {
    /// Read Uncommitted (not recommended)
    ///
    /// Transactions can see uncommitted changes from other transactions (dirty reads).
    /// Not fully supported - maps to ReadCommitted for safety.
    ReadUncommitted,
    /// Read Committed (PostgreSQL default)
    ///
    /// Transactions only see committed changes from other transactions.
    /// Each statement sees a fresh snapshot at statement start.
    ReadCommitted,
    /// Repeatable Read — i.e. SNAPSHOT ISOLATION.
    ///
    /// The transaction reads one consistent snapshot taken at BEGIN, so no
    /// dirty reads and no non-repeatable reads. Concurrent writers to the same
    /// row are resolved first-committer-wins: the loser aborts with a
    /// `40001 serialization_failure`.
    ///
    /// WRITE SKEW is permitted — two transactions may each read a set the
    /// other is about to change, and both commit.
    RepeatableRead,
    /// Serializable — **accepted as a name, served as SNAPSHOT ISOLATION**.
    ///
    /// Task #103: HeliosDB-Nano has no read-set tracking and no
    /// dangerous-structure detection (no SSI), so this level is byte-identical
    /// to [`TransactionIsolation::RepeatableRead`] — same snapshot, same
    /// first-committer-wins write-write validation, and the same WRITE SKEW
    /// exposure. The classic anomaly it does NOT prevent:
    ///
    /// ```text
    /// -- both transactions read the same rows, each then writes a DIFFERENT one
    /// T1: BEGIN; SELECT count(*) FROM doctors WHERE on_call;  -- 2
    /// T2: BEGIN; SELECT count(*) FROM doctors WHERE on_call;  -- 2
    /// T1: UPDATE doctors SET on_call=false WHERE name='alice'; COMMIT;
    /// T2: UPDATE doctors SET on_call=false WHERE name='bob';   COMMIT;
    /// -- PostgreSQL SERIALIZABLE: one transaction aborts (40001).
    /// -- HeliosDB-Nano: BOTH commit; nobody is left on call.
    /// ```
    ///
    /// Requesting this level raises a wire `WARNING` — or an error — according
    /// to [`SerializablePolicy`] (`storage.serializable_policy`).
    Serializable,
}

/// Task #103: how a request for `SERIALIZABLE` isolation is answered.
///
/// `SERIALIZABLE` is served as snapshot isolation (see
/// [`TransactionIsolation::Serializable`]). Rather than silently accept a level
/// it does not implement, the engine either says so or refuses. There is
/// deliberately NO "silent" variant: the point of the knob is that a client
/// asking for serializability is never left believing it got it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SerializablePolicy {
    /// Default. Run the transaction (as snapshot isolation) and warn: a
    /// PostgreSQL `WARNING` notice on the wire naming write skew, plus a
    /// server-log `WARN` line.
    Warn,
    /// Refuse `BEGIN … ISOLATION LEVEL SERIALIZABLE` with an error, for
    /// deployments that need the real guarantee or nothing.
    Error,
}

impl<'de> Deserialize<'de> for SerializablePolicy {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        // Hand-rolled (not derived), like `SnapshotSchemaEvolution`: a typo in
        // a knob whose whole job is honesty must fail config parse loudly
        // rather than fall back to a default the operator did not choose.
        let raw = String::deserialize(deserializer)?;
        match raw.as_str() {
            "warn" => Ok(SerializablePolicy::Warn),
            "error" => Ok(SerializablePolicy::Error),
            "silent" | "accept" | "off" => Err(serde::de::Error::custom(
                "storage.serializable_policy has no silent-accept value: SERIALIZABLE is served as \
                 snapshot isolation and does not prevent write skew, so the request is always \
                 reported. Use \"warn\" (the default) or \"error\"",
            )),
            other => Err(serde::de::Error::custom(format!(
                "invalid storage.serializable_policy '{}': expected \"warn\" or \"error\"",
                other
            ))),
        }
    }
}

/// Encryption configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct EncryptionConfig {
    /// Encryption enabled
    pub enabled: bool,
    /// Encryption algorithm
    pub algorithm: EncryptionAlgorithm,
    /// Key source
    pub key_source: KeySource,
    /// Key rotation interval (days)
    pub rotation_interval_days: u32,
    /// Zero-Knowledge Encryption mode (v3.5)
    #[serde(default)]
    pub zke: ZkeEncryptionConfig,
}

impl Default for EncryptionConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            algorithm: EncryptionAlgorithm::Aes256Gcm,
            key_source: KeySource::Environment("HELIOSDB_ENCRYPTION_KEY".to_string()),
            rotation_interval_days: 90,
            zke: ZkeEncryptionConfig::default(),
        }
    }
}

/// Zero-Knowledge Encryption configuration (v3.5)
///
/// ZKE ensures encryption keys never leave the client. The server only
/// ever sees encrypted data and cannot decrypt without client-provided keys.
///
/// # Security Properties
///
/// - **Client-Side Encryption**: All data encrypted before transmission
/// - **Per-Request Keys**: Keys provided with each request, never stored
/// - **Key Hash Validation**: Server validates key via SHA-256 hash
/// - **Nonce-Based Replay Protection**: Each request has unique nonce
///
/// # Example Configuration
///
/// ```toml
/// [encryption.zke]
/// enabled = true
/// mode = "per_request"
/// require_key_hash = true
/// replay_protection = true
/// nonce_window_secs = 300
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ZkeEncryptionConfig {
    /// Enable Zero-Knowledge Encryption mode
    pub enabled: bool,
    /// ZKE mode: "full", "hybrid", or "per_request"
    pub mode: ZkeMode,
    /// Require key hash validation on every request
    pub require_key_hash: bool,
    /// Enable nonce-based replay protection
    pub replay_protection: bool,
    /// Nonce validity window in seconds (default: 300 = 5 minutes)
    pub nonce_window_secs: u64,
    /// Maximum cached nonces for replay protection (default: 10000)
    pub max_cached_nonces: usize,
}

impl Default for ZkeEncryptionConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            mode: ZkeMode::PerRequest,
            require_key_hash: true,
            replay_protection: true,
            nonce_window_secs: 300,
            max_cached_nonces: 10000,
        }
    }
}

/// Zero-Knowledge Encryption mode
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ZkeMode {
    /// Full zero-knowledge: client encrypts all data
    /// - No server-side search capabilities
    /// - Maximum privacy
    Full,
    /// Hybrid: metadata visible, row data encrypted
    /// - Table/column names unencrypted
    /// - Basic filtering possible
    Hybrid,
    /// Per-request decryption with client-provided key
    /// - Full SQL capabilities
    /// - Key zeroed after each request
    PerRequest,
}

/// Encryption algorithm
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EncryptionAlgorithm {
    /// AES-256-GCM (recommended)
    Aes256Gcm,
}

/// Encryption key source
///
/// Specifies where the encryption key is retrieved from. For security,
/// keys should never be stored in configuration files directly.
///
/// # Security Recommendations
///
/// - **Production**: Use `Kms` with a cloud provider for key management
/// - **Development**: Use `Environment` with a secure secret manager
/// - **Testing**: Use `File` with proper file permissions (chmod 600)
///
/// # Examples
///
/// ```toml
/// # Environment variable (recommended for development)
/// [encryption]
/// key_source = { Environment = "HELIOSDB_KEY" }
///
/// # AWS KMS (recommended for production)
/// [encryption]
/// key_source = { Kms = { provider = "aws", key_id = "arn:aws:kms:..." } }
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum KeySource {
    /// Read key from environment variable
    Environment(String),
    /// Read key from file (ensure proper file permissions)
    File(PathBuf),
    /// Use cloud Key Management Service
    Kms {
        /// Cloud provider: "aws", "azure", or "gcp"
        provider: String,
        /// Key identifier (ARN for AWS, key URI for others)
        key_id: String,
    },
}

/// Server configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    /// Listen address
    pub listen_addr: String,
    /// PostgreSQL protocol port
    pub port: u16,
    /// Oracle TNS protocol port (optional, disabled if None)
    pub oracle_port: Option<u16>,
    /// Maximum connections
    pub max_connections: usize,
    /// Idle connection timeout in seconds (0 = no timeout, default 300s = 5 min)
    #[serde(default = "default_idle_timeout_secs")]
    pub idle_timeout_secs: u64,
    /// Maximum number of rows buffered in memory while decoding a single
    /// `COPY … FROM STDIN` stream before the server aborts the copy with a clean
    /// error and zero rows applied (0 = unlimited). Bounds peak RSS on very large
    /// streaming COPY loads: the PG-wire decoder holds typed rows, not the raw
    /// byte stream, so this caps the dominant remaining allocation.
    #[serde(default = "default_copy_max_buffered_rows")]
    pub copy_max_buffered_rows: usize,
    /// Maximum bytes accumulated for a SINGLE in-progress `COPY … FROM STDIN`
    /// record before the server aborts the copy with a clean error and zero rows
    /// applied (0 = unlimited). `copy_max_buffered_rows` only counts COMPLETED
    /// rows; this caps the partial-record state it never sees — an unterminated
    /// text line, or an unclosed quoted CSV field — so a stream with no record
    /// separator (e.g. a multi-GB single-line blob) cannot accumulate unbounded.
    #[serde(default = "default_copy_max_record_bytes")]
    pub copy_max_record_bytes: usize,
    /// TLS enabled
    pub tls_enabled: bool,
    /// TLS certificate path
    pub tls_cert_path: Option<PathBuf>,
    /// TLS key path
    pub tls_key_path: Option<PathBuf>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen_addr: "127.0.0.1".to_string(),
            port: 5432,
            oracle_port: Some(1521), // Enable Oracle protocol by default
            max_connections: 100,
            idle_timeout_secs: default_idle_timeout_secs(),
            copy_max_buffered_rows: default_copy_max_buffered_rows(),
            copy_max_record_bytes: default_copy_max_record_bytes(),
            tls_enabled: false,
            tls_cert_path: None,
            tls_key_path: None,
        }
    }
}

/// Performance configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PerformanceConfig {
    /// Worker threads
    pub worker_threads: usize,
    /// Query timeout (seconds)
    pub query_timeout_secs: u64,
    /// Enable SIMD
    pub simd_enabled: bool,
    /// Parallel query execution
    pub parallel_query: bool,
    /// Read-hot-path lock-contention census (W3.1). Samples the plan/parse/
    /// result cache shard mutexes and the ART index registry via
    /// try-lock-first accounting and surfaces the `heliosdb_lock_census`
    /// system view. Requires the `lock-census` build feature — ignored, and
    /// zero-cost, otherwise. Off by default: diagnostic only.
    pub lock_census: bool,
    /// Per-statement-class write-volume census (W3.2). Counts durable bytes
    /// written to `data:` vs the `v:`/`v_idx:` version chain vs secondary-index
    /// keys, split by statement class, and surfaces the `heliosdb_write_volume`
    /// system view. Runtime-only (no build feature); zero-cost when off — one
    /// relaxed atomic load per write funnel. Off by default: diagnostic only.
    pub write_volume_stats: bool,
    /// COPY fast-path phase-timing census (W3.4). Splits the COPY bulk-insert
    /// funnel's wall time across its phases (wire decode, type conversion,
    /// constraint checks, WriteBatch build, durable commit, ART maintenance) and
    /// surfaces the `heliosdb_copy_phase_stats` system view, to measure the ART-
    /// maintenance share of a COPY. Runtime-only (no build feature); zero-cost
    /// when off — one relaxed atomic load per phase boundary. Off by default:
    /// diagnostic only.
    pub copy_phase_stats: bool,
    /// Rows staged per chunk when `INSERT … SELECT` runs INSIDE a transaction
    /// (#100). Validated rows are collected in chunks of this size and handed
    /// to the transactional multi-row insert path, so `ROLLBACK` removes them
    /// and a failure on row N removes rows 1..N-1. Bounds the transient
    /// per-chunk buffer; the transaction's write set still holds every row
    /// until COMMIT, exactly as for a multi-row `INSERT … VALUES`.
    /// `0` = a single chunk. Default 1000.
    #[serde(default = "default_insert_select_txn_batch_rows")]
    pub insert_select_txn_batch_rows: usize,
}

fn default_insert_select_txn_batch_rows() -> usize {
    1000
}

impl Default for PerformanceConfig {
    fn default() -> Self {
        Self {
            worker_threads: num_cpus::get(),
            query_timeout_secs: 300,
            simd_enabled: true,
            parallel_query: true,
            lock_census: false,
            write_volume_stats: false,
            copy_phase_stats: false,
            insert_select_txn_batch_rows: default_insert_select_txn_batch_rows(),
        }
    }
}

// Add num_cpus to dependencies
// For now use a simple fallback
mod num_cpus {
    pub fn get() -> usize {
        std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4)
    }
}

/// Optimizer configuration (v2.1)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct OptimizerConfig {
    /// Enable query optimizer
    pub enabled: bool,
    /// Enable sequential scan
    pub enable_seqscan: bool,
    /// Enable index scan
    pub enable_indexscan: bool,
    /// Enable hash join
    pub enable_hashjoin: bool,
    /// Enable merge join
    pub enable_mergejoin: bool,
    /// Enable nested loop join
    pub enable_nestloop: bool,
    /// Cost model parameters
    pub seq_page_cost: f64,
    pub random_page_cost: f64,
    pub cpu_tuple_cost: f64,
    pub cpu_index_tuple_cost: f64,
}

impl Default for OptimizerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            enable_seqscan: true,
            enable_indexscan: true,
            enable_hashjoin: true,
            enable_mergejoin: true,
            enable_nestloop: true,
            seq_page_cost: 1.0,
            random_page_cost: 4.0,
            cpu_tuple_cost: 0.01,
            cpu_index_tuple_cost: 0.005,
        }
    }
}

/// Authentication configuration (v2.1)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AuthenticationConfig {
    /// Enable authentication
    pub enabled: bool,
    /// Authentication method
    pub method: AuthMethod,
    /// JWT secret key (for JWT auth)
    pub jwt_secret: Option<String>,
    /// JWT expiration time (seconds)
    pub jwt_expiration_secs: u64,
    /// Password hash algorithm
    pub password_hash_algorithm: PasswordHashAlgorithm,
    /// Users file path (for file-based auth)
    pub users_file: Option<PathBuf>,
    /// Compatibility escape for the pre-4.20 `GRANT`/`REVOKE` behaviour.
    ///
    /// **HeliosDB does not enforce SQL privileges either way.** Roles and
    /// grants are persisted and introspectable (`pg_roles`, `pg_authid`,
    /// `information_schema.table_privileges`, MySQL `SHOW GRANTS`); no
    /// privilege is checked at any read or write path. This flag only decides
    /// how *strict* the statements are about names.
    ///
    /// * `false` (default): `GRANT`/`REVOKE` naming a role or table that does
    ///   not exist raise an error, and `SET ROLE <x>` over the PostgreSQL wire
    ///   raises `0A000 feature_not_supported` instead of silently acking — on
    ///   the simple query path AND the extended (Parse/Bind/Execute) path, which
    ///   is what psycopg3 / JDBC / sqlx / node-postgres use.
    /// * `true`: unknown grantees/objects are skipped and the statement
    ///   succeeds (grants whose grantee AND object exist are still stored),
    ///   and `SET ROLE` goes back to the silent `SET` acknowledgement.
    ///
    /// Set it to `true` only to unblock a pre-existing script or restore that
    /// depended on the old always-succeed behaviour. The old `SET ROLE` ack in
    /// particular was a false claim that the session had dropped privileges.
    pub legacy_acl_noop: bool,
}

impl Default for AuthenticationConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            method: AuthMethod::Trust,
            jwt_secret: None,
            jwt_expiration_secs: 86400, // 24 hours
            password_hash_algorithm: PasswordHashAlgorithm::Argon2,
            users_file: None,
            // Strict by default: a GRANT that names a nonexistent role is a
            // bug in the caller, and a silently-acked SET ROLE is a false
            // security claim. Opt back in only for compatibility.
            legacy_acl_noop: false,
        }
    }
}

/// Authentication method
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AuthMethod {
    /// No authentication (dev mode)
    Trust,
    /// Password authentication
    Password,
    /// JWT authentication
    Jwt,
    /// LDAP authentication
    Ldap,
}

/// Password hash algorithm
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PasswordHashAlgorithm {
    /// Argon2 (recommended)
    Argon2,
    /// BCrypt
    Bcrypt,
    /// PBKDF2
    Pbkdf2,
}

/// Compression configuration (v2.1)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CompressionConfig {
    /// Default compression type
    pub default_type: CompressionType,
    /// Compression level (1-22 for zstd)
    pub level: i32,
    /// Enable ALP compression for numeric columns
    pub enable_alp: bool,
    /// Enable FSST compression for string columns
    pub enable_fsst: bool,
    /// Minimum data size to trigger compression (bytes)
    pub min_size_bytes: usize,
}

impl Default for CompressionConfig {
    fn default() -> Self {
        Self {
            default_type: CompressionType::Zstd,
            level: 3,
            enable_alp: true,
            enable_fsst: true,
            min_size_bytes: 1024, // 1KB
        }
    }
}

/// Materialized view configuration (v2.3 - Incremental MVs)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MaterializedViewConfig {
    /// Enable auto-refresh by default
    pub auto_refresh_default: bool,
    /// Default max CPU percentage for refresh
    pub default_max_cpu_percent: u8,
    /// Refresh check interval (seconds)
    pub refresh_check_interval_secs: u64,
    /// Maximum concurrent refreshes
    pub max_concurrent_refreshes: usize,
    /// Enable incremental refresh feature (v2.3.0)
    pub enable_incremental: bool,
    /// Enable delta tracking for base tables (v2.3.0)
    pub enable_delta_tracking: bool,
    /// Enable CPU-aware scheduling (v2.3.0)
    pub enable_scheduler: bool,
    /// Delta retention period in hours (purge old deltas)
    pub delta_retention_hours: u64,
}

impl Default for MaterializedViewConfig {
    fn default() -> Self {
        Self {
            auto_refresh_default: false,
            default_max_cpu_percent: 15,
            refresh_check_interval_secs: 60,
            max_concurrent_refreshes: 2,
            enable_incremental: false,    // Disabled by default, opt-in
            enable_delta_tracking: false, // Disabled by default
            enable_scheduler: false,      // Disabled by default
            delta_retention_hours: 168,   // 7 days
        }
    }
}

/// Vector index configuration (v2.1)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct VectorConfig {
    /// Default vector index type
    pub default_index_type: VectorIndexType,
    /// HNSW ef_construction parameter
    pub hnsw_ef_construction: usize,
    /// HNSW M parameter (connections per layer)
    pub hnsw_m: usize,
    /// Enable product quantization
    pub enable_pq: bool,
    /// PQ subvector count
    pub pq_subvectors: usize,
    /// PQ bits per subvector
    pub pq_bits: usize,
}

impl Default for VectorConfig {
    fn default() -> Self {
        Self {
            default_index_type: VectorIndexType::Hnsw,
            hnsw_ef_construction: 200,
            hnsw_m: 16,
            enable_pq: true,
            pq_subvectors: 8,
            pq_bits: 8,
        }
    }
}

/// Vector index type
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VectorIndexType {
    /// Flat (brute force, exact)
    Flat,
    /// HNSW (approximate, fast)
    Hnsw,
    /// IVF (inverted file)
    Ivf,
}

/// Sync configuration (v2.3 - Experimental)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SyncConfig {
    /// Enable sync protocol
    pub enabled: bool,
    /// Node ID for this instance (generated if None)
    pub node_id: Option<String>,
    /// Sync server URL (for clients)
    pub server_url: Option<String>,
    /// Client ID (for authentication)
    pub client_id: Option<String>,
    /// Sync interval in seconds
    pub sync_interval_secs: u64,
    /// Enable change log capture
    pub change_log_enabled: bool,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            node_id: None,
            server_url: None,
            client_id: None,
            sync_interval_secs: 30,
            change_log_enabled: true,
        }
    }
}

/// Session configuration (v3.1.0)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionConfig {
    /// Session timeout in seconds (default: 3600 = 1 hour)
    pub timeout_secs: u64,
    /// Maximum sessions per user (default: 10)
    pub max_sessions_per_user: u32,
    /// Session cleanup interval in seconds (default: 300 = 5 minutes)
    pub cleanup_interval_secs: u64,
    /// Maximum nested user-defined-function invocations on one thread
    /// (default: 32).
    ///
    /// A UDF body re-enters the parser/planner/executor, so a self-recursive
    /// function would otherwise recurse until the thread's stack is exhausted —
    /// an abort, not an error. When the limit is reached the call fails loudly
    /// with a message naming this setting. Raise it if a legitimately deep
    /// nesting of functions is needed; lower it to fail faster on runaway
    /// recursion. Must be at least 1 (0 would make every UDF call fail).
    pub udf_max_call_depth: u32,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            timeout_secs: 3600,
            max_sessions_per_user: 10,
            cleanup_interval_secs: 300,
            udf_max_call_depth: 32,
        }
    }
}

impl SessionConfig {
    /// Validate configuration values
    pub fn validate(&self) -> crate::Result<()> {
        if self.timeout_secs < 1 {
            return Err(crate::Error::config("session.timeout_secs must be at least 1 second"));
        }
        if self.max_sessions_per_user < 1 {
            return Err(crate::Error::config("session.max_sessions_per_user must be at least 1"));
        }
        if self.cleanup_interval_secs < 1 {
            return Err(crate::Error::config(
                "session.cleanup_interval_secs must be at least 1 second",
            ));
        }
        if self.udf_max_call_depth < 1 {
            return Err(crate::Error::config(
                "session.udf_max_call_depth must be at least 1 (0 would refuse every function call)",
            ));
        }
        Ok(())
    }
}

/// Lock configuration (v3.1.0)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LockConfig {
    /// Pessimistic row-lock acquisition timeout in milliseconds (default: 1000).
    ///
    /// A same-row write-write waiter can never be granted the lock while the
    /// holder stays open, so this bounds a *futile* spin (see the futility note
    /// on `storage::LockManager::with_default_timeout`): a short bound turns a
    /// server-wide stall into a fast, retriable serialization failure (40001).
    /// The env override `NANO_LOCK_TIMEOUT_MS` takes precedence over this value
    /// for backward compatibility (env > config > built-in default).
    pub timeout_ms: u32,
    /// Deadlock detection interval in milliseconds (default: 100)
    pub deadlock_check_interval_ms: u32,
    /// Maximum number of concurrent lock holders (default: 10000)
    pub max_lock_holders: u32,
    /// W3.3: maximum automatic retries for an AUTOCOMMIT statement that hits a
    /// same-row write conflict (serialization failure, SQLSTATE 40001) against
    /// a fresh snapshot. `0` = OFF (surface 40001, today's behavior). Explicit
    /// transactions never auto-retry regardless of this setting — the client
    /// owns their retry (PostgreSQL contract). Default: 0.
    pub statement_retry_max: u32,
    /// W3.3: base backoff between statement retries in milliseconds; grows
    /// exponentially with full jitter up to `statement_retry_backoff_max_ms`
    /// (anti-livelock). Default: 5.
    pub statement_retry_backoff_ms: u64,
    /// W3.3: cap on the backoff sleep per statement retry (milliseconds).
    /// Default: 100.
    pub statement_retry_backoff_max_ms: u64,
    /// Maximum number of DISTINCT advisory-lock keys one session may hold at
    /// once (`pg_advisory_lock` family). `0` = unlimited.
    ///
    /// The advisory-lock table is process-global and grows only when a client
    /// asks it to, so an unbounded table is a client-controlled memory sink —
    /// PostgreSQL bounds the equivalent with `max_locks_per_transaction`
    /// against a fixed shared-memory pool. Exceeding the cap REFUSES the new
    /// lock (fails closed) rather than evicting someone else's. Re-acquiring a
    /// key the session already holds is never refused. Default: 1024.
    pub max_advisory_locks_per_session: u32,
}

impl Default for LockConfig {
    fn default() -> Self {
        Self {
            // 1000 ms is the effective production default the futility note
            // established (previously only reachable via NANO_LOCK_TIMEOUT_MS;
            // W3.3 wires this config value to the LockManager too).
            timeout_ms: 1000,
            deadlock_check_interval_ms: 100,
            max_lock_holders: 10000,
            statement_retry_max: 0,
            statement_retry_backoff_ms: 5,
            statement_retry_backoff_max_ms: 100,
            max_advisory_locks_per_session: 1024,
        }
    }
}

impl LockConfig {
    /// Validate configuration values
    pub fn validate(&self) -> crate::Result<()> {
        if self.timeout_ms < 100 {
            return Err(crate::Error::config(
                "locks.timeout_ms must be at least 100 milliseconds",
            ));
        }
        if self.deadlock_check_interval_ms < 10 {
            return Err(crate::Error::config(
                "locks.deadlock_check_interval_ms must be at least 10 milliseconds",
            ));
        }
        if self.max_lock_holders < 1 {
            return Err(crate::Error::config("locks.max_lock_holders must be at least 1"));
        }
        // The backoff cap must not sit below the base, or the full-jitter
        // exponential backoff would have an empty range to draw from.
        if self.statement_retry_backoff_max_ms < self.statement_retry_backoff_ms {
            return Err(crate::Error::config(
                "locks.statement_retry_backoff_max_ms must be >= locks.statement_retry_backoff_ms",
            ));
        }
        Ok(())
    }
}

/// Dump configuration (v3.1.0)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DumpConfig {
    /// Enable automatic dumps on a schedule (default: false)
    pub auto_dump_enabled: bool,
    /// Cron-style schedule for automatic dumps (e.g., "0 */6 * * *")
    pub schedule: String,
    /// Compression algorithm: "zstd", "gzip", "none" (default: "zstd")
    pub compression: String,
    /// Maximum size of a single dump file in MB before rolling (default: 10000)
    pub max_dump_size_mb: u64,
    /// Number of old dumps to keep (0 = keep all, default: 10)
    pub keep_dumps: usize,
    /// Directory to store dumps (default: ".dumps")
    pub dump_dir: String,
}

impl Default for DumpConfig {
    fn default() -> Self {
        Self {
            auto_dump_enabled: false,
            schedule: String::new(),
            compression: "zstd".to_string(),
            max_dump_size_mb: 10000,
            keep_dumps: 10,
            dump_dir: ".dumps".to_string(),
        }
    }
}

impl DumpConfig {
    /// Validate configuration values
    pub fn validate(&self) -> crate::Result<()> {
        // Validate compression type
        match self.compression.as_str() {
            "zstd" | "gzip" | "none" => {}
            _ => {
                return Err(crate::Error::config(format!(
                    "dump.compression must be 'zstd', 'gzip', or 'none', got '{}'",
                    self.compression
                )));
            }
        }

        // Validate max dump size
        if self.max_dump_size_mb < 1 {
            return Err(crate::Error::config("dump.max_dump_size_mb must be at least 1 MB"));
        }

        // Validate cron schedule if auto_dump_enabled
        if self.auto_dump_enabled && !self.schedule.is_empty() {
            Self::validate_cron_schedule(&self.schedule)?;
        }

        Ok(())
    }

    /// Validate cron schedule format (basic validation)
    fn validate_cron_schedule(schedule: &str) -> crate::Result<()> {
        let parts: Vec<&str> = schedule.split_whitespace().collect();
        if parts.len() != 5 {
            return Err(crate::Error::config(format!(
                "Invalid cron schedule format '{}'. Expected 5 fields: minute hour day month weekday",
                schedule
            )));
        }
        Ok(())
    }
}

/// Resource quota configuration (v3.1.0)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ResourceQuotaConfig {
    /// Memory limit per user in MB (default: 1024)
    pub memory_limit_per_user_mb: u64,
    /// Maximum concurrent queries per user (default: 100)
    pub max_concurrent_queries: u32,
    /// Query execution timeout in seconds (default: 300 = 5 minutes)
    pub query_timeout_secs: u64,
}

impl Default for ResourceQuotaConfig {
    fn default() -> Self {
        Self {
            memory_limit_per_user_mb: 1024,
            max_concurrent_queries: 100,
            query_timeout_secs: 300,
        }
    }
}

impl ResourceQuotaConfig {
    /// Validate configuration values
    pub fn validate(&self) -> crate::Result<()> {
        if self.memory_limit_per_user_mb < 1 {
            return Err(crate::Error::config(
                "resource_quotas.memory_limit_per_user_mb must be at least 1 MB",
            ));
        }
        if self.max_concurrent_queries < 1 {
            return Err(crate::Error::config(
                "resource_quotas.max_concurrent_queries must be at least 1",
            ));
        }
        if self.query_timeout_secs < 1 {
            return Err(crate::Error::config(
                "resource_quotas.query_timeout_secs must be at least 1 second",
            ));
        }
        Ok(())
    }
}

/// BaaS REST API configuration (v3.2.0)
///
/// Controls the PostgREST-compatible REST API layer that exposes tables
/// as RESTful endpoints at `/rest/v1/`.
///
/// # Example (TOML)
///
/// ```toml
/// [api]
/// jwt_secret = "super-secret-key"
/// anon_key = "eyJhbGciOi..."
/// service_role_key = "eyJhbGciOi..."
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ApiConfig {
    /// JWT secret used to sign and verify authentication tokens.
    ///
    /// If not set, a random 256-bit hex string is generated at startup and
    /// [`ApiConfig::jwt_secret_is_ephemeral`] is true.
    pub jwt_secret: String,
    /// True when `jwt_secret` was GENERATED rather than configured.
    ///
    /// Not deserialized from the file — it is derived in
    /// [`Config::from_toml_str`] by asking the raw TOML document whether
    /// `[api] jwt_secret` was actually present, because after deserialization a
    /// generated value is indistinguishable from a configured one.
    ///
    /// The server warns when this is true: an ephemeral key means every token it
    /// issues stops verifying at restart. The alternative — a constant fallback
    /// secret — is not on the table, because a published signing key lets anyone
    /// mint a valid session (the same shape as the `--auth md5` fail-open fixed
    /// in v4.26.0).
    #[serde(skip)]
    pub jwt_secret_is_ephemeral: bool,
    /// Anonymous API key that grants unauthenticated read access.
    pub anon_key: Option<String>,
    /// Service-role API key that grants full admin access (bypasses RLS).
    pub service_role_key: Option<String>,
    /// OAuth2 provider configurations (Google, GitHub, etc.).
    ///
    /// # Example (TOML)
    ///
    /// ```toml
    /// [[api.oauth_providers]]
    /// name = "google"
    /// client_id = "123456.apps.googleusercontent.com"
    /// client_secret = "GOCSPX-..."
    /// redirect_uri = "http://localhost:8080/auth/v1/callback"
    ///
    /// [[api.oauth_providers]]
    /// name = "github"
    /// client_id = "Iv1.abc123"
    /// client_secret = "deadbeef..."
    /// redirect_uri = "http://localhost:8080/auth/v1/callback"
    /// ```
    #[serde(default)]
    pub oauth_providers: Vec<OAuthProviderConfig>,
}

/// Configuration for a single OAuth2 provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthProviderConfig {
    /// Provider name: `"google"` or `"github"`.
    pub name: String,
    /// OAuth2 Client ID issued by the provider.
    pub client_id: String,
    /// OAuth2 Client Secret issued by the provider.
    pub client_secret: String,
    /// The absolute URL the provider should redirect back to after authorization.
    pub redirect_uri: String,
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            jwt_secret: generate_random_secret(),
            // Default() means nobody configured one.
            jwt_secret_is_ephemeral: true,
            anon_key: None,
            service_role_key: None,
            oauth_providers: Vec::new(),
        }
    }
}

/// Generate a random 64-character hex string (256 bits) for use as a JWT secret.
///
/// This value SIGNS AUTHENTICATION TOKENS once the HTTP API is mounted, so it is
/// generated from the OS entropy source via `rand::thread_rng` (a CSPRNG).
///
/// The previous implementation derived it from two `RandomState` hashers and then
/// wrote them out TWICE — `{h1}{h2}{h1}{h2}`. That is 128 bits of entropy formatted
/// to look like 256, from a source seeded for HashDoS resistance rather than for
/// key generation. It signed nothing at the time, because the API router was never
/// mounted; it does now, so it has to be a real key.
pub fn generate_jwt_secret() -> String {
    generate_random_secret()
}

fn generate_random_secret() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes.iter().fold(String::with_capacity(64), |mut acc, b| {
        use std::fmt::Write;
        let _ = write!(acc, "{b:02x}");
        acc
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // ------------------------------------------------------------------
    // D3: profile bundles
    // ------------------------------------------------------------------

    #[test]
    fn test_profile_fast_applies_bundle() {
        let config = Config::from_toml_str("profile = \"fast\"").expect("parse");
        assert_eq!(config.profile, Some(ProfileConfig::Fast));
        assert_eq!(config.storage.wal_sync_mode, WalSyncModeConfig::GroupCommit);
        assert!(!config.storage.time_travel_enabled);
        assert!(!config.storage.durable_commit);
    }

    #[test]
    fn test_profile_balanced_applies_bundle() {
        let config = Config::from_toml_str("profile = \"balanced\"").expect("parse");
        assert_eq!(config.storage.wal_sync_mode, WalSyncModeConfig::GroupCommit);
        assert!(config.storage.time_travel_enabled);
    }

    #[test]
    fn test_profile_agent_applies_bundle() {
        // D6: the agent bundle = group-commit writes + time-travel ON
        // (AS OF reads and AS OF branch anchors must keep working).
        let config = Config::from_toml_str("profile = \"agent\"").expect("parse");
        assert_eq!(config.profile, Some(ProfileConfig::Agent));
        assert_eq!(config.storage.wal_sync_mode, WalSyncModeConfig::GroupCommit);
        assert!(config.storage.time_travel_enabled);
        // durable_commit untouched (defaults to false, but an explicit
        // operator value must never be downgraded — see next test).
        assert!(!config.storage.durable_commit);
        assert_eq!(ProfileConfig::Agent.as_str(), "agent");
    }

    #[test]
    fn test_agent_profile_does_not_downgrade_durable_commit() {
        let config = Config::from_toml_str(
            r#"
            profile = "agent"

            [storage]
            durable_commit = true
            "#,
        )
        .expect("parse");
        assert!(config.storage.durable_commit);
        // Bundled fields still come from the profile.
        assert_eq!(config.storage.wal_sync_mode, WalSyncModeConfig::GroupCommit);
        assert!(config.storage.time_travel_enabled);
    }

    #[test]
    fn test_profile_safe_matches_defaults() {
        let config = Config::from_toml_str("profile = \"safe\"").expect("parse");
        let defaults = Config::default();
        assert_eq!(config.storage.wal_sync_mode, defaults.storage.wal_sync_mode);
        assert_eq!(config.storage.time_travel_enabled, defaults.storage.time_travel_enabled);
        assert_eq!(config.storage.durable_commit, defaults.storage.durable_commit);
    }

    #[test]
    fn test_explicit_field_wins_over_profile() {
        // Precedence: explicit [storage] keys in the file beat the profile.
        let config = Config::from_toml_str(
            r#"
            profile = "fast"

            [storage]
            time_travel_enabled = true
            wal_sync_mode = "sync"
            "#,
        )
        .expect("parse");
        assert!(config.storage.time_travel_enabled, "explicit field must win");
        assert_eq!(config.storage.wal_sync_mode, WalSyncModeConfig::Sync);
        // Non-explicit bundled field still comes from the profile.
        assert!(!config.storage.durable_commit);
    }

    #[test]
    fn test_safe_profile_does_not_downgrade_durable_commit() {
        let config = Config::from_toml_str(
            r#"
            profile = "safe"

            [storage]
            durable_commit = true
            "#,
        )
        .expect("parse");
        assert!(config.storage.durable_commit);
    }

    #[test]
    fn test_no_profile_keeps_defaults() {
        let config = Config::from_toml_str("").expect("parse");
        assert_eq!(config.profile, None);
        assert_eq!(config.storage.wal_sync_mode, WalSyncModeConfig::Sync);
        assert!(config.storage.time_travel_enabled);
    }

    #[test]
    fn test_with_profile_constructor() {
        let config = Config::with_profile(ProfileConfig::Fast);
        assert_eq!(config.profile, Some(ProfileConfig::Fast));
        assert_eq!(config.storage.wal_sync_mode, WalSyncModeConfig::GroupCommit);
        assert!(!config.storage.time_travel_enabled);
    }

    #[test]
    fn test_unknown_profile_rejected() {
        let err = Config::from_toml_str("profile = \"ludicrous\"").expect_err("must fail");
        assert!(err.to_string().contains("Failed to parse config"));
    }

    #[test]
    fn test_config_example_toml_parses() {
        // The shipped example config must always load (D3 validation gate).
        let content = include_str!("../config.example.toml");
        let config = Config::from_toml_str(content).expect("config.example.toml must parse");
        config.validate().expect("config.example.toml must validate");
        // The example documents the safe profile semantics explicitly.
        assert_eq!(config.storage.wal_sync_mode, WalSyncModeConfig::Sync);
    }

    // ------------------------------------------------------------------
    // #103: SERIALIZABLE honesty knob
    // ------------------------------------------------------------------

    #[test]
    fn test_serializable_policy_defaults_to_warn() {
        // The default must never be "accept silently": a client asking for a
        // level the engine does not implement has to be told.
        assert_eq!(StorageConfig::default().serializable_policy, SerializablePolicy::Warn);
    }

    #[test]
    fn test_serializable_policy_parses_error_and_rejects_silence() {
        let config = Config::from_toml_str("[storage]\nserializable_policy = \"error\"\n").expect("parse");
        assert_eq!(config.storage.serializable_policy, SerializablePolicy::Error);

        let config = Config::from_toml_str("[storage]\nserializable_policy = \"warn\"\n").expect("parse");
        assert_eq!(config.storage.serializable_policy, SerializablePolicy::Warn);

        // No silent-accept value exists, and asking for one says why.
        let err = Config::from_toml_str("[storage]\nserializable_policy = \"silent\"\n")
            .expect_err("silent must be rejected");
        assert!(err.to_string().contains("silent-accept"), "{}", err);

        // A typo fails config parse instead of falling back to a default.
        let err =
            Config::from_toml_str("[storage]\nserializable_policy = \"warm\"\n").expect_err("typo must be rejected");
        assert!(err.to_string().contains("serializable_policy"), "{}", err);
    }

    #[test]
    fn test_profile_round_trips_through_serialization() {
        let config = Config::with_profile(ProfileConfig::Balanced);
        let toml_str = toml::to_string(&config).expect("serialize");
        assert!(
            toml_str.starts_with("profile = \"balanced\""),
            "profile must be first: {toml_str}"
        );
        let back = Config::from_toml_str(&toml_str).expect("reparse");
        assert_eq!(back.profile, Some(ProfileConfig::Balanced));
        assert_eq!(back.storage.wal_sync_mode, WalSyncModeConfig::GroupCommit);
    }

    #[test]
    fn test_session_config_default() {
        let config = SessionConfig::default();
        assert_eq!(config.timeout_secs, 3600);
        assert_eq!(config.max_sessions_per_user, 10);
        assert_eq!(config.cleanup_interval_secs, 300);
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_session_config_validation() {
        let mut config = SessionConfig::default();

        // Valid config
        assert!(config.validate().is_ok());

        // Invalid timeout
        config.timeout_secs = 0;
        assert!(config.validate().is_err());
        config.timeout_secs = 3600;

        // Invalid max sessions
        config.max_sessions_per_user = 0;
        assert!(config.validate().is_err());
        config.max_sessions_per_user = 10;

        // Invalid cleanup interval
        config.cleanup_interval_secs = 0;
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_lock_config_default() {
        let config = LockConfig::default();
        // W3.3: the default now matches the effective production spin bound
        // (was an orphaned 30000 never wired to the LockManager).
        assert_eq!(config.timeout_ms, 1000);
        assert_eq!(config.deadlock_check_interval_ms, 100);
        assert_eq!(config.max_lock_holders, 10000);
        // W3.3: statement-retry defaults OFF (behavior-preserving).
        assert_eq!(config.statement_retry_max, 0);
        assert_eq!(config.statement_retry_backoff_ms, 5);
        assert_eq!(config.statement_retry_backoff_max_ms, 100);
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_lock_config_retry_backoff_bounds_validation() {
        let mut config = LockConfig::default();
        // A cap below the base leaves the jitter range empty — rejected.
        config.statement_retry_backoff_ms = 200;
        config.statement_retry_backoff_max_ms = 100;
        assert!(config.validate().is_err());
        // Equal base/cap is fine (fixed backoff).
        config.statement_retry_backoff_max_ms = 200;
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_lock_config_retry_knobs_parse_from_toml() {
        let toml_str = r#"
            [locks]
            statement_retry_max = 4
            statement_retry_backoff_ms = 10
            statement_retry_backoff_max_ms = 250
        "#;
        let config: Config = toml::from_str(toml_str).expect("Failed to deserialize config");
        assert_eq!(config.locks.statement_retry_max, 4);
        assert_eq!(config.locks.statement_retry_backoff_ms, 10);
        assert_eq!(config.locks.statement_retry_backoff_max_ms, 250);
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_lock_config_validation() {
        let mut config = LockConfig::default();

        // Valid config
        assert!(config.validate().is_ok());

        // Invalid timeout (too low)
        config.timeout_ms = 50;
        assert!(config.validate().is_err());
        config.timeout_ms = 30000;

        // Invalid deadlock check interval
        config.deadlock_check_interval_ms = 5;
        assert!(config.validate().is_err());
        config.deadlock_check_interval_ms = 100;

        // Invalid max lock holders
        config.max_lock_holders = 0;
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_dump_config_default() {
        let config = DumpConfig::default();
        assert!(!config.auto_dump_enabled);
        assert_eq!(config.schedule, "");
        assert_eq!(config.compression, "zstd");
        assert_eq!(config.max_dump_size_mb, 10000);
        assert_eq!(config.keep_dumps, 10);
        assert_eq!(config.dump_dir, ".dumps");
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_dump_config_validation() {
        let mut config = DumpConfig::default();

        // Valid config
        assert!(config.validate().is_ok());

        // Invalid compression type
        config.compression = "invalid".to_string();
        assert!(config.validate().is_err());
        config.compression = "zstd".to_string();

        // Valid compression types
        config.compression = "gzip".to_string();
        assert!(config.validate().is_ok());
        config.compression = "none".to_string();
        assert!(config.validate().is_ok());
        config.compression = "zstd".to_string();

        // Invalid max dump size
        config.max_dump_size_mb = 0;
        assert!(config.validate().is_err());
        config.max_dump_size_mb = 10000;

        // Valid cron schedule
        config.auto_dump_enabled = true;
        config.schedule = "0 */6 * * *".to_string();
        assert!(config.validate().is_ok());

        // Invalid cron schedule
        config.schedule = "invalid".to_string();
        assert!(config.validate().is_err());

        // Empty schedule with auto_dump_enabled is OK
        config.schedule = "".to_string();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_resource_quota_config_default() {
        let config = ResourceQuotaConfig::default();
        assert_eq!(config.memory_limit_per_user_mb, 1024);
        assert_eq!(config.max_concurrent_queries, 100);
        assert_eq!(config.query_timeout_secs, 300);
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_resource_quota_config_validation() {
        let mut config = ResourceQuotaConfig::default();

        // Valid config
        assert!(config.validate().is_ok());

        // Invalid memory limit
        config.memory_limit_per_user_mb = 0;
        assert!(config.validate().is_err());
        config.memory_limit_per_user_mb = 1024;

        // Invalid max concurrent queries
        config.max_concurrent_queries = 0;
        assert!(config.validate().is_err());
        config.max_concurrent_queries = 100;

        // Invalid query timeout
        config.query_timeout_secs = 0;
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_config_with_new_sections() {
        let config = Config::default();

        // Verify new sections are initialized
        assert_eq!(config.session.timeout_secs, 3600);
        assert_eq!(config.locks.timeout_ms, 1000);
        assert_eq!(config.dump.compression, "zstd");
        assert_eq!(config.resource_quotas.memory_limit_per_user_mb, 1024);

        // Validate entire config
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_config_serialization() {
        let config = Config::default();

        // Serialize to TOML
        let toml_str = toml::to_string(&config).expect("Failed to serialize config");

        // Should contain new sections
        assert!(toml_str.contains("[session]"));
        assert!(toml_str.contains("[locks]"));
        assert!(toml_str.contains("[dump]"));
        assert!(toml_str.contains("[resource_quotas]"));
    }

    #[test]
    fn test_config_deserialization() {
        let toml_str = r#"
            [storage]
            memory_only = true
            wal_enabled = false
            wal_sync_mode = "sync"
            cache_size = 536870912
            compression = "Zstd"
            time_travel_enabled = true
            transaction_isolation = "READ_COMMITTED"

            [session]
            timeout_secs = 7200
            max_sessions_per_user = 20
            cleanup_interval_secs = 600

            [locks]
            timeout_ms = 60000
            deadlock_check_interval_ms = 200
            max_lock_holders = 20000

            [dump]
            auto_dump_enabled = true
            schedule = "0 2 * * *"
            compression = "gzip"
            max_dump_size_mb = 5000
            keep_dumps = 5
            dump_dir = "/var/dumps"

            [resource_quotas]
            memory_limit_per_user_mb = 2048
            max_concurrent_queries = 200
            query_timeout_secs = 600
        "#;

        let config: Config = toml::from_str(toml_str).expect("Failed to deserialize config");

        // Verify session config
        assert_eq!(config.session.timeout_secs, 7200);
        assert_eq!(config.session.max_sessions_per_user, 20);
        assert_eq!(config.session.cleanup_interval_secs, 600);

        // Verify lock config
        assert_eq!(config.locks.timeout_ms, 60000);
        assert_eq!(config.locks.deadlock_check_interval_ms, 200);
        assert_eq!(config.locks.max_lock_holders, 20000);

        // Verify dump config
        assert!(config.dump.auto_dump_enabled);
        assert_eq!(config.dump.schedule, "0 2 * * *");
        assert_eq!(config.dump.compression, "gzip");
        assert_eq!(config.dump.max_dump_size_mb, 5000);
        assert_eq!(config.dump.keep_dumps, 5);
        assert_eq!(config.dump.dump_dir, "/var/dumps");

        // Verify resource quota config
        assert_eq!(config.resource_quotas.memory_limit_per_user_mb, 2048);
        assert_eq!(config.resource_quotas.max_concurrent_queries, 200);
        assert_eq!(config.resource_quotas.query_timeout_secs, 600);

        // Validate entire config
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_config_with_missing_sections() {
        // Config with only storage section (old format)
        let toml_str = r#"
            [storage]
            memory_only = true
            wal_enabled = false
            wal_sync_mode = "sync"
            cache_size = 536870912
            compression = "Zstd"
            time_travel_enabled = true
            transaction_isolation = "READ_COMMITTED"
        "#;

        let config: Config = toml::from_str(toml_str).expect("Failed to deserialize config");

        // New sections should have default values
        assert_eq!(config.session.timeout_secs, 3600);
        assert_eq!(config.locks.timeout_ms, 1000);
        assert_eq!(config.dump.compression, "zstd");
        assert_eq!(config.resource_quotas.memory_limit_per_user_mb, 1024);

        // Should still validate
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_cron_schedule_validation() {
        // Valid cron schedules
        assert!(DumpConfig::validate_cron_schedule("0 */6 * * *").is_ok());
        assert!(DumpConfig::validate_cron_schedule("0 2 * * *").is_ok());
        assert!(DumpConfig::validate_cron_schedule("*/15 * * * *").is_ok());
        assert!(DumpConfig::validate_cron_schedule("0 0 1 * *").is_ok());
        assert!(DumpConfig::validate_cron_schedule("30 3 * * 0").is_ok());

        // Invalid cron schedules
        assert!(DumpConfig::validate_cron_schedule("invalid").is_err());
        assert!(DumpConfig::validate_cron_schedule("0 * * *").is_err()); // Only 4 fields
        assert!(DumpConfig::validate_cron_schedule("0 * * * * *").is_err()); // 6 fields
        assert!(DumpConfig::validate_cron_schedule("").is_err()); // Empty
    }
}
