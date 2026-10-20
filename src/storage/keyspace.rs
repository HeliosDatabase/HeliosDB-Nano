//! One authoritative map of every RocksDB key prefix the engine writes.
//!
//! A physical standby applies the primary's write batches verbatim and must
//! then bring its in-memory derived state (ART indexes, HNSW graphs, the
//! time-travel snapshot map, caches) in line with the keys each batch touched.
//! That is only possible if every key can be attributed to the subsystem that
//! owns it. Until this module that knowledge lived in some sixty `format!` and
//! byte-literal constructors spread over the storage layer; [`PREFIXES`] and
//! [`FIXED_KEYS`] are now the single place that says what a key is.
//!
//! Every entry is copied from the constructor named in its comment, not
//! reconstructed from documentation. Two tripwire tests keep the table honest:
//!
//! * `tests/keyspace_classification.rs` lexes every `.rs` file under `src/`,
//!   collects each string literal shaped like a key prefix and fails, naming
//!   `file:line`, if [`classify`] calls one `Unknown`. It also fails if an entry
//!   below is no longer backed by any literal in `src/` (a guessed or stale
//!   entry).
//! * `tests/keyspace_wal_corpus.rs` drives a primary through a corpus touching
//!   every subsystem and classifies every key in the resulting RocksDB WAL and
//!   live keyspace.
//!
//! Adding a key family therefore means adding a row here, and the tests say
//! exactly which literal was missed.

/// Every key class the engine writes to the default column family.
///
/// All of them are replicated verbatim to a physical standby except the two
/// receiver cursors, [`KeyClass::ReplAppliedLsn`] and
/// [`KeyClass::PhysicalCursor`]. A physical receiver keeps its cursor in the
/// [`LOCAL_CF`] column family; `PhysicalCursor` names only where a receiver
/// kept it before that family existed, which the engine and the receiver
/// refuse. `ReplAppliedLsn` is a logical standby's cursor and still lives in
/// the default column family (`PHYSICAL_STANDBY_REPLICATION.md` D2). Nothing
/// else node-local uses the default column family.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeyClass {
    /// `data:{table}:{row_id}` — the current row image.
    Data,
    /// `v:{table}:{row_id}:{commit_ts}` — a time-travel row version.
    Version,
    /// `v_idx:{table}:{row_id}:{reverse_ts:020}` — the version index.
    VersionIdx,
    /// `snapshot:{ts}` — registered snapshot metadata.
    Snapshot,
    /// `txn_map:{txn}` / `scn_map:{scn}` — legacy snapshot mappings. Nothing
    /// writes them since R1.4; snapshot GC still deletes them, so they appear
    /// as deletes in the WAL and as live keys in older databases.
    SnapshotMap,
    /// `vmeta:{table}:{first:020}:{last:020}` — a COPY / multi-row INSERT
    /// visibility marker standing in for per-row versions.
    Vmeta,
    /// `meta:table:{table}` — table schema.
    MetaTable,
    /// `meta:index:{index}` — persisted index definition.
    MetaIndex,
    /// `meta:schema:{schema}` — declared schema marker.
    MetaSchema,
    /// `meta:enum_type:{type}` — enum type labels.
    MetaEnumType,
    /// `meta:identity:{table}` — identity-column list.
    MetaIdentity,
    /// `meta:partchild:{parent}` / `meta:partparent:{child}` — partition links.
    MetaPartition,
    /// `meta:sequence:{seq}` — sequence definition.
    MetaSequence,
    /// `meta:seqstate:{seq}` — sequence high-water state.
    MetaSeqState,
    /// `meta:mv:{view}` — materialized-view metadata.
    MetaMv,
    /// `meta:role:{role}` and the `meta:role_oid_next` allocator.
    MetaRole,
    /// `meta:acl:{type}:{object}:{grantee}` — stored grants.
    MetaAcl,
    /// `meta:function:{name}` — user-defined function definition.
    MetaFunction,
    /// `meta:procedure:{name}` — stored procedure definition.
    MetaProcedure,
    /// `meta:col_storage:{table}:{column}` — written only by WAL replay of
    /// `AlterColumnStorage`.
    MetaColStorage,
    /// `meta:delta:last_id` — the MV delta id allocator.
    MetaDelta,
    /// `meta:tde:keycheck` — the encryption key-check sentinel.
    MetaTdeKeyCheck,
    /// `table_constraints:{table}` (and the caller-less `constraint:` key).
    TableConstraints,
    /// `trigger:{table}:{trigger}` — trigger definition.
    Trigger,
    /// `trigger_rowmut:{table}:{trigger}` — BEFORE-row rewrite recipe.
    TriggerRowMut,
    /// `statistics:table:{table}` — ANALYZE statistics.
    Statistics,
    /// `compression:config:{table}` / `compression:stats:{table}`.
    Compression,
    /// `__view_metadata__{view}` — view definition.
    ViewMeta,
    /// `counter:{table}` — the table's row-id high-water mark.
    Counter,
    /// `meta:counter:{table}` — pre-task-#36 row counter; read for
    /// compatibility, written by nothing.
    LegacyCounter,
    /// `dict:{table}:{column}` — a column dictionary.
    Dict,
    /// `col:{table}:{column}:{batch}` — a columnar batch.
    Columnar,
    /// `colz:{table}:{column}:{batch}` — a batch's zone-stats sidecar.
    ColumnarZ,
    /// `colp:{table}:{batch}` — a batch's presence sidecar.
    ColumnarP,
    /// `colpm:{table}` — the presence-manifest marker.
    ColumnarPm,
    /// `colzm:{table}:{column}` — the zone-stats manifest marker.
    ColumnarZm,
    /// `cas:{blake3}` — a content-addressed payload.
    Cas,
    /// `delta:{table}:{id:020}` — an MV delta record (and the caller-less
    /// `mv_delta:` key of the same subsystem).
    Delta,
    /// `branch:meta:*`, `branch:children:*`, `branch:registry`,
    /// `branch:pending_gc` — the branch registry.
    Branch,
    /// `bdata:{branch}:{table}:{row_id}` — a row on a branch overlay.
    BranchData,
    /// `bdel:{branch}:{table}:{row_id}` — a branch-local delete marker.
    BranchDel,
    /// `bv:{branch}:{table}:{row_id}:{ts}` — a branch row version.
    BranchVersion,
    /// `git:*` — Git integration state.
    Git,
    /// `__vidx:{index_id}:*` — a persistent HNSW index's own keyspace.
    Vidx,
    /// `idxsnap:{table}` — a durable ART index snapshot.
    IdxSnap,
    /// `idxsnapv:{table}` — the ART snapshot validity marker.
    IdxSnapMarker,
    /// `vecsnap:{index}` — an HNSW graph-dump sidecar.
    VecSnap,
    /// `vecsnapv:{index}` — the HNSW snapshot validity marker.
    VecSnapMarker,
    /// `wal:entries:{lsn:020}` — a logical WAL entry.
    WalEntries,
    /// `wal:last_lsn` — the logical WAL high-water LSN.
    WalLastLsn,
    /// `wal:checkpoint` — the logical WAL redo floor.
    WalCheckpoint,
    /// `wal:pruned_through` — the logical WAL history floor.
    WalPrunedThrough,
    /// `repl:applied_lsn` — a logical standby's applied cursor (node-local).
    ReplAppliedLsn,
    /// `__nano_physical_replica_cursor_v1` — where a physical receiver kept
    /// its cursor before [`LOCAL_CF`] (node-local; refused on open).
    PhysicalCursor,
    /// `vgc:low_watermark` — the version-GC horizon.
    VgcWatermark,
    /// `vgc:anchor:{unix_secs:020}` — a wall-clock → logical-ts anchor.
    VgcAnchor,
    /// `w3_2_elide_used` — the W3.2 elision sentinel.
    ElideSentinel,
    /// `change_log:{lsn:020}` — sync change log (module not compiled today).
    ChangeLog,
    /// `change_index:{table}:{ts:020}` — sync change index.
    ChangeIndex,
    /// `change_meta:*` — sync change-log counters.
    ChangeMeta,
    /// `sync:applied_lsn:{lsn}` — sync delta-applicator progress.
    SyncAppliedLsn,
    /// No entry matches. Never guessed: the caller decides what an
    /// unattributable key means.
    Unknown,
}

/// The class of one key and, for the row-shaped classes, the table it belongs
/// to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Classified<'a> {
    pub class: KeyClass,
    /// `Some` only for [`KeyClass::Data`], [`KeyClass::Version`],
    /// [`KeyClass::VersionIdx`], [`KeyClass::Vmeta`], [`KeyClass::Counter`],
    /// [`KeyClass::BranchData`], [`KeyClass::BranchDel`] and
    /// [`KeyClass::BranchVersion`], and only when the key is well formed. A
    /// malformed key yields `None` rather than a guessed table.
    pub table: Option<&'a str>,
}

/// The column family a physical standby keeps its node-local state in (the
/// applier cursor, its posture), so that the default column family stays a
/// byte-for-byte copy of the primary's. Its presence is what marks a directory
/// as a standby's: `StorageEngine` refuses every open mode but the physical
/// standby one when `DB::list_cf` reports it, and requires it in that mode.
/// Nothing in this table describes it, because nothing in it is replicated.
pub const LOCAL_CF: &str = "__nano_local";

/// Every `(literal prefix, class)` pair, in match order: [`classify`] takes the
/// first entry the key starts with, so an entry must precede any shorter entry
/// that is a prefix of it (longest prefix wins). No two entries overlap today;
/// the ordering is pinned by a unit test so a later catch-all cannot shadow a
/// specific family. Each comment names the constructor the literal was copied
/// from (paths relative to `src/`).
pub const PREFIXES: &[(&[u8], KeyClass)] = &[
    // --- Row data and MVCC ---
    // storage/engine.rs:2249 `build_data_key`
    (b"data:", KeyClass::Data),
    // storage/time_travel.rs:1078 `format!("v:{}:{}:{}")`
    (b"v:", KeyClass::Version),
    // storage/time_travel.rs:982 `format!("v_idx:{}:{}:{:020}")`
    (b"v_idx:", KeyClass::VersionIdx),
    // storage/time_travel.rs:1535 `format!("snapshot:{}")`
    (b"snapshot:", KeyClass::Snapshot),
    // storage/time_travel.rs:1685 `format!("txn_map:{}")` (GC delete only)
    (b"txn_map:", KeyClass::SnapshotMap),
    // storage/time_travel.rs:1686 `format!("scn_map:{}")` (GC delete only)
    (b"scn_map:", KeyClass::SnapshotMap),
    // storage/copy_marker.rs:38 `VMETA_PREFIX`
    (b"vmeta:", KeyClass::Vmeta),
    // storage/catalog.rs:4894 `table_counter_key`
    (b"counter:", KeyClass::Counter),
    // storage/engine.rs:2228 `extract_table_from_key` legacy read arm
    (b"meta:counter:", KeyClass::LegacyCounter),
    // --- Catalog ---
    // storage/catalog.rs:3895 `table_metadata_key`
    (b"meta:table:", KeyClass::MetaTable),
    // storage/catalog.rs:3899 `index_metadata_key`
    (b"meta:index:", KeyClass::MetaIndex),
    // storage/catalog.rs:3914 `schema_metadata_key`
    (b"meta:schema:", KeyClass::MetaSchema),
    // storage/catalog.rs:4125 `enum_type_key`
    (b"meta:enum_type:", KeyClass::MetaEnumType),
    // storage/catalog.rs:4175 `identity_key`
    (b"meta:identity:", KeyClass::MetaIdentity),
    // storage/catalog.rs:4250 `partition_children_key`
    (b"meta:partchild:", KeyClass::MetaPartition),
    // storage/catalog.rs:4254 `partition_parent_key`
    (b"meta:partparent:", KeyClass::MetaPartition),
    // storage/catalog.rs:4424 `sequence_def_key`
    (b"meta:sequence:", KeyClass::MetaSequence),
    // storage/catalog.rs:4428 `sequence_state_key`
    (b"meta:seqstate:", KeyClass::MetaSeqState),
    // storage/catalog.rs:4640 `role_key`
    (b"meta:role:", KeyClass::MetaRole),
    // storage/catalog.rs:4648 `acl_key`
    (b"meta:acl:", KeyClass::MetaAcl),
    // storage/materialized_view.rs:555 `mv_metadata_key`
    (b"meta:mv:", KeyClass::MetaMv),
    // lib.rs:5772 `FUNCTION_META_PREFIX`
    (b"meta:function:", KeyClass::MetaFunction),
    // lib.rs:5775 `PROCEDURE_META_PREFIX`
    (b"meta:procedure:", KeyClass::MetaProcedure),
    // storage/engine.rs:11877 `AlterColumnStorage` replay
    (b"meta:col_storage:", KeyClass::MetaColStorage),
    // storage/catalog.rs:5399 `table_constraints_key`
    (b"table_constraints:", KeyClass::TableConstraints),
    // storage/catalog.rs:5394 `constraint_key` (no caller today)
    (b"constraint:", KeyClass::TableConstraints),
    // storage/catalog.rs:5018 `trigger_metadata_key`
    (b"trigger:", KeyClass::Trigger),
    // storage/catalog.rs:5328 `trigger_row_mutation_key`
    (b"trigger_rowmut:", KeyClass::TriggerRowMut),
    // storage/catalog.rs:4909 `table_statistics_key`
    (b"statistics:table:", KeyClass::Statistics),
    // storage/catalog.rs:4899 `compression_config_key`
    (b"compression:config:", KeyClass::Compression),
    // storage/catalog.rs:4904 `compression_stats_key`
    (b"compression:stats:", KeyClass::Compression),
    // storage/view_catalog.rs:14 `VIEW_METADATA_PREFIX`
    (b"__view_metadata__", KeyClass::ViewMeta),
    // --- Per-column storage sidecars ---
    // storage/dictionary.rs:124 `dict_key`
    (b"dict:", KeyClass::Dict),
    // storage/columnar.rs:483 `batch_key`
    (b"col:", KeyClass::Columnar),
    // storage/columnar.rs:495 `stats_key`
    (b"colz:", KeyClass::ColumnarZ),
    // storage/columnar.rs:507 `stats_marker_key`
    (b"colzm:", KeyClass::ColumnarZm),
    // storage/columnar.rs:530 `presence_key`
    (b"colp:", KeyClass::ColumnarP),
    // storage/columnar.rs:542 `presence_marker_key`
    (b"colpm:", KeyClass::ColumnarPm),
    // storage/content_addr.rs:43 `cas_key`
    (b"cas:", KeyClass::Cas),
    // --- Materialized-view deltas ---
    // storage/mv_delta.rs:369 `record_delta`
    (b"delta:", KeyClass::Delta),
    // storage/mv_delta.rs:221 `Delta::storage_key` (no caller today)
    (b"mv_delta:", KeyClass::Delta),
    // --- Branches and Git ---
    // storage/branch.rs:23 `BRANCH_META_PREFIX`
    (b"branch:meta:", KeyClass::Branch),
    // storage/branch.rs:25 `BRANCH_CHILDREN_PREFIX`
    (b"branch:children:", KeyClass::Branch),
    // storage/branch.rs:26 `BRANCH_DATA_PREFIX`; storage/engine.rs:15711 `branch_aware_data_key`
    (b"bdata:", KeyClass::BranchData),
    // storage/engine.rs:16379 `format!("bdel:{}:{}:{}")`
    (b"bdel:", KeyClass::BranchDel),
    // storage/engine.rs:16036 `format!("bv:{}:{}:{}:{}")`
    (b"bv:", KeyClass::BranchVersion),
    // storage/branch.rs:30 `GIT_LINK_PREFIX`
    (b"git:link:", KeyClass::Git),
    // storage/branch.rs:31 `GIT_COMMIT_PREFIX`
    (b"git:commit:", KeyClass::Git),
    // storage/branch.rs:32 `GIT_DDL_HISTORY_PREFIX` (also `git:ddl:next_id`)
    (b"git:ddl:", KeyClass::Git),
    // storage/branch.rs:33 `GIT_SCHEMA_SNAPSHOT_PREFIX` (also `...:next_id`)
    (b"git:schema_snapshot:", KeyClass::Git),
    // storage/branch.rs:34 `GIT_PR_PREFIX`
    (b"git:pr:", KeyClass::Git),
    // --- Index storage and durable index snapshots ---
    // vector/persistent.rs:304 `prefix`
    (b"__vidx:", KeyClass::Vidx),
    // storage/index_snapshot.rs:79 `ART_SNAPSHOT_PREFIX`
    (b"idxsnap:", KeyClass::IdxSnap),
    // storage/index_snapshot.rs:80 `ART_SNAPSHOT_MARKER_PREFIX`
    (b"idxsnapv:", KeyClass::IdxSnapMarker),
    // storage/index_snapshot.rs:81 `VEC_SNAPSHOT_PREFIX`
    (b"vecsnap:", KeyClass::VecSnap),
    // storage/index_snapshot.rs:82 `VEC_SNAPSHOT_MARKER_PREFIX`
    (b"vecsnapv:", KeyClass::VecSnapMarker),
    // --- Logical WAL and version GC ---
    // storage/wal.rs:1182 `ENTRY_PREFIX`
    (b"wal:entries:", KeyClass::WalEntries),
    // storage/version_gc.rs:69 `ANCHOR_PREFIX`
    (b"vgc:anchor:", KeyClass::VgcAnchor),
    // --- Sync change capture ---
    // `src/sync` is not compiled today (lib.rs:290); classified anyway so
    // re-enabling it cannot introduce an unattributed family.
    // sync/change_log.rs:455 `format!("change_log:{:020}")`
    (b"change_log:", KeyClass::ChangeLog),
    // sync/change_log.rs:459 `format!("change_index:{}:{:020}")`
    (b"change_index:", KeyClass::ChangeIndex),
    // sync/change_log.rs:463 `change_meta:current_lsn`, 688 `change_meta:compaction_watermark`
    (b"change_meta:", KeyClass::ChangeMeta),
    // sync/delta_applicator.rs:196 `format!("sync:applied_lsn:{}")`
    (b"sync:applied_lsn:", KeyClass::SyncAppliedLsn),
];

/// Keys that exist exactly once. Matched by equality, not as prefixes, so a new
/// key that merely extends one of these names (`wal:last_lsn_v2`) is `Unknown`
/// and trips the scan test instead of silently inheriting this class.
pub const FIXED_KEYS: &[(&[u8], KeyClass)] = &[
    // storage/catalog.rs:4644 `role_oid_counter_key`
    (b"meta:role_oid_next", KeyClass::MetaRole),
    // storage/mv_delta.rs:332 `save_last_delta_id`
    (b"meta:delta:last_id", KeyClass::MetaDelta),
    // storage/tde.rs:366 `KEY_CHECK_KEY`
    (b"meta:tde:keycheck", KeyClass::MetaTdeKeyCheck),
    // storage/branch.rs:24 `BRANCH_REGISTRY_KEY`
    (b"branch:registry", KeyClass::Branch),
    // storage/branch.rs:475 `save_pending_gc` `PENDING_GC_KEY`
    (b"branch:pending_gc", KeyClass::Branch),
    // storage/branch.rs:29 `GIT_CONFIG_KEY`
    (b"git:config", KeyClass::Git),
    // storage/wal.rs:881 `WriteAheadLog::stage`
    (b"wal:last_lsn", KeyClass::WalLastLsn),
    // storage/wal.rs:1179 `CHECKPOINT_KEY`
    (b"wal:checkpoint", KeyClass::WalCheckpoint),
    // storage/wal.rs:1192 `PRUNED_THROUGH_KEY`
    (b"wal:pruned_through", KeyClass::WalPrunedThrough),
    // storage/engine.rs:11415 `REPLICATION_CURSOR_KEY`
    (b"repl:applied_lsn", KeyClass::ReplAppliedLsn),
    // replication/physical.rs `LEGACY_CURSOR_KEY`
    (b"__nano_physical_replica_cursor_v1", KeyClass::PhysicalCursor),
    // storage/version_gc.rs:67 `LOW_WATERMARK_KEY`
    (b"vgc:low_watermark", KeyClass::VgcWatermark),
    // storage/time_travel.rs:52 `ELIDE_SENTINEL_KEY`
    (b"w3_2_elide_used", KeyClass::ElideSentinel),
];

/// Classify one key: the class of the matching [`FIXED_KEYS`] or [`PREFIXES`]
/// entry, `Unknown` when none matches.
///
/// The table is extracted only for the row-shaped classes listed on
/// [`Classified::table`], and only by mirroring the split the engine itself
/// uses for that key family (named per arm in `table_of`). The engine helpers
/// disagree on keys whose table name contains `:` (`extract_table_from_key`
/// splits at the first colon, `parse_data_key_table` at the last); such a key
/// fails the strictest reading and yields `None`, never a guess.
pub fn classify(key: &[u8]) -> Classified<'_> {
    if let Some(&(_, class)) = FIXED_KEYS.iter().find(|(fixed, _)| *fixed == key) {
        return Classified { class, table: None };
    }
    match PREFIXES.iter().find(|(prefix, _)| key.starts_with(prefix)) {
        Some(&(prefix, class)) => Classified {
            class,
            table: key.get(prefix.len()..).and_then(|rest| table_of(class, rest)),
        },
        None => Classified {
            class: KeyClass::Unknown,
            table: None,
        },
    }
}

/// The table named by the part of a row-shaped key after its prefix.
fn table_of(class: KeyClass, rest: &[u8]) -> Option<&str> {
    if !matches!(
        class,
        KeyClass::Data
            | KeyClass::Version
            | KeyClass::VersionIdx
            | KeyClass::Vmeta
            | KeyClass::Counter
            | KeyClass::BranchData
            | KeyClass::BranchDel
            | KeyClass::BranchVersion
    ) {
        return None;
    }
    let rest = std::str::from_utf8(rest).ok()?;
    let table = match class {
        // `data:{table}:{row_id}`: `StorageEngine::extract_table_from_key`
        // takes everything before the first ':' after the prefix, and
        // `StorageEngine::parse_row_id_after_prefix` requires the remainder to
        // be a decimal u64.
        KeyClass::Data => table_then_row_id(rest)?,
        // `v:{table}:{row_id}:{commit_ts}` and
        // `v_idx:{table}:{row_id}:{reverse_ts:020}`: the same three-part
        // ':'-split `SnapshotManager::scan_versions_between` applies to `v:`
        // keys, with both numeric parts decimal.
        KeyClass::Version | KeyClass::VersionIdx => {
            let mut parts = rest.split(':');
            let (table, row_id, ts) = (parts.next()?, parts.next()?, parts.next()?);
            if parts.next().is_some() || !is_decimal_u64(row_id) || !is_decimal_u64(ts) {
                return None;
            }
            table
        }
        // `vmeta:{table}:{first:020}:{last:020}`: `CopyMarkers::parse_marker`
        // splits from the RIGHT, so this is the one family whose table name may
        // itself contain ':'.
        KeyClass::Vmeta => {
            let (rest, last) = rest.rsplit_once(':')?;
            let (table, first) = rest.rsplit_once(':')?;
            if !is_decimal_u64(first) || !is_decimal_u64(last) {
                return None;
            }
            table
        }
        // `counter:{table}`: `StorageEngine::load_counters` takes the whole
        // suffix (strict UTF-8 on the physical-snapshot path, as here).
        KeyClass::Counter => rest,
        // `bdata:{branch_id}:{table}:{row_id}` / `bdel:{branch_id}:{table}:{row_id}`,
        // the decimal form `StorageEngine::branch_aware_data_key` and the merge
        // loop write, whose row id `parse_row_id_after_prefix` reads after the
        // `{branch_id}:{table}:` prefix. The binary form
        // `branch::encode_branch_data_key` writes (`bdata:{be_u64}:{user_key}`)
        // has a non-decimal branch id and so names no table.
        KeyClass::BranchData | KeyClass::BranchDel => {
            let (branch_id, rest) = rest.split_once(':')?;
            if !is_decimal_u64(branch_id) {
                return None;
            }
            table_then_row_id(rest)?
        }
        // `bv:{branch_id}:{table}:{row_id}:{ts}` (engine.rs `bv:{}:{}:{}:{}`).
        KeyClass::BranchVersion => {
            let mut parts = rest.split(':');
            let (branch_id, table, row_id, ts) = (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
            if parts.next().is_some() || !is_decimal_u64(branch_id) || !is_decimal_u64(row_id) || !is_decimal_u64(ts) {
                return None;
            }
            table
        }
        _ => return None,
    };
    (!table.is_empty()).then_some(table)
}

/// `{table}:{row_id}` split at the first ':' with a decimal u64 row id.
fn table_then_row_id(rest: &str) -> Option<&str> {
    let (table, row_id) = rest.split_once(':')?;
    is_decimal_u64(row_id).then_some(table)
}

/// ASCII digits only (no sign, unlike `str::parse`) that fit a u64 — the rule
/// `StorageEngine::parse_row_id_after_prefix` applies to row ids.
fn is_decimal_u64(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()) && text.parse::<u64>().is_ok()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// One realistic key per class, with the table `classify` must report.
    /// Shapes are the constructors' own formats.
    const ROUND_TRIP: &[(&[u8], KeyClass, Option<&str>)] = &[
        (b"data:users:42", KeyClass::Data, Some("users")),
        (b"v:users:42:1700", KeyClass::Version, Some("users")),
        (
            b"v_idx:users:42:18446744073709549915",
            KeyClass::VersionIdx,
            Some("users"),
        ),
        (b"snapshot:1700", KeyClass::Snapshot, None),
        (b"txn_map:7", KeyClass::SnapshotMap, None),
        (b"scn_map:7", KeyClass::SnapshotMap, None),
        (
            b"vmeta:users:00000000000000000001:00000000000000000009",
            KeyClass::Vmeta,
            Some("users"),
        ),
        (b"meta:table:users", KeyClass::MetaTable, None),
        (b"meta:index:users_name_idx", KeyClass::MetaIndex, None),
        (b"meta:schema:app", KeyClass::MetaSchema, None),
        (b"meta:enum_type:mood", KeyClass::MetaEnumType, None),
        (b"meta:identity:users", KeyClass::MetaIdentity, None),
        (b"meta:partchild:events", KeyClass::MetaPartition, None),
        (b"meta:partparent:events_2026", KeyClass::MetaPartition, None),
        (b"meta:sequence:ids", KeyClass::MetaSequence, None),
        (b"meta:seqstate:ids", KeyClass::MetaSeqState, None),
        (b"meta:mv:totals", KeyClass::MetaMv, None),
        (b"meta:role:app_user", KeyClass::MetaRole, None),
        (b"meta:role_oid_next", KeyClass::MetaRole, None),
        (b"meta:acl:table:users:app_user", KeyClass::MetaAcl, None),
        (b"meta:function:double_it", KeyClass::MetaFunction, None),
        (b"meta:procedure:log_it", KeyClass::MetaProcedure, None),
        (b"meta:col_storage:users:bio", KeyClass::MetaColStorage, None),
        (b"meta:delta:last_id", KeyClass::MetaDelta, None),
        (b"meta:tde:keycheck", KeyClass::MetaTdeKeyCheck, None),
        (b"table_constraints:users", KeyClass::TableConstraints, None),
        (b"constraint:users:users_pkey", KeyClass::TableConstraints, None),
        (b"trigger:users:audit", KeyClass::Trigger, None),
        (b"trigger_rowmut:users:audit", KeyClass::TriggerRowMut, None),
        (b"statistics:table:users", KeyClass::Statistics, None),
        (b"compression:config:users", KeyClass::Compression, None),
        (b"compression:stats:users", KeyClass::Compression, None),
        (b"__view_metadata__active_users", KeyClass::ViewMeta, None),
        (b"counter:users", KeyClass::Counter, Some("users")),
        (b"meta:counter:users", KeyClass::LegacyCounter, None),
        (b"dict:users:status", KeyClass::Dict, None),
        (b"col:users:tag:0", KeyClass::Columnar, None),
        (b"colz:users:tag:0", KeyClass::ColumnarZ, None),
        (b"colzm:users:tag", KeyClass::ColumnarZm, None),
        (b"colp:users:0", KeyClass::ColumnarP, None),
        (b"colpm:users", KeyClass::ColumnarPm, None),
        (b"cas:\x01\x02\x03", KeyClass::Cas, None),
        (b"delta:users:00000000000000000001", KeyClass::Delta, None),
        (b"mv_delta:users:00000000000000000001:4", KeyClass::Delta, None),
        (b"branch:meta:dev", KeyClass::Branch, None),
        (
            b"branch:children:\x00\x00\x00\x00\x00\x00\x00\x01",
            KeyClass::Branch,
            None,
        ),
        (b"branch:registry", KeyClass::Branch, None),
        (b"branch:pending_gc", KeyClass::Branch, None),
        (b"bdata:2:users:42", KeyClass::BranchData, Some("users")),
        (b"bdel:2:users:42", KeyClass::BranchDel, Some("users")),
        (b"bv:2:users:42:1700", KeyClass::BranchVersion, Some("users")),
        (b"git:config", KeyClass::Git, None),
        (b"git:link:feature", KeyClass::Git, None),
        (b"git:commit:abc123", KeyClass::Git, None),
        (b"git:ddl:next_id", KeyClass::Git, None),
        (b"git:schema_snapshot:next_id", KeyClass::Git, None),
        (b"git:pr:17", KeyClass::Git, None),
        (b"__vidx:3:meta", KeyClass::Vidx, None),
        (b"idxsnap:users", KeyClass::IdxSnap, None),
        (b"idxsnapv:users", KeyClass::IdxSnapMarker, None),
        (b"vecsnap:docs_emb_idx", KeyClass::VecSnap, None),
        (b"vecsnapv:docs_emb_idx", KeyClass::VecSnapMarker, None),
        (b"wal:entries:00000000000000000001", KeyClass::WalEntries, None),
        (b"wal:last_lsn", KeyClass::WalLastLsn, None),
        (b"wal:checkpoint", KeyClass::WalCheckpoint, None),
        (b"wal:pruned_through", KeyClass::WalPrunedThrough, None),
        (b"repl:applied_lsn", KeyClass::ReplAppliedLsn, None),
        (b"__nano_physical_replica_cursor_v1", KeyClass::PhysicalCursor, None),
        (b"vgc:low_watermark", KeyClass::VgcWatermark, None),
        (b"vgc:anchor:00000000001700000000", KeyClass::VgcAnchor, None),
        (b"w3_2_elide_used", KeyClass::ElideSentinel, None),
        (b"change_log:00000000000000000001", KeyClass::ChangeLog, None),
        (b"change_index:users:00000000000000001700", KeyClass::ChangeIndex, None),
        (b"change_meta:current_lsn", KeyClass::ChangeMeta, None),
        (b"sync:applied_lsn:9", KeyClass::SyncAppliedLsn, None),
    ];

    fn show(key: &[u8]) -> String {
        String::from_utf8_lossy(key).into_owned()
    }

    #[test]
    fn one_key_per_class_round_trips() {
        for &(key, class, table) in ROUND_TRIP {
            assert_eq!(classify(key), Classified { class, table }, "classify({:?})", show(key));
        }
    }

    /// Every class the tables can produce has a sample above, so a new family
    /// cannot be added without a round-trip case.
    #[test]
    fn every_reachable_class_has_a_round_trip_case() {
        let sampled: HashSet<KeyClass> = ROUND_TRIP.iter().map(|&(_, class, _)| class).collect();
        for &(entry, class) in PREFIXES.iter().chain(FIXED_KEYS) {
            assert!(
                sampled.contains(&class),
                "{class:?} (entry {:?}) has no ROUND_TRIP case",
                show(entry)
            );
        }
        assert!(!sampled.contains(&KeyClass::Unknown));
    }

    #[test]
    fn every_entry_classifies_as_itself() {
        for &(entry, class) in PREFIXES.iter().chain(FIXED_KEYS) {
            assert_eq!(classify(entry).class, class, "entry {:?}", show(entry));
        }
    }

    /// First match must equal longest match: no entry may sit behind a shorter
    /// entry that is a prefix of it, or that entry is unreachable.
    #[test]
    fn prefixes_are_in_match_order() {
        for (i, &(earlier, _)) in PREFIXES.iter().enumerate() {
            for &(later, _) in PREFIXES.iter().skip(i + 1) {
                assert!(
                    !later.starts_with(earlier),
                    "{:?} is shadowed by the earlier, shorter {:?}",
                    show(later),
                    show(earlier)
                );
            }
        }
    }

    /// An exact key that also matched a prefix entry would have two classes.
    #[test]
    fn fixed_keys_and_prefixes_are_disjoint() {
        let mut seen = HashSet::new();
        for &(fixed, _) in FIXED_KEYS {
            assert!(seen.insert(fixed), "duplicate fixed key {:?}", show(fixed));
            for &(prefix, _) in PREFIXES {
                assert!(
                    !fixed.starts_with(prefix),
                    "fixed key {:?} also matches prefix {:?}",
                    show(fixed),
                    show(prefix)
                );
            }
        }
    }

    /// The near-misses: families whose names share a stem but not a prefix.
    #[test]
    fn longest_prefix_and_lookalike_cases() {
        let cases: &[(&[u8], KeyClass)] = &[
            (b"v_idx:t:1:00000000000000000001", KeyClass::VersionIdx),
            (b"v:t:1:1", KeyClass::Version),
            (b"meta:counter:t", KeyClass::LegacyCounter),
            (b"counter:t", KeyClass::Counter),
            (b"trigger_rowmut:t:g", KeyClass::TriggerRowMut),
            (b"trigger:t:g", KeyClass::Trigger),
            (b"colzm:t:c", KeyClass::ColumnarZm),
            (b"colz:t:c:0", KeyClass::ColumnarZ),
            (b"colpm:t", KeyClass::ColumnarPm),
            (b"colp:t:0", KeyClass::ColumnarP),
            (b"col:t:c:0", KeyClass::Columnar),
            (b"idxsnapv:t", KeyClass::IdxSnapMarker),
            (b"idxsnap:t", KeyClass::IdxSnap),
            (b"vecsnapv:i", KeyClass::VecSnapMarker),
            (b"vecsnap:i", KeyClass::VecSnap),
            (b"meta:role_oid_next", KeyClass::MetaRole),
            (b"meta:role:r", KeyClass::MetaRole),
            (b"bdata:1:t:1", KeyClass::BranchData),
            (b"data:t:1", KeyClass::Data),
            (b"mv_delta:t:1:1", KeyClass::Delta),
            (b"delta:t:1", KeyClass::Delta),
        ];
        for &(key, class) in cases {
            assert_eq!(classify(key).class, class, "classify({:?})", show(key));
        }
    }

    #[test]
    fn unknown_keys_are_unknown() {
        let keys: &[&[u8]] = &[
            b"zzz:",
            b"",
            b"meta:",
            b"meta:plain:put",
            b"wal:",
            b"wal:last_lsn_v2",
            b"branch:registry2",
            b"w3_2_elide_used\0",
            b"data",
            b"t:users:",
        ];
        let unknown = Classified {
            class: KeyClass::Unknown,
            table: None,
        };
        for &key in keys {
            assert_eq!(classify(key), unknown, "classify({:?})", show(key));
        }
    }

    /// Fail closed: a malformed row-shaped key keeps its class but names no
    /// table.
    #[test]
    fn malformed_row_keys_name_no_table() {
        let cases: &[(&[u8], KeyClass, &str)] = &[
            (b"data:users", KeyClass::Data, "no row id"),
            (b"data:", KeyClass::Data, "nothing after the prefix"),
            (b"data::1", KeyClass::Data, "empty table"),
            (b"data:users:", KeyClass::Data, "empty row id"),
            (b"data:users:4x", KeyClass::Data, "non-decimal row id"),
            (b"data:users:+4", KeyClass::Data, "signed row id"),
            (
                b"data:users:18446744073709551616",
                KeyClass::Data,
                "row id overflows u64",
            ),
            (b"data:a:b:1", KeyClass::Data, "':' in the table name"),
            (b"data:\xff\xfe:1", KeyClass::Data, "not UTF-8"),
            (b"v:users:1", KeyClass::Version, "no timestamp"),
            (b"v:users:1:2:3", KeyClass::Version, "extra part"),
            (
                b"v_idx:users:x:00000000000000000001",
                KeyClass::VersionIdx,
                "non-decimal row id",
            ),
            (b"vmeta:users:1", KeyClass::Vmeta, "one bound only"),
            (b"vmeta:users:a:b", KeyClass::Vmeta, "non-decimal bounds"),
            (b"counter:", KeyClass::Counter, "empty table"),
            (
                b"bdata:\x00\x00\x00\x00\x00\x00\x00\x01:users:1",
                KeyClass::BranchData,
                "binary branch id",
            ),
            (b"bdata:2:users", KeyClass::BranchData, "no row id"),
            (b"bdel:x:users:1", KeyClass::BranchDel, "non-decimal branch id"),
            (b"bv:2:users:1", KeyClass::BranchVersion, "no timestamp"),
        ];
        for &(key, class, why) in cases {
            assert_eq!(
                classify(key),
                Classified { class, table: None },
                "classify({:?}): {why}",
                show(key)
            );
        }
    }

    /// The engine helpers' own documented examples, so a drift between this
    /// mirror and `extract_table_from_key` / `parse_marker` shows up here.
    #[test]
    fn table_extraction_mirrors_the_engine_helpers() {
        assert_eq!(
            classify(b"data:user_accounts_2024:999").table,
            Some("user_accounts_2024")
        );
        assert_eq!(classify(b"data:s.orders:7").table, Some("s.orders"));
        assert_eq!(classify(b"data:users:18446744073709551615").table, Some("users"));
        // `parse_marker` splits from the right, so a ':' in the table survives.
        assert_eq!(
            classify(b"vmeta:a:b:00000000000000000001:00000000000000000009").table,
            Some("a:b")
        );
        assert_eq!(classify(b"counter:pg_temp_7.scratch").table, Some("pg_temp_7.scratch"));
    }
}
