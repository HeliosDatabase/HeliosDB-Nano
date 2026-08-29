//! Catalog management for table metadata
//!
//! Handles table schemas, row IDs, and metadata storage in RocksDB.

use super::compression::{CompressionConfig, CompressionStats};
use super::statistics::TableStatistics;
use super::StorageEngine;
use crate::sql::{TriggerDefinition, TriggerPersistence};
use crate::{DataType, Error, Result, Schema, Value};
use serde::{Deserialize, Serialize};

/// Persisted CREATE INDEX definition.
///
/// The actual ART and non-persistent HNSW structures are rebuilt in memory on
/// open; this record is the version-portable catalog entry that survives a
/// binary swap against the same data directory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedIndexDefinition {
    pub table_name: String,
    pub column_name: String,
    pub index_type: Option<String>,
    #[serde(default)]
    pub options: Vec<crate::sql::logical_plan::IndexOption>,
}

/// Which physical structure owns an index — the ONE mapping from an index-type
/// tag to the manager that builds, rebuilds and drops it.
///
/// See [`index_family`] for why this exists as a shared classifier rather than
/// as a `matches!` repeated at each site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexFamily {
    /// `ArtIndexManager` — the in-memory adaptive radix tree behind every
    /// ordinary secondary index (`art`, and the `btree` / `hash` spellings the
    /// planner passes through from `USING`).
    Art,
    /// `VectorIndexManager` — HNSW in every flavour (in-memory, PQ-quantized,
    /// RocksDB-backed persistent).
    Vector,
    /// Accepted for syntax compatibility and DELIBERATELY builds nothing
    /// (`gin` / `gist`: the `@@` operator scans). The catalog record IS the
    /// whole index, so creating and dropping one is a pure catalog operation.
    DdlOnly,
}

/// Classify an index-type tag into the structure that owns it. `None` is the
/// pre-v3.37.2 legacy record shape and means [`IndexFamily::Art`]; an unknown
/// tag returns `None` so the caller can fail by name instead of guessing.
///
/// # Why this is shared
///
/// The mapping had drifted into three independent copies — `handle_create_index`
/// (which branch builds the index), `Catalog::rebuild_vector_indexes` (which
/// records to reopen at open) and `ddl::handle_drop_index` (which manager to
/// call). `CREATE INDEX … USING hnsw … WITH (persistent = true)` persists
/// `persistent_hnsw`, which the create and rebuild copies knew about and the
/// drop copy did not — making such an index permanently UNDROPPABLE with an
/// error that told the user their catalog was corrupt. One classifier means a
/// future index type cannot be added to one site and forgotten at another.
///
/// # The accepted tags
///
/// Every literal here is a tag some caller actually writes. Verified against
/// every `persist_index_definition` call site (`sql/executor/ddl.rs`: `art`,
/// `gin`/`gist`, `hnsw`, `hnsw_pq`, `persistent_hnsw`) plus WAL replay
/// (`WalOperation::CreateIndex`, which round-trips whatever CREATE logged) and
/// the legacy untagged shape. `btree` / `hash` are never PERSISTED (the ART
/// branch normalizes them to `art`) but ARE accepted spellings of `USING`, so
/// they belong here too — this function classifies both the requested spelling
/// and the persisted tag, and the two vocabularies overlap.
///
/// `persistent_pq_hnsw` / `persistent_hnsw_pq` are deliberately absent: they are
/// DISPLAY spellings only (`EmbeddedDatabase::get_vector_store`,
/// `embedded_db_dump`), never written to a `meta:index:` record — a persistent
/// index with PQ enabled is persisted as `persistent_hnsw`. If that ever
/// changes, add them HERE and both sites follow.
pub fn index_family(tag: Option<&str>) -> Option<IndexFamily> {
    match tag {
        None | Some("art") | Some("btree") | Some("hash") => Some(IndexFamily::Art),
        Some("gin") | Some("gist") => Some(IndexFamily::DdlOnly),
        Some("hnsw") | Some("hnsw_pq") | Some("persistent_hnsw") => Some(IndexFamily::Vector),
        Some(_) => None,
    }
}

/// On-disk format tag for a persisted index definition: a 4-byte magic plus a
/// single version byte, written ahead of the bincode body. The tag lets a
/// future format change be *detected* (and skipped/migrated) rather than
/// silently aborting the whole index rebuild — which is the failure mode behind
/// the upgrade bug where every secondary index vanished. Records written before
/// the tag existed (raw bincode, or the older WAL-replay tuple) are still read
/// via the legacy fallbacks in `decode_persisted_index_definition`.
const INDEX_DEF_MAGIC: &[u8; 4] = b"HIDX";
const INDEX_DEF_FORMAT_VERSION: u8 = 1;

/// Persisted `CREATE SEQUENCE` definition (the immutable-ish config half).
///
/// Split from the high-water counter (`PersistedSeqState`) on purpose, mirroring
/// the engine's own definition-vs-counter split (index definition vs row
/// counter): the definition is written on CREATE/ALTER and rides the normal
/// post-statement durability barrier, while the *state* record is the tiny,
/// hot, explicitly-fsynced record that backs the no-duplicate invariant.
///
/// `name` follows the index/object-name convention (NOT lowercased) so that
/// quoted, case-sensitive sequence names round-trip.
/// Default `CACHE` for a sequence created without an explicit `CACHE` clause
/// (and for auto-vivified sequences). R-D1: each cached block costs exactly one
/// durable high-water fsync, and the no-duplicate-on-crash invariant fsyncs
/// unconditionally (even when `durable_commit=false`), so `CACHE 1` made every
/// `nextval` a serialized fsync — a hard ceiling near one value per
/// fsync-latency (~90/s on a ~11ms-fsync disk) that no group commit could
/// coalesce. Reserving 32 per block cuts that to ~one fsync per 32 values
/// (~32×). This matches PostgreSQL's own durability granularity: PG WAL-logs
/// sequence advances in batches of 32 (`SEQ_LOG_VALS`), so a PG sequence can
/// already gap by up to 32 on a crash regardless of its `CACHE` parameter.
/// Explicit `CREATE SEQUENCE … CACHE n` / `ALTER SEQUENCE … CACHE n` are
/// unaffected. SERIAL columns do not use this path (they fill from the
/// lock-free row counter).
pub const DEFAULT_SEQUENCE_CACHE: i64 = 32;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PersistedSequence {
    pub name: String,
    /// "smallint" | "integer" | "bigint" (default "bigint").
    pub data_type: String,
    pub start_value: i64,
    /// Non-zero. CREATE coerces 0 -> 1; ALTER rejects 0.
    pub increment_by: i64,
    pub min_value: i64,
    pub max_value: i64,
    /// >= 1. The number of values reserved per durable high-water fsync.
    pub cache: i64,
    pub cycle: bool,
    pub owned_by_table: Option<String>,
    pub owned_by_column: Option<String>,
}

impl PersistedSequence {
    /// Inclusive bigint floor (`i64::MIN + 1`) — avoids the negation-UB hazard
    /// of `-i64::MIN` when a descending sequence mirrors the range.
    pub const BIGINT_MIN: i64 = i64::MIN + 1;
    pub const BIGINT_MAX: i64 = i64::MAX;

    /// Type-driven `[min, max]` bounds for a missing MINVALUE/MAXVALUE.
    pub fn type_bounds(data_type: &str) -> (i64, i64) {
        match data_type {
            "smallint" => (i16::MIN as i64, i16::MAX as i64),
            "integer" => (i32::MIN as i64, i32::MAX as i64),
            _ => (Self::BIGINT_MIN, Self::BIGINT_MAX),
        }
    }

    /// A lenient default sequence (bigint, ascending from 1) used by the
    /// auto-vivify path when `nextval` names an unknown sequence.
    pub fn default_named(name: &str) -> Self {
        let (_tmin, tmax) = Self::type_bounds("bigint");
        Self {
            name: name.to_string(),
            data_type: "bigint".to_string(),
            start_value: 1,
            increment_by: 1,
            min_value: 1,
            max_value: tmax,
            cache: DEFAULT_SEQUENCE_CACHE,
            cycle: false,
            owned_by_table: None,
            owned_by_column: None,
        }
    }
}

/// Durable high-water mark for a sequence.
///
/// `last_reserved` is the highest value any block grant has reserved; on
/// restart the runtime resumes strictly PAST this value, so a value is never
/// served twice across a crash. `is_called=false` (after CREATE / `setval(.,
/// false)`) means the next `nextval` returns the start value itself.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct PersistedSeqState {
    pub last_reserved: i64,
    pub is_called: bool,
}

/// On-disk tags for the two sequence records (frame == index-def: magic +
/// version byte + bincode body), so a future format bump is detectable.
const SEQ_DEF_MAGIC: &[u8; 4] = b"HSEQ";
const SEQ_DEF_FORMAT_VERSION: u8 = 1;
const SEQ_STATE_MAGIC: &[u8; 4] = b"HSQS";
const SEQ_STATE_FORMAT_VERSION: u8 = 1;

/// On-disk tags for the role / ACL catalog records (same frame shape).
const ROLE_MAGIC: &[u8; 4] = b"HROL";
const ROLE_FORMAT_VERSION: u8 = 1;
const ACL_MAGIC: &[u8; 4] = b"HACL";
const ACL_FORMAT_VERSION: u8 = 1;

/// On-disk tag for the BEFORE-row trigger rewrite recipe sidecar
/// (`trigger_rowmut:{table}:{trigger}`). Framed for the same reason as the role
/// and ACL records: `TriggerRowMutation` embeds `Vec<TriggerEvent>` and
/// `Vec<(String, LogicalExpr)>`, both bincode-POSITIONAL, so a mid-enum
/// insertion in `LogicalExpr` would otherwise decode as garbage — or, worse,
/// silently as a different expression — instead of being rejected. The frame
/// turns that into a loud, skippable "not in a recognised format".
const TRIGGER_ROWMUT_MAGIC: &[u8; 4] = b"HTRM";
const TRIGGER_ROWMUT_FORMAT_VERSION: u8 = 1;

/// PostgreSQL's `FirstNormalObjectId`. The first OID handed to a
/// user-created role; everything below it is reserved for built-ins.
///
/// A protocol-convention constant, NOT a tunable: drivers and psql compare
/// role OIDs against PostgreSQL's own reserved range, so changing it would
/// break compatibility rather than tune anything.
pub const FIRST_ROLE_OID: u32 = 16_384;

/// OID of the virtual built-in role `postgres`. Virtual = synthesized by the
/// catalog-view builders, never persisted, not creatable and not droppable.
/// Kept at PostgreSQL's bootstrap-superuser OID so existing introspection
/// (which JOINs `pg_namespace.nspowner` / `pg_class.relowner` = 10) keeps
/// resolving an owner name.
pub const BUILTIN_POSTGRES_ROLE_OID: u32 = 10;

/// OID of the virtual built-in role `helios`. See `BUILTIN_POSTGRES_ROLE_OID`.
pub const BUILTIN_HELIOS_ROLE_OID: u32 = 11;

/// The three role names HeliosDB reserves: the two virtual built-ins plus the
/// SQL-standard `PUBLIC` pseudo-role. None can be created, altered or dropped;
/// `public` IS accepted as a GRANT/REVOKE grantee.
pub const RESERVED_ROLE_NAMES: [&str; 3] = ["postgres", "helios", "public"];

/// True if `name` is one of the reserved role names (case-insensitive —
/// unquoted identifiers are already lowercased by the planner, this also
/// catches a quoted `"Postgres"`).
pub fn is_reserved_role_name(name: &str) -> bool {
    RESERVED_ROLE_NAMES.iter().any(|r| name.eq_ignore_ascii_case(r))
}

/// A persisted SQL role (`CREATE ROLE` / `CREATE USER`).
///
/// *** NOT A SECURITY BOUNDARY. *** No code path consults these attribute
/// bits to authorise anything: `rolsuper`, `rolbypassrls` and friends are
/// recorded so `pg_roles` / `pg_authid` can report what was asked for, and
/// nothing else reads them. `password` is likewise stored verbatim and is
/// NOT wired into wire authentication (`AuthManager` still knows only the
/// `--password` users); it is never emitted by any catalog view.
///
/// FIELD DISCIPLINE: this struct is bincode-encoded, which is positional.
/// New fields must be APPENDED at the end (and old records re-read only by a
/// binary that bumps `ROLE_FORMAT_VERSION`), never inserted in the middle.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RoleRecord {
    /// Stable identity, allocated from `meta:role_oid_next` at `FIRST_ROLE_OID`.
    pub oid: u32,
    /// Role name as normalised by the planner (unquoted names are lowercased).
    pub name: String,
    pub rolsuper: bool,
    pub rolinherit: bool,
    pub rolcreaterole: bool,
    pub rolcreatedb: bool,
    pub rolcanlogin: bool,
    pub rolreplication: bool,
    pub rolbypassrls: bool,
    /// `-1` means unlimited (PostgreSQL convention).
    pub rolconnlimit: i64,
    /// Verbatim `VALID UNTIL` text, or `None`.
    pub rolvaliduntil: Option<String>,
    /// Verbatim password as written. NEVER rendered by a catalog view.
    pub password: Option<String>,
}

impl RoleRecord {
    /// A role with PostgreSQL's `CREATE ROLE` defaults: no attributes except
    /// `INHERIT`, no login, unlimited connections.
    pub fn new_default(oid: u32, name: &str) -> Self {
        Self {
            oid,
            name: name.to_string(),
            rolsuper: false,
            rolinherit: true,
            rolcreaterole: false,
            rolcreatedb: false,
            rolcanlogin: false,
            rolreplication: false,
            rolbypassrls: false,
            rolconnlimit: -1,
            rolvaliduntil: None,
            password: None,
        }
    }
}

/// One grantee's privilege set on one object, as recorded by `GRANT` /
/// `REVOKE`.
///
/// *** STORED, INTROSPECTABLE, NOT ENFORCED. *** No DML choke point reads
/// this. A row here means "somebody ran GRANT", never "access is restricted".
///
/// FIELD DISCIPLINE: bincode-positional, append-only — see `RoleRecord`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AclRecord {
    /// `"table"` or `"sequence"` (the only object kinds this slice accepts).
    pub object_type: String,
    /// Resolved storage key of the object (`schema.table` when non-`public`).
    pub object_name: String,
    /// Role name, or the literal `public` pseudo-role.
    pub grantee: String,
    /// Who issued the GRANT. Always `helios` until session identity lands —
    /// the authenticated wire username does not reach SQL yet.
    pub grantor: String,
    /// `(privilege, is_grantable)`, sorted by privilege name.
    pub privileges: Vec<(String, bool)>,
}

fn vector_distance_metric(options: &[crate::sql::logical_plan::IndexOption]) -> crate::vector::DistanceMetric {
    use crate::sql::logical_plan::IndexOption;
    use crate::vector::DistanceMetric;

    options
        .iter()
        .find_map(|option| match option {
            IndexOption::DistanceMetric(name) => match name.as_str() {
                "cosine" => Some(DistanceMetric::Cosine),
                "ip" | "inner_product" => Some(DistanceMetric::InnerProduct),
                _ => Some(DistanceMetric::L2),
            },
            _ => None,
        })
        .unwrap_or(DistanceMetric::L2)
}

/// R5.V6: HNSW construction parameters for an index — persisted
/// `WITH (m = .., ef_construction = ..)` options win, the `[vector]`
/// config section supplies the defaults.
fn hnsw_construction_params(
    options: &[crate::sql::logical_plan::IndexOption],
    config: &crate::config::Config,
) -> (usize, usize) {
    use crate::sql::logical_plan::IndexOption;

    let mut m = config.vector.hnsw_m;
    let mut ef_construction = config.vector.hnsw_ef_construction;
    for option in options {
        match option {
            IndexOption::HnswM(n) => m = *n,
            IndexOption::EfConstruction(n) => ef_construction = *n,
            _ => {}
        }
    }
    (m, ef_construction)
}

/// Catalog manager for table metadata
pub struct Catalog<'a> {
    storage: &'a StorageEngine,
}

impl<'a> Catalog<'a> {
    /// Create a new catalog
    pub fn new(storage: &'a StorageEngine) -> Self {
        Self { storage }
    }

    /// Get a reference to the storage engine
    pub fn storage(&self) -> &'a StorageEngine {
        self.storage
    }

    /// Create a table with the given schema
    pub fn create_table(&self, table_name: &str, schema: Schema) -> Result<()> {
        // Check if table already exists
        if self.table_exists(table_name)? {
            return Err(Error::query_execution(format!("Table '{}' already exists", table_name)));
        }

        // Log CreateTable to WAL first (for replication to standbys)
        // This must happen before the actual table creation so standbys
        // receive and apply the operation in the correct order.
        self.storage.log_create_table(table_name, &schema)?;

        // Store schema
        let key = Self::table_metadata_key(table_name);
        let value =
            bincode::serialize(&schema).map_err(|e| Error::storage(format!("Failed to serialize schema: {}", e)))?;

        self.storage.put(&key, &value)?;

        // Update in-memory schema cache
        self.storage.cache_schema(table_name, schema.clone());

        // Initialize row counter to 0
        let counter_key = Self::table_counter_key(table_name);
        let counter_value =
            bincode::serialize(&0u64).map_err(|e| Error::storage(format!("Failed to serialize counter: {}", e)))?;
        self.storage.put(&counter_key, &counter_value)?;

        // Auto-create ART indexes for PRIMARY KEY and UNIQUE constraints
        let art_manager = self.storage.art_indexes();

        // Collect PRIMARY KEY columns
        let pk_columns: Vec<String> = schema
            .columns
            .iter()
            .filter(|c| c.primary_key)
            .map(|c| c.name.clone())
            .collect();

        if !pk_columns.is_empty() {
            if let Err(e) = art_manager.create_pk_index(table_name, &pk_columns) {
                tracing::warn!("Failed to create PK ART index for table '{}': {}", table_name, e);
            } else {
                tracing::debug!(
                    "Created PK ART index for table '{}' on columns {:?}",
                    table_name,
                    pk_columns
                );
            }
        }

        // Collect UNIQUE columns (non-PK) and create individual UNIQUE indexes
        for col in schema.columns.iter() {
            if col.unique && !col.primary_key {
                let unique_columns = vec![col.name.clone()];
                if let Err(e) = art_manager.create_unique_index(table_name, &unique_columns, Some(&col.name)) {
                    tracing::warn!(
                        "Failed to create UNIQUE ART index for table '{}' column '{}': {}",
                        table_name,
                        col.name,
                        e
                    );
                } else {
                    tracing::debug!(
                        "Created UNIQUE ART index for table '{}' on column '{}'",
                        table_name,
                        col.name
                    );
                }
            }
        }

        // W1.3: a new table flips a prior `Missing` classification; bump so the
        // existence cache recomputes (covers wire/REPL/HTTP/embedded/restore/
        // WAL-recovery, which all funnel through this method).
        self.storage.bump_schema_generation();

        Ok(())
    }

    /// Check if a table exists
    pub fn table_exists(&self, table_name: &str) -> Result<bool> {
        let key = Self::table_metadata_key(table_name);
        Ok(self.storage.get(&key)?.is_some())
    }

    /// Get table schema
    ///
    /// This method first checks if the table exists as a regular table.
    /// If not found, it checks if it exists as a materialized view and
    /// returns the MV's schema if found.
    pub fn get_table_schema(&self, table_name: &str) -> Result<Schema> {
        // Resolve the raw schema (in-memory cache -> on-disk metadata -> materialized view).
        let mut schema = if let Some(schema) = self.storage.get_cached_schema(table_name) {
            schema
        } else {
            let key = Self::table_metadata_key(table_name);
            match self.storage.get(&key)? {
                Some(data) => {
                    let schema: Schema = bincode::deserialize(&data)
                        .map_err(|e| Error::storage(format!("Failed to deserialize schema: {}", e)))?;
                    // Cache for future lookups
                    self.storage.cache_schema(table_name, schema.clone());
                    schema
                }
                None => {
                    // Table not found, check if it's a materialized view
                    let mv_catalog = self.storage.mv_catalog();
                    if mv_catalog.view_exists(table_name)? {
                        let mv_metadata = mv_catalog.get_view(table_name)?;
                        // Cache MV schema too
                        self.storage.cache_schema(table_name, mv_metadata.schema.clone());
                        mv_metadata.schema
                    } else {
                        return Err(Error::query_execution(format!("Table '{}' does not exist", table_name)));
                    }
                }
            }
        };

        // B31 backward-compat: a column always belongs to its table, but tables
        // created by older binaries persisted columns with `source_table_name = None`.
        // That breaks table-qualified column resolution (e.g. `SELECT t.col`) on the
        // schema-derivation path used to build the extended-query RowDescription
        // (`derive_result_schema` -> `LogicalPlan::schema()`): freshly-created tables
        // resolve, but old ones raise `Column 't.col' not found in schema`. Stamp the
        // canonical table name here so every consumer (Describe, planner, evaluator)
        // resolves `t.col` regardless of the on-disk format.
        for col in &mut schema.columns {
            if col.source_table_name.is_none() {
                col.source_table_name = Some(table_name.to_string());
            }
        }
        Ok(schema)
    }

    /// Update table schema (for ALTER TABLE operations)
    ///
    /// Updates the schema metadata for an existing table.
    /// This is used by ALTER TABLE ALTER COLUMN SET STORAGE to
    /// persist storage mode changes.
    pub fn update_table_schema(&self, table_name: &str, schema: &Schema) -> Result<()> {
        // Verify table exists
        if !self.table_exists(table_name)? {
            return Err(Error::query_execution(format!("Table '{}' does not exist", table_name)));
        }

        // Store updated schema
        let key = Self::table_metadata_key(table_name);
        let value =
            bincode::serialize(schema).map_err(|e| Error::storage(format!("Failed to serialize schema: {}", e)))?;

        self.storage.put(&key, &value)?;

        // Update in-memory schema cache
        self.storage.cache_schema(table_name, schema.clone());

        // W2.5 (via the W1.3 primitive): every ALTER variant funnels through
        // here, and a schema-shape change makes the per-table committed-write
        // watermarks untrustworthy — an open-snapshot reader taking the
        // watermark fast path would project the NEW catalog over OLD-shape
        // tuples ("Column index N out of bounds"). bump_schema_generation
        // clears the watermarks (and the existence cache, harmlessly — the
        // name's kind is unchanged), forcing default-closed snapshot reads
        // until the next tracked commit. Caught by
        // w2_5_watermark_read_tests::alter_add_column_hidden_from_open_snapshot_reader.
        self.storage.bump_schema_generation();

        Ok(())
    }

    /// Drop a table
    pub fn drop_table(&self, table_name: &str) -> Result<()> {
        if !self.table_exists(table_name)? {
            return Err(Error::query_execution(format!("Table '{}' does not exist", table_name)));
        }

        // Log DropTable to WAL first (for replication to standbys)
        self.storage.log_drop_table(table_name)?;

        // Drop all ART indexes for this table
        let art_manager = self.storage.art_indexes();
        if let Err(e) = art_manager.drop_table_indexes(table_name) {
            tracing::warn!("Failed to drop ART indexes for table '{}': {}", table_name, e);
        }

        // Invalidate statistics cache
        let cache = self.storage.statistics_cache();
        cache.invalidate(table_name)?;

        // Invalidate schema cache
        self.storage.invalidate_schema_cache(table_name);

        // Batch-delete all metadata keys in a single RocksDB write
        {
            let mut batch = rocksdb::WriteBatch::default();
            batch.delete(Self::table_metadata_key(table_name));
            batch.delete(Self::table_counter_key(table_name));
            batch.delete(Self::compression_config_key(table_name));
            batch.delete(Self::compression_stats_key(table_name));
            self.storage
                .db
                .write(batch)
                .map_err(|e| crate::Error::storage(format!("Batch delete failed: {}", e)))?;
        }

        // W1.3: bump immediately after the metadata delete commits — that is
        // the point where `table_exists` flips false. The remaining steps
        // (data-row deletes, sidecar purge) don't change existence and each can
        // early-return with `?`; bumping here means even those error paths
        // leave no stale `Table` entry for the dropped table.
        self.storage.bump_schema_generation();

        // A dropped table's TRIGGER records die with it — both the
        // `trigger:{table}:*` definitions and the `trigger_rowmut:{table}:*`
        // rewrite recipes.
        //
        // This belongs HERE, in the one funnel every table removal goes
        // through, and not only one layer up in
        // `EmbeddedDatabase::on_table_dropped` (which the two executor
        // families' catch-alls call). Two paths reach a table drop WITHOUT
        // passing through that layer:
        //   * WAL REPLAY (`StorageEngine::apply_wal_operation`'s
        //     `WalOperation::DropTable` arm) calls this directly. The replayed
        //     `WalOperation::CreateTrigger` earlier in the same log RE-WRITES
        //     the `trigger:{table}:` record, so a table dropped after its
        //     trigger was created came back at the next open with the trigger
        //     attached — and then applied that trigger to a NEW table created
        //     under the same name. That is a data-correctness bug, not a leak.
        //   * The Stage-0 partition cascade (`drop_table_and_partition_children`)
        //     recurses through this funnel for CHILD tables, whose names never
        //     appear in the drop plan the upper layer inspects.
        //
        // Best effort by design, exactly like the ART-index teardown above: the
        // table is already gone by this point, and turning a successful
        // DROP TABLE into an error because a cleanup scan failed would be worse
        // than the leak. Failures are warned, loudly and by name.
        if let Err(e) = self.delete_table_triggers(table_name) {
            tracing::warn!("DROP TABLE '{}': failed to delete trigger records: {}", table_name, e);
        }

        // Delete all data rows using prefix seek (jumps directly to table's key range)
        // Key format: data:{table_name}:{row_id}
        let data_prefix = format!("data:{}:", table_name);
        let prefix_bytes = data_prefix.as_bytes();

        // Batch-delete all data rows in a SINGLE RocksDB write (prefix seek,
        // O(rows_in_table) keys, not O(all_keys)).
        //
        // The previous implementation deleted rows one at a time via
        // `self.storage.delete()`, which appends a WAL `Delete` entry per row —
        // and the WAL append issues a synchronous `fdatasync`. So DROP TABLE
        // cost one fsync PER ROW: a 200-row table took ~8s and a Pagila-sized
        // table appeared to hang and monopolized the WAL writer, stalling other
        // sessions. The drop is already WAL-logged as a single DDL op above
        // (`log_drop_table`, replayed on recovery/replication), so the per-row
        // WAL entries were redundant. One batched write is correct and turns
        // O(rows) fsyncs into O(1) — matching the metadata batch just above.
        let mut data_batch = rocksdb::WriteBatch::default();
        let mut read_opts = rocksdb::ReadOptions::default();
        read_opts.set_total_order_seek(false); // Enable prefix-based seek
        let iter = self.storage.db.iterator_opt(
            rocksdb::IteratorMode::From(prefix_bytes, rocksdb::Direction::Forward),
            read_opts,
        );
        for item in iter {
            let (key, _) = item.map_err(|e| Error::storage(format!("Iterator error: {}", e)))?;
            if !key.starts_with(prefix_bytes) {
                break; // Past the prefix range
            }
            data_batch.delete(key);
        }
        self.storage
            .db
            .write(data_batch)
            .map_err(|e| Error::storage(format!("Batch delete of table data failed: {}", e)))?;

        // R3.3: purge columnar sidecars (col:/colz:/colzm:/colp:/colpm:) so a
        // re-created table with the same name never reads stale batches, zone
        // stats or live-row presence. No-op prefix seeks for row-only tables.
        super::ColumnarStore::purge_table_sidecars(&self.storage.db, table_name)?;

        Ok(())
    }

    /// Get next row ID for a table
    pub fn next_row_id(&self, table_name: &str) -> Result<u64> {
        self.storage.next_row_id(table_name)
    }

    /// List every catalogued table as `(schema, name)`.
    /// Default schema is `"public"`; `_hdb_code_*` and `_hdb_graph_*`
    /// flat-prefix tables are reported under their dotted-form
    /// schemas (`_hdb_code` / `_hdb_graph`).  Stable order, sorted
    /// lexicographically.
    pub fn list_tables_qualified(&self) -> Result<Vec<(String, String)>> {
        let names = self.list_tables()?;
        let mut out: Vec<(String, String)> = names
            .into_iter()
            .map(|n| {
                if let Some(rest) = n.strip_prefix("_hdb_code_") {
                    ("_hdb_code".to_string(), rest.to_string())
                } else if let Some(rest) = n.strip_prefix("_hdb_graph_") {
                    ("_hdb_graph".to_string(), rest.to_string())
                } else if let Some(idx) = n.find('.') {
                    let (s, t) = n.split_at(idx);
                    (s.to_string(), t[1..].to_string())
                } else {
                    ("public".to_string(), n)
                }
            })
            .collect();
        out.sort();
        Ok(out)
    }

    /// List the distinct schemas seen across every catalogued
    /// table.  Useful for `pg_namespace` materialisation.
    pub fn list_schemas(&self) -> Result<Vec<String>> {
        use std::collections::BTreeSet;
        let mut s: BTreeSet<String> = BTreeSet::new();
        for (sch, _) in self.list_tables_qualified()? {
            s.insert(sch);
        }
        // A schema DECLARED with `CREATE SCHEMA` but not yet populated exists
        // only as a `meta:schema:` marker (no member-table key to derive it
        // from). Fold those in so an empty schema still materialises a
        // `pg_namespace` row.
        for name in self.list_registered_schemas()? {
            s.insert(name);
        }
        Ok(s.into_iter().collect())
    }

    /// Names of all schemas DECLARED via `CREATE SCHEMA` — the `meta:schema:`
    /// markers — including schemas that have no member tables yet. Does NOT
    /// include schemas implied only by a `schema.table` storage key; callers
    /// wanting the full set use [`Self::list_schemas`], which unions both.
    pub fn list_registered_schemas(&self) -> Result<Vec<String>> {
        let prefix = b"meta:schema:";
        let mut out = Vec::new();
        let mut read_opts = rocksdb::ReadOptions::default();
        read_opts.set_total_order_seek(true);
        let iter = self.storage.db.iterator_opt(
            rocksdb::IteratorMode::From(prefix, rocksdb::Direction::Forward),
            read_opts,
        );
        for item in iter {
            let (key, _) = item.map_err(|e| Error::storage(format!("Iterator error: {}", e)))?;
            if !key.starts_with(prefix) {
                break; // Past the `meta:schema:` range.
            }
            let name = String::from_utf8_lossy(key.get(prefix.len()..).unwrap_or_default()).to_string();
            if !name.is_empty() {
                out.push(name);
            }
        }
        Ok(out)
    }

    /// List all tables in the database
    pub fn list_tables(&self) -> Result<Vec<String>> {
        let prefix = b"meta:table:";
        let mut tables = Vec::new();

        // Use prefix seek to jump directly to "meta:table:" range
        let mut read_opts = rocksdb::ReadOptions::default();
        read_opts.set_total_order_seek(true);
        let iter = self.storage.db.iterator_opt(
            rocksdb::IteratorMode::From(prefix, rocksdb::Direction::Forward),
            read_opts,
        );
        for item in iter {
            let (key, _) = item.map_err(|e| Error::storage(format!("Iterator error: {}", e)))?;
            if !key.starts_with(prefix) {
                break; // Past the prefix range
            }
            let table_name = String::from_utf8_lossy(key.get(prefix.len()..).unwrap_or_default()).to_string();
            tables.push(table_name);
        }

        tables.sort();
        Ok(tables)
    }

    /// Persist a user-created index definition in the catalog.
    pub fn save_index_definition(&self, index_name: &str, definition: &PersistedIndexDefinition) -> Result<()> {
        let key = Self::index_metadata_key(index_name);
        let body = bincode::serialize(definition)
            .map_err(|e| Error::storage(format!("Failed to serialize index definition: {}", e)))?;
        let mut value = Vec::with_capacity(INDEX_DEF_MAGIC.len() + 1 + body.len());
        value.extend_from_slice(INDEX_DEF_MAGIC);
        value.push(INDEX_DEF_FORMAT_VERSION);
        value.extend_from_slice(&body);
        self.storage.put(&key, &value)
    }

    /// Decode one persisted index record, tolerating every on-disk format we
    /// have ever written. Returns `None` (after a `warn!`) for an undecodable
    /// or unknown-version record instead of erroring, so a single bad record
    /// degrades to a scan for *that* index rather than aborting the rebuild of
    /// every other index in the database.
    fn decode_persisted_index_definition(index_name: &str, value: &[u8]) -> Option<PersistedIndexDefinition> {
        // Current tagged format: magic + version byte + bincode(definition).
        if value.len() > INDEX_DEF_MAGIC.len() && value.starts_with(INDEX_DEF_MAGIC) {
            let version = value[INDEX_DEF_MAGIC.len()];
            let body = &value[INDEX_DEF_MAGIC.len() + 1..];
            if version == INDEX_DEF_FORMAT_VERSION {
                match bincode::deserialize::<PersistedIndexDefinition>(body) {
                    Ok(definition) => return Some(definition),
                    Err(e) => {
                        tracing::warn!(
                            "Index rebuild: skipping index '{}' — v{} record failed to decode \
                             ({}); it will fall back to a scan until rebuilt with CREATE INDEX",
                            index_name,
                            version,
                            e
                        );
                        return None;
                    }
                }
            }
            tracing::warn!(
                "Index rebuild: skipping index '{}' — on-disk format version {} is newer than \
                 this binary supports (v{}); REINDEX after upgrading",
                index_name,
                version,
                INDEX_DEF_FORMAT_VERSION
            );
            return None;
        }

        // Legacy untagged format A: raw bincode(PersistedIndexDefinition),
        // written by v3.37.2 .. the introduction of the tag.
        if let Ok(definition) = bincode::deserialize::<PersistedIndexDefinition>(value) {
            return Some(definition);
        }

        // Legacy untagged format B: the old WAL-replay tuple
        // (table, column, index_type, options_bytes).
        if let Ok((table_name, column_name, index_type, options_bytes)) =
            bincode::deserialize::<(String, String, Option<String>, Vec<u8>)>(value)
        {
            let options = if options_bytes.is_empty() {
                Vec::new()
            } else {
                bincode::deserialize(&options_bytes).unwrap_or_default()
            };
            return Some(PersistedIndexDefinition {
                table_name,
                column_name,
                index_type,
                options,
            });
        }

        tracing::warn!(
            "Index rebuild: skipping index '{}' — record ({} bytes) is not decodable in any known \
             format; it will fall back to a scan until rebuilt with CREATE INDEX",
            index_name,
            value.len()
        );
        None
    }

    /// Drop a persisted user-created index definition.
    pub fn drop_index_definition(&self, index_name: &str) -> Result<()> {
        let key = Self::index_metadata_key(index_name);
        self.storage.delete(&key)
    }

    /// Fetch ONE persisted `CREATE INDEX` definition by name.
    ///
    /// `Ok(None)` means "no such user-created index" — which is exactly the
    /// question `DROP INDEX` has to answer, and the reason this is a point `get`
    /// rather than a filter over [`Catalog::list_index_definitions`]: the drop
    /// must not become O(number of indexes), and it must not depend on the sort
    /// the list performs.
    ///
    /// Decoding uses the same [`Catalog::decode_persisted_index_definition`]
    /// every other reader uses, so all on-disk formats are understood
    /// identically — one rule, one implementation. Reads through
    /// [`StorageEngine::get`], so it is correct on an encrypted data directory.
    ///
    /// A record that EXISTS but cannot be decoded returns `Ok(None)`, matching
    /// the rebuild path's per-record resilience. `drop_index_definition` deletes
    /// by key regardless of decodability, so such a record is still removable.
    pub fn get_index_definition(&self, index_name: &str) -> Result<Option<PersistedIndexDefinition>> {
        let key = Self::index_metadata_key(index_name);
        match self.storage.get(&key)? {
            Some(bytes) => Ok(Self::decode_persisted_index_definition(index_name, &bytes)),
            None => Ok(None),
        }
    }

    /// True when a `meta:index:<name>` record exists, whether or not its body
    /// decodes. Distinguishes "no such index" from "unreadable index record",
    /// which `get_index_definition` deliberately collapses.
    pub fn index_definition_exists(&self, index_name: &str) -> Result<bool> {
        let key = Self::index_metadata_key(index_name);
        Ok(self.storage.get(&key)?.is_some())
    }

    /// List persisted CREATE INDEX definitions.
    ///
    /// TDE CORRECTNESS (v4.21.0). This used to read record VALUES straight off
    /// `self.storage.db.iterator_opt`. `save_index_definition` writes through
    /// [`StorageEngine::put`], which ENCRYPTS when a key manager is configured —
    /// so on an encrypted data directory the raw iterator handed back
    /// CIPHERTEXT. Combined with the per-record resilience below (an undecodable
    /// record is warned and SKIPPED, never surfaced as an error), the effect was
    /// silent and total: `rebuild_all_indexes` is the only thing that
    /// re-registers user secondary indexes at open, so on a TDE database EVERY
    /// `CREATE INDEX` index disappeared at EVERY restart. Queries stayed
    /// correct — they just full-scanned forever, with nothing above `warn!` to
    /// say so.
    ///
    /// The fix is the same one applied to `list_roles` / `list_acls`: read
    /// through [`StorageEngine::meta_blobs_with_prefix`], which fetches values
    /// via `get` — the one place decryption happens. Do not "optimize" this back
    /// into a raw iterator.
    pub fn list_index_definitions(&self) -> Result<Vec<(String, PersistedIndexDefinition)>> {
        let mut indexes = Vec::new();

        for (index_name, value) in self.storage.meta_blobs_with_prefix("meta:index:") {
            // Per-record resilience: a single undecodable record must NOT abort
            // the whole rebuild (that would silently un-index every *other*
            // index on the database — the failure mode behind the upgrade bug).
            if let Some(definition) = Self::decode_persisted_index_definition(&index_name, &value) {
                indexes.push((index_name, definition));
            }
        }

        indexes.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(indexes)
    }

    /// Re-register and re-populate every ART index from on-disk state.
    ///
    /// Called once at engine startup so that a fresh process attaching to an
    /// existing data directory has the same in-memory ART state any process
    /// that created the rows would have. Without this, PK / UNIQUE constraint
    /// checks fall back to scans, and INSERT … ON CONFLICT can't find rows
    /// committed by a previous process.
    ///
    /// Behaviour:
    /// - PK and UNIQUE indexes are registered from the persisted schema.
    /// - FK indexes are registered from the persisted `table_constraints`.
    /// - Every row in every user table is replayed through `on_insert` to
    ///   populate the indexes.
    /// - Errors are logged but do not abort startup; a missing or corrupt
    ///   index will fall back to a scan path at query time.
    ///
    /// Cost is `O(rows + indexes)` at startup with no extra cost on the OLTP
    /// hot path. This is the v3.21 interim; persistent index pages backed by
    /// a RocksDB column family are tracked separately for v3.22+.
    pub fn rebuild_all_indexes(&self) -> Result<()> {
        let started = std::time::Instant::now();
        let art_manager = self.storage.art_indexes();
        let mut total_rows: u64 = 0;
        let mut total_tables: u64 = 0;
        let mut report = super::index_snapshot::IndexOpenReport::default();
        let persisted_indexes = self.list_index_definitions()?;

        // R4.2: read every valid snapshot BEFORE registering any index — the
        // registrations below count as mutations and consume the validity
        // markers, so the read phase must come first.
        let art_snapshots = self.storage.load_art_table_snapshots();
        let vector_sidecars = self.storage.load_vector_sidecars();

        for table_name in self.list_tables()? {
            // Skip system / internal bookkeeping tables — they have no
            // user-facing constraint indexes and rebuilding them just
            // wastes time at startup.
            if table_name.starts_with("helios_") {
                continue;
            }

            let schema = match self.get_table_schema(&table_name) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(
                        "Index rebuild: skipping table {} — schema load failed: {}",
                        table_name,
                        e
                    );
                    continue;
                }
            };

            // (Re)register the PK index structure if the table has one.
            let pk_columns: Vec<String> = schema
                .columns
                .iter()
                .filter(|c| c.primary_key)
                .map(|c| c.name.clone())
                .collect();
            if !pk_columns.is_empty() {
                if let Err(e) = art_manager.create_pk_index(&table_name, &pk_columns) {
                    // IndexAlreadyExists is expected if create_table ran in
                    // the same process; log at debug, continue.
                    tracing::debug!("Index rebuild: PK index for {} already registered: {}", table_name, e);
                }
            }

            // (Re)register UNIQUE indexes (one per UNIQUE non-PK column).
            for col in &schema.columns {
                if col.unique && !col.primary_key {
                    let cols = vec![col.name.clone()];
                    if let Err(e) = art_manager.create_unique_index(&table_name, &cols, Some(&col.name)) {
                        tracing::debug!(
                            "Index rebuild: UNIQUE index for {}.{} already registered: {}",
                            table_name,
                            col.name,
                            e
                        );
                    }
                }
            }

            // (Re)register FK indexes from persisted constraints.
            if let Ok(constraints) = self.load_table_constraints(&table_name) {
                for fk in &constraints.foreign_keys {
                    if let Err(e) = art_manager.create_fk_index(
                        &fk.table_name,
                        &fk.columns,
                        &fk.references_table,
                        &fk.references_columns,
                        Some(&fk.name),
                    ) {
                        tracing::debug!("Index rebuild: FK index {} already registered: {}", fk.name, e);
                    }
                }
            }

            // (Re)register user-created scalar secondary indexes from the
            // durable index catalog.
            for (index_name, definition) in persisted_indexes
                .iter()
                .filter(|(_, definition)| definition.table_name == table_name)
            {
                if matches!(definition.index_type.as_deref(), None | Some("art" | "btree" | "hash")) {
                    let columns = vec![definition.column_name.clone()];
                    if let Err(e) = art_manager.create_manual_index(index_name, &table_name, &columns) {
                        // IndexAlreadyExists is expected when CREATE INDEX ran
                        // earlier in this same process. Anything else is a real
                        // degradation — the index stays unregistered and queries
                        // silently full-scan — so it must be surfaced, not
                        // swallowed at debug level.
                        if matches!(e, super::art_index::ArtIndexError::IndexAlreadyExists(_)) {
                            tracing::debug!("Index rebuild: manual index {} already registered", index_name);
                        } else {
                            tracing::warn!(
                                "Index rebuild: manual index '{}' on {}.{} FAILED to register ({}); \
                                 queries will full-scan until it is rebuilt with CREATE INDEX",
                                index_name,
                                table_name,
                                definition.column_name,
                                e
                            );
                        }
                    }
                }
            }

            // R4.2 fast path: a valid snapshot whose index set matches the
            // registrations above is bulk-loaded directly into the trees —
            // no row scan, no tuple deserialization.
            if let Some(snapshot) = art_snapshots.get(&table_name) {
                match self.load_table_from_snapshot(&table_name, &schema, snapshot) {
                    Ok(entries) => {
                        report.entries_loaded += entries;
                        report.tables_from_snapshot += 1;
                        total_tables += 1;
                        continue;
                    }
                    Err(e) => {
                        tracing::warn!(
                            "Index rebuild: snapshot load for '{}' failed ({}) — falling back to scan",
                            table_name,
                            e
                        );
                        // Drop any partially loaded entries; registrations stay.
                        art_manager.clear_table_indexes(&table_name);
                    }
                }
            }

            // Replay every existing row through on_insert so the indexes
            // know about pre-existing data.
            let tuples = match self.storage.scan_table_with_schema(&table_name, &schema) {
                Ok(t) => t,
                Err(e) => {
                    tracing::warn!("Index rebuild: scan failed for table {}: {}", table_name, e);
                    continue;
                }
            };

            let mut max_row_id: Option<u64> = None;
            for tuple in &tuples {
                let row_id = match tuple.row_id {
                    Some(id) => id,
                    None => continue,
                };
                max_row_id = Some(max_row_id.map_or(row_id, |m| m.max(row_id)));
                let mut col_values = std::collections::HashMap::with_capacity(schema.columns.len());
                for (idx, col) in schema.columns.iter().enumerate() {
                    if let Some(v) = tuple.values.get(idx) {
                        col_values.insert(col.name.clone(), v.clone());
                    }
                }
                if let Err(e) = art_manager.on_insert(&table_name, row_id, &col_values) {
                    tracing::debug!(
                        "Index rebuild: on_insert skipped for {} row {}: {}",
                        table_name,
                        row_id,
                        e
                    );
                }
                total_rows += 1;
            }

            // Reconcile the row-id counter against the highest row id actually
            // scanned. This scan-fallback branch only runs when no valid R4.2
            // index snapshot exists — which is exactly the crash-reopen case
            // where `load_counters` may have seeded `row_counters` from a stale
            // durable `counter:{table}` value (the fast INSERT path only
            // re-persists it every 64 rows, and the unconditional
            // `flush_all_row_counters` runs only on a CLEAN shutdown). Without
            // this, the next INSERT could hand back an already-used row id and
            // silently overwrite an existing `data:` row. An empty table leaves
            // `max_row_id` as None and is correctly a no-op (its durable counter,
            // seeded at 0 by CREATE TABLE, is already correct).
            if let Some(max_id) = max_row_id {
                if let Err(e) = self.storage.reseed_row_counter_from_max_row_id(&table_name, max_id) {
                    tracing::warn!(
                        "Index rebuild: row-counter reseed for '{}' failed ({}) — a crash \
                         before the next 64-row flush boundary could still reuse a row id",
                        table_name,
                        e
                    );
                }
            }

            report.tables_scanned += 1;
            total_tables += 1;
        }

        self.rebuild_vector_indexes(&persisted_indexes, &vector_sidecars, &mut report)?;

        report.tables_total = total_tables;
        report.rows_scanned = total_rows;
        report.elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
        tracing::info!(
            "Index rebuild complete: {} tables ({} from snapshot, {} scanned, {} rows), \
             vector: {} reloaded / {} rebuilt / {} reopened / {} degraded, {:.1}ms",
            report.tables_total,
            report.tables_from_snapshot,
            report.tables_scanned,
            report.rows_scanned,
            report.vector_reloaded,
            report.vector_rebuilt,
            report.vector_reopened_persistent,
            report.vector_degraded,
            report.elapsed_ms,
        );
        self.storage.set_last_index_open_report(report);
        Ok(())
    }

    /// R4.2: load one table's ART indexes from a snapshot. Fails (and the
    /// caller falls back to the scan rebuild) when the snapshot's index set
    /// does not exactly match the registered set — e.g. an index was created
    /// or dropped between the checkpoint and a crash.
    fn load_table_from_snapshot(
        &self,
        table_name: &str,
        schema: &Schema,
        snapshot: &super::index_snapshot::ArtTableSnapshot,
    ) -> Result<u64> {
        use super::art_manager::index_type_tag;

        let art_manager = self.storage.art_indexes();
        let mut registered: Vec<(String, u8, Vec<String>)> = art_manager
            .list_table_indexes(table_name)
            .into_iter()
            .map(|(name, index_type, columns)| (name, index_type_tag(index_type), columns))
            .collect();
        registered.sort_by(|a, b| a.0.cmp(&b.0));
        let snapshotted: Vec<(String, u8, Vec<String>)> = snapshot
            .indexes
            .iter()
            .map(|s| (s.name.clone(), s.index_type, s.columns.clone()))
            .collect();
        if registered != snapshotted {
            return Err(Error::storage(format!(
                "snapshot index set mismatch (registered {:?} vs snapshot {:?})",
                registered.iter().map(|(n, _, _)| n).collect::<Vec<_>>(),
                snapshotted.iter().map(|(n, _, _)| n).collect::<Vec<_>>(),
            )));
        }

        // Byte width for dense-int PK stats (single-column integer PKs only).
        let pk_int_width = {
            let pk_cols: Vec<&crate::Column> = schema.columns.iter().filter(|c| c.primary_key).collect();
            match pk_cols.as_slice() {
                [col] => match col.data_type {
                    DataType::Int2 => Some(2),
                    DataType::Int4 => Some(4),
                    DataType::Int8 => Some(8),
                    _ => None,
                },
                _ => None,
            }
        };

        let mut loaded: u64 = 0;
        for index_snapshot in &snapshot.indexes {
            let width = if index_snapshot.index_type == index_type_tag(crate::storage::ArtIndexType::PrimaryKey) {
                pk_int_width
            } else {
                None
            };
            loaded += art_manager
                .load_index_entries(&index_snapshot.name, &index_snapshot.entries, width)
                .map_err(|e| Error::storage(format!("loading '{}': {}", index_snapshot.name, e)))?
                as u64;
        }
        Ok(loaded)
    }

    fn rebuild_vector_indexes(
        &self,
        indexes: &[(String, PersistedIndexDefinition)],
        sidecars: &std::collections::HashMap<String, super::index_snapshot::VectorGraphSidecar>,
        report: &mut super::index_snapshot::IndexOpenReport,
    ) -> Result<()> {
        let dump_dir = self.storage.hnsw_snapshot_dir();
        for (index_name, definition) in indexes {
            // Shared classifier, not a local tag list: this used to be a
            // `matches!` copy of the same mapping `handle_create_index` and
            // `handle_drop_index` each kept privately, and they drifted.
            if index_family(definition.index_type.as_deref()) != Some(IndexFamily::Vector) {
                continue;
            }

            let schema = match self.get_table_schema(&definition.table_name) {
                Ok(schema) => schema,
                Err(e) => {
                    tracing::warn!(
                        "Vector index rebuild: skipping {} — schema load failed: {}",
                        index_name,
                        e
                    );
                    self.storage
                        .vector_indexes()
                        .mark_degraded(index_name, format!("schema load failed at open: {e}"));
                    report.vector_degraded += 1;
                    continue;
                }
            };
            let Some(column) = schema.get_column(&definition.column_name) else {
                tracing::warn!(
                    "Vector index rebuild: skipping {} — column {}.{} not found",
                    index_name,
                    definition.table_name,
                    definition.column_name
                );
                continue;
            };
            let DataType::Vector(dimension) = &column.data_type else {
                tracing::warn!(
                    "Vector index rebuild: skipping {} — {}.{} is not VECTOR",
                    index_name,
                    definition.table_name,
                    definition.column_name
                );
                continue;
            };

            let vector_indexes = self.storage.vector_indexes();
            if vector_indexes.index_exists(index_name) {
                continue;
            }

            // R4.2 / R5.V2: reopen RocksDB-backed persistent indexes in place
            // instead of downgrading them to a rebuilt in-memory Standard
            // index (the catalog downgrade flagged by the fastest-HTAP audit).
            #[cfg(feature = "vector-persist")]
            if matches!(definition.index_type.as_deref(), Some("persistent_hnsw")) {
                let metric = vector_distance_metric(&definition.options);
                let config = super::vector_index::PersistentVectorConfig {
                    dimension: *dimension,
                    distance_metric: metric,
                    pq_enabled: false,
                    rerank_precision: "f32".to_string(),
                };
                match vector_indexes.reopen_persistent_index(
                    index_name,
                    &definition.table_name,
                    &definition.column_name,
                    config,
                    std::sync::Arc::clone(&self.storage.db),
                ) {
                    Ok(()) => {
                        report.vector_reopened_persistent += 1;
                        continue;
                    }
                    Err(e) => {
                        tracing::warn!(
                            "Vector index rebuild: persistent reopen of '{}' failed: {} — rebuilding in memory",
                            index_name,
                            e
                        );
                    }
                }
            }

            // R4.2 fast path: reload a dumped HNSW graph instead of
            // re-scanning the table and rebuilding the graph from scratch.
            if matches!(definition.index_type.as_deref(), Some("hnsw")) {
                if let (Some(sidecar), Some(dir)) = (sidecars.get(index_name), dump_dir.as_ref()) {
                    match vector_indexes.reload_standard_index(sidecar, dir) {
                        Ok(()) => {
                            report.vector_reloaded += 1;
                            continue;
                        }
                        Err(e) => {
                            // Surface loudly and fall back to the full rebuild —
                            // never leave a silently empty index behind.
                            tracing::warn!(
                                "Vector index rebuild: graph reload of '{}' failed: {} — rebuilding from rows",
                                index_name,
                                e
                            );
                        }
                    }
                }
            }

            let metric = vector_distance_metric(&definition.options);
            // R5.V6: rebuild with the same construction parameters the index
            // was created with — persisted `WITH (m = .., ef_construction = ..)`
            // options first, the `[vector]` config section as the default.
            let (m, ef_construction) = hnsw_construction_params(&definition.options, self.storage.config());
            if let Err(e) = vector_indexes.create_index_with_params(
                index_name.clone(),
                definition.table_name.clone(),
                definition.column_name.clone(),
                *dimension,
                metric,
                m,
                ef_construction,
            ) {
                tracing::warn!("Vector index rebuild: create {} failed: {}", index_name, e);
                vector_indexes.mark_degraded(index_name, format!("create failed at open: {e}"));
                report.vector_degraded += 1;
                continue;
            }

            let vectors = match self.collect_existing_vectors(
                &schema,
                &definition.table_name,
                &definition.column_name,
                *dimension,
            ) {
                Ok(vectors) => vectors,
                Err(e) => {
                    let _ = vector_indexes.drop_index(index_name);
                    tracing::warn!("Vector index rebuild: scan {} failed: {}", index_name, e);
                    vector_indexes.mark_degraded(index_name, format!("backfill scan failed at open: {e}"));
                    report.vector_degraded += 1;
                    continue;
                }
            };
            if let Err(e) = vector_indexes.insert_vectors_batch(index_name, &vectors) {
                let _ = vector_indexes.drop_index(index_name);
                tracing::warn!("Vector index rebuild: backfill {} failed: {}", index_name, e);
                vector_indexes.mark_degraded(index_name, format!("backfill failed at open: {e}"));
                report.vector_degraded += 1;
                continue;
            }
            vector_indexes.clear_degraded(index_name);
            report.vector_rebuilt += 1;
        }
        Ok(())
    }

    fn collect_existing_vectors(
        &self,
        schema: &Schema,
        table_name: &str,
        column_name: &str,
        dimension: usize,
    ) -> Result<Vec<(u64, crate::vector::Vector)>> {
        let col_idx = schema
            .get_column_index(column_name)
            .ok_or_else(|| Error::query_execution(format!("Column '{}' not found in schema", column_name)))?;
        let tuples = self
            .storage
            .scan_table_with_schema_columns(table_name, schema, &[col_idx])?;
        let mut vectors = Vec::with_capacity(tuples.len());

        for tuple in tuples {
            match tuple.values.get(col_idx) {
                Some(Value::Vector(vec)) => {
                    if vec.len() != dimension {
                        return Err(Error::query_execution(format!(
                            "Vector dimension mismatch while rebuilding '{}.{}': expected {}, got {}",
                            table_name,
                            column_name,
                            dimension,
                            vec.len()
                        )));
                    }
                    let row_id = tuple.row_id.ok_or_else(|| {
                        Error::query_execution(format!(
                            "Cannot rebuild vector index on '{}.{}' from tuple without row_id",
                            table_name, column_name
                        ))
                    })?;
                    vectors.push((row_id, vec.clone()));
                }
                Some(Value::Null) | None => {}
                Some(other) => {
                    return Err(Error::query_execution(format!(
                        "Cannot rebuild vector index on '{}.{}' from non-vector value {:?}",
                        table_name, column_name, other
                    )))
                }
            }
        }

        Ok(vectors)
    }

    /// Rename a table atomically
    ///
    /// This operation renames a table by updating its metadata and moving all data rows
    /// to use the new table name. This is used for concurrent materialized view refresh.
    pub fn rename_table(&self, old_name: &str, new_name: &str) -> Result<()> {
        // Check that new table name is not already in use
        if self.table_exists(new_name)? {
            return Err(Error::query_execution(format!("Table '{}' already exists", new_name)));
        }
        self.rename_table_inner(old_name, new_name)
    }

    /// Replay-path rename: WAL replay re-applies operations onto a state
    /// where earlier entries have already resurrected the old name AND the
    /// checkpointed data already contains the new name — so the
    /// target-must-not-exist validation must not apply (the re-moved keys are
    /// byte-identical). Runtime callers go through `rename_table`.
    pub(crate) fn rename_table_replay(&self, old_name: &str, new_name: &str) -> Result<()> {
        self.rename_table_inner(old_name, new_name)
    }

    fn rename_table_inner(&self, old_name: &str, new_name: &str) -> Result<()> {
        // Check that old table exists
        if !self.table_exists(old_name)? {
            return Err(Error::query_execution(format!("Table '{}' does not exist", old_name)));
        }

        // Log the rename to the logical WAL before touching data (mirrors
        // log_drop_table ordering) — no-op during replay or on standbys.
        self.storage.log_rename_table(old_name, new_name)?;

        // Get the schema from old table
        let schema = self.get_table_schema(old_name)?;

        // Get current row counter
        let old_counter_key = Self::table_counter_key(old_name);
        let counter_value = match self.storage.get(&old_counter_key)? {
            Some(data) => data,
            None => {
                // Default to 0 if counter doesn't exist
                bincode::serialize(&0u64).map_err(|e| Error::storage(format!("Failed to serialize counter: {}", e)))?
            }
        };

        // Get compression config if it exists
        let compression_config = self.get_compression_config(old_name)?;

        // Get compression stats if they exist
        let compression_stats = self.get_compression_stats(old_name)?;

        // Copy compression config to new table
        if let Some(config) = compression_config {
            self.set_compression_config(new_name, &config)?;
        }

        // Copy compression stats to new table
        if let Some(stats) = compression_stats {
            self.set_compression_stats(new_name, &stats)?;
        }

        // The entire rename — new metadata + counter, every data row moved,
        // all old keys removed — lands in ONE atomic RocksDB write.
        //
        // The previous implementation moved rows via per-row
        // `storage.put()` + `storage.delete()`, which had two bugs (the same
        // family as the c478286 DROP-TABLE stall):
        //   1. every `delete()` appends a logical-WAL entry with a synchronous
        //      fdatasync — a 50k-row RENAME was ~50k fsyncs: 15+ minutes,
        //      non-cancellable (the statement runs on after client disconnect),
        //      and it monopolized the WAL writer ("server wedged").
        //   2. `put()` re-encrypts the value bytes, but the iterator yields the
        //      *stored* (already-encrypted) form — with encryption enabled every
        //      moved row was double-encrypted and unreadable after the rename.
        // Raw batched writes fix both, and make a crash mid-rename atomic
        // instead of leaving two half-tables.
        //
        // NOTE: rename is not logged to the logical WAL as a DDL op (no
        // WalOperation::RenameTable exists) — true before this change too; the
        // per-row Delete entries a standby used to receive only deleted the old
        // rows there without creating the new ones. Proper rename DDL logging
        // is tracked in docs/plans/PERF_STABILITY_2026_07 (C-II).
        let new_metadata_key = Self::table_metadata_key(new_name);
        let schema_bytes =
            bincode::serialize(&schema).map_err(|e| Error::storage(format!("Failed to serialize schema: {}", e)))?;
        let new_counter_key = Self::table_counter_key(new_name);

        let mut batch = rocksdb::WriteBatch::default();
        batch.put(&new_metadata_key, &schema_bytes);
        batch.put(&new_counter_key, &counter_value);

        let old_data_prefix = format!("data:{}:", old_name);
        let old_prefix_bytes = old_data_prefix.as_bytes();
        let new_data_prefix = format!("data:{}:", new_name);

        // Seek to the table's data prefix instead of scanning from the start
        // of the keyspace; stage each move (raw bytes, verbatim) in the batch.
        let mut read_opts = rocksdb::ReadOptions::default();
        read_opts.set_total_order_seek(true);
        let iter = self.storage.db.iterator_opt(
            rocksdb::IteratorMode::From(old_prefix_bytes, rocksdb::Direction::Forward),
            read_opts,
        );
        for item in iter {
            let (key, value) = item.map_err(|e| Error::storage(format!("Iterator error: {}", e)))?;

            if !key.starts_with(old_prefix_bytes) {
                break;
            }
            // Extract row_id from old key: data:{old_name}:{row_id}
            let key_str = String::from_utf8_lossy(&key);
            if let Some(row_id_str) = key_str.strip_prefix(&old_data_prefix) {
                let new_key = format!("{}{}", new_data_prefix, row_id_str).into_bytes();
                batch.put(&new_key, &value);
                batch.delete(&key);
            }
        }

        // Old-name cleanup rides the same atomic batch (these keys are not
        // `meta:`-exempt in storage.delete(), so they each cost a WAL fsync
        // on the old path).
        batch.delete(Self::table_metadata_key(old_name));
        batch.delete(&old_counter_key);
        batch.delete(Self::compression_config_key(old_name));
        batch.delete(Self::compression_stats_key(old_name));

        self.storage
            .db
            .write(batch)
            .map_err(|e| Error::storage(format!("Rename batch write failed: {}", e)))?;

        // Carry the VOLATILE in-memory row-id counter to the new name. The batch
        // above moved the durable `counter:{table}` key, but `next_row_id_volatile`
        // seeds a missing in-memory entry at 0 (not from the durable key), so the
        // first insert into the renamed table would otherwise reuse a row id and
        // overwrite a pre-existing row (and strand its PK-index key, inflating the
        // COUNT(*) fast path).
        self.storage.rename_row_counter(old_name, new_name);

        // Rename compression manager resources (no-op - compression handled by RocksDB LZ4)
        super::CompressionManager::new().rename_table(old_name, new_name)?;

        // Rename ART indexes
        let art_manager = self.storage.art_indexes();
        if let Err(e) = art_manager.rename_table_indexes(old_name, new_name) {
            tracing::warn!(
                "Failed to rename ART indexes from '{}' to '{}': {}",
                old_name,
                new_name,
                e
            );
        }

        // Update schema cache: remove old, add new
        self.storage.invalidate_schema_cache(old_name);
        self.storage.cache_schema(new_name, schema);

        // W1.3: rename changes existence for BOTH names (old → Missing,
        // new → Table); bump so the existence cache recomputes both.
        self.storage.bump_schema_generation();

        Ok(())
    }

    /// Migrate the per-table SIDE records that [`Self::rename_table`] does NOT
    /// move — the constraint metadata, the IDENTITY-column record and the
    /// Stage-0 partition registry. Used by `ALTER TABLE … SET SCHEMA`, which
    /// relocates a table to a new storage key via `rename_table` (data + schema
    /// + counter + compression + ART indexes) and must carry these along or the
    /// moved table loses its FK/CHECK enforcement and its partition-cascade
    /// links. Best-effort per record; a missing record is a no-op. Not atomic
    /// with the `rename_table` batch (Stage-0 DDL is non-transactional
    /// generally), but every step is idempotent on replay.
    pub fn move_table_side_records(&self, old: &str, new: &str) -> Result<()> {
        // Constraint metadata (FK / UNIQUE / CHECK). Rewrite each constraint's
        // owning-table field to the new key, and any SELF-referential FK's
        // `references_table` too (a table that references itself moves both
        // ends), so the moved table's constraints stay self-consistent.
        let mut constraints = self.load_table_constraints(old)?;
        for fk in constraints.foreign_keys.iter_mut() {
            if fk.references_table == old {
                fk.references_table = new.to_string();
            }
            fk.table_name = new.to_string();
        }
        for uc in constraints.unique_constraints.iter_mut() {
            uc.table_name = new.to_string();
        }
        for cc in constraints.check_constraints.iter_mut() {
            cc.table_name = new.to_string();
        }
        if !constraints.foreign_keys.is_empty()
            || !constraints.unique_constraints.is_empty()
            || !constraints.check_constraints.is_empty()
        {
            self.save_table_constraints(new, &constraints)?;
        }
        self.storage.delete(&Self::table_constraints_key(old))?;
        self.storage.clear_referencing_fk_cache();

        // IDENTITY-column side record.
        let identity = self.list_identity_columns(old)?;
        if !identity.is_empty() {
            self.register_identity_columns(new, &identity)?;
            self.drop_identity_columns(old)?;
        }

        // Partition registry — reverse link: `old` is itself a child of `parent`.
        // Rewrite `parent`'s child-list entry old→new and move the reverse link,
        // so a later `DROP TABLE parent` cascades to the moved child.
        if let Some(bytes) = self.storage.get(&Self::partition_parent_key(old))? {
            let parent: String = bincode::deserialize(&bytes)
                .map_err(|e| Error::query_execution(format!("partition-parent deserialize: {e}")))?;
            let mut siblings = self.partition_children(&parent)?;
            for c in siblings.iter_mut() {
                if c == old {
                    *c = new.to_string();
                }
            }
            let value = bincode::serialize(&siblings)
                .map_err(|e| Error::query_execution(format!("partition-child serialize: {e}")))?;
            self.storage.put(&Self::partition_children_key(&parent), &value)?;
            self.storage.delete(&Self::partition_parent_key(old))?;
            let pv = bincode::serialize(new)
                .map_err(|e| Error::query_execution(format!("partition-parent serialize: {e}")))?;
            self.storage.put(&Self::partition_parent_key(new), &pv)?;
        }

        // Partition registry — forward list: `old` is a parent with children.
        // Repoint each child's reverse link at the new parent key and move the
        // child list, so the moved parent still cascade-drops its children.
        let children = self.partition_children(old)?;
        if !children.is_empty() {
            for child in &children {
                let pv = bincode::serialize(new)
                    .map_err(|e| Error::query_execution(format!("partition-parent serialize: {e}")))?;
                self.storage.put(&Self::partition_parent_key(child), &pv)?;
            }
            let value = bincode::serialize(&children)
                .map_err(|e| Error::query_execution(format!("partition-child serialize: {e}")))?;
            self.storage.put(&Self::partition_children_key(new), &value)?;
            self.storage.delete(&Self::partition_children_key(old))?;
        }
        Ok(())
    }

    /// Build metadata key for table schema
    fn table_metadata_key(table_name: &str) -> Vec<u8> {
        format!("meta:table:{}", table_name).into_bytes()
    }

    fn index_metadata_key(index_name: &str) -> Vec<u8> {
        format!("meta:index:{}", index_name).into_bytes()
    }

    // -------------------------------------------------------------------
    // Schema namespacing — CREATE SCHEMA / DROP SCHEMA.
    //
    // A declared schema is recorded at `meta:schema:<name>` (empty value).
    // Member tables need NO separate record: a table in schema `s` is keyed
    // `s.<table>` (see planner `normalize_object_name`), so membership is a
    // catalog prefix scan (`schema_members`). The marker exists so an empty
    // schema still reports as present (CREATE SCHEMA duplicate error / DROP
    // SCHEMA existence) even before any table is created in it.
    // -------------------------------------------------------------------

    fn schema_metadata_key(name: &str) -> Vec<u8> {
        format!("meta:schema:{}", name).into_bytes()
    }

    /// Record a declared schema. Returns `false` if it was already present
    /// (so callers can honor CREATE SCHEMA duplicate semantics).
    pub fn register_schema(&self, name: &str) -> Result<bool> {
        if self.schema_exists(name)? {
            return Ok(false);
        }
        let key = Self::schema_metadata_key(name);
        self.storage.put(&key, &[])?;
        Ok(true)
    }

    /// True if the schema is declared (`meta:schema:` marker) OR still has at
    /// least one member table (a `schema.` key). Either makes DROP SCHEMA see
    /// it as existing.
    pub fn schema_exists(&self, name: &str) -> Result<bool> {
        let key = Self::schema_metadata_key(name);
        if self.storage.get(&key)?.is_some() {
            return Ok(true);
        }
        Ok(!self.schema_members(name)?.is_empty())
    }

    /// Remove the schema marker. Member tables must be dropped separately
    /// (CASCADE) — this only clears the declaration.
    pub fn drop_schema_marker(&self, name: &str) -> Result<()> {
        let key = Self::schema_metadata_key(name);
        self.storage.delete(&key)
    }

    /// The PRIMARY KEY columns of `table` (its declared key, in column order),
    /// or an empty Vec if it has none. `table` must be a resolved storage key
    /// (schema-qualified when non-`public`). Used to default a foreign key's
    /// referenced-column list when the `REFERENCES parent` clause omits it —
    /// PostgreSQL parity: `REFERENCES parent` binds to `parent`'s primary key.
    pub fn primary_key_columns(&self, table: &str) -> Result<Vec<String>> {
        let schema = self.get_table_schema(table)?;
        Ok(schema
            .columns
            .iter()
            .filter(|c| c.primary_key)
            .map(|c| c.name.clone())
            .collect())
    }

    /// The member tables of a schema: every catalogued table whose key is
    /// `<schema>.<table>`. Returns the FULL qualified keys (drop-ready).
    pub fn schema_members(&self, schema: &str) -> Result<Vec<String>> {
        let prefix = format!("{schema}.");
        Ok(self
            .list_tables()?
            .into_iter()
            .filter(|t| t.starts_with(prefix.as_str()))
            .collect())
    }

    // -------------------------------------------------------------------
    // KanttBan #20 (v3.31.0) — CREATE TYPE … AS ENUM
    //
    // Storage: each registered enum type lives at
    // `meta:enum_type:<name>` → bincode-encoded `Vec<String>` of labels.
    // Persistence + lookup happens here at the catalog layer so that
    // plan-time `CREATE TABLE foo (col my_enum)` can resolve labels
    // and synthesize an implicit CHECK (col IN (labels…)) constraint.
    // The column itself is stored as TEXT.
    // -------------------------------------------------------------------

    fn enum_type_key(name: &str) -> Vec<u8> {
        format!("meta:enum_type:{}", name.to_lowercase()).into_bytes()
    }

    /// Register an enum type with its labels. Overwrites any existing
    /// entry — callers that need IF NOT EXISTS semantics should check
    /// `enum_type_exists` first.
    pub fn register_enum_type(&self, name: &str, labels: &[String]) -> Result<()> {
        let key = Self::enum_type_key(name);
        let value = bincode::serialize(labels).map_err(|e| Error::query_execution(format!("enum serialize: {e}")))?;
        self.storage.put(&key, &value)
    }

    /// Look up the labels for a registered enum type. Returns None if
    /// the name isn't registered.
    pub fn get_enum_labels(&self, name: &str) -> Result<Option<Vec<String>>> {
        let key = Self::enum_type_key(name);
        match self.storage.get(&key)? {
            Some(bytes) => {
                let labels: Vec<String> = bincode::deserialize(&bytes)
                    .map_err(|e| Error::query_execution(format!("enum deserialize: {e}")))?;
                Ok(Some(labels))
            }
            None => Ok(None),
        }
    }

    /// True if an enum with this name exists.
    pub fn enum_type_exists(&self, name: &str) -> Result<bool> {
        Ok(self.get_enum_labels(name)?.is_some())
    }

    /// Drop a registered enum type. No-op if it doesn't exist
    /// (callers wanting strict semantics should pre-check).
    pub fn drop_enum_type(&self, name: &str) -> Result<()> {
        let key = Self::enum_type_key(name);
        self.storage.delete(&key)
    }

    // -------------------------------------------------------------------
    // KanttBan #23 (v3.31.1 phase 2) — IDENTITY column tracking.
    //
    // Stored as `meta:identity:<table>` → bincode `Vec<String>` of
    // column names. Could've added the bool to the Column struct
    // itself, but that propagates through 200+ struct literals
    // across the codebase. A side table keeps the change additive.
    // Callers: pg_sequences / pg_attrdef / information_schema.columns
    // executors enumerate identity columns to populate their rows.
    // -------------------------------------------------------------------

    fn identity_key(table_name: &str) -> Vec<u8> {
        format!("meta:identity:{}", table_name.to_lowercase()).into_bytes()
    }

    /// Record that the given column on the given table was declared
    /// with `GENERATED [ALWAYS | BY DEFAULT] AS IDENTITY`. Overwrites
    /// any previous list for that table — callers passing the full
    /// post-CREATE-TABLE column set is the expected shape.
    pub fn register_identity_columns(&self, table_name: &str, columns: &[String]) -> Result<()> {
        let key = Self::identity_key(table_name);
        if columns.is_empty() {
            // Avoid storing empty entries — keeps the prefix scan
            // narrow when many tables have no identity columns.
            self.storage.delete(&key)?;
            return Ok(());
        }
        let value =
            bincode::serialize(columns).map_err(|e| Error::query_execution(format!("identity serialize: {e}")))?;
        self.storage.put(&key, &value)
    }

    /// List the identity columns recorded for a table. Returns an
    /// empty Vec when the table has none (or when no record exists).
    pub fn list_identity_columns(&self, table_name: &str) -> Result<Vec<String>> {
        let key = Self::identity_key(table_name);
        match self.storage.get(&key)? {
            Some(bytes) => {
                bincode::deserialize(&bytes).map_err(|e| Error::query_execution(format!("identity deserialize: {e}")))
            }
            None => Ok(Vec::new()),
        }
    }

    /// True if the given (table, column) pair was marked IDENTITY.
    pub fn is_identity_column(&self, table_name: &str, column_name: &str) -> Result<bool> {
        let cols = self.list_identity_columns(table_name)?;
        Ok(cols.iter().any(|c| c.eq_ignore_ascii_case(column_name)))
    }

    /// Drop the identity record for a table (called from DROP TABLE).
    pub fn drop_identity_columns(&self, table_name: &str) -> Result<()> {
        let key = Self::identity_key(table_name);
        self.storage.delete(&key)
    }

    // -------------------------------------------------------------------
    // Round-3 PARTITION BY Stage-0 — parent→child dependency registry.
    //
    // Stage-0 flattens `CREATE TABLE child PARTITION OF parent …` into an
    // independent standalone `child` table, but PostgreSQL makes a partition a
    // *dependent* of its parent: `DROP TABLE parent` drops every partition. To
    // honor that under the flatten, we record the (parent → child) link here so
    // the executor's DROP path can cascade. Two disjoint key families, mirroring
    // the IDENTITY side-record above (durable catalog KV, so the registry
    // survives a process restart on the same data dir — the same guarantee the
    // schema/identity/enum side-records give; it is NOT separately WAL-shipped
    // to standbys, which is Stage-1's durable-partition-catalog scope):
    //   meta:partchild:<parent>  -> bincode Vec<String> (the parent's children)
    //   meta:partparent:<child>  -> bincode String      (that child's parent)
    // The forward record answers "does this table have children?" in one point
    // get, so an ordinary DROP of a non-partition table stays O(1) and cheap.
    // The reverse record lets a *direct* child DROP detach itself from its
    // parent's list (PG parity: dropping a partition directly is allowed and
    // unregisters it, so a later parent DROP never touches a re-created name).
    //
    // Names are keyed EXACTLY as they arrive — already resolved to the catalog
    // storage key by the planner's `resolve_partition_name` (session
    // `search_path` applied: `s.parent` under a non-public schema, bare under
    // public), the same key `table_metadata_key` relies on — so registration
    // and the DROP-cascade lookup resolve a `PARTITION OF` parent to the
    // identical registry key. No extra lowercasing here (unlike `identity_key`)
    // precisely so the key derived from a name matches that name's `meta:table:`
    // key byte-for-byte.
    // -------------------------------------------------------------------

    fn partition_children_key(parent: &str) -> Vec<u8> {
        format!("meta:partchild:{}", parent).into_bytes()
    }

    fn partition_parent_key(child: &str) -> Vec<u8> {
        format!("meta:partparent:{}", child).into_bytes()
    }

    /// Record a Stage-0 `PARTITION OF` dependency: `child` is a partition of
    /// `parent`. Appends `child` (deduplicated) to the parent's child list and
    /// stores the reverse `child → parent` link. Both names must already be
    /// normalized (bare catalog keys). Idempotent for a repeated (parent, child).
    pub fn register_partition_child(&self, parent: &str, child: &str) -> Result<()> {
        let mut children = self.partition_children(parent)?;
        if !children.iter().any(|c| c == child) {
            children.push(child.to_string());
            let value = bincode::serialize(&children)
                .map_err(|e| Error::query_execution(format!("partition-child serialize: {e}")))?;
            self.storage.put(&Self::partition_children_key(parent), &value)?;
        }
        let pv = bincode::serialize(parent)
            .map_err(|e| Error::query_execution(format!("partition-parent serialize: {e}")))?;
        self.storage.put(&Self::partition_parent_key(child), &pv)?;
        Ok(())
    }

    /// The partition children registered under `parent`. Empty when the table
    /// has no registered partitions (the common, zero-cost case).
    pub fn partition_children(&self, parent: &str) -> Result<Vec<String>> {
        match self.storage.get(&Self::partition_children_key(parent))? {
            Some(bytes) => bincode::deserialize(&bytes)
                .map_err(|e| Error::query_execution(format!("partition-child deserialize: {e}"))),
            None => Ok(Vec::new()),
        }
    }

    /// Called from the DROP TABLE path for `table`. Performs the registry
    /// bookkeeping and returns the children that were registered under `table`
    /// so the caller can cascade the drop to them. Specifically:
    ///  1. If `table` is itself a registered partition child, detaches it from
    ///     its parent's child list and deletes its reverse link (so a re-created
    ///     name is never cascade-dropped by a later parent DROP).
    ///  2. Removes `table`'s own child-list record and returns its children.
    /// Idempotent; returns an empty Vec for a table with no partition links.
    pub fn take_partition_children_on_drop(&self, table: &str) -> Result<Vec<String>> {
        // (1) detach from parent, if any.
        if let Some(bytes) = self.storage.get(&Self::partition_parent_key(table))? {
            let parent: String = bincode::deserialize(&bytes)
                .map_err(|e| Error::query_execution(format!("partition-parent deserialize: {e}")))?;
            let mut siblings = self.partition_children(&parent)?;
            siblings.retain(|c| c != table);
            if siblings.is_empty() {
                self.storage.delete(&Self::partition_children_key(&parent))?;
            } else {
                let value = bincode::serialize(&siblings)
                    .map_err(|e| Error::query_execution(format!("partition-child serialize: {e}")))?;
                self.storage.put(&Self::partition_children_key(&parent), &value)?;
            }
            self.storage.delete(&Self::partition_parent_key(table))?;
        }
        // (2) take + clear this table's own child list.
        let children = self.partition_children(table)?;
        if !children.is_empty() {
            self.storage.delete(&Self::partition_children_key(table))?;
        }
        Ok(children)
    }

    // -------------------------------------------------------------------
    // v3.60.0 — durable + scalable SEQUENCES.
    //
    // Two records per sequence so the hot, explicitly-fsynced record stays
    // tiny (mirroring the engine's definition-vs-counter split):
    //   meta:sequence:<name>  -> PersistedSequence  (config; rides the
    //                            normal post-statement barrier)
    //   meta:seqstate:<name>  -> PersistedSeqState  (high-water; fsynced
    //                            explicitly by flush_sequence_state before
    //                            any value in a block is served)
    // The two prefixes are disjoint (they diverge at byte 7, 'u' vs 't'),
    // and the list scan breaks on the first key outside its exact prefix,
    // so `meta:seqstate:` never bleeds into the `meta:sequence:` scan.
    // -------------------------------------------------------------------

    fn sequence_def_key(name: &str) -> Vec<u8> {
        format!("meta:sequence:{}", name).into_bytes()
    }

    fn sequence_state_key(name: &str) -> Vec<u8> {
        format!("meta:seqstate:{}", name).into_bytes()
    }

    /// Persist a `CREATE SEQUENCE` definition. Overwrites any existing entry —
    /// callers wanting IF NOT EXISTS semantics must check `sequence_exists`
    /// first. Definition durability rides the normal post-statement barrier
    /// (like enum/identity); a lost CREATE means the sequence is *absent* on
    /// restart, never a duplicate value.
    pub fn save_sequence(&self, def: &PersistedSequence) -> Result<()> {
        let key = Self::sequence_def_key(&def.name);
        let body = bincode::serialize(def)
            .map_err(|e| Error::storage(format!("Failed to serialize sequence definition: {}", e)))?;
        let mut value = Vec::with_capacity(SEQ_DEF_MAGIC.len() + 1 + body.len());
        value.extend_from_slice(SEQ_DEF_MAGIC);
        value.push(SEQ_DEF_FORMAT_VERSION);
        value.extend_from_slice(&body);
        self.storage.put(&key, &value)
    }

    /// Decode one persisted sequence-definition record, tolerating a future
    /// format bump or a raw (untagged) bincode body. Returns `None` (after a
    /// `warn!`) for an undecodable / unknown-version record instead of
    /// erroring, so one bad record degrades to "that sequence is missing"
    /// rather than aborting the load of every other sequence — the same
    /// per-record resilience the index rebuild relies on.
    fn decode_persisted_sequence(name: &str, value: &[u8]) -> Option<PersistedSequence> {
        if value.len() > SEQ_DEF_MAGIC.len() && value.starts_with(SEQ_DEF_MAGIC) {
            let version = value[SEQ_DEF_MAGIC.len()];
            let body = &value[SEQ_DEF_MAGIC.len() + 1..];
            if version == SEQ_DEF_FORMAT_VERSION {
                match bincode::deserialize::<PersistedSequence>(body) {
                    Ok(def) => return Some(def),
                    Err(e) => {
                        tracing::warn!(
                            "Sequence load: skipping sequence '{}' — v{} record failed to decode ({})",
                            name,
                            version,
                            e
                        );
                        return None;
                    }
                }
            }
            tracing::warn!(
                "Sequence load: skipping sequence '{}' — on-disk format version {} is newer than \
                 this binary supports (v{})",
                name,
                version,
                SEQ_DEF_FORMAT_VERSION
            );
            return None;
        }

        // Legacy untagged fallback: raw bincode(PersistedSequence).
        if let Ok(def) = bincode::deserialize::<PersistedSequence>(value) {
            return Some(def);
        }

        tracing::warn!(
            "Sequence load: skipping sequence '{}' — record ({} bytes) is not decodable in any known format",
            name,
            value.len()
        );
        None
    }

    /// Look up a single persisted sequence definition by name.
    pub fn get_sequence(&self, name: &str) -> Result<Option<PersistedSequence>> {
        let key = Self::sequence_def_key(name);
        match self.storage.get(&key)? {
            Some(bytes) => Ok(Self::decode_persisted_sequence(name, &bytes)),
            None => Ok(None),
        }
    }

    /// True if a sequence definition with this name exists.
    pub fn sequence_exists(&self, name: &str) -> Result<bool> {
        let key = Self::sequence_def_key(name);
        Ok(self.storage.get(&key)?.is_some())
    }

    /// List every persisted sequence definition. Per-record resilient: a
    /// single undecodable record is skipped (with a `warn!`), never aborting
    /// the whole load. Used by startup warm-load and by the pg_sequences /
    /// information_schema.sequences / pg_class introspection views.
    ///
    /// TDE CORRECTNESS (v4.21.0): reads through
    /// [`StorageEngine::meta_blobs_with_prefix`] for exactly the reason
    /// [`Catalog::list_index_definitions`] does. `save_sequence` writes through
    /// the encrypting `put`, and this reader is per-record resilient, so the
    /// raw-iterator version made every sequence definition silently VANISH on
    /// an encrypted data directory — taking `nextval`, `SERIAL` defaults and the
    /// `pg_sequences` / `information_schema.sequences` views with it. Same
    /// defect class, same fix. `meta:seqstate:` is not a prefix of
    /// `meta:sequence:` (they diverge at byte 7), so the state records stay out
    /// of this scan.
    pub fn list_sequences(&self) -> Result<Vec<PersistedSequence>> {
        let mut sequences = Vec::new();

        for (name, value) in self.storage.meta_blobs_with_prefix("meta:sequence:") {
            if let Some(def) = Self::decode_persisted_sequence(&name, &value) {
                sequences.push(def);
            }
        }

        sequences.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(sequences)
    }

    /// Drop a sequence: deletes BOTH the definition and the high-water state
    /// records. No-op for whichever key is already absent.
    pub fn drop_sequence(&self, name: &str) -> Result<()> {
        self.storage.delete(&Self::sequence_def_key(name))?;
        self.storage.delete(&Self::sequence_state_key(name))?;
        Ok(())
    }

    /// Read the durable high-water state for a sequence (the resume point).
    pub fn get_sequence_state(&self, name: &str) -> Result<Option<PersistedSeqState>> {
        let key = Self::sequence_state_key(name);
        match self.storage.get(&key)? {
            Some(bytes) => {
                // Tagged frame: magic + version + bincode.
                if bytes.len() > SEQ_STATE_MAGIC.len() && bytes.starts_with(SEQ_STATE_MAGIC) {
                    let version = bytes[SEQ_STATE_MAGIC.len()];
                    let body = &bytes[SEQ_STATE_MAGIC.len() + 1..];
                    if version == SEQ_STATE_FORMAT_VERSION {
                        return Ok(bincode::deserialize::<PersistedSeqState>(body).ok());
                    }
                    tracing::warn!(
                        "Sequence load: state for '{}' has unsupported version {} (this binary v{})",
                        name,
                        version,
                        SEQ_STATE_FORMAT_VERSION
                    );
                    return Ok(None);
                }
                // Legacy untagged fallback.
                Ok(bincode::deserialize::<PersistedSeqState>(&bytes).ok())
            }
            None => Ok(None),
        }
    }

    /// Persist the high-water state record. This performs the `put` only — it
    /// does NOT fsync. The no-duplicate invariant's fsync is forced by
    /// `StorageEngine::flush_sequence_state`, which wraps this call.
    pub fn save_sequence_state(&self, name: &str, st: &PersistedSeqState) -> Result<()> {
        let key = Self::sequence_state_key(name);
        let body =
            bincode::serialize(st).map_err(|e| Error::storage(format!("Failed to serialize sequence state: {}", e)))?;
        let mut value = Vec::with_capacity(SEQ_STATE_MAGIC.len() + 1 + body.len());
        value.extend_from_slice(SEQ_STATE_MAGIC);
        value.push(SEQ_STATE_FORMAT_VERSION);
        value.extend_from_slice(&body);
        self.storage.put(&key, &value)
    }

    // -------------------------------------------------------------------
    // SQL ROLES + ACL RECORDS (HC4 storage slice).
    //
    // *** NO PRIVILEGE IS ENFORCED ANYWHERE IN THIS BUILD. ***
    //
    // These records exist so that `CREATE/ALTER/DROP ROLE` and
    // `GRANT`/`REVOKE` stop being silent no-ops and so the catalog views
    // (`pg_roles` / `pg_user` / `pg_authid` /
    // `information_schema.table_privileges` / `role_table_grants`) report
    // what was actually asked for instead of two fabricated all-privilege
    // superusers and permanently-empty grant views. NOTHING reads an
    // `AclRecord` to decide whether a statement is allowed — there is no
    // check at any DML choke point. Treat this as an introspection catalog,
    // never as a security boundary.
    //
    // Storage follows the existing `meta:`-prefix pattern exactly
    // (`register_schema`, `save_sequence`, `register_enum_type`,
    // `save_trigger`):
    //   meta:role:<name>                        -> RoleRecord
    //   meta:acl:<objtype>:<objname>:<grantee>   -> AclRecord
    //   meta:role_oid_next                       -> u32 LE, next free OID
    //
    // Prefix-scan safety: `meta:role_oid_next` is NOT inside the
    // `meta:role:` scan — the two diverge at byte 10 (':' 0x3A vs '_' 0x5F)
    // and ':' sorts first, so the forward scan hits every `meta:role:` key
    // before the counter and breaks on it. Same reasoning the
    // `meta:sequence:` / `meta:seqstate:` split relies on.
    // -------------------------------------------------------------------

    fn role_key(name: &str) -> Vec<u8> {
        format!("meta:role:{}", name).into_bytes()
    }

    fn role_oid_counter_key() -> Vec<u8> {
        b"meta:role_oid_next".to_vec()
    }

    fn acl_key(object_type: &str, object_name: &str, grantee: &str) -> Vec<u8> {
        format!("meta:acl:{}:{}:{}", object_type, object_name, grantee).into_bytes()
    }

    /// Frame a record: magic + version byte + bincode body, so a future
    /// format bump is *detectable* rather than silently misread.
    fn encode_tagged<T: Serialize>(magic: &[u8; 4], version: u8, value: &T, what: &str) -> Result<Vec<u8>> {
        let body = bincode::serialize(value)
            .map_err(|e| Error::storage(format!("Failed to serialize {what} catalog record: {e}")))?;
        let mut out = Vec::with_capacity(magic.len() + 1 + body.len());
        out.extend_from_slice(magic);
        out.push(version);
        out.extend_from_slice(&body);
        Ok(out)
    }

    /// Decode a framed record. Deliberately LOUD: an undecodable or
    /// future-version role/ACL record is an error, not a skipped row.
    /// Silently dropping it would under-report the catalog — exactly the
    /// "views lie about privileges" failure this slice exists to remove.
    fn decode_tagged<T: for<'de> Deserialize<'de>>(
        magic: &[u8; 4],
        version: u8,
        bytes: &[u8],
        what: &str,
    ) -> Result<T> {
        if bytes.len() <= magic.len() || !bytes.starts_with(magic) {
            return Err(Error::storage(format!(
                "{what} catalog record is not in a recognised format ({} bytes)",
                bytes.len()
            )));
        }
        let found = bytes[magic.len()];
        if found != version {
            return Err(Error::storage(format!(
                "{what} catalog record has on-disk format version {found}, this binary supports v{version}"
            )));
        }
        // In-bounds by the length check above (`bytes.len() > magic.len()`).
        let body = &bytes[magic.len() + 1..];
        bincode::deserialize(body).map_err(|e| Error::storage(format!("Failed to decode {what} catalog record: {e}")))
    }

    /// Allocate the next role OID and persist the bumped counter. OIDs are
    /// never reused (a dropped role's OID stays retired), so a role keeps a
    /// stable identity for the lifetime of the data directory.
    pub fn allocate_role_oid(&self) -> Result<u32> {
        let key = Self::role_oid_counter_key();
        let next = match self.storage.get(&key)? {
            Some(bytes) => {
                if bytes.len() != 4 {
                    return Err(Error::storage(format!(
                        "role OID counter (meta:role_oid_next) is corrupt: {} bytes, expected 4",
                        bytes.len()
                    )));
                }
                u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
            }
            None => FIRST_ROLE_OID,
        };
        let after = next
            .checked_add(1)
            .ok_or_else(|| Error::storage("role OID space exhausted (u32 overflow)"))?;
        self.storage.put(&key, &after.to_le_bytes())?;
        Ok(next)
    }

    /// Persist a role record, overwriting any existing entry with the same
    /// name. Existence/duplicate semantics belong to the caller (the
    /// executor's CREATE/ALTER ROLE arms).
    pub fn save_role(&self, role: &RoleRecord) -> Result<()> {
        let key = Self::role_key(&role.name);
        let value = Self::encode_tagged(ROLE_MAGIC, ROLE_FORMAT_VERSION, role, "role")?;
        self.storage.put(&key, &value)?;
        // Catalog-visibility change: drop cached `SELECT … FROM pg_roles`
        // style results so the next read reflects this role.
        self.storage.bump_schema_generation();
        Ok(())
    }

    /// Look up one persisted role by name.
    pub fn get_role(&self, name: &str) -> Result<Option<RoleRecord>> {
        match self.storage.get(&Self::role_key(name))? {
            Some(bytes) => Ok(Some(Self::decode_tagged(
                ROLE_MAGIC,
                ROLE_FORMAT_VERSION,
                &bytes,
                "role",
            )?)),
            None => Ok(None),
        }
    }

    /// True if a role with this name is persisted. Built-in virtual roles
    /// (`postgres`, `helios`, `public`) are NOT persisted and report false
    /// here — see `is_reserved_role_name`.
    pub fn role_exists(&self, name: &str) -> Result<bool> {
        Ok(self.storage.get(&Self::role_key(name))?.is_some())
    }

    /// Remove a role record. No-op when absent (the executor pre-checks so it
    /// can honour `IF EXISTS` and emit the PostgreSQL error text otherwise).
    pub fn delete_role(&self, name: &str) -> Result<()> {
        self.storage.delete(&Self::role_key(name))?;
        self.storage.bump_schema_generation();
        Ok(())
    }

    /// Every persisted role, sorted by name.
    ///
    /// Reads through [`StorageEngine::meta_blobs_with_prefix`], NOT off a raw
    /// RocksDB iterator: `save_role` writes through `StorageEngine::put`, which
    /// ENCRYPTS when a key manager is configured, so on a TDE data dir a raw
    /// iterator hands back ciphertext and `decode_tagged` rejects every record.
    /// `get_role` already reads through `get`; this keeps ONE on-disk read
    /// discipline for the `meta:role:` namespace. The `meta:role:` prefix does
    /// not match the `meta:role_oid_next` counter (they diverge at byte 10), so
    /// the counter is not scanned.
    pub fn list_roles(&self) -> Result<Vec<RoleRecord>> {
        let mut roles = Vec::new();
        for (_suffix, value) in self.storage.meta_blobs_with_prefix("meta:role:") {
            roles.push(Self::decode_tagged(ROLE_MAGIC, ROLE_FORMAT_VERSION, &value, "role")?);
        }
        roles.sort_by(|a: &RoleRecord, b: &RoleRecord| a.name.cmp(&b.name));
        Ok(roles)
    }

    /// Read one ACL record (the full privilege set held by `grantee` on one
    /// object).
    pub fn get_acl(&self, object_type: &str, object_name: &str, grantee: &str) -> Result<Option<AclRecord>> {
        match self.storage.get(&Self::acl_key(object_type, object_name, grantee))? {
            Some(bytes) => Ok(Some(Self::decode_tagged(ACL_MAGIC, ACL_FORMAT_VERSION, &bytes, "ACL")?)),
            None => Ok(None),
        }
    }

    /// Every persisted ACL record, sorted by (object_type, object_name,
    /// grantee) — the order the privilege views render in.
    ///
    /// Reads through [`StorageEngine::meta_blobs_with_prefix`] for the same
    /// reason as [`Catalog::list_roles`]: `grant_privileges` writes through
    /// `StorageEngine::put`, which encrypts on a TDE data dir, and `get_acl`
    /// already reads through `get`. Two readers of one key namespace must not
    /// disagree about the on-disk format.
    pub fn list_acls(&self) -> Result<Vec<AclRecord>> {
        let mut acls = Vec::new();
        for (_suffix, value) in self.storage.meta_blobs_with_prefix("meta:acl:") {
            acls.push(Self::decode_tagged(ACL_MAGIC, ACL_FORMAT_VERSION, &value, "ACL")?);
        }
        acls.sort_by(|a: &AclRecord, b: &AclRecord| {
            (&a.object_type, &a.object_name, &a.grantee).cmp(&(&b.object_type, &b.object_name, &b.grantee))
        });
        Ok(acls)
    }

    /// Every ACL record naming `grantee`. Used by DROP ROLE's dependency
    /// check and by MySQL `SHOW GRANTS`.
    pub fn list_acls_for_grantee(&self, grantee: &str) -> Result<Vec<AclRecord>> {
        Ok(self
            .list_acls()?
            .into_iter()
            .filter(|acl| acl.grantee == grantee)
            .collect())
    }

    /// GRANT: merge `privileges` into the record for
    /// (object_type, object_name, grantee), creating it when absent. An
    /// already-held privilege has its grantable flag OR-ed with `grantable`
    /// (PostgreSQL: re-granting WITH GRANT OPTION upgrades, never downgrades).
    ///
    /// STORES ONLY. Nothing consults the result to authorise a statement.
    pub fn grant_privileges(
        &self,
        object_type: &str,
        object_name: &str,
        grantee: &str,
        grantor: &str,
        privileges: &[String],
        grantable: bool,
    ) -> Result<()> {
        let mut record = self
            .get_acl(object_type, object_name, grantee)?
            .unwrap_or_else(|| AclRecord {
                object_type: object_type.to_string(),
                object_name: object_name.to_string(),
                grantee: grantee.to_string(),
                grantor: grantor.to_string(),
                privileges: Vec::new(),
            });
        for privilege in privileges {
            match record.privileges.iter_mut().find(|(p, _)| p == privilege) {
                Some(existing) => existing.1 = existing.1 || grantable,
                None => record.privileges.push((privilege.clone(), grantable)),
            }
        }
        record.privileges.sort_by(|a, b| a.0.cmp(&b.0));
        let key = Self::acl_key(object_type, object_name, grantee);
        let value = Self::encode_tagged(ACL_MAGIC, ACL_FORMAT_VERSION, &record, "ACL")?;
        self.storage.put(&key, &value)?;
        self.storage.bump_schema_generation();
        Ok(())
    }

    /// REVOKE: remove `privileges` from the record, deleting the record once
    /// it holds nothing. Revoking something that was never granted succeeds
    /// silently — PostgreSQL emits a warning and succeeds too.
    ///
    /// `REVOKE GRANT OPTION FOR …` is deliberately not modelled: sqlparser
    /// 0.53 cannot parse that spelling at all, so it fails LOUD at the parse
    /// stage rather than being half-implemented here.
    pub fn revoke_privileges(
        &self,
        object_type: &str,
        object_name: &str,
        grantee: &str,
        privileges: &[String],
    ) -> Result<()> {
        let mut record = match self.get_acl(object_type, object_name, grantee)? {
            Some(record) => record,
            None => return Ok(()),
        };
        record.privileges.retain(|(p, _)| !privileges.iter().any(|r| r == p));
        let key = Self::acl_key(object_type, object_name, grantee);
        if record.privileges.is_empty() {
            self.storage.delete(&key)?;
        } else {
            let value = Self::encode_tagged(ACL_MAGIC, ACL_FORMAT_VERSION, &record, "ACL")?;
            self.storage.put(&key, &value)?;
        }
        self.storage.bump_schema_generation();
        Ok(())
    }

    /// Drop every ACL record naming `grantee`. Not used by DROP ROLE (which
    /// refuses while grants exist, PostgreSQL-style); provided so a future
    /// `DROP OWNED BY` / `DROP ROLE … CASCADE` has one implementation to call.
    pub fn delete_acls_for_grantee(&self, grantee: &str) -> Result<()> {
        for acl in self.list_acls_for_grantee(grantee)? {
            self.storage
                .delete(&Self::acl_key(&acl.object_type, &acl.object_name, &acl.grantee))?;
        }
        self.storage.bump_schema_generation();
        Ok(())
    }

    /// Build counter key for table row IDs
    fn table_counter_key(table_name: &str) -> Vec<u8> {
        format!("counter:{}", table_name).into_bytes()
    }

    /// Build compression config key for a table
    fn compression_config_key(table_name: &str) -> Vec<u8> {
        format!("compression:config:{}", table_name).into_bytes()
    }

    /// Build compression stats key for a table
    fn compression_stats_key(table_name: &str) -> Vec<u8> {
        format!("compression:stats:{}", table_name).into_bytes()
    }

    /// Build statistics key for a table
    fn table_statistics_key(table_name: &str) -> Vec<u8> {
        format!("statistics:table:{}", table_name).into_bytes()
    }

    /// Set compression configuration for a table
    pub fn set_compression_config(&self, table_name: &str, config: &CompressionConfig) -> Result<()> {
        let key = Self::compression_config_key(table_name);
        let value = bincode::serialize(config)
            .map_err(|e| Error::storage(format!("Failed to serialize compression config: {}", e)))?;
        self.storage.put(&key, &value)
    }

    /// Get compression configuration for a table
    pub fn get_compression_config(&self, table_name: &str) -> Result<Option<CompressionConfig>> {
        let key = Self::compression_config_key(table_name);
        match self.storage.get(&key)? {
            Some(data) => {
                let config = bincode::deserialize(&data)
                    .map_err(|e| Error::storage(format!("Failed to deserialize compression config: {}", e)))?;
                Ok(Some(config))
            }
            None => Ok(None),
        }
    }

    /// Set compression statistics for a table
    pub fn set_compression_stats(&self, table_name: &str, stats: &CompressionStats) -> Result<()> {
        let key = Self::compression_stats_key(table_name);
        let value = bincode::serialize(stats)
            .map_err(|e| Error::storage(format!("Failed to serialize compression stats: {}", e)))?;
        self.storage.put(&key, &value)
    }

    /// Get compression statistics for a table
    pub fn get_compression_stats(&self, table_name: &str) -> Result<Option<CompressionStats>> {
        let key = Self::compression_stats_key(table_name);
        match self.storage.get(&key)? {
            Some(data) => {
                let stats = bincode::deserialize(&data)
                    .map_err(|e| Error::storage(format!("Failed to deserialize compression stats: {}", e)))?;
                Ok(Some(stats))
            }
            None => Ok(None),
        }
    }

    /// Set table statistics
    pub fn set_table_statistics(&self, table_name: &str, stats: &TableStatistics) -> Result<()> {
        let key = Self::table_statistics_key(table_name);
        let value = bincode::serialize(stats)
            .map_err(|e| Error::storage(format!("Failed to serialize table statistics: {}", e)))?;
        self.storage.put(&key, &value)
    }

    /// Get table statistics
    pub fn get_table_statistics(&self, table_name: &str) -> Result<Option<TableStatistics>> {
        // Try cache first
        let cache = self.storage.statistics_cache();
        if let Some(cached_stats) = cache.get(table_name)? {
            return Ok(Some((*cached_stats).clone()));
        }

        // Cache miss - load from storage
        let key = Self::table_statistics_key(table_name);
        match self.storage.get(&key)? {
            Some(data) => {
                let stats: TableStatistics = bincode::deserialize(&data)
                    .map_err(|e| Error::storage(format!("Failed to deserialize table statistics: {}", e)))?;

                // Store in cache
                cache.put(table_name.to_string(), stats.clone())?;

                Ok(Some(stats))
            }
            None => Ok(None),
        }
    }

    /// Analyze a table and update statistics
    ///
    /// Performs a full table scan to collect statistics for query planning.
    /// This should be called periodically or after significant data changes.
    pub fn analyze_table(&self, table_name: &str) -> Result<()> {
        use super::statistics::StatisticsAnalyzer;

        // Get table schema
        let schema = self.get_table_schema(table_name)?;

        // Scan all tuples
        let tuples = self.storage.scan_table(table_name)?;

        // Analyze and collect statistics
        let stats = StatisticsAnalyzer::analyze_table(table_name, &tuples, &schema)?;

        // Invalidate cache before storing new statistics
        let cache = self.storage.statistics_cache();
        cache.invalidate(table_name)?;

        // Store statistics
        self.set_table_statistics(table_name, &stats)?;

        tracing::info!("Analyzed table '{}' and invalidated statistics cache", table_name);

        Ok(())
    }

    // === Trigger Persistence Methods ===

    /// Build trigger metadata key
    fn trigger_metadata_key(table_name: &str, trigger_name: &str) -> Vec<u8> {
        format!("trigger:{}:{}", table_name, trigger_name).into_bytes()
    }

    /// Save a trigger to persistent storage
    pub fn save_trigger(&self, definition: &crate::sql::TriggerDefinition) -> Result<()> {
        let key = Self::trigger_metadata_key(&definition.table_name, &definition.name);
        let value = bincode::serialize(definition)
            .map_err(|e| Error::storage(format!("Failed to serialize trigger definition: {}", e)))?;
        self.storage.put(&key, &value)
    }

    /// Load a trigger from persistent storage
    pub fn load_trigger(&self, table_name: &str, trigger_name: &str) -> Result<Option<crate::sql::TriggerDefinition>> {
        let key = Self::trigger_metadata_key(table_name, trigger_name);
        match self.storage.get(&key)? {
            Some(data) => {
                let definition = bincode::deserialize(&data)
                    .map_err(|e| Error::storage(format!("Failed to deserialize trigger definition: {}", e)))?;
                Ok(Some(definition))
            }
            None => Ok(None),
        }
    }

    /// Delete a trigger from persistent storage
    pub fn delete_trigger(&self, table_name: &str, trigger_name: &str) -> Result<()> {
        let key = Self::trigger_metadata_key(table_name, trigger_name);
        self.storage.delete(&key)
    }

    /// Load all triggers from persistent storage.
    ///
    /// Values are fetched through [`StorageEngine::meta_blobs_with_prefix`],
    /// i.e. through `StorageEngine::get`, NOT off a raw RocksDB iterator: on a
    /// TDE data dir the raw iterator hands back ciphertext (`save_trigger`
    /// writes through `put`, which encrypts), so the previous raw-iterator read
    /// could only ever have produced a deserialize error on an encrypted
    /// database. It had no callers, so nothing noticed.
    ///
    /// An unreadable entry is warned about and SKIPPED rather than failing the
    /// whole load: the caller is the open-time loader, and one corrupt trigger
    /// record must degrade to "that trigger is missing", never to "the database
    /// will not open".
    ///
    /// The `trigger:` prefix does NOT collide with the `trigger_rowmut:`
    /// sidecar namespace — they diverge at byte 7 (`:` 0x3A vs `_` 0x5F).
    pub fn load_all_triggers(&self) -> Result<Vec<crate::sql::TriggerDefinition>> {
        let mut triggers = Vec::new();
        for (suffix, value) in self.storage.meta_blobs_with_prefix("trigger:") {
            match bincode::deserialize::<crate::sql::TriggerDefinition>(&value) {
                Ok(definition) => triggers.push(definition),
                Err(e) => tracing::warn!("persisted trigger 'trigger:{}' is unreadable, skipped: {}", suffix, e),
            }
        }
        Ok(triggers)
    }

    /// Delete all triggers for a table (called when table is dropped).
    ///
    /// Removes BOTH the `trigger:{table}:*` definitions and the
    /// `trigger_rowmut:{table}:*` rewrite recipes — the two are always written
    /// and deleted together.
    pub fn delete_table_triggers(&self, table_name: &str) -> Result<usize> {
        let count = self.delete_keys_with_prefix(&format!("trigger:{}:", table_name))?;
        self.delete_keys_with_prefix(&format!("trigger_rowmut:{}:", table_name))?;
        Ok(count)
    }

    /// Delete every key under `prefix`, returning how many were removed.
    fn delete_keys_with_prefix(&self, prefix: &str) -> Result<usize> {
        let prefix_bytes = prefix.as_bytes();
        let mut keys_to_delete = Vec::new();

        // Seek to the prefix instead of scanning from the keyspace start.
        let mut read_opts = rocksdb::ReadOptions::default();
        read_opts.set_total_order_seek(true);
        let iter = self.storage.db.iterator_opt(
            rocksdb::IteratorMode::From(prefix_bytes, rocksdb::Direction::Forward),
            read_opts,
        );
        for item in iter {
            let (key, _) = item.map_err(|e| Error::storage(format!("Iterator error: {}", e)))?;

            if !key.starts_with(prefix_bytes) {
                break;
            }
            keys_to_delete.push(key.to_vec());
        }

        let count = keys_to_delete.len();
        for key in keys_to_delete {
            self.storage.delete(&key)?;
        }

        Ok(count)
    }

    // === BEFORE-row rewrite recipe (`TriggerRowMutation`) persistence ===
    //
    // A SEPARATE, purely ADDITIVE key namespace, deliberately not a field on
    // `TriggerDefinition`. That struct is bincode-POSITIONAL and is stored both
    // in `trigger:{table}:{name}` and inside `WalOperation::CreateTrigger`, and
    // is replicated to standbys as a `SchemaChange` — adding a field to it
    // would break replay of every record an older build wrote. Do not
    // "simplify" this by folding the recipe into the definition.

    /// Build the rewrite-recipe key: `trigger_rowmut:{table}:{trigger}`.
    fn trigger_row_mutation_key(table_name: &str, trigger_name: &str) -> Vec<u8> {
        format!("trigger_rowmut:{}:{}", table_name, trigger_name).into_bytes()
    }

    /// Persist a trigger's BEFORE-row rewrite recipe.
    pub fn save_trigger_row_mutation(
        &self,
        table_name: &str,
        trigger_name: &str,
        mutation: &crate::sql::triggers::TriggerRowMutation,
    ) -> Result<()> {
        let key = Self::trigger_row_mutation_key(table_name, trigger_name);
        let value = Self::encode_tagged(
            TRIGGER_ROWMUT_MAGIC,
            TRIGGER_ROWMUT_FORMAT_VERSION,
            mutation,
            "trigger row mutation",
        )?;
        self.storage.put(&key, &value)
    }

    /// Remove a trigger's BEFORE-row rewrite recipe.
    pub fn delete_trigger_row_mutation(&self, table_name: &str, trigger_name: &str) -> Result<()> {
        let key = Self::trigger_row_mutation_key(table_name, trigger_name);
        self.storage.delete(&key)
    }

    /// Every persisted rewrite recipe, as `(table_name, trigger_name, recipe)`.
    ///
    /// Unreadable entries are warned about and skipped, for the same reason
    /// [`Catalog::load_all_triggers`] skips them.
    pub fn load_all_trigger_row_mutations(
        &self,
    ) -> Result<Vec<(String, String, crate::sql::triggers::TriggerRowMutation)>> {
        let mut out = Vec::new();
        for (suffix, value) in self.storage.meta_blobs_with_prefix("trigger_rowmut:") {
            // `suffix` is `{table}:{trigger}`. Split from the RIGHT: trigger
            // names are bare identifiers, while a table name may be schema
            // qualified (`s.t`).
            let Some((table_name, trigger_name)) = suffix.rsplit_once(':') else {
                tracing::warn!(
                    "malformed trigger row-mutation key 'trigger_rowmut:{}' — skipped",
                    suffix
                );
                continue;
            };
            match Self::decode_tagged::<crate::sql::triggers::TriggerRowMutation>(
                TRIGGER_ROWMUT_MAGIC,
                TRIGGER_ROWMUT_FORMAT_VERSION,
                &value,
                "trigger row mutation",
            ) {
                Ok(mutation) => out.push((table_name.to_string(), trigger_name.to_string(), mutation)),
                Err(e) => tracing::warn!(
                    "persisted trigger row mutation 'trigger_rowmut:{}' is unreadable and was skipped: {}",
                    suffix,
                    e
                ),
            }
        }
        Ok(out)
    }

    // === Constraint Persistence Methods ===

    /// Build constraint metadata key
    fn constraint_key(table_name: &str, constraint_name: &str) -> Vec<u8> {
        format!("constraint:{}:{}", table_name, constraint_name).into_bytes()
    }

    /// Build table constraints key (for storing all constraints of a table)
    fn table_constraints_key(table_name: &str) -> Vec<u8> {
        format!("table_constraints:{}", table_name).into_bytes()
    }

    /// Save table constraints to persistent storage
    pub fn save_table_constraints(&self, table_name: &str, constraints: &crate::sql::TableConstraints) -> Result<()> {
        let key = Self::table_constraints_key(table_name);
        let value = bincode::serialize(constraints)
            .map_err(|e| Error::storage(format!("Failed to serialize table constraints: {}", e)))?;
        self.storage.put(&key, &value)?;
        self.storage.cache_table_constraints(table_name, constraints.clone());
        self.storage.clear_referencing_fk_cache();
        Ok(())
    }

    /// Load table constraints from persistent storage
    pub fn load_table_constraints(&self, table_name: &str) -> Result<crate::sql::TableConstraints> {
        if let Some(constraints) = self.storage.get_cached_table_constraints(table_name) {
            return Ok(constraints);
        }

        let key = Self::table_constraints_key(table_name);
        let constraints = match self.storage.get(&key)? {
            Some(data) => bincode::deserialize(&data)
                .map_err(|e| Error::storage(format!("Failed to deserialize table constraints: {}", e)))?,
            None => crate::sql::TableConstraints::default(),
        };
        self.storage.cache_table_constraints(table_name, constraints.clone());
        Ok(constraints)
    }

    /// Add a foreign key constraint to a table
    pub fn add_foreign_key(&self, fk: crate::sql::ForeignKeyConstraint) -> Result<()> {
        let mut constraints = self.load_table_constraints(&fk.table_name)?;
        constraints.add_foreign_key(fk.clone());
        self.save_table_constraints(&fk.table_name, &constraints)?;

        // Auto-create ART index for FK lookups.
        let art_manager = self.storage.art_indexes();
        match art_manager.create_fk_index(
            &fk.table_name,
            &fk.columns,
            &fk.references_table,
            &fk.references_columns,
            Some(&fk.name),
        ) {
            Ok(index_name) => {
                // Backfill from rows already in the table. An FK added after the
                // data is loaded (the common bulk-migration order) would
                // otherwise leave an empty index, and equality lookups /
                // FK-column joins that the planner answers from it silently
                // return zero matches. CREATE INDEX backfills the same way.
                match self.get_table_schema(&fk.table_name) {
                    Ok(schema) => match self.storage.scan_table_with_schema(&fk.table_name, &schema) {
                        Ok(tuples) => {
                            if !tuples.is_empty() {
                                if let Err(e) = art_manager.backfill_fk_index(&index_name, &schema, &tuples) {
                                    tracing::warn!(
                                        "Failed to backfill FK ART index '{}' on '{}': {}",
                                        index_name,
                                        fk.table_name,
                                        e
                                    );
                                }
                            }
                        }
                        Err(e) => tracing::warn!(
                            "Failed to scan '{}' to backfill FK index '{}': {}",
                            fk.table_name,
                            index_name,
                            e
                        ),
                    },
                    Err(e) => tracing::warn!(
                        "Failed to load schema of '{}' to backfill FK index '{}': {}",
                        fk.table_name,
                        index_name,
                        e
                    ),
                }
                tracing::debug!(
                    "Created FK ART index for constraint '{}' on table '{}'",
                    fk.name,
                    fk.table_name
                );
            }
            Err(e) => {
                tracing::warn!("Failed to create FK ART index for constraint '{}': {}", fk.name, e);
            }
        }

        Ok(())
    }

    /// Get all foreign key constraints referencing a specific table
    pub fn get_referencing_fks(&self, referenced_table: &str) -> Result<Vec<crate::sql::ForeignKeyConstraint>> {
        if let Some(cached) = self.storage.get_cached_referencing_fks(referenced_table) {
            return Ok(cached);
        }

        let mut result = Vec::new();
        let prefix = b"table_constraints:";

        // Seek directly to the `table_constraints:` prefix instead of iterating
        // from the start of the keyspace. With `IteratorMode::Start` this walked
        // every `data:` row key (which sort before `table_constraints:`), making
        // every reverse-FK lookup -- and therefore every DELETE -- O(table rows).
        // `total_order_seek` is required because the DB uses a 5-byte prefix extractor.
        let mut read_opts = rocksdb::ReadOptions::default();
        read_opts.set_total_order_seek(true);
        let iter = self.storage.db.iterator_opt(
            rocksdb::IteratorMode::From(prefix, rocksdb::Direction::Forward),
            read_opts,
        );
        for item in iter {
            let (key, value) = item.map_err(|e| Error::storage(format!("Iterator error: {}", e)))?;

            if !key.starts_with(prefix) {
                // We seeked to `table_constraints:`; the first key that no longer
                // shares the prefix means the whole group has been read.
                break;
            }

            let constraints: crate::sql::TableConstraints = bincode::deserialize(&value)
                .map_err(|e| Error::storage(format!("Failed to deserialize constraints: {}", e)))?;

            for fk in constraints.foreign_keys {
                if fk.references_table == referenced_table {
                    result.push(fk);
                }
            }
        }

        self.storage.cache_referencing_fks(referenced_table, result.clone());
        Ok(result)
    }

    /// Delete all constraints for a table (called when table is dropped)
    pub fn delete_table_constraints(&self, table_name: &str) -> Result<()> {
        let key = Self::table_constraints_key(table_name);
        self.storage.delete(&key)?;
        self.storage.invalidate_table_constraints_cache(table_name);
        Ok(())
    }

    /// Drop a specific constraint by name
    pub fn drop_constraint(&self, table_name: &str, constraint_name: &str) -> Result<bool> {
        let mut constraints = self.load_table_constraints(table_name)?;
        let initial_fk_len = constraints.foreign_keys.len();
        let initial_unique_len = constraints.unique_constraints.len();
        let initial_check_len = constraints.check_constraints.len();

        // Find FK constraint to drop its ART index
        let fk_to_drop = constraints
            .foreign_keys
            .iter()
            .find(|fk| fk.name == constraint_name)
            .cloned();

        // Find unique constraint to drop its ART index
        let unique_to_drop = constraints
            .unique_constraints
            .iter()
            .find(|u| u.name == constraint_name)
            .cloned();

        constraints.foreign_keys.retain(|fk| fk.name != constraint_name);
        constraints.check_constraints.retain(|c| c.name != constraint_name);
        constraints.unique_constraints.retain(|u| u.name != constraint_name);

        let final_len =
            constraints.foreign_keys.len() + constraints.check_constraints.len() + constraints.unique_constraints.len();
        let initial_len = initial_fk_len + initial_check_len + initial_unique_len;

        if initial_len != final_len {
            self.save_table_constraints(table_name, &constraints)?;

            // Drop associated ART indexes
            let art_manager = self.storage.art_indexes();

            // Drop FK ART index if constraint was a foreign key
            if let Some(fk) = fk_to_drop {
                let fk_index_name = format!("fk_{}_{}", fk.table_name, fk.name);
                if let Err(e) = art_manager.drop_index(&fk_index_name) {
                    tracing::warn!("Failed to drop FK ART index '{}': {}", fk_index_name, e);
                }
            }

            // Drop UNIQUE ART index if constraint was a unique constraint
            if let Some(unique) = unique_to_drop {
                let unique_index_name = format!("unique_{}_{}", table_name, unique.name);
                if let Err(e) = art_manager.drop_index(&unique_index_name) {
                    tracing::warn!("Failed to drop UNIQUE ART index '{}': {}", unique_index_name, e);
                }
            }

            Ok(true)
        } else {
            Ok(false)
        }
    }
}

/// Implement TriggerPersistence trait for Catalog
impl TriggerPersistence for Catalog<'_> {
    fn save_trigger(&self, definition: &TriggerDefinition) -> Result<()> {
        self.save_trigger(definition)
    }

    fn load_trigger(&self, table_name: &str, trigger_name: &str) -> Result<Option<TriggerDefinition>> {
        self.load_trigger(table_name, trigger_name)
    }

    fn delete_trigger(&self, table_name: &str, trigger_name: &str) -> Result<()> {
        self.delete_trigger(table_name, trigger_name)
    }

    fn load_all_triggers(&self) -> Result<Vec<TriggerDefinition>> {
        self.load_all_triggers()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::{Column, Config, DataType};

    /// Every tag any writer persists into a `meta:index:` record must be
    /// classifiable, and must classify into the family whose manager actually
    /// owns that structure.
    ///
    /// `persistent_hnsw` is the reason this test exists: `handle_drop_index`
    /// shipped its first draft with a hand-written tag list that omitted it, so
    /// `CREATE INDEX … USING hnsw … WITH (persistent = true)` produced an index
    /// that `rebuild_vector_indexes` reopened at every start and NOTHING could
    /// drop — reported as "unsupported persisted index type", i.e. the engine
    /// telling the user their own catalog was corrupt.
    ///
    /// If you add an index type, add its tag to `index_family` and to this
    /// list. The literals below are the exact strings passed to
    /// `persist_index_definition` in `sql/executor/ddl.rs`.
    #[test]
    fn every_persisted_index_tag_is_classified() {
        for (tag, expected) in [
            // Legacy pre-v3.37.2 records carry no tag at all.
            (None, IndexFamily::Art),
            (Some("art"), IndexFamily::Art),
            // Accepted `USING` spellings, normalized to `art` when persisted.
            (Some("btree"), IndexFamily::Art),
            (Some("hash"), IndexFamily::Art),
            (Some("gin"), IndexFamily::DdlOnly),
            (Some("gist"), IndexFamily::DdlOnly),
            (Some("hnsw"), IndexFamily::Vector),
            (Some("hnsw_pq"), IndexFamily::Vector),
            (Some("persistent_hnsw"), IndexFamily::Vector),
        ] {
            assert_eq!(
                index_family(tag),
                Some(expected),
                "index tag {tag:?} must classify as {expected:?} — an unclassified tag makes \
                 every index of that type UNDROPPABLE"
            );
        }
    }

    /// An unknown tag is `None`, never a guess. `handle_drop_index` turns that
    /// into a named error rather than removing the wrong structure and
    /// reporting a drop that did not happen.
    #[test]
    fn an_unknown_index_tag_is_not_guessed() {
        for tag in ["brin", "spgist", "ivfflat", "", "HNSW"] {
            assert_eq!(
                index_family(Some(tag)),
                None,
                "'{tag}' is not an index type this build implements and must not be classified"
            );
        }
    }

    #[test]
    fn test_create_table() {
        let config = Config::in_memory();
        let storage = StorageEngine::open_in_memory(&config).expect("Failed to open in-memory storage");
        let catalog = Catalog::new(&storage);

        let schema = Schema::new(vec![
            Column {
                name: "id".to_string(),
                data_type: DataType::Int4,
                nullable: false,
                primary_key: true,
                source_table: None,
                source_table_name: None,
                default_expr: None,
                unique: false,
                storage_mode: crate::ColumnStorageMode::Default,
            },
            Column {
                name: "name".to_string(),
                data_type: DataType::Text,
                nullable: true,
                primary_key: false,
                source_table: None,
                source_table_name: None,
                default_expr: None,
                unique: false,
                storage_mode: crate::ColumnStorageMode::Default,
            },
        ]);

        catalog
            .create_table("users", schema.clone())
            .expect("Failed to create table");

        // Verify table exists
        assert!(catalog.table_exists("users").expect("Failed to check if table exists"));

        // Verify schema. B31: get_table_schema now stamps the owning table name on
        // every column (so qualified `t.col` resolution works regardless of the
        // on-disk format), so compare structurally and assert the stamp rather than
        // requiring exact equality with the unstamped input.
        let retrieved_schema = catalog.get_table_schema("users").expect("Failed to get table schema");
        assert_eq!(retrieved_schema.columns.len(), schema.columns.len());
        for (got, want) in retrieved_schema.columns.iter().zip(schema.columns.iter()) {
            assert_eq!(got.name, want.name);
            assert_eq!(got.data_type, want.data_type);
            assert_eq!(got.source_table_name.as_deref(), Some("users"));
        }
    }

    #[test]
    fn b31_get_table_schema_stamps_source_table_name() {
        // Regression for B31: tables persisted by older binaries stored columns with
        // `source_table_name = None`, which broke `SELECT t.col` resolution on the
        // extended-query schema-derivation path (`derive_result_schema` ->
        // `LogicalPlan::schema()`). get_table_schema must stamp the owning table name
        // so qualified resolution succeeds regardless of the on-disk format.
        let config = Config::in_memory();
        let storage = StorageEngine::open_in_memory(&config).expect("Failed to open in-memory storage");
        let catalog = Catalog::new(&storage);

        // Old-format schema: every column has source_table_name = None.
        let schema = Schema::new(vec![
            Column {
                name: "id".to_string(),
                data_type: DataType::Text,
                nullable: false,
                primary_key: true,
                source_table: None,
                source_table_name: None,
                default_expr: None,
                unique: false,
                storage_mode: crate::ColumnStorageMode::Default,
            },
            Column {
                name: "email".to_string(),
                data_type: DataType::Text,
                nullable: true,
                primary_key: false,
                source_table: None,
                source_table_name: None,
                default_expr: None,
                unique: false,
                storage_mode: crate::ColumnStorageMode::Default,
            },
        ]);
        catalog.create_table("leads", schema).expect("Failed to create table");

        let loaded = catalog.get_table_schema("leads").expect("Failed to get table schema");
        assert!(
            loaded.get_qualified_column_index(Some("leads"), "id").is_some(),
            "qualified resolution of leads.id must succeed after B31 stamping"
        );
        assert!(
            loaded.get_qualified_column_index(Some("leads"), "email").is_some(),
            "qualified resolution of leads.email must succeed after B31 stamping"
        );
        for col in &loaded.columns {
            assert_eq!(col.source_table_name.as_deref(), Some("leads"));
        }
    }

    #[test]
    fn test_next_row_id() {
        let config = Config::in_memory();
        let storage = StorageEngine::open_in_memory(&config).expect("Failed to open in-memory storage");
        let catalog = Catalog::new(&storage);

        let schema = Schema::new(vec![Column::new("id", DataType::Int4)]);

        catalog.create_table("test", schema).expect("Failed to create table");

        // Get sequential row IDs
        assert_eq!(catalog.next_row_id("test").expect("Failed to get row ID 1"), 1);
        assert_eq!(catalog.next_row_id("test").expect("Failed to get row ID 2"), 2);
        assert_eq!(catalog.next_row_id("test").expect("Failed to get row ID 3"), 3);
    }

    #[test]
    fn test_drop_table() {
        let config = Config::in_memory();
        let storage = StorageEngine::open_in_memory(&config).expect("Failed to open in-memory storage");
        let catalog = Catalog::new(&storage);

        let schema = Schema::new(vec![Column::new("id", DataType::Int4)]);

        catalog.create_table("temp", schema).expect("Failed to create table");
        assert!(catalog.table_exists("temp").expect("Failed to check if table exists"));

        catalog.drop_table("temp").expect("Failed to drop table");
        assert!(!catalog
            .table_exists("temp")
            .expect("Failed to check if table exists after drop"));
    }

    #[test]
    fn test_drop_table_deletes_data_rows() {
        let config = Config::in_memory();
        let storage = StorageEngine::open_in_memory(&config).expect("Failed to open in-memory storage");
        let catalog = Catalog::new(&storage);

        let schema = Schema::new(vec![
            Column::new("id", DataType::Int4),
            Column::new("name", DataType::Text),
        ]);

        // Create table and insert some data
        catalog.create_table("users", schema).expect("Failed to create table");

        // Insert test data rows using the storage engine
        use crate::Value;
        let tuple1 = crate::Tuple::new(vec![Value::Int4(1), Value::String("Alice".to_string())]);
        let tuple2 = crate::Tuple::new(vec![Value::Int4(2), Value::String("Bob".to_string())]);

        storage.insert_tuple("users", tuple1).expect("Failed to insert tuple 1");
        storage.insert_tuple("users", tuple2).expect("Failed to insert tuple 2");

        // Verify data exists before drop
        let data_before = storage.scan_table("users").expect("Failed to scan table before drop");
        assert_eq!(data_before.len(), 2, "Should have 2 rows before drop");

        // Drop the table
        catalog.drop_table("users").expect("Failed to drop table");

        // Verify metadata is gone
        assert!(!catalog.table_exists("users").expect("Failed to check if table exists"));

        // Verify data rows are actually deleted by checking the raw database
        let data_prefix = b"data:users:";
        let iter = storage.db.iterator(rocksdb::IteratorMode::Start);
        let mut orphaned_keys = Vec::new();

        for item in iter {
            let (key, _) = item.expect("Iterator error");
            if key.starts_with(data_prefix) {
                orphaned_keys.push(String::from_utf8_lossy(&key).to_string());
            }
        }

        assert_eq!(
            orphaned_keys.len(),
            0,
            "Should have no orphaned data rows, found: {:?}",
            orphaned_keys
        );
    }

    #[test]
    fn list_index_definitions_is_resilient_to_bad_records() {
        let config = Config::in_memory();
        let storage = StorageEngine::open_in_memory(&config).expect("open in-memory");
        let catalog = Catalog::new(&storage);

        // A valid definition written through the normal (tagged) path.
        let good = PersistedIndexDefinition {
            table_name: "t".to_string(),
            column_name: "c".to_string(),
            index_type: Some("art".to_string()),
            options: Vec::new(),
        };
        catalog.save_index_definition("good_idx", &good).expect("save good");

        // A legacy untagged record (raw bincode, the pre-tag on-disk format)
        // must still load for back-compat across the upgrade boundary.
        let legacy = PersistedIndexDefinition {
            table_name: "t2".to_string(),
            column_name: "c2".to_string(),
            index_type: None,
            options: Vec::new(),
        };
        storage
            .put(
                &b"meta:index:legacy_idx".to_vec(),
                &bincode::serialize(&legacy).unwrap(),
            )
            .expect("put legacy");

        // A corrupt record — the kind a torn write or a future format change
        // could leave behind. It must be SKIPPED, not abort the whole listing
        // (the failure mode that silently un-indexed the database on upgrade).
        storage
            .put(&b"meta:index:corrupt_idx".to_vec(), &[0xde, 0xad, 0xbe, 0xef, 0x01])
            .expect("put corrupt");

        let defs = catalog.list_index_definitions().expect("list must not error");
        let names: Vec<&str> = defs.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"good_idx"), "tagged index dropped: {names:?}");
        assert!(names.contains(&"legacy_idx"), "legacy index dropped: {names:?}");
        assert!(
            !names.contains(&"corrupt_idx"),
            "corrupt record should be skipped, not surfaced: {names:?}"
        );
    }
}
