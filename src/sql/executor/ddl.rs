//! DDL (Data Definition Language) operations
//!
//! This module handles CREATE/DROP INDEX and other DDL operations.

#![allow(elided_lifetimes_in_paths)]

use super::scan::ScanOperator;
use super::{Executor, PhysicalOperator};
use crate::sql::LogicalPlan;
use crate::{Error, Result};
use rocksdb::{IteratorMode, ReadOptions};
use std::sync::Arc;

fn empty_ddl_result(executor: &Executor) -> Box<dyn PhysicalOperator> {
    Box::new(
        ScanOperator::new(
            "".to_string(),
            Arc::new(crate::Schema { columns: vec![] }),
            None,
            vec![],
            vec![],
        )
        .with_timeout(executor.timeout_ctx()),
    )
}

fn create_art_secondary_index(
    storage: &crate::storage::StorageEngine,
    name: &str,
    table_name: &str,
    column_name: &str,
    if_not_exists: bool,
) -> Result<Option<usize>> {
    if storage.is_branch_active() {
        return Err(Error::query_execution(
            "CREATE INDEX for ART secondary indexes must run on the main branch",
        ));
    }

    let art_manager = storage.art_indexes();
    if art_manager.index_exists(name) {
        if if_not_exists {
            return Ok(None);
        }
        return Err(Error::query_execution(format!("ART index '{}' already exists", name)));
    }

    let catalog = storage.catalog();
    let schema = catalog.get_table_schema(table_name)?;
    if !schema.columns.iter().any(|c| c.name == column_name) {
        return Err(Error::query_execution(format!(
            "Column '{}' not found in table '{}'",
            column_name, table_name
        )));
    }

    let columns = vec![column_name.to_string()];
    art_manager
        .create_manual_index(name, table_name, &columns)
        .map_err(|e| Error::query_execution(format!("Failed to create ART index: {}", e)))?;

    let tuples = storage.scan_table_with_schema(table_name, &schema)?;
    match art_manager.backfill_manual_index(name, &schema, &tuples) {
        Ok(backfilled) => Ok(Some(backfilled)),
        Err(e) => {
            let _ = art_manager.drop_index(name);
            Err(Error::query_execution(format!("Failed to backfill ART index: {}", e)))
        }
    }
}

fn vector_distance_metric(options: &[crate::sql::logical_plan::IndexOption]) -> Result<crate::vector::DistanceMetric> {
    use crate::sql::logical_plan::IndexOption;
    use crate::vector::DistanceMetric;

    let mut metric = DistanceMetric::L2;
    for option in options {
        if let IndexOption::DistanceMetric(name) = option {
            metric = match name.as_str() {
                "l2" | "euclidean" => DistanceMetric::L2,
                "cosine" => DistanceMetric::Cosine,
                "ip" | "inner_product" => DistanceMetric::InnerProduct,
                other => {
                    return Err(Error::query_execution(format!(
                        "Unsupported vector index metric '{}'",
                        other
                    )))
                }
            };
        }
    }
    Ok(metric)
}

fn collect_existing_vectors(
    storage: &crate::storage::StorageEngine,
    schema: &crate::Schema,
    table_name: &str,
    column_name: &str,
    dimension: usize,
) -> Result<Vec<(u64, crate::vector::Vector)>> {
    let col_idx = schema
        .get_column_index(column_name)
        .ok_or_else(|| Error::query_execution(format!("Column '{}' not found in schema", column_name)))?;
    let tuples = storage.scan_table_with_schema_columns(table_name, schema, &[col_idx])?;
    let mut vectors = Vec::with_capacity(tuples.len());

    for tuple in tuples {
        match tuple.values.get(col_idx) {
            Some(crate::Value::Vector(vec)) => {
                if vec.len() != dimension {
                    return Err(Error::query_execution(format!(
                        "Vector dimension mismatch while backfilling '{}.{}': expected {}, got {}",
                        table_name,
                        column_name,
                        dimension,
                        vec.len()
                    )));
                }
                let row_id = tuple.row_id.ok_or_else(|| {
                    Error::query_execution(format!(
                        "Cannot backfill vector index on '{}.{}' from tuple without row_id",
                        table_name, column_name
                    ))
                })?;
                vectors.push((row_id, vec.clone()));
            }
            Some(crate::Value::Null) | None => {}
            Some(other) => {
                return Err(Error::query_execution(format!(
                    "Cannot backfill vector index on '{}.{}' from non-vector value {:?}",
                    table_name, column_name, other
                )))
            }
        }
    }

    Ok(vectors)
}

fn backfill_vector_index(
    vector_indexes: &crate::storage::VectorIndexManager,
    index_name: &str,
    vectors: &[(u64, crate::vector::Vector)],
) -> Result<usize> {
    // Parallel bulk build: hands the whole batch to the index, which uses
    // Rayon (`parallel_insert`) to construct the HNSW graph across all cores.
    // This replaces the old single-threaded per-row `insert_vector` loop and
    // is the dominant cost of `CREATE INDEX ... USING hnsw` on a populated
    // table (e.g. ~1120s → far less for 678k vectors). Results are identical
    // to sequential insertion; only build wall-time changes.
    if let Err(e) = vector_indexes.insert_vectors_batch(index_name, vectors) {
        let _ = vector_indexes.drop_index(index_name);
        return Err(Error::query_execution(format!(
            "Failed to backfill HNSW index '{}': {}",
            index_name, e
        )));
    }
    Ok(vectors.len())
}

fn persist_index_definition(
    storage: &crate::storage::StorageEngine,
    name: &str,
    table_name: &str,
    column_name: &str,
    index_type: Option<&str>,
    options: &[crate::sql::logical_plan::IndexOption],
) -> Result<()> {
    let definition = crate::storage::PersistedIndexDefinition {
        table_name: table_name.to_string(),
        column_name: column_name.to_string(),
        index_type: index_type.map(str::to_string),
        options: options.to_vec(),
    };
    storage.catalog().save_index_definition(name, &definition)
}

fn encoded_index_options(options: &[crate::sql::logical_plan::IndexOption]) -> Vec<u8> {
    bincode::serialize(options).unwrap_or_default()
}

/// Handle CREATE INDEX logical plan node.
///
/// The `USING <type>` spelling is routed through the SHARED
/// [`crate::storage::index_family`] classifier — the same one
/// [`handle_drop_index`] and `Catalog::rebuild_vector_indexes` use — so the
/// set of index types this build understands is defined in exactly one place.
///
/// GH#16: `name` is NOT always something the user typed. PostgreSQL makes the
/// index name optional (`CREATE INDEX ON items USING hnsw (embedding
/// vector_cosine_ops)` — pgvector's README spelling), and the planner derives
/// `{table}_{cols}_idx` for those, uniquified against the persisted
/// definitions, the live ART registry (which owns the `_pkey` / `_key` /
/// `_fkey` CONSTRAINT namespace), the live vector registry and the table names.
/// It arrives here as an ordinary name and MUST be treated as one by every
/// branch below — in particular every branch that builds something must also
/// `persist_index_definition`, because that record is what `DROP INDEX`
/// dispatches on and what `Catalog::rebuild_all_indexes` restores at open. A
/// generated name that were registered but not persisted would be an index the
/// user could neither drop nor keep across a restart.
pub(super) fn handle_create_index(executor: &Executor, plan: &LogicalPlan) -> Result<Box<dyn PhysicalOperator>> {
    use crate::storage::{index_family, IndexFamily};

    if let LogicalPlan::CreateIndex {
        name,
        table_name,
        column_name,
        index_type,
        if_not_exists,
        options,
    } = plan
    {
        if let Some(storage) = executor.storage() {
            let family = index_family(index_type.as_deref());
            if family == Some(IndexFamily::Art) {
                if let Some(backfilled) =
                    create_art_secondary_index(storage, name, table_name, column_name, *if_not_exists)?
                {
                    tracing::info!(
                        "Created ART index '{}' on table '{}' column '{}' with {} existing rows",
                        name,
                        table_name,
                        column_name,
                        backfilled
                    );
                    persist_index_definition(storage, name, table_name, column_name, Some("art"), options)?;

                    let encoded_options = encoded_index_options(options);
                    if let Err(e) =
                        storage.log_create_index(name, table_name, column_name, Some("art"), &encoded_options)
                    {
                        tracing::warn!("Failed to log CREATE INDEX to WAL: {}", e);
                    }
                }
            } else if let Some(idx_type) = index_type {
                if family == Some(IndexFamily::DdlOnly) {
                    // Postgres FTS/GIN/GiST index.
                    //
                    // Accepted for syntactic compatibility (Django, Rails,
                    // and hand-written migrations emit CREATE INDEX ...
                    // USING gin) but does NOT yet build a real inverted
                    // index — the @@ operator walks the table row by row
                    // using the in-evaluator BM25 scorer. On realistic
                    // text volumes this is fine; at scale, consider the
                    // native search::bm25 API until a persistent GIN
                    // backend lands.
                    //
                    // See docs/compatibility/fts.md for the full list of
                    // behaviours we do and do not implement.
                    tracing::info!(
                        "Accepted CREATE INDEX {} USING {} ON {} ({}) — \
                         DDL-only (no backing index yet)",
                        name,
                        idx_type,
                        table_name,
                        column_name
                    );
                    persist_index_definition(storage, name, table_name, column_name, Some(idx_type.as_str()), options)?;
                    let encoded_options = encoded_index_options(options);
                    if let Err(e) = storage.log_create_index(
                        name,
                        table_name,
                        column_name,
                        Some(idx_type.as_str()),
                        &encoded_options,
                    ) {
                        tracing::warn!("Failed to log CREATE INDEX to WAL: {}", e);
                    }
                } else if family == Some(IndexFamily::Vector) {
                    // Check if index already exists
                    let vector_indexes = storage.vector_indexes();
                    if vector_indexes.index_exists(name) {
                        if *if_not_exists {
                            // IF NOT EXISTS specified, return silently
                            return Ok(empty_ddl_result(executor));
                        } else {
                            // Error: index already exists
                            return Err(Error::query_execution(format!("Index '{}' already exists", name)));
                        }
                    }

                    let catalog = storage.catalog();
                    let schema = catalog.get_table_schema(table_name)?;

                    // Find the column to index
                    let column = schema.get_column(column_name).ok_or_else(|| {
                        Error::query_execution(format!("Column '{}' not found in table '{}'", column_name, table_name))
                    })?;

                    // Extract vector dimension from Vector(n) type
                    let dimension = match column.data_type {
                        crate::DataType::Vector(dim) => dim,
                        _ => {
                            return Err(Error::query_execution(format!(
                                "Column '{}' is not a vector type, cannot create HNSW index",
                                column_name
                            )))
                        }
                    };
                    let distance_metric = vector_distance_metric(options)?;
                    let existing_vectors =
                        collect_existing_vectors(storage, &schema, table_name, column_name, dimension)?;

                    // Parse quantization options
                    use crate::sql::logical_plan::{IndexOption, QuantizationType};

                    let mut quantization_type = QuantizationType::None;
                    let mut pq_subquantizers: Option<usize> = None;
                    let mut pq_centroids: Option<usize> = None;
                    let mut persistent = false;
                    // R5.V6: HNSW construction parameters default from the
                    // `[vector]` config section; `WITH (m = ..,
                    // ef_construction = ..)` overrides per index. These were
                    // parsed but silently ignored (hardcoded 16/200) before.
                    let vector_cfg = &storage.config().vector;
                    let mut hnsw_m = vector_cfg.hnsw_m;
                    let mut hnsw_ef_construction = vector_cfg.hnsw_ef_construction;
                    #[cfg(feature = "vector-persist")]
                    let mut rerank_precision: Option<crate::vector::persistent::VectorPrecision> = None;
                    #[cfg(not(feature = "vector-persist"))]
                    let rerank_precision: Option<()> = None;

                    for option in options {
                        match option {
                            IndexOption::Quantization(qt) => quantization_type = *qt,
                            IndexOption::PqSubquantizers(n) => pq_subquantizers = Some(*n),
                            IndexOption::PqCentroids(n) => pq_centroids = Some(*n),
                            IndexOption::HnswM(n) => hnsw_m = *n,
                            IndexOption::EfConstruction(n) => hnsw_ef_construction = *n,
                            IndexOption::Persistent(enabled) => persistent = *enabled,
                            IndexOption::RerankPrecision(precision) => {
                                #[cfg(feature = "vector-persist")]
                                {
                                    rerank_precision = Some(match precision.as_str() {
                                        "f32" => crate::vector::persistent::VectorPrecision::F32,
                                        "f16" => crate::vector::persistent::VectorPrecision::F16,
                                        "i8" => crate::vector::persistent::VectorPrecision::I8,
                                        other => {
                                            return Err(Error::query_execution(format!(
                                                "Unsupported rerank_precision '{}'",
                                                other
                                            )))
                                        }
                                    });
                                }
                                #[cfg(not(feature = "vector-persist"))]
                                {
                                    let _ = precision;
                                }
                            }
                            _ => {} // Ignore other options for now
                        }
                    }

                    if persistent {
                        let training_vectors: Vec<crate::vector::Vector> =
                            existing_vectors.iter().map(|(_, vector)| vector.clone()).collect();

                        let pq_config = if quantization_type == QuantizationType::Product {
                            let mut cfg = crate::vector::ProductQuantizerConfig::default_for_dimension(dimension)
                                .map_err(|e| Error::query_execution(format!("Invalid PQ config: {}", e)))?;
                            if let Some(n) = pq_subquantizers {
                                cfg.num_subquantizers = n;
                            }
                            if let Some(n) = pq_centroids {
                                cfg.num_centroids = n;
                            }
                            cfg.validate()
                                .map_err(|e| Error::query_execution(format!("Invalid PQ config: {}", e)))?;
                            Some(cfg)
                        } else {
                            None
                        };

                        vector_indexes.create_persistent_index(
                            name.clone(),
                            table_name.clone(),
                            column_name.clone(),
                            dimension,
                            distance_metric,
                            pq_config,
                            rerank_precision,
                            &training_vectors,
                            storage.db(),
                        )?;
                        let backfilled = backfill_vector_index(vector_indexes, name, &existing_vectors)?;
                        tracing::info!(
                            "Created persistent HNSW index '{}' on table '{}' column '{}' with {} existing vectors",
                            name,
                            table_name,
                            column_name,
                            backfilled
                        );
                        persist_index_definition(
                            storage,
                            name,
                            table_name,
                            column_name,
                            Some("persistent_hnsw"),
                            options,
                        )?;

                        let encoded_options = encoded_index_options(options);
                        if let Err(e) = storage.log_create_index(
                            name,
                            table_name,
                            column_name,
                            Some("persistent_hnsw"),
                            &encoded_options,
                        ) {
                            tracing::warn!("Failed to log CREATE INDEX to WAL: {}", e);
                        }
                        return Ok(empty_ddl_result(executor));
                    }

                    // Check if we should create a quantized index
                    match quantization_type {
                        QuantizationType::Product => {
                            // Create quantized index
                            use crate::vector::ProductQuantizerConfig;

                            // Build PQ config
                            let mut pq_config = ProductQuantizerConfig::default_for_dimension(dimension)
                                .map_err(|e| Error::query_execution(format!("Invalid PQ config: {}", e)))?;

                            if let Some(n) = pq_subquantizers {
                                pq_config.num_subquantizers = n;
                            }
                            if let Some(n) = pq_centroids {
                                pq_config.num_centroids = n;
                            }

                            // Validate config
                            pq_config
                                .validate()
                                .map_err(|e| Error::query_execution(format!("Invalid PQ config: {}", e)))?;

                            let training_vectors: Vec<crate::vector::Vector> =
                                existing_vectors.iter().map(|(_, vector)| vector.clone()).collect();

                            vector_indexes.create_quantized_index_with_params(
                                name.clone(),
                                table_name.clone(),
                                column_name.clone(),
                                dimension,
                                distance_metric,
                                pq_config,
                                &training_vectors,
                                hnsw_m,
                                hnsw_ef_construction,
                            )?;
                            let backfilled = backfill_vector_index(vector_indexes, name, &existing_vectors)?;
                            tracing::info!(
                                "Created quantized HNSW index '{}' on table '{}' column '{}' with {} existing vectors",
                                name,
                                table_name,
                                column_name,
                                backfilled
                            );
                            persist_index_definition(storage, name, table_name, column_name, Some("hnsw_pq"), options)?;

                            // Log to WAL for replication
                            let encoded_options = encoded_index_options(options);
                            if let Err(e) = storage.log_create_index(
                                name,
                                table_name,
                                column_name,
                                Some("hnsw_pq"),
                                &encoded_options,
                            ) {
                                tracing::warn!("Failed to log CREATE INDEX to WAL: {}", e);
                            }
                        }
                        _ => {
                            // Create standard non-quantized index
                            vector_indexes.create_index_with_params(
                                name.clone(),
                                table_name.clone(),
                                column_name.clone(),
                                dimension,
                                distance_metric,
                                hnsw_m,
                                hnsw_ef_construction,
                            )?;
                            let backfilled = backfill_vector_index(vector_indexes, name, &existing_vectors)?;
                            tracing::info!(
                                "Created HNSW index '{}' on table '{}' column '{}' with {} existing vectors",
                                name,
                                table_name,
                                column_name,
                                backfilled
                            );
                            // The tag records what was BUILT, not what the user
                            // typed. It used to echo `index_type` back, which was
                            // only ever "hnsw" because the branch guard compared
                            // against that literal; now that the guard is the
                            // shared family classifier, an alternative spelling
                            // reaching here must still persist the canonical tag
                            // for a standard in-memory HNSW — otherwise the open
                            // path would try to reopen a persistent backend that
                            // was never created.
                            persist_index_definition(storage, name, table_name, column_name, Some("hnsw"), options)?;

                            // Log to WAL for replication
                            let encoded_options = encoded_index_options(options);
                            if let Err(e) =
                                storage.log_create_index(name, table_name, column_name, Some("hnsw"), &encoded_options)
                            {
                                tracing::warn!("Failed to log CREATE INDEX to WAL: {}", e);
                            }
                        }
                    }
                } else {
                    // An access method this build has no implementation for
                    // (`USING brin`, `USING spgist`, `USING ivfflat`, …).
                    //
                    // PRE-EXISTING BEHAVIOUR, deliberately unchanged here: the
                    // statement reports success and builds nothing. That IS a
                    // silent success and it is filed as such — but making it an
                    // error is a user-visible change to CREATE INDEX, not part
                    // of the DROP INDEX slice, and it would start failing
                    // migrations that this build has accepted since v3.x. The
                    // one thing this arm now does is SAY SO, by name, instead of
                    // falling off the end of an if/else chain invisibly.
                    tracing::warn!(
                        "CREATE INDEX {} ON {} ({}) USING {}: this build has no '{}' access method — \
                         the statement is accepted for compatibility and NO index is built \
                         (queries fall back to a scan). Use art/btree/hash, gin/gist or hnsw.",
                        name,
                        table_name,
                        column_name,
                        idx_type,
                        idx_type
                    );
                }
            }
        }

        // Return empty result set for DDL
        Ok(empty_ddl_result(executor))
    } else {
        Err(Error::query_execution("Expected CreateIndex plan node"))
    }
}

/// The constraint an ART index backs, as `(user-facing noun, owning table)` —
/// `None` for a plain secondary index created by `CREATE INDEX`.
fn constraint_index_kind(storage: &crate::storage::StorageEngine, index_name: &str) -> Option<(&'static str, String)> {
    use crate::storage::art_index::ArtIndexType;

    let (kind, table) = storage.art_indexes().index_kind_and_table(index_name)?;
    let noun = match kind {
        ArtIndexType::PrimaryKey => "PRIMARY KEY",
        ArtIndexType::Unique => "UNIQUE",
        ArtIndexType::ForeignKey => "FOREIGN KEY",
        ArtIndexType::Manual => return None,
    };
    Some((noun, table))
}

/// Handle DROP INDEX logical plan node — the mirror of `handle_create_index`,
/// and the caller that `Catalog::drop_index_definition`, `ArtManager::
/// drop_index`, `VectorIndexManager::drop_index` and
/// `StorageEngine::log_drop_index` had been waiting for since they were
/// written. Reached from the SHARED `plan_to_operator`, so `db.execute()` and
/// `db.execute_params()` (i.e. the PostgreSQL EXTENDED protocol, i.e. every
/// real driver) get identical behaviour from ONE arm.
///
/// The drop dispatches on the PERSISTED definition's `index_type` through the
/// SHARED [`crate::storage::index_family`] classifier — the same function
/// `handle_create_index` routes on and `Catalog::rebuild_vector_indexes` filters
/// on — so it undoes exactly the branch of `handle_create_index` that created
/// it:
///   * [`IndexFamily::Art`] (`art` / `btree` / `hash` / absent)
///        → `ArtManager::drop_index`
///   * [`IndexFamily::Vector`] (`hnsw` / `hnsw_pq` / `persistent_hnsw`)
///        → `VectorIndexManager::drop_index`
///   * [`IndexFamily::DdlOnly`] (`gin` / `gist`)
///        → nothing to drop; those never had a backing index (see the CREATE
///          branch's comment).
/// An unclassifiable tag errors by name rather than pretending to have dropped
/// it.
///
/// The classifier is shared for a reason this function got wrong on its first
/// draft: it enumerated the tags itself and MISSED `persistent_hnsw`, the tag
/// `CREATE INDEX … USING hnsw … WITH (persistent = true)` persists. Such an
/// index was reopened at every start by `rebuild_vector_indexes` and could never
/// be dropped, with an error that blamed the user's catalog for being corrupt.
///
/// Removing the `meta:index:` definition is what makes the drop DURABLE:
/// `Catalog::rebuild_all_indexes` re-registers every user secondary index from
/// those records at open, so an index whose definition survived would simply
/// come back on the next restart.
///
/// [`IndexFamily::Art`]: crate::storage::IndexFamily::Art
/// [`IndexFamily::Vector`]: crate::storage::IndexFamily::Vector
/// [`IndexFamily::DdlOnly`]: crate::storage::IndexFamily::DdlOnly
pub(super) fn handle_drop_index(executor: &Executor, name: &str, if_exists: bool) -> Result<Box<dyn PhysicalOperator>> {
    use crate::storage::{index_family, IndexFamily};

    let Some(storage) = executor.storage() else {
        return Err(Error::query_execution("No storage engine available"));
    };

    // Mirrors `create_art_secondary_index`: index state lives on main, so a
    // branch session must not be able to mutate it. CREATE refuses; DROP has to
    // refuse for the stronger reason — it would delete main's durable
    // definition from inside a branch that is supposed to be discardable.
    if storage.is_branch_active() {
        return Err(Error::query_execution("DROP INDEX must run on the main branch"));
    }

    // *** SAFETY GATE — checked BEFORE anything is removed. ***
    //
    // A PRIMARY KEY / UNIQUE / FOREIGN KEY constraint is ENFORCED through its
    // backing ART index. Dropping one would not report anything: inserts would
    // simply stop being checked and duplicates would start landing silently.
    // That is the worst outcome this statement has, so it is guarded twice.
    //
    // The structural guarantee is that constraint indexes are unreachable here
    // at all: they are registered by `create_pk_index` / `create_unique_index`
    // / `create_fk_index` under generated names, and ONLY `handle_create_index`
    // ever calls `persist_index_definition` — so no constraint index has a
    // `meta:index:` record, and the lookup below would return `None` for one.
    // But that is a property of today's CALLERS, not of the data model, and it
    // would be silently voided by any future code path that persisted a
    // definition for a constraint index. So the live ART registry is also
    // consulted directly and by name. If both an explicit CREATE INDEX
    // definition and a constraint registration somehow shared a name, this
    // refuses — the safe direction.
    //
    // Message shape is PostgreSQL's ("cannot drop index … because constraint …
    // requires it") so the PG wire classifier maps it to 2BP01
    // dependent_objects_still_exist rather than XX000.
    if let Some((kind, owner)) = constraint_index_kind(storage, name) {
        return Err(Error::query_execution(format!(
            "cannot drop index \"{name}\" because constraint {kind} on \"{owner}\" requires it; \
             drop the constraint instead"
        )));
    }

    let catalog = storage.catalog();
    let definition = match catalog.get_index_definition(name)? {
        Some(definition) => Some(definition),
        // Record present but undecodable (a future on-disk format, or
        // corruption). `rebuild_all_indexes` skips such a record, so no index is
        // registered for it — but the record itself would otherwise be
        // undeletable forever. Fall through with an unknown type, which lands on
        // the ART arm and warns; the definition delete below is the point.
        None if catalog.index_definition_exists(name)? => {
            tracing::warn!(
                "DROP INDEX '{}': the catalog record exists but is not decodable by this build; \
                 removing the record",
                name
            );
            None
        }
        None => {
            // IF EXISTS now genuinely silences this. In v4.20.0 it deliberately
            // did NOT: nothing was dropped either way, so reporting success
            // would have been a silent no-op. A real drop exists now, so
            // PostgreSQL semantics apply — an absent index means the
            // post-condition already holds.
            if if_exists {
                return Ok(empty_ddl_result(executor));
            }
            // Deliberately says "index", never "table"/"relation", so the PG
            // wire's message-shape classifier maps it to 42704
            // undefined_object rather than 42P01 undefined_table.
            return Err(Error::query_execution(format!("index \"{name}\" does not exist")));
        }
    };

    // Dispatch on the recorded type. `None` is the pre-v3.37.2 legacy shape and
    // means the ART family, exactly as `rebuild_all_indexes` reads it — which is
    // not restated here, it is `index_family`'s single definition.
    let recorded_type = definition.as_ref().and_then(|d| d.index_type.as_deref());
    match index_family(recorded_type) {
        Some(IndexFamily::Art) => {
            let art_manager = storage.art_indexes();
            if let Err(e) = art_manager.drop_index(name) {
                // A definition with no live registration is a REAL state, not a
                // swallowed error: `rebuild_all_indexes` warns and continues
                // when a manual index fails to register at open, leaving exactly
                // this shape. The user's intent — the index is gone — is still
                // fully achieved by deleting the definition below, so the drop
                // proceeds. Every OTHER error propagates.
                if matches!(e, crate::storage::art_index::ArtIndexError::IndexNotFound(_)) {
                    tracing::warn!(
                        "DROP INDEX '{}': no live ART registration (the index was not rebuilt at open); \
                         removing the catalog definition anyway",
                        name
                    );
                } else {
                    return Err(Error::query_execution(format!(
                        "Failed to drop ART index '{}': {}",
                        name, e
                    )));
                }
            }
        }
        Some(IndexFamily::Vector) => {
            let vector_indexes = storage.vector_indexes();
            // Checked rather than string-matched on the error: `index_exists`
            // answers the same question precisely, and the same
            // definition-without-registration state is possible here too.
            //
            // `persistent_hnsw` lands here too (it is `IndexFamily::Vector`):
            // `VectorIndexManager::drop_index` matches on the live
            // `IndexStorage` and calls `drop_storage()` for a persistent index,
            // which deletes only that index's `prefix(index_id)` keyspace.
            if vector_indexes.index_exists(name) {
                vector_indexes.drop_index(name)?;
            } else {
                tracing::warn!(
                    "DROP INDEX '{}': no live HNSW index registered; removing the catalog definition anyway",
                    name
                );
            }
            // The graph dump + sidecar written by the last checkpoint outlive
            // the index otherwise: later checkpoints only write keys for LIVE
            // indexes, so a drop/recreate cycle accumulated dead `vecsnap:`
            // blobs and `.hnsw.graph` / `.hnsw.data` files forever. Best effort
            // and after the drop, never a reason to fail it.
            storage.remove_vector_index_snapshot(name);
        }
        Some(IndexFamily::DdlOnly) => {
            // DDL-only by construction — `handle_create_index` persists the
            // definition and builds NOTHING (the `@@` operator scans). So there
            // is no backing structure to remove and this is not a silent skip:
            // deleting the definition is the entire drop.
            tracing::info!(
                "DROP INDEX '{}': gin/gist indexes are DDL-only in this build; \
                 removing the catalog definition (there is no backing index)",
                name
            );
        }
        None => {
            // `index_family` could not classify the tag. Only
            // `handle_create_index` and the `WalOperation::CreateIndex` replay
            // of what it logged write these records, so an unclassifiable tag
            // means a downgrade (a newer binary wrote an index type this one has
            // never heard of) or a corrupt record — name it, do not guess which
            // structure to remove and do not report a drop that did not happen.
            let other = recorded_type.unwrap_or("<none>");
            return Err(Error::query_execution(format!(
                "Cannot drop index \"{name}\": unsupported persisted index type '{other}'. \
                 This build knows art, btree, hash, gin, gist, hnsw, hnsw_pq and persistent_hnsw."
            )));
        }
    }

    // Durability: this is the step that makes the drop survive a restart.
    catalog.drop_index_definition(name)?;

    // Replication / recovery. Same warn-and-continue posture as
    // `handle_create_index`'s `log_create_index`: the local drop has already
    // been made durable above, so a WAL append failure must not resurrect it.
    if let Err(e) = storage.log_drop_index(name) {
        tracing::warn!("Failed to log DROP INDEX to WAL: {}", e);
    }

    tracing::info!("Dropped index '{}'", name);
    Ok(empty_ddl_result(executor))
}

/// Handle DROP TABLE logical plan node.
///
/// Round-3 PARTITION BY Stage-0: `DROP TABLE parent` also drops every table
/// recorded as one of its Stage-0 `PARTITION OF` children (PostgreSQL parity),
/// recursively — a child may itself be a sub-partitioned parent. The cascade
/// reuses the ordinary `Catalog::drop_table` funnel for EVERY table (so ART
/// indexes, statistics cache, schema cache, columnar sidecars and the WAL
/// DropTable log are cleaned identically for parent and children), and is a
/// no-op (one point get on the registry) for a table with no partition links.
pub(super) fn handle_drop_table(
    executor: &Executor,
    table_name: &str,
    if_exists: bool,
) -> Result<Box<dyn PhysicalOperator>> {
    if let Some(storage) = executor.storage() {
        drop_table_and_partition_children(storage, table_name, if_exists)?;
        // Return empty result set for DDL
        Ok(empty_ddl_result(executor))
    } else {
        Err(Error::query_execution("No storage engine available"))
    }
}

/// Drop `table_name` and, recursively, its registered Stage-0 partition
/// children. `if_exists` governs only the top-level target; the cascade drops
/// children with IF-EXISTS semantics (a child dropped directly earlier is
/// simply absent). Every table is removed via the same `Catalog::drop_table`
/// funnel a plain DROP uses.
///
/// NON-ATOMIC (review-pinned): a mid-cascade child-drop failure leaves the
/// parent and earlier children dropped and the failing child alive with a
/// dangling `meta:partparent:` record pointing at the gone parent. That record
/// self-heals on the child's next direct drop and cannot mis-drop anything
/// (cascade reads only the parent's forward list, which is consumed up front)
/// — matching the engine's non-transactional DDL semantics generally.
fn drop_table_and_partition_children(
    storage: &crate::storage::StorageEngine,
    table_name: &str,
    if_exists: bool,
) -> Result<()> {
    let catalog = storage.catalog();

    // Resolve existence first so error semantics match a plain DROP exactly.
    if catalog.get_table_schema(table_name).is_err() {
        if !if_exists {
            return Err(Error::query_execution(format!("Table '{}' does not exist", table_name)));
        }
        // IF EXISTS on a missing table: nothing to drop or cascade. A missing
        // table cannot carry live registry links in normal operation.
        return Ok(());
    }

    // Drop this table via the ordinary funnel FIRST, so a drop failure leaves
    // the registry untouched.
    catalog.drop_table(table_name)?;
    // KanttBan #23 (v3.31.1 phase 2): clean up the identity side-table record.
    // Best-effort; a missing record is fine.
    let _ = catalog.drop_identity_columns(table_name);

    // Registry bookkeeping: detach this table from its own parent (if it is
    // itself a partition child) and take the list of children registered under
    // it. Empty for a non-partition table → no cascade.
    let children = catalog.take_partition_children_on_drop(table_name)?;

    // Cascade to partition children (dependents). Each recursion repeats the
    // same funnel + registry cleanup, so sub-partitioned children unwind fully.
    for child in children {
        drop_table_and_partition_children(storage, &child, true)?;
    }
    Ok(())
}

/// Handle TRUNCATE logical plan node
pub(super) fn handle_truncate(executor: &Executor, table_name: &str) -> Result<Box<dyn PhysicalOperator>> {
    if let Some(storage) = executor.storage() {
        let catalog = storage.catalog();

        // Check if table exists
        if catalog.get_table_schema(table_name).is_err() {
            return Err(Error::query_execution(format!("Table '{}' does not exist", table_name)));
        }

        // Item #2: preserve AS-OF history for COPY marker-covered rows before
        // their `data:` is removed below — TRUNCATE leaves `v:`/`v_idx:` intact
        // (so the per-row baseline stays queryable AS-OF), and markers must
        // match that by materializing their insert versions first.
        storage.materialize_copy_markers_for_table(table_name)?;

        // Delete all rows from the table
        let prefix = format!("data:{}:", table_name);
        let prefix_bytes = prefix.as_bytes();
        let mut keys_to_delete = Vec::new();

        // Collect all keys for this table
        // Use total_order_seek to bypass prefix bloom filter for full table scans
        let mut read_opts = ReadOptions::default();
        read_opts.set_total_order_seek(true);
        let iter = storage.db.iterator_opt(IteratorMode::Start, read_opts);
        for item in iter {
            let (key, _) = item.map_err(|e| Error::storage(format!("Iterator error: {}", e)))?;

            if !key.starts_with(prefix_bytes) {
                if let (Some(&k), Some(&p)) = (key.first(), prefix_bytes.first()) {
                    if k > p {
                        break;
                    }
                }
                continue;
            }

            keys_to_delete.push(key.to_vec());
        }

        // Delete all collected keys
        for key in &keys_to_delete {
            storage.delete(key)?;
        }

        // R3.3: purge columnar sidecars (col:/colz:/colzm:/colp:/colpm:) with
        // the rows — stale batches or live-row presence must not resurrect
        // truncated rows. No-op prefix seeks for row-only tables.
        crate::storage::ColumnarStore::purge_table_sidecars(&storage.db, table_name)?;

        // Clear ART index entries for this table so that stale PK/UNIQUE
        // values do not block re-insertion of the same values.
        // Skip clearing if branches exist or time-travel snapshots are
        // retained, because branch data and snapshots may still
        // reference the indexed values.
        // Check for user-created branches (exclude the auto-created "main" branch).
        // Branch data uses separate key prefixes and does not share the ART index,
        // but as a safety measure we skip clearing when user branches exist.
        let has_user_branches = storage
            .list_branches()
            .map(|b| b.iter().any(|br| br.name != "main"))
            .unwrap_or(false);
        if !has_user_branches {
            storage.art_indexes().clear_table_indexes(table_name);
        }

        // Log to WAL for replication
        if let Err(e) = storage.log_truncate(table_name) {
            tracing::warn!("Failed to log TRUNCATE to WAL: {}", e);
        }

        // Return empty result set for DDL
        Ok(Box::new(
            ScanOperator::new(
                "".to_string(),
                Arc::new(crate::Schema { columns: vec![] }),
                None,
                vec![],
                vec![],
            )
            .with_timeout(executor.timeout_ctx()),
        ))
    } else {
        Err(Error::query_execution("No storage engine available"))
    }
}

// =============================================================================
// HA Operations (ha-tier1 feature)
// =============================================================================

/// Handle SWITCHOVER to target node
/// Example: SELECT helios_switchover('node-uuid')
#[cfg(feature = "ha-tier1")]
pub(super) fn handle_switchover(_executor: &Executor, target_node: &str) -> Result<Box<dyn PhysicalOperator>> {
    use crate::replication::ha_state::ha_state;
    use crate::replication::topology_manager;
    use uuid::Uuid;

    // Resolve target node (can be alias or UUID)
    let target_uuid = topology_manager()
        .resolve_node_id(target_node)
        .or_else(|| {
            // Fallback: try parsing as UUID directly if not in topology
            Uuid::parse_str(target_node).ok()
        })
        .ok_or_else(|| {
            Error::query_execution(format!(
                "Target node '{}' not found. Specify a valid node alias or UUID.",
                target_node
            ))
        })?;

    // Get HA state registry
    let ha_registry = ha_state();

    // Check if this node is primary
    if ha_registry.get_role() != crate::replication::ha_state::HARole::Primary {
        return Err(Error::query_execution(
            "Switchover can only be initiated from the primary node",
        ));
    }

    // Check if target standby exists and is healthy
    let standbys = ha_registry.get_standbys();
    let target_standby = standbys.iter().find(|s| s.node_id == target_uuid);

    if target_standby.is_none() {
        return Err(Error::query_execution(format!(
            "Target standby '{}' ({}) not found or not connected",
            target_node, target_uuid
        )));
    }

    // Get the display name for user feedback
    let display_name = topology_manager()
        .get_node(target_uuid)
        .map(|n| n.display_name())
        .unwrap_or_else(|| target_node.to_string());

    // For now, return a message indicating switchover would be initiated
    // Full implementation requires async coordination with SwitchoverCoordinator
    let msg = format!(
        "Switchover to node {} ({}) initiated. This is a placeholder - full async switchover requires runtime integration.",
        display_name, target_uuid
    );

    Ok(Box::new(super::StatusMessageOperator::new(msg)))
}

/// Handle SWITCHOVER CHECK to validate preconditions
/// Example: SELECT helios_switchover_check('node-uuid') or SELECT helios_switchover_check('alias')
#[cfg(feature = "ha-tier1")]
pub(super) fn handle_switchover_check(_executor: &Executor, target_node: &str) -> Result<Box<dyn PhysicalOperator>> {
    use crate::replication::ha_state::ha_state;
    use crate::replication::topology_manager;
    use crate::{Column, DataType, Schema, Tuple, Value};
    use uuid::Uuid;

    // Resolve target node (can be alias or UUID)
    let target_uuid = topology_manager()
        .resolve_node_id(target_node)
        .or_else(|| {
            // Fallback: try parsing as UUID directly if not in topology
            Uuid::parse_str(target_node).ok()
        })
        .ok_or_else(|| {
            Error::query_execution(format!(
                "Target node '{}' not found. Specify a valid node alias or UUID.",
                target_node
            ))
        })?;

    // Get HA state registry
    let ha_registry = ha_state();

    // Build check result
    let mut can_proceed = true;
    let mut target_healthy = false;
    let mut target_lsn: u64 = 0;
    let primary_lsn = ha_registry.get_lsn();
    let mut warnings = Vec::new();
    let mut blockers = Vec::new();

    // Check if this node is primary
    if ha_registry.get_role() != crate::replication::ha_state::HARole::Primary {
        can_proceed = false;
        blockers.push("This node is not the primary".to_string());
    }

    // Check target standby
    let standbys = ha_registry.get_standbys();
    if let Some(standby) = standbys.iter().find(|s| s.node_id == target_uuid) {
        target_healthy = true;
        target_lsn = standby.apply_lsn;

        let lag = primary_lsn.saturating_sub(target_lsn);
        if lag > 0 {
            warnings.push(format!("Target standby is {} LSN behind", lag));
        }
    } else {
        can_proceed = false;
        blockers.push(format!("Target node {} ({}) not found", target_node, target_uuid));
    }

    let lag_bytes = primary_lsn.saturating_sub(target_lsn) as i64;

    // Create result tuple
    let schema = Arc::new(Schema {
        columns: vec![
            Column::new("can_proceed", DataType::Boolean),
            Column::new("target_healthy", DataType::Boolean),
            Column::new("target_lsn", DataType::Int8),
            Column::new("primary_lsn", DataType::Int8),
            Column::new("lag_bytes", DataType::Int8),
            Column::new("warnings", DataType::Text),
            Column::new("blockers", DataType::Text),
        ],
    });

    let tuple = Tuple::new(vec![
        Value::Boolean(can_proceed),
        Value::Boolean(target_healthy),
        Value::Int8(target_lsn as i64),
        Value::Int8(primary_lsn as i64),
        Value::Int8(lag_bytes),
        Value::String(warnings.join("; ")),
        Value::String(blockers.join("; ")),
    ]);

    Ok(Box::new(SingleTupleOperator::new(tuple, schema)))
}

/// Handle CLUSTER STATUS query
/// Example: SELECT * FROM helios_cluster_status()
#[cfg(feature = "ha-tier1")]
pub(super) fn handle_cluster_status(_executor: &Executor) -> Result<Box<dyn PhysicalOperator>> {
    use crate::replication::ha_state::{ha_state, HARole};
    use crate::{Column, DataType, Schema, Tuple, Value};

    let ha_registry = ha_state();

    let schema = Arc::new(Schema {
        columns: vec![
            Column::new("node_id", DataType::Text),
            Column::new("role", DataType::Text),
            Column::new("address", DataType::Text),
            Column::new("is_healthy", DataType::Boolean),
            Column::new("lsn", DataType::Int8),
            Column::new("lag_ms", DataType::Int8),
            Column::new("priority", DataType::Int4),
        ],
    });

    let mut tuples = Vec::new();

    // Add primary info if available
    if let Some(config) = ha_registry.get_config() {
        let role_str = match ha_registry.get_role() {
            HARole::Primary => "primary",
            HARole::Standby => "standby",
            HARole::Standalone => "standalone",
            HARole::Observer => "observer",
        };

        tuples.push(Tuple::new(vec![
            Value::String(config.node_id.to_string()),
            Value::String(role_str.to_string()),
            Value::String(config.listen_addr.clone()),
            Value::Boolean(true), // Local node is always "healthy" from its perspective
            Value::Int8(ha_registry.get_lsn() as i64),
            Value::Int8(0),   // No lag for self
            Value::Int4(100), // Default priority - config doesn't store priority yet
        ]));
    }

    // Add standby info
    for standby in ha_registry.get_standbys() {
        tuples.push(Tuple::new(vec![
            Value::String(standby.node_id.to_string()),
            Value::String("standby".to_string()),
            Value::String(standby.address.clone()),
            Value::Boolean(true), // Connected standbys are healthy
            Value::Int8(standby.apply_lsn as i64),
            Value::Int8(standby.lag_ms as i64),
            Value::Int4(0), // Priority not stored in StandbyInfo yet
        ]));
    }

    Ok(Box::new(MultiTupleOperator::new(tuples, schema)))
}

/// Single tuple operator for returning one result row
#[cfg(feature = "ha-tier1")]
struct SingleTupleOperator {
    tuple: Option<crate::Tuple>,
    schema: Arc<crate::Schema>,
}

#[cfg(feature = "ha-tier1")]
impl SingleTupleOperator {
    fn new(tuple: crate::Tuple, schema: Arc<crate::Schema>) -> Self {
        Self {
            tuple: Some(tuple),
            schema,
        }
    }
}

#[cfg(feature = "ha-tier1")]
impl super::PhysicalOperator for SingleTupleOperator {
    fn next(&mut self) -> Result<Option<crate::Tuple>> {
        Ok(self.tuple.take())
    }

    fn schema(&self) -> Arc<crate::Schema> {
        self.schema.clone()
    }
}

/// Multi tuple operator for returning multiple result rows
#[cfg(feature = "ha-tier1")]
struct MultiTupleOperator {
    tuples: std::collections::VecDeque<crate::Tuple>,
    schema: Arc<crate::Schema>,
}

#[cfg(feature = "ha-tier1")]
impl MultiTupleOperator {
    fn new(tuples: Vec<crate::Tuple>, schema: Arc<crate::Schema>) -> Self {
        Self {
            tuples: tuples.into_iter().collect(),
            schema,
        }
    }
}

#[cfg(feature = "ha-tier1")]
impl super::PhysicalOperator for MultiTupleOperator {
    fn next(&mut self) -> Result<Option<crate::Tuple>> {
        Ok(self.tuples.pop_front())
    }

    fn schema(&self) -> Arc<crate::Schema> {
        self.schema.clone()
    }
}

/// Handle SET NODE ALIAS command
#[cfg(feature = "ha-tier1")]
pub(super) fn handle_set_node_alias(
    _executor: &Executor,
    node_id: &str,
    alias: &Option<String>,
) -> Result<Box<dyn PhysicalOperator>> {
    use crate::replication::topology_manager;
    use crate::{Column, DataType, Schema, Tuple, Value};
    use uuid::Uuid;

    let topology = topology_manager();

    // Resolve the node_id (could be existing alias or UUID)
    let target_uuid = topology
        .resolve_node_id(node_id)
        .or_else(|| Uuid::parse_str(node_id).ok())
        .ok_or_else(|| {
            Error::query_execution(format!(
                "Node '{}' not found in cluster topology. Use SHOW TOPOLOGY to see available nodes.",
                node_id
            ))
        })?;

    // Set or clear the alias
    let result_msg = if let Some(ref new_alias) = alias {
        // Validate alias format (no spaces, not a valid UUID pattern)
        if new_alias.contains(' ') {
            return Err(Error::query_execution("Alias cannot contain spaces"));
        }
        if Uuid::parse_str(new_alias).is_ok() {
            return Err(Error::query_execution("Alias cannot be a valid UUID format"));
        }
        if new_alias.is_empty() {
            return Err(Error::query_execution("Alias cannot be empty"));
        }

        if !topology.set_alias(target_uuid, Some(new_alias.clone())) {
            return Err(Error::query_execution(format!(
                "Failed to set alias: '{}' is already in use by another node",
                new_alias
            )));
        }

        format!("Node alias '{}' set for node '{}'", new_alias, target_uuid)
    } else {
        // Clearing alias - this should always succeed if the node exists
        topology.set_alias(target_uuid, None);
        format!("Node alias removed for node '{}'", target_uuid)
    };

    let schema = Arc::new(Schema {
        columns: vec![Column::new("result", DataType::Text)],
    });

    let tuple = Tuple::new(vec![Value::String(result_msg)]);

    Ok(Box::new(SingleTupleOperator::new(tuple, schema)))
}

/// Handle SHOW TOPOLOGY command - displays detailed cluster topology
#[cfg(feature = "ha-tier1")]
pub(super) fn handle_show_topology(_executor: &Executor) -> Result<Box<dyn PhysicalOperator>> {
    use crate::replication::ha_state::{ha_state, HARole};
    use crate::replication::topology_manager;
    use crate::{Column, DataType, Schema, Tuple, Value};

    let ha_registry = ha_state();
    let topology = topology_manager();

    let schema = Arc::new(Schema {
        columns: vec![
            Column::new("node_id", DataType::Text),
            Column::new("alias", DataType::Text),
            Column::new("role", DataType::Text),
            Column::new("client_addr", DataType::Text),
            Column::new("replication_addr", DataType::Text),
            Column::new("healthy", DataType::Boolean),
            Column::new("health_msg", DataType::Text),
            Column::new("last_seen_secs", DataType::Int8),
            Column::new("lsn", DataType::Int8),
            Column::new("lag_ms", DataType::Int8),
            Column::new("priority", DataType::Int4),
            Column::new("weight", DataType::Int4),
        ],
    });

    let mut tuples = Vec::new();

    // Helper to get alias for a node
    let get_alias = |node_id: uuid::Uuid| -> Value {
        topology
            .get_node(node_id)
            .and_then(|n| n.alias.clone())
            .map(Value::String)
            .unwrap_or(Value::Null)
    };

    // Helper to get node info from topology
    let get_topology_info = |node_id: uuid::Uuid| -> (u32, u32, Option<String>) {
        topology
            .get_node(node_id)
            .map(|n| (n.priority, n.weight, n.health_message.clone()))
            .unwrap_or((100, 100, None))
    };

    // Add local node info
    if let Some(config) = ha_registry.get_config() {
        let role_str = match ha_registry.get_role() {
            HARole::Primary => "Primary",
            HARole::Standby => "Standby",
            HARole::Standalone => "Standalone",
            HARole::Observer => "Observer",
        };

        let alias = get_alias(config.node_id);
        let (priority, weight, health_msg) = get_topology_info(config.node_id);

        tuples.push(Tuple::new(vec![
            Value::String(config.node_id.to_string()),
            alias,
            Value::String(role_str.to_string()),
            Value::String(config.listen_addr.clone()),
            Value::String(format!("{}:{}", config.listen_addr, config.replication_port)),
            Value::Boolean(true), // Local node is always "healthy" from its perspective
            Value::String(health_msg.unwrap_or_else(|| "OK".to_string())),
            Value::Int8(0), // last seen
            Value::Int8(ha_registry.get_lsn() as i64),
            Value::Int8(0), // No lag for self
            Value::Int4(priority as i32),
            Value::Int4(weight as i32),
        ]));
    }

    // Add standby info from HA registry, enriched with topology data
    for standby in ha_registry.get_standbys() {
        let alias = get_alias(standby.node_id);
        let (priority, weight, health_msg) = get_topology_info(standby.node_id);

        tuples.push(Tuple::new(vec![
            Value::String(standby.node_id.to_string()),
            alias,
            Value::String("Standby".to_string()),
            Value::String(standby.address.clone()),
            Value::String(standby.address.clone()), // replication addr same as client for now
            Value::Boolean(true),                   // Connected standbys are healthy
            Value::String(health_msg.unwrap_or_else(|| "Connected".to_string())),
            Value::Int8(0), // last seen
            Value::Int8(standby.apply_lsn as i64),
            Value::Int8(standby.lag_ms as i64),
            Value::Int4(priority as i32),
            Value::Int4(weight as i32),
        ]));
    }

    Ok(Box::new(MultiTupleOperator::new(tuples, schema)))
}
