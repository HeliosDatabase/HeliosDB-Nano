//! Table and index scanning operators
//!
//! This module provides operators for reading data from tables and indexes.

#![allow(elided_lifetimes_in_paths)]

use super::{Executor, PhysicalOperator, TimeoutContext};
use crate::sql::logical_plan::LogicalExpr;
use crate::sql::LogicalPlan;
use crate::storage::predicate_pushdown::AnalyzedPredicate;
use crate::{DataType, Error, Result, Schema, Tuple, Value};
use std::collections::HashSet;
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ScanDecodeHint {
    Prefix(usize),
    Columns(Vec<usize>),
}

/// Compute a conservative single-table prefix-decode hint for a read plan (issue #1
/// follow-up). `Some((table, prefix_len))` means every column the plan references in
/// `table` lives at an index `< prefix_len`, so a scan of `table` only needs to decode
/// that many leading columns. Returns `None` (→ full decode) on anything uncertain:
/// more than one distinct table, a wildcard (`SELECT *`), any subquery, an unresolved
/// column, or an unrecognized plan node. The `LogicalExpr` match is intentionally
/// exhaustive so a newly added expression variant is a compile error here (forcing a
/// correctness review) rather than a silent miss.
pub(super) fn compute_scan_prefix_hint(plan: &LogicalPlan) -> Option<(String, usize)> {
    let (table_name, indices, _total_cols) = collect_single_table_column_indices(plan)?;
    let prefix_len = indices.last().map(|idx| idx + 1).unwrap_or(0);
    Some((table_name, prefix_len))
}

/// Compute the row-decode strategy for a single-table read plan. Prefix decode is
/// fastest when all required columns are early. Selected-column decode handles the
/// common analytics shape where a query needs a few later columns but should still
/// skip unrelated leading/middle/tail values.
pub(super) fn compute_scan_decode_hint(plan: &LogicalPlan) -> Option<(String, ScanDecodeHint)> {
    let (table_name, indices, total_cols) = collect_single_table_column_indices(plan)?;
    choose_scan_decode_hint(&indices, total_cols).map(|hint| (table_name, hint))
}

/// Compute row-decode hints for every table in a read plan. This extends the
/// single-table fast path to joins only when column references can be mapped
/// unambiguously to a scan table/alias. Anything uncertain falls back to full
/// decode for the whole plan.
pub(super) fn compute_scan_decode_hints(plan: &LogicalPlan) -> Vec<(String, ScanDecodeHint)> {
    if let Some(hint) = compute_scan_decode_hint(plan) {
        return vec![hint];
    }

    let mut tables = Vec::new();
    let mut bail = false;
    collect_scan_tables(plan, &mut tables, &mut bail);
    if bail || tables.len() < 2 || !table_qualifiers_are_unique(&tables) {
        return Vec::new();
    }

    let mut needed: Vec<HashSet<usize>> = (0..tables.len()).map(|_| HashSet::new()).collect();
    collect_plan_columns_by_table(plan, &tables, &mut needed, &mut bail, true);
    if bail {
        return Vec::new();
    }

    let mut hints = Vec::new();
    for (idx, table) in tables.iter().enumerate() {
        let mut indices: Vec<usize> = needed[idx].iter().copied().collect();
        indices.sort_unstable();
        if let Some(hint) = choose_scan_decode_hint(&indices, table.schema.columns.len()) {
            hints.push((table.table_name.clone(), hint));
        }
    }
    hints
}

fn choose_scan_decode_hint(indices: &[usize], total_cols: usize) -> Option<ScanDecodeHint> {
    let prefix_len = indices.last().map(|idx| idx + 1).unwrap_or(0);

    if is_prefix_contiguous(indices) && should_use_prefix_decode(prefix_len, total_cols) {
        return Some(ScanDecodeHint::Prefix(prefix_len));
    }

    if should_use_selected_decode(indices, total_cols) {
        return Some(ScanDecodeHint::Columns(indices.to_vec()));
    }

    None
}

fn columnar_scan_columns(
    schema: &Schema,
    projection: Option<&Vec<usize>>,
    hint: Option<&ScanDecodeHint>,
) -> Option<Vec<usize>> {
    if let Some(hint) = hint {
        let indices = match hint {
            ScanDecodeHint::Prefix(prefix_len) => (0..*prefix_len).collect(),
            ScanDecodeHint::Columns(columns) => columns.clone(),
        };
        if indices_are_columnar(schema, &indices) {
            return Some(indices);
        }
        return None;
    }

    let indices: Vec<usize> = if let Some(projection) = projection {
        projection.clone()
    } else if schema
        .columns
        .iter()
        .all(|column| column.storage_mode == crate::ColumnStorageMode::Columnar)
    {
        (0..schema.columns.len()).collect()
    } else {
        return None;
    };

    indices_are_columnar(schema, &indices).then_some(indices)
}

fn columnar_scan_columns_with_predicates(
    schema: &Schema,
    projection: Option<&Vec<usize>>,
    hint: Option<&ScanDecodeHint>,
    predicates: &[AnalyzedPredicate],
) -> Option<Vec<usize>> {
    let mut indices = columnar_scan_columns(schema, projection, hint)?;
    indices.extend(predicates.iter().map(|predicate| predicate.column_index));
    indices.sort_unstable();
    indices.dedup();
    indices_are_columnar(schema, &indices).then_some(indices)
}

fn indices_are_columnar(schema: &Schema, indices: &[usize]) -> bool {
    indices.iter().all(|&idx| {
        schema.columns.get(idx).map_or(false, |column| {
            column.storage_mode == crate::ColumnStorageMode::Columnar
        })
    })
}

fn should_apply_columnar_predicates(predicates: &[AnalyzedPredicate]) -> bool {
    predicates.len() > 1
        || predicates.iter().any(|predicate| {
            matches!(
                predicate.op,
                crate::storage::predicate_pushdown::PredicateOp::Eq
                    | crate::storage::predicate_pushdown::PredicateOp::Lt
                    | crate::storage::predicate_pushdown::PredicateOp::LtEq
                    | crate::storage::predicate_pushdown::PredicateOp::Gt
                    | crate::storage::predicate_pushdown::PredicateOp::GtEq
                    | crate::storage::predicate_pushdown::PredicateOp::In
                    | crate::storage::predicate_pushdown::PredicateOp::IsNull
            )
        })
}

pub(crate) fn storage_predicates_are_sql_safe(schema: &Schema, predicates: &[AnalyzedPredicate]) -> bool {
    predicates.iter().all(|predicate| {
        let Some(column) = schema.columns.get(predicate.column_index) else {
            return false;
        };
        match predicate.op {
            crate::storage::predicate_pushdown::PredicateOp::IsNull
            | crate::storage::predicate_pushdown::PredicateOp::IsNotNull => true,
            crate::storage::predicate_pushdown::PredicateOp::Between => {
                storage_filter_value_matches_type(&column.data_type, &predicate.value)
                    && predicate
                        .value2
                        .as_ref()
                        .is_some_and(|value| storage_filter_value_matches_type(&column.data_type, value))
            }
            crate::storage::predicate_pushdown::PredicateOp::In => predicate
                .value_list
                .iter()
                .all(|value| storage_filter_value_matches_type(&column.data_type, value)),
            _ => storage_filter_value_matches_type(&column.data_type, &predicate.value),
        }
    })
}

fn storage_filter_value_matches_type(data_type: &DataType, value: &Value) -> bool {
    if matches!(value, Value::Null) {
        return true;
    }

    match data_type {
        DataType::Int2 | DataType::Int4 | DataType::Int8 => {
            matches!(value, Value::Int2(_) | Value::Int4(_) | Value::Int8(_))
        }
        DataType::Float4 | DataType::Float8 => matches!(value, Value::Float4(_) | Value::Float8(_)),
        DataType::Text | DataType::Varchar(_) | DataType::Char(_) => matches!(value, Value::String(_)),
        DataType::Boolean => matches!(value, Value::Boolean(_)),
        DataType::Bytea => matches!(value, Value::Bytes(_)),
        DataType::Uuid => matches!(value, Value::Uuid(_)),
        DataType::Date => matches!(value, Value::Date(_)),
        DataType::Timestamp | DataType::Timestamptz => matches!(value, Value::Timestamp(_)),
        DataType::Time => matches!(value, Value::Time(_)),
        DataType::Interval => matches!(value, Value::Interval(_)),
        DataType::Numeric => matches!(value, Value::Numeric(_)),
        DataType::Json | DataType::Jsonb => matches!(value, Value::Json(_)),
        DataType::Vector(_) => matches!(value, Value::Vector(_)),
        DataType::Array(_) => matches!(value, Value::Array(_)),
    }
}

/// Borrowed-source variant of [`filter_tuples_with_evaluator`] for
/// Arc-shared CTE tuples (R3.5 item 5): clones ONLY the rows that pass the
/// predicate instead of deep-cloning the whole materialized set first.
fn filter_shared_tuples_with_evaluator(
    tuples: &[Tuple],
    schema: Arc<Schema>,
    predicate: &LogicalExpr,
    parameters: &[Value],
) -> Result<Vec<Tuple>> {
    let evaluator = crate::sql::Evaluator::with_parameters(schema, parameters.to_vec());
    let predicate = evaluator.bind(predicate.clone());
    let mut filtered = Vec::new();
    for tuple in tuples {
        match evaluator.evaluate(&predicate, tuple)? {
            Value::Boolean(true) => filtered.push(tuple.clone()),
            Value::Boolean(false) | Value::Null => {}
            result => {
                return Err(Error::query_execution(format!(
                    "Filter predicate must evaluate to boolean, got: {:?}",
                    result
                )));
            }
        }
    }
    Ok(filtered)
}

fn filter_tuples_with_evaluator(
    tuples: Vec<Tuple>,
    schema: Arc<Schema>,
    predicate: &LogicalExpr,
    parameters: &[Value],
) -> Result<Vec<Tuple>> {
    let evaluator = crate::sql::Evaluator::with_parameters(schema, parameters.to_vec());
    let mut filtered = Vec::with_capacity(tuples.len());
    for tuple in tuples {
        match evaluator.evaluate(predicate, &tuple)? {
            Value::Boolean(true) => filtered.push(tuple),
            Value::Boolean(false) | Value::Null => {}
            result => {
                return Err(Error::query_execution(format!(
                    "Filter predicate must evaluate to boolean, got: {:?}",
                    result
                )));
            }
        }
    }
    Ok(filtered)
}

pub(super) fn try_index_point_lookup_for_scan(
    executor: &Executor,
    input: &LogicalPlan,
    predicate: &LogicalExpr,
) -> Result<Option<Box<dyn PhysicalOperator>>> {
    let storage = match executor.storage() {
        Some(storage) => storage,
        None => return Ok(None),
    };
    if storage.is_branch_active() {
        return Ok(None);
    }

    let LogicalPlan::Scan {
        table_name,
        alias,
        schema,
        projection,
        as_of,
    } = input
    else {
        return Ok(None);
    };
    // R2.3: inside a transaction the ART probe stays enabled only for
    // ReadCommitted session transactions with no staged writes touching this
    // table (statement-fresh snapshot ⇒ current-storage probe is equivalent;
    // staged writes ⇒ slow path for read-your-writes).
    if executor.txn_forces_slow_reads_for_table(table_name) {
        return Ok(None);
    }
    if as_of.is_some()
        || executor.get_cte(table_name).is_some()
        || storage.mv_catalog().view_exists(table_name)?
        || !storage.catalog().table_exists(table_name)?
    {
        return Ok(None);
    }

    let materialized_predicate = executor.materialize_subqueries(predicate)?;
    let Some((index_name, lookup_value)) = indexed_equality_lookup(
        storage,
        table_name,
        schema.as_ref(),
        &materialized_predicate,
        executor.parameters(),
    ) else {
        return Ok(None);
    };

    let key = crate::storage::ArtIndexManager::encode_key_from_values(std::iter::once(&lookup_value));
    let row_ids = storage.art_indexes().index_get_all(&index_name, &key);
    let mut tuples = Vec::with_capacity(row_ids.len());
    for row_id in row_ids {
        if let Some(tuple) = storage.get_row_by_id(table_name, row_id, schema.as_ref())? {
            tuples.push(tuple);
        }
    }

    let source_name = alias.as_ref().unwrap_or(table_name);
    let actual_schema = Arc::new(schema_with_source(schema.as_ref(), source_name, table_name));
    let tuples = filter_tuples_with_evaluator(
        tuples,
        actual_schema.clone(),
        &materialized_predicate,
        executor.parameters(),
    )?;

    Ok(Some(Box::new(
        ScanOperator::new(
            table_name.clone(),
            actual_schema,
            projection.clone(),
            tuples,
            executor.parameters().to_vec(),
        )
        .with_timeout(executor.timeout_ctx()),
    )))
}

// ═══════════════════════════════════════════════════════════════════════════
// R4.4: index range scans — `col > / >= / < / <= / BETWEEN bound` predicates
// on a single-column ART-indexed column become an ordered, bounded index
// iteration (seek + bounded iterate) instead of a full scan + filter.
// ═══════════════════════════════════════════════════════════════════════════

/// Detected index range scan: encoded bounds plus display strings for EXPLAIN.
pub(super) struct IndexRangeSpec {
    pub(super) index_name: String,
    pub(super) column_name: String,
    /// Encoded `(bound, inclusive)` — `None` = unbounded on that side.
    pub(super) lower: Option<(Vec<u8>, bool)>,
    pub(super) upper: Option<(Vec<u8>, bool)>,
    /// Human-readable bound description, e.g. `score > 10 AND score <= 90`.
    pub(super) display: String,
}

/// Diagnostic / safety kill switch for the index range scan and ordered
/// index top-k fast paths (mirrors `HELIOS_KNN_FAST_OFF`).
pub(super) fn index_range_fast_path_disabled() -> bool {
    std::env::var_os("HELIOS_INDEX_RANGE_OFF").is_some()
}

/// Column types whose v2 key encoding is order-preserving, i.e. byte order of
/// encoded keys == SQL value order. Range scans are only planned for these.
pub(super) fn range_scannable_type(data_type: &DataType) -> bool {
    matches!(
        data_type,
        DataType::Int2
            | DataType::Int4
            | DataType::Int8
            | DataType::Float4
            | DataType::Float8
            | DataType::Text
            | DataType::Varchar(_)
            | DataType::Char(_)
    )
}

fn flatten_and<'a>(expr: &'a LogicalExpr, out: &mut Vec<&'a LogicalExpr>) {
    use crate::sql::BinaryOperator;
    if let LogicalExpr::BinaryExpr {
        left,
        op: BinaryOperator::And,
        right,
    } = expr
    {
        flatten_and(left, out);
        flatten_and(right, out);
    } else {
        out.push(expr);
    }
}

fn range_column_ref<'a>(schema: &'a Schema, expr: &LogicalExpr) -> Option<&'a crate::Column> {
    let LogicalExpr::Column { table, name } = expr else {
        return None;
    };
    let idx = schema
        .get_qualified_column_index(table.as_deref(), name)
        .or_else(|| schema.get_column_index(name))?;
    schema.columns.get(idx)
}

#[derive(Default)]
struct RawColumnBounds {
    /// `(bound value, inclusive)`
    lowers: Vec<(Value, bool)>,
    uppers: Vec<(Value, bool)>,
}

/// Analyze an (already materialized) predicate for range constraints on a
/// single-column-indexed, range-scannable column. Returns the spec with
/// encoded + tightened bounds, or `None` when no conjunct qualifies (mixed /
/// uncoercible bound types simply fall back to the normal scan path).
pub(super) fn indexed_range_lookup(
    storage: &crate::storage::StorageEngine,
    table_name: &str,
    schema: &Schema,
    predicate: &LogicalExpr,
    parameters: &[Value],
) -> Option<IndexRangeSpec> {
    use crate::sql::BinaryOperator;

    let mut conjuncts = Vec::new();
    flatten_and(predicate, &mut conjuncts);

    let mut by_column: std::collections::HashMap<String, RawColumnBounds> = std::collections::HashMap::new();
    for conjunct in &conjuncts {
        match conjunct {
            LogicalExpr::BinaryExpr { left, op, right } => {
                let (column, value_expr, op) = if let Some(col) = range_column_ref(schema, left) {
                    (col, right.as_ref(), *op)
                } else if let Some(col) = range_column_ref(schema, right) {
                    // `bound < col` ≡ `col > bound`: flip the comparison.
                    let flipped = match op {
                        BinaryOperator::Lt => BinaryOperator::Gt,
                        BinaryOperator::LtEq => BinaryOperator::GtEq,
                        BinaryOperator::Gt => BinaryOperator::Lt,
                        BinaryOperator::GtEq => BinaryOperator::LtEq,
                        other => *other,
                    };
                    (col, left.as_ref(), flipped)
                } else {
                    continue;
                };
                let Some(raw) = lookup_bound_value(value_expr, parameters) else {
                    continue;
                };
                if matches!(raw, Value::Null) {
                    continue;
                }
                let bounds = by_column.entry(column.name.clone()).or_default();
                match op {
                    BinaryOperator::Gt => bounds.lowers.push((raw, false)),
                    BinaryOperator::GtEq => bounds.lowers.push((raw, true)),
                    BinaryOperator::Lt => bounds.uppers.push((raw, false)),
                    BinaryOperator::LtEq => bounds.uppers.push((raw, true)),
                    _ => {}
                }
            }
            LogicalExpr::Between {
                expr,
                low,
                high,
                negated: false,
            } => {
                let Some(column) = range_column_ref(schema, expr) else {
                    continue;
                };
                let (Some(low), Some(high)) = (
                    lookup_bound_value(low, parameters),
                    lookup_bound_value(high, parameters),
                ) else {
                    continue;
                };
                if matches!(low, Value::Null) || matches!(high, Value::Null) {
                    continue;
                }
                let bounds = by_column.entry(column.name.clone()).or_default();
                bounds.lowers.push((low, true));
                bounds.uppers.push((high, true));
            }
            _ => {}
        }
    }
    if by_column.is_empty() {
        return None;
    }

    // Choose the first qualifying column in schema order (deterministic).
    for column in &schema.columns {
        let Some(raw_bounds) = by_column.get(&column.name) else {
            continue;
        };
        if !range_scannable_type(&column.data_type) {
            continue;
        }
        let Some(index_name) = storage.art_indexes().find_column_index(table_name, &column.name) else {
            continue;
        };

        // Coerce every bound to the column type; any failure (mixed types
        // the evaluator may still accept or reject) disqualifies the column
        // and falls back to the normal scan path.
        let mut encoded_lowers: Vec<(Vec<u8>, bool, String)> = Vec::new();
        let mut encoded_uppers: Vec<(Vec<u8>, bool, String)> = Vec::new();
        let mut ok = true;
        for (raw, inclusive) in &raw_bounds.lowers {
            match coerce_index_lookup_value(raw.clone(), &column.data_type) {
                Some(v) if !matches!(v, Value::Null) => {
                    let key = crate::storage::ArtIndexManager::encode_key_from_values(std::iter::once(&v));
                    encoded_lowers.push((key, *inclusive, format!("{v}")));
                }
                _ => {
                    ok = false;
                    break;
                }
            }
        }
        if ok {
            for (raw, inclusive) in &raw_bounds.uppers {
                match coerce_index_lookup_value(raw.clone(), &column.data_type) {
                    Some(v) if !matches!(v, Value::Null) => {
                        let key = crate::storage::ArtIndexManager::encode_key_from_values(std::iter::once(&v));
                        encoded_uppers.push((key, *inclusive, format!("{v}")));
                    }
                    _ => {
                        ok = false;
                        break;
                    }
                }
            }
        }
        if !ok || (encoded_lowers.is_empty() && encoded_uppers.is_empty()) {
            continue;
        }

        // Tighten: max lower / min upper (exclusive beats inclusive on ties).
        // Encoded byte order == value order for range-scannable types, so the
        // comparison happens on the encoded form.
        let lower = encoded_lowers
            .into_iter()
            .max_by(|a, b| a.0.cmp(&b.0).then_with(|| b.1.cmp(&a.1)))
            .map(|(key, inclusive, text)| (key, inclusive, text));
        let upper = encoded_uppers
            .into_iter()
            .min_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)))
            .map(|(key, inclusive, text)| (key, inclusive, text));

        let mut parts = Vec::new();
        if let Some((_, inclusive, text)) = &lower {
            parts.push(format!(
                "{} {} {}",
                column.name,
                if *inclusive { ">=" } else { ">" },
                text
            ));
        }
        if let Some((_, inclusive, text)) = &upper {
            parts.push(format!(
                "{} {} {}",
                column.name,
                if *inclusive { "<=" } else { "<" },
                text
            ));
        }
        let display = parts.join(" AND ");

        // Fixed-width types: synthesize an unbounded-side lower bound at the
        // type minimum so the 1-byte NULL key (0x00) is never visited.
        let lower = lower.map(|(key, inclusive, _)| (key, inclusive)).or_else(|| {
            let width = match column.data_type {
                DataType::Int2 => Some(2),
                DataType::Int4 => Some(4),
                DataType::Int8 | DataType::Float8 => Some(8),
                DataType::Float4 => Some(4),
                _ => None,
            };
            width.map(|w| (vec![0u8; w], true))
        });
        let upper = upper.map(|(key, inclusive, _)| (key, inclusive));

        return Some(IndexRangeSpec {
            index_name,
            column_name: column.name.clone(),
            lower,
            upper,
            display,
        });
    }
    None
}

/// Execute a `Filter`/`FilteredScan` predicate over a base table as an index
/// range scan when one qualifies. Candidate rows are fetched by row id in
/// index (== value) order, then the FULL predicate is re-applied — so NULL
/// handling, residual conjuncts, and exact bound semantics are byte-for-byte
/// identical to the scan path the planner would have used.
pub(super) fn try_index_range_scan_for_scan(
    executor: &Executor,
    input: &LogicalPlan,
    predicate: &LogicalExpr,
) -> Result<Option<Box<dyn PhysicalOperator>>> {
    if index_range_fast_path_disabled() {
        return Ok(None);
    }
    let storage = match executor.storage() {
        Some(storage) => storage,
        None => return Ok(None),
    };
    let LogicalPlan::Scan {
        table_name,
        alias,
        schema,
        projection,
        as_of,
    } = input
    else {
        return Ok(None);
    };
    if as_of.is_some() {
        return Ok(None);
    }

    // Detect a qualifying range FIRST: pure in-memory predicate analysis, so
    // queries whose WHERE carries no indexed range bound pay (almost)
    // nothing here. The catalog/MV/transaction gates below involve storage
    // reads and only run once a range was actually found. Detection needs no
    // subquery materialization — only literals, parameters, and casts
    // qualify as bounds.
    let Some(spec) = indexed_range_lookup(storage, table_name, schema.as_ref(), predicate, executor.parameters())
    else {
        return Ok(None);
    };

    if storage.is_branch_active() {
        return Ok(None);
    }
    // Same transaction / snapshot gates as the point-lookup fast path.
    if executor.txn_forces_slow_reads_for_table(table_name) {
        return Ok(None);
    }
    if executor.get_cte(table_name).is_some()
        || storage.mv_catalog().view_exists(table_name)?
        || !storage.catalog().table_exists(table_name)?
    {
        return Ok(None);
    }

    let materialized_predicate = executor.materialize_subqueries(predicate)?;

    let Some(pairs) = guarded_range_pairs(storage, &spec) else {
        return Ok(None);
    };

    // Fetch candidates in ROW-ID order, not index-key order: storage data
    // keys are row-id ordered, so sorted access turns random block reads
    // into near-sequential ones and stops LRU row-cache thrash on large
    // ranges. This path carries no ordering contract — the full predicate is
    // re-applied below, and any ORDER BY is a separate plan node above this
    // scan (the ordered top-k fast path does its own bounded iteration).
    let mut row_ids: Vec<crate::storage::RowId> = pairs.into_iter().map(|(_, row_id)| row_id).collect();
    row_ids.sort_unstable();

    // Adaptive cold-storage abort: point gets are µs-scale when rows sit in
    // the row cache / memtable but can be ms-scale per get on cold blocks
    // (R4.1 keyspace reality: zero bloom selectivity, one shared CF). For
    // large fetch sets, give the fetch a time budget comparable to one
    // sequential scan; if it blows the budget, hand the query back to the
    // scan path (`Ok(None)`), whose cost is bounded by table size. Small
    // fetch sets never abort — their worst case is already bounded. EXPLAIN
    // shows the planned index path; this abort is a runtime fallback only.
    const ABORT_MIN_ROWS: usize = 256;
    const ABORT_BUDGET: std::time::Duration = std::time::Duration::from_millis(100);
    let fetch_started = std::time::Instant::now();

    let mut tuples = Vec::with_capacity(row_ids.len());
    for (i, row_id) in row_ids.iter().enumerate() {
        if row_ids.len() >= ABORT_MIN_ROWS && i & 0x3F == 0x3F && fetch_started.elapsed() > ABORT_BUDGET {
            tracing::debug!(
                "index range scan aborted after {} of {} fetches in {:?} (cold storage); \
                 falling back to sequential scan for '{}' on {} ({})",
                i + 1,
                row_ids.len(),
                fetch_started.elapsed(),
                spec.index_name,
                table_name,
                spec.display,
            );
            return Ok(None);
        }
        if let Some(tuple) = storage.get_row_by_id(table_name, *row_id, schema.as_ref())? {
            tuples.push(tuple);
        }
    }

    let source_name = alias.as_ref().unwrap_or(table_name);
    let actual_schema = Arc::new(schema_with_source(schema.as_ref(), source_name, table_name));
    let tuples = filter_tuples_with_evaluator(
        tuples,
        actual_schema.clone(),
        &materialized_predicate,
        executor.parameters(),
    )?;

    tracing::debug!(
        "index range scan: '{}' on {}.{} ({}) served {} rows",
        spec.index_name,
        table_name,
        spec.column_name,
        spec.display,
        tuples.len(),
    );

    Ok(Some(Box::new(
        ScanOperator::new(
            table_name.clone(),
            actual_schema,
            projection.clone(),
            tuples,
            executor.parameters().to_vec(),
        )
        .with_timeout(executor.timeout_ctx()),
    )))
}

/// Run the bounded index scan for `spec` with the selectivity guard applied:
/// point-fetching more than 25% of the table loses to one sequential scan, so
/// the walk is capped at `total/4 + 1` entries and `None` (= "use the normal
/// scan path") is returned the moment the cap is hit — rejection costs
/// O(total/4), never a full index walk. Shared by the executor fast path and
/// the EXPLAIN annotator so the displayed plan is the executed plan. The kill
/// switch `HELIOS_INDEX_RANGE_OFF` covers pathological cases.
pub(super) fn guarded_range_pairs(
    storage: &crate::storage::StorageEngine,
    spec: &IndexRangeSpec,
) -> Option<Vec<(Vec<u8>, crate::storage::RowId)>> {
    let art = storage.art_indexes();
    let total = art.index_entry_count(&spec.index_name)?;
    let cap = usize::try_from(total / 4 + 1).ok()?;
    let pairs = art.index_range_scan(
        &spec.index_name,
        spec.lower.as_ref().map(|(key, inclusive)| (key.as_slice(), *inclusive)),
        spec.upper.as_ref().map(|(key, inclusive)| (key.as_slice(), *inclusive)),
        Some(cap),
    );
    if total > 0 && (pairs.len() as u64).saturating_mul(4) > total {
        return None;
    }
    Some(pairs)
}

fn indexed_equality_lookup(
    storage: &crate::storage::StorageEngine,
    table_name: &str,
    schema: &Schema,
    predicate: &LogicalExpr,
    parameters: &[Value],
) -> Option<(String, Value)> {
    use crate::sql::BinaryOperator;

    match predicate {
        LogicalExpr::BinaryExpr {
            left,
            op: BinaryOperator::And,
            right,
        } => indexed_equality_lookup(storage, table_name, schema, left, parameters)
            .or_else(|| indexed_equality_lookup(storage, table_name, schema, right, parameters)),
        LogicalExpr::BinaryExpr {
            left,
            op: BinaryOperator::Eq,
            right,
        } => equality_lookup_from_sides(storage, table_name, schema, left, right, parameters)
            .or_else(|| equality_lookup_from_sides(storage, table_name, schema, right, left, parameters)),
        _ => None,
    }
}

fn equality_lookup_from_sides(
    storage: &crate::storage::StorageEngine,
    table_name: &str,
    schema: &Schema,
    column_expr: &LogicalExpr,
    value_expr: &LogicalExpr,
    parameters: &[Value],
) -> Option<(String, Value)> {
    let LogicalExpr::Column { table, name } = column_expr else {
        return None;
    };
    let column_idx = schema
        .get_qualified_column_index(table.as_deref(), name)
        .or_else(|| schema.get_column_index(name))?;
    let column = schema.columns.get(column_idx)?;
    let index_name = storage.art_indexes().find_column_index(table_name, &column.name)?;
    let raw_value = lookup_bound_value(value_expr, parameters)?;
    let lookup_value = coerce_index_lookup_value(raw_value, &column.data_type)?;
    Some((index_name, lookup_value))
}

fn lookup_bound_value(expr: &LogicalExpr, parameters: &[Value]) -> Option<Value> {
    match expr {
        LogicalExpr::Literal(value) => Some(value.clone()),
        LogicalExpr::Parameter { index } if *index > 0 => parameters.get(index - 1).cloned(),
        LogicalExpr::Cast { expr, .. } => lookup_bound_value(expr, parameters),
        _ => None,
    }
}

fn coerce_index_lookup_value(value: Value, data_type: &DataType) -> Option<Value> {
    use crate::{DataType, Value};

    if matches!(value, Value::Null) {
        return Some(Value::Null);
    }

    match data_type {
        DataType::Int2 => value_to_i64(&value)
            .and_then(|value| i16::try_from(value).ok())
            .map(Value::Int2),
        DataType::Int4 => value_to_i64(&value)
            .and_then(|value| i32::try_from(value).ok())
            .map(Value::Int4),
        DataType::Int8 => value_to_i64(&value).map(Value::Int8),
        DataType::Float4 => value_to_f64(&value).map(|value| Value::Float4(value as f32)),
        DataType::Float8 => value_to_f64(&value).map(Value::Float8),
        DataType::Text | DataType::Varchar(_) | DataType::Char(_) => match value {
            Value::String(_) => Some(value),
            _ => None,
        },
        DataType::Boolean => match value {
            Value::Boolean(_) => Some(value),
            Value::String(s) => Some(Value::Boolean(matches!(
                s.as_str(),
                "1" | "true" | "TRUE" | "t" | "yes"
            ))),
            _ => None,
        },
        DataType::Uuid => match value {
            Value::Uuid(_) => Some(value),
            Value::String(s) => uuid::Uuid::parse_str(&s).ok().map(Value::Uuid),
            _ => None,
        },
        DataType::Date => match value {
            Value::Date(_) => Some(value),
            Value::Timestamp(ts) => Some(Value::Date(ts.date_naive())),
            Value::String(s) => parse_date_for_index(&s).map(Value::Date),
            _ => None,
        },
        DataType::Timestamp | DataType::Timestamptz => match value {
            Value::Timestamp(_) => Some(value),
            Value::Date(date) => date
                .and_hms_opt(0, 0, 0)
                .map(|ts| Value::Timestamp(chrono::DateTime::from_naive_utc_and_offset(ts, chrono::Utc))),
            Value::String(s) => parse_timestamp_for_index(&s).map(Value::Timestamp),
            _ => None,
        },
        DataType::Time => match value {
            Value::Time(_) => Some(value),
            Value::String(s) => chrono::NaiveTime::parse_from_str(&s, "%H:%M:%S%.f")
                .ok()
                .map(Value::Time),
            _ => None,
        },
        DataType::Bytea => matches!(value, Value::Bytes(_)).then_some(value),
        DataType::Interval => matches!(value, Value::Interval(_)).then_some(value),
        DataType::Numeric => matches!(value, Value::Numeric(_)).then_some(value),
        DataType::Json | DataType::Jsonb => matches!(value, Value::Json(_)).then_some(value),
        DataType::Vector(_) => matches!(value, Value::Vector(_)).then_some(value),
        DataType::Array(_) => matches!(value, Value::Array(_)).then_some(value),
    }
}

fn value_to_i64(value: &Value) -> Option<i64> {
    match value {
        Value::Int2(value) => Some(i64::from(*value)),
        Value::Int4(value) => Some(i64::from(*value)),
        Value::Int8(value) => Some(*value),
        Value::String(value) => value.parse::<i64>().ok(),
        _ => None,
    }
}

fn value_to_f64(value: &Value) -> Option<f64> {
    match value {
        Value::Int2(value) => Some(f64::from(*value)),
        Value::Int4(value) => Some(f64::from(*value)),
        Value::Int8(value) => Some(*value as f64),
        Value::Float4(value) => Some(f64::from(*value)),
        Value::Float8(value) => Some(*value),
        Value::String(value) => value.parse::<f64>().ok(),
        _ => None,
    }
}

fn parse_timestamp_for_index(value: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    if let Ok(ts) = chrono::DateTime::parse_from_rfc3339(value) {
        return Some(ts.with_timezone(&chrono::Utc));
    }
    if let Ok(ts) = chrono::NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S%.f") {
        return Some(chrono::DateTime::from_naive_utc_and_offset(ts, chrono::Utc));
    }
    if let Ok(date) = chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d") {
        return date
            .and_hms_opt(0, 0, 0)
            .map(|ts| chrono::DateTime::from_naive_utc_and_offset(ts, chrono::Utc));
    }
    None
}

fn parse_date_for_index(value: &str) -> Option<chrono::NaiveDate> {
    chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d")
        .ok()
        .or_else(|| parse_timestamp_for_index(value).map(|ts| ts.date_naive()))
}

pub(super) fn schema_with_source(schema: &Schema, source_name: &str, table_name: &str) -> Schema {
    Schema {
        columns: schema
            .columns
            .iter()
            .cloned()
            .map(|mut col| {
                col.source_table = Some(source_name.to_string());
                col.source_table_name = Some(table_name.to_string());
                col
            })
            .collect(),
    }
}

fn collect_single_table_column_indices(plan: &LogicalPlan) -> Option<(String, Vec<usize>, usize)> {
    let mut cols: HashSet<String> = HashSet::new();
    let mut tables: Vec<(String, Arc<Schema>)> = Vec::new();
    let mut bail = false;
    collect_plan_columns(plan, &mut cols, &mut tables, &mut bail, true);
    if bail {
        return None;
    }
    // Exactly one distinct table (a repeated table implies a join/union, already bailed).
    let mut names: Vec<&str> = tables.iter().map(|(n, _)| n.as_str()).collect();
    names.sort_unstable();
    names.dedup();
    if names.len() != 1 {
        return None;
    }
    let (table_name, schema) = &tables[0];
    let mut indices = Vec::new();
    for c in &cols {
        // A name that doesn't resolve to a base column is a *derived* column — an
        // aggregate output (`agg_0`) or a projection alias referenced by a node above
        // the one that produced it. Its base-column dependencies were already collected
        // at the producing node's expression, so skipping it here is safe (and never
        // under-counts the prefix). Only base columns widen the prefix.
        if let Some(idx) = resolve_col_index(schema, c) {
            indices.push(idx);
        }
    }
    indices.sort_unstable();
    indices.dedup();
    Some((table_name.clone(), indices, schema.columns.len()))
}

fn resolve_col_index(schema: &Schema, name: &str) -> Option<usize> {
    // Match the bare column (after any "table."/"alias." qualifier), case-insensitively
    // so a base-column reference is never mistaken for a derived one.
    let bare = name.rsplit('.').next().unwrap_or(name);
    schema.columns.iter().position(|c| c.name.eq_ignore_ascii_case(bare))
}

fn should_use_prefix_decode(prefix_len: usize, total_columns: usize) -> bool {
    // Prefix decoding has its own branch/deserialization overhead. It wins on
    // COUNT(*) and early-column plans, but measured slower when it only skips a
    // narrow suffix. Keep it conservative until column-width stats can guide this.
    prefix_len < total_columns && prefix_len.saturating_mul(2) <= total_columns
}

fn is_prefix_contiguous(indices: &[usize]) -> bool {
    indices
        .iter()
        .copied()
        .enumerate()
        .all(|(expected, idx)| idx == expected)
}

fn should_use_selected_decode(indices: &[usize], total_columns: usize) -> bool {
    if total_columns == 0 || indices.len() >= total_columns {
        return false;
    }
    if indices.is_empty() {
        return true;
    }

    let skips_leading = indices.first().copied().unwrap_or(0) > 0;
    let skipped_columns = total_columns.saturating_sub(indices.len());
    let sparse = indices.len().saturating_mul(2) <= total_columns;

    skips_leading || sparse || skipped_columns >= 2
}

#[derive(Clone)]
struct ScanTableRef {
    table_name: String,
    qualifiers: HashSet<String>,
    schema: Arc<Schema>,
}

fn collect_scan_tables(plan: &LogicalPlan, tables: &mut Vec<ScanTableRef>, bail: &mut bool) {
    if *bail {
        return;
    }
    match plan {
        LogicalPlan::Scan {
            table_name,
            alias,
            schema,
            ..
        }
        | LogicalPlan::FilteredScan {
            table_name,
            alias,
            schema,
            ..
        } => {
            let mut qualifiers = HashSet::new();
            qualifiers.insert(table_name.to_ascii_lowercase());
            if let Some(alias) = alias {
                qualifiers.insert(alias.to_ascii_lowercase());
            }
            tables.push(ScanTableRef {
                table_name: table_name.clone(),
                qualifiers,
                schema: schema.clone(),
            });
        }
        LogicalPlan::Filter { input, .. }
        | LogicalPlan::Project { input, .. }
        | LogicalPlan::Aggregate { input, .. }
        | LogicalPlan::Sort { input, .. }
        | LogicalPlan::Limit { input, .. } => collect_scan_tables(input, tables, bail),
        LogicalPlan::Join {
            left, right, lateral, ..
        } => {
            if *lateral {
                *bail = true;
                return;
            }
            collect_scan_tables(left, tables, bail);
            collect_scan_tables(right, tables, bail);
        }
        _ => *bail = true,
    }
}

fn table_qualifiers_are_unique(tables: &[ScanTableRef]) -> bool {
    let mut table_names = HashSet::new();
    let mut qualifiers = HashSet::new();
    for table in tables {
        if !table_names.insert(table.table_name.to_ascii_lowercase()) {
            return false;
        }
        for qualifier in &table.qualifiers {
            if !qualifiers.insert(qualifier.clone()) {
                return false;
            }
        }
    }
    true
}

fn collect_plan_columns_by_table(
    plan: &LogicalPlan,
    tables: &[ScanTableRef],
    needed: &mut [HashSet<usize>],
    bail: &mut bool,
    output_required: bool,
) {
    if *bail {
        return;
    }
    match plan {
        LogicalPlan::Scan {
            table_name,
            schema,
            projection,
            ..
        } => {
            if let Some(table_idx) = find_scan_table_index(tables, table_name) {
                collect_projection_indices(schema, projection.as_ref(), &mut needed[table_idx], bail);
                if output_required && projection.is_none() {
                    collect_all_schema_indices(schema, &mut needed[table_idx]);
                }
            } else {
                *bail = true;
            }
        }
        LogicalPlan::FilteredScan {
            table_name,
            schema,
            projection,
            predicate,
            ..
        } => {
            if let Some(table_idx) = find_scan_table_index(tables, table_name) {
                collect_projection_indices(schema, projection.as_ref(), &mut needed[table_idx], bail);
                if output_required && projection.is_none() {
                    collect_all_schema_indices(schema, &mut needed[table_idx]);
                }
                if let Some(predicate) = predicate {
                    collect_expr_columns_by_table(predicate, tables, needed, bail);
                }
            } else {
                *bail = true;
            }
        }
        LogicalPlan::Filter { input, predicate } => {
            collect_expr_columns_by_table(predicate, tables, needed, bail);
            collect_plan_columns_by_table(input, tables, needed, bail, output_required);
        }
        LogicalPlan::Project {
            input,
            exprs,
            distinct_on,
            ..
        } => {
            for expr in exprs {
                collect_expr_columns_by_table(expr, tables, needed, bail);
            }
            if let Some(distinct_on) = distinct_on {
                for expr in distinct_on {
                    collect_expr_columns_by_table(expr, tables, needed, bail);
                }
            }
            collect_plan_columns_by_table(input, tables, needed, bail, false);
        }
        LogicalPlan::Aggregate {
            input,
            group_by,
            aggr_exprs,
            having,
        } => {
            for expr in group_by {
                collect_expr_columns_by_table(expr, tables, needed, bail);
            }
            for expr in aggr_exprs {
                collect_expr_columns_by_table(expr, tables, needed, bail);
            }
            if let Some(having) = having {
                collect_expr_columns_by_table(having, tables, needed, bail);
            }
            collect_plan_columns_by_table(input, tables, needed, bail, false);
        }
        LogicalPlan::Sort { input, exprs, .. } => {
            for expr in exprs {
                collect_expr_columns_by_table(expr, tables, needed, bail);
            }
            collect_plan_columns_by_table(input, tables, needed, bail, output_required);
        }
        LogicalPlan::Limit { input, .. } => collect_plan_columns_by_table(input, tables, needed, bail, output_required),
        LogicalPlan::Join {
            left,
            right,
            on,
            lateral,
            ..
        } => {
            if *lateral {
                *bail = true;
                return;
            }
            if let Some(on) = on {
                collect_expr_columns_by_table(on, tables, needed, bail);
            }
            collect_plan_columns_by_table(left, tables, needed, bail, output_required);
            collect_plan_columns_by_table(right, tables, needed, bail, output_required);
        }
        _ => *bail = true,
    }
}

fn find_scan_table_index(tables: &[ScanTableRef], table_name: &str) -> Option<usize> {
    let table_name = table_name.to_ascii_lowercase();
    tables
        .iter()
        .position(|table| table.table_name.eq_ignore_ascii_case(&table_name))
}

fn collect_projection_indices(
    schema: &Schema,
    projection: Option<&Vec<usize>>,
    needed: &mut HashSet<usize>,
    bail: &mut bool,
) {
    if let Some(indices) = projection {
        for &idx in indices {
            if idx < schema.columns.len() {
                needed.insert(idx);
            } else {
                *bail = true;
                return;
            }
        }
    }
}

fn collect_all_schema_indices(schema: &Schema, needed: &mut HashSet<usize>) {
    needed.extend(0..schema.columns.len());
}

fn collect_expr_columns_by_table(
    expr: &LogicalExpr,
    tables: &[ScanTableRef],
    needed: &mut [HashSet<usize>],
    bail: &mut bool,
) {
    if *bail {
        return;
    }
    match expr {
        LogicalExpr::Column { table, name } => {
            if let Some((table_idx, column_idx)) = resolve_table_column(tables, table.as_deref(), name, bail) {
                needed[table_idx].insert(column_idx);
            }
        }
        LogicalExpr::NewRow { .. } | LogicalExpr::OldRow { .. } => *bail = true,
        // Physical-only node (R3.5): never present in plan expressions, which
        // is what this collector walks. Bail to a full decode if ever seen.
        LogicalExpr::BoundColumn { .. } => *bail = true,
        LogicalExpr::Wildcard => {
            for (table_idx, table) in tables.iter().enumerate() {
                collect_all_schema_indices(&table.schema, &mut needed[table_idx]);
            }
        }
        LogicalExpr::ScalarSubquery { .. } | LogicalExpr::InSubquery { .. } | LogicalExpr::Exists { .. } => {
            *bail = true;
        }
        LogicalExpr::BinaryExpr { left, right, .. } => {
            collect_expr_columns_by_table(left, tables, needed, bail);
            collect_expr_columns_by_table(right, tables, needed, bail);
        }
        LogicalExpr::UnaryExpr { expr, .. } => collect_expr_columns_by_table(expr, tables, needed, bail),
        LogicalExpr::AggregateFunction {
            fun: crate::sql::logical_plan::AggregateFunction::Count,
            args,
            ..
        } if args.iter().all(|arg| matches!(arg, LogicalExpr::Wildcard)) => {}
        LogicalExpr::AggregateFunction { args, .. } | LogicalExpr::ScalarFunction { args, .. } => {
            for arg in args {
                collect_expr_columns_by_table(arg, tables, needed, bail);
            }
        }
        LogicalExpr::Case {
            expr,
            when_then,
            else_result,
        } => {
            if let Some(expr) = expr {
                collect_expr_columns_by_table(expr, tables, needed, bail);
            }
            for (when, then) in when_then {
                collect_expr_columns_by_table(when, tables, needed, bail);
                collect_expr_columns_by_table(then, tables, needed, bail);
            }
            if let Some(expr) = else_result {
                collect_expr_columns_by_table(expr, tables, needed, bail);
            }
        }
        LogicalExpr::Cast { expr, .. } => collect_expr_columns_by_table(expr, tables, needed, bail),
        LogicalExpr::IsNull { expr, .. } => collect_expr_columns_by_table(expr, tables, needed, bail),
        LogicalExpr::Between { expr, low, high, .. } => {
            collect_expr_columns_by_table(expr, tables, needed, bail);
            collect_expr_columns_by_table(low, tables, needed, bail);
            collect_expr_columns_by_table(high, tables, needed, bail);
        }
        LogicalExpr::InList { expr, list, .. } => {
            collect_expr_columns_by_table(expr, tables, needed, bail);
            for item in list {
                collect_expr_columns_by_table(item, tables, needed, bail);
            }
        }
        LogicalExpr::InSet { expr, .. } => collect_expr_columns_by_table(expr, tables, needed, bail),
        LogicalExpr::ArraySubscript { array, index } => {
            collect_expr_columns_by_table(array, tables, needed, bail);
            collect_expr_columns_by_table(index, tables, needed, bail);
        }
        LogicalExpr::WindowFunction {
            args,
            partition_by,
            order_by,
            ..
        } => {
            for arg in args {
                collect_expr_columns_by_table(arg, tables, needed, bail);
            }
            for expr in partition_by {
                collect_expr_columns_by_table(expr, tables, needed, bail);
            }
            for (expr, _) in order_by {
                collect_expr_columns_by_table(expr, tables, needed, bail);
            }
        }
        LogicalExpr::Tuple { items } => {
            for item in items {
                collect_expr_columns_by_table(item, tables, needed, bail);
            }
        }
        LogicalExpr::Literal(_) | LogicalExpr::Parameter { .. } | LogicalExpr::DefaultValue => {}
    }
}

fn resolve_table_column(
    tables: &[ScanTableRef],
    qualifier: Option<&str>,
    name: &str,
    bail: &mut bool,
) -> Option<(usize, usize)> {
    if let Some(qualifier) = qualifier {
        let qualifier = qualifier.to_ascii_lowercase();
        let Some(table_idx) = tables.iter().position(|table| table.qualifiers.contains(&qualifier)) else {
            // Derived or outer-reference column; no base table decode needed here.
            return None;
        };
        if let Some(column_idx) = resolve_col_index(&tables[table_idx].schema, name) {
            return Some((table_idx, column_idx));
        }
        *bail = true;
        return None;
    }

    let mut found = None;
    for (table_idx, table) in tables.iter().enumerate() {
        if let Some(column_idx) = resolve_col_index(&table.schema, name) {
            if found.is_some() {
                *bail = true;
                return None;
            }
            found = Some((table_idx, column_idx));
        }
    }
    found
}

fn collect_plan_columns(
    plan: &LogicalPlan,
    cols: &mut HashSet<String>,
    tables: &mut Vec<(String, Arc<Schema>)>,
    bail: &mut bool,
    output_required: bool,
) {
    if *bail {
        return;
    }
    match plan {
        LogicalPlan::Scan {
            table_name,
            schema,
            projection,
            ..
        } => {
            tables.push((table_name.clone(), schema.clone()));
            collect_projection_columns(schema, projection.as_ref(), cols, bail);
            if output_required && projection.is_none() {
                collect_all_schema_columns(schema, cols);
            }
        }
        LogicalPlan::FilteredScan {
            table_name,
            schema,
            projection,
            predicate,
            ..
        } => {
            tables.push((table_name.clone(), schema.clone()));
            collect_projection_columns(schema, projection.as_ref(), cols, bail);
            if output_required && projection.is_none() {
                collect_all_schema_columns(schema, cols);
            }
            if let Some(p) = predicate {
                collect_expr_columns(p, cols, bail);
            }
        }
        LogicalPlan::Filter { input, predicate } => {
            collect_expr_columns(predicate, cols, bail);
            collect_plan_columns(input, cols, tables, bail, output_required);
        }
        LogicalPlan::Project {
            input,
            exprs,
            distinct_on,
            ..
        } => {
            for e in exprs {
                collect_expr_columns(e, cols, bail);
            }
            if let Some(d) = distinct_on {
                for e in d {
                    collect_expr_columns(e, cols, bail);
                }
            }
            collect_plan_columns(input, cols, tables, bail, false);
        }
        LogicalPlan::Aggregate {
            input,
            group_by,
            aggr_exprs,
            having,
        } => {
            for e in group_by {
                collect_expr_columns(e, cols, bail);
            }
            for e in aggr_exprs {
                collect_expr_columns(e, cols, bail);
            }
            if let Some(h) = having {
                collect_expr_columns(h, cols, bail);
            }
            collect_plan_columns(input, cols, tables, bail, false);
        }
        LogicalPlan::Sort { input, exprs, .. } => {
            for e in exprs {
                collect_expr_columns(e, cols, bail);
            }
            collect_plan_columns(input, cols, tables, bail, output_required);
        }
        LogicalPlan::Limit { input, .. } => collect_plan_columns(input, cols, tables, bail, output_required),
        // Joins, set ops, DML, and anything else: don't optimize (full decode).
        _ => *bail = true,
    }
}

fn collect_all_schema_columns(schema: &Schema, cols: &mut HashSet<String>) {
    for col in &schema.columns {
        cols.insert(col.name.clone());
    }
}

fn collect_projection_columns(
    schema: &Schema,
    projection: Option<&Vec<usize>>,
    cols: &mut HashSet<String>,
    bail: &mut bool,
) {
    if let Some(indices) = projection {
        for &idx in indices {
            match schema.columns.get(idx) {
                Some(col) => {
                    cols.insert(col.name.clone());
                }
                None => {
                    *bail = true;
                    return;
                }
            }
        }
    }
}

fn collect_expr_columns(expr: &LogicalExpr, cols: &mut HashSet<String>, bail: &mut bool) {
    if *bail {
        return;
    }
    match expr {
        LogicalExpr::Column { name, .. } | LogicalExpr::BoundColumn { name, .. } => {
            cols.insert(name.clone());
        }
        LogicalExpr::NewRow { column } | LogicalExpr::OldRow { column } => {
            cols.insert(column.clone());
        }
        // Reading every column / correlated columns we can't bound → full decode.
        LogicalExpr::Wildcard
        | LogicalExpr::ScalarSubquery { .. }
        | LogicalExpr::InSubquery { .. }
        | LogicalExpr::Exists { .. } => *bail = true,
        LogicalExpr::BinaryExpr { left, right, .. } => {
            collect_expr_columns(left, cols, bail);
            collect_expr_columns(right, cols, bail);
        }
        LogicalExpr::UnaryExpr { expr, .. } => collect_expr_columns(expr, cols, bail),
        LogicalExpr::AggregateFunction {
            fun: crate::sql::logical_plan::AggregateFunction::Count,
            args,
            ..
        } if args.iter().all(|arg| matches!(arg, LogicalExpr::Wildcard)) => {}
        LogicalExpr::AggregateFunction { args, .. } | LogicalExpr::ScalarFunction { args, .. } => {
            for a in args {
                collect_expr_columns(a, cols, bail);
            }
        }
        LogicalExpr::Case {
            expr,
            when_then,
            else_result,
        } => {
            if let Some(e) = expr {
                collect_expr_columns(e, cols, bail);
            }
            for (w, t) in when_then {
                collect_expr_columns(w, cols, bail);
                collect_expr_columns(t, cols, bail);
            }
            if let Some(e) = else_result {
                collect_expr_columns(e, cols, bail);
            }
        }
        LogicalExpr::Cast { expr, .. } => collect_expr_columns(expr, cols, bail),
        LogicalExpr::IsNull { expr, .. } => collect_expr_columns(expr, cols, bail),
        LogicalExpr::Between { expr, low, high, .. } => {
            collect_expr_columns(expr, cols, bail);
            collect_expr_columns(low, cols, bail);
            collect_expr_columns(high, cols, bail);
        }
        LogicalExpr::InList { expr, list, .. } => {
            collect_expr_columns(expr, cols, bail);
            for i in list {
                collect_expr_columns(i, cols, bail);
            }
        }
        LogicalExpr::InSet { expr, .. } => collect_expr_columns(expr, cols, bail),
        LogicalExpr::ArraySubscript { array, index } => {
            collect_expr_columns(array, cols, bail);
            collect_expr_columns(index, cols, bail);
        }
        LogicalExpr::WindowFunction {
            args,
            partition_by,
            order_by,
            ..
        } => {
            for a in args {
                collect_expr_columns(a, cols, bail);
            }
            for p in partition_by {
                collect_expr_columns(p, cols, bail);
            }
            for (o, _) in order_by {
                collect_expr_columns(o, cols, bail);
            }
        }
        LogicalExpr::Tuple { items } => {
            for i in items {
                collect_expr_columns(i, cols, bail);
            }
        }
        LogicalExpr::Literal(_) | LogicalExpr::Parameter { .. } | LogicalExpr::DefaultValue => {}
    }
}

/// Tuple source for [`ScanOperator`] (R3.5 item 5).
///
/// `Owned` is the historical mode: the operator owns the materialized rows
/// and serves them destructively (`mem::take`, zero copies). `Shared` serves
/// an `Arc`-shared materialization (CTE results) by cloning each row as it
/// is emitted — same total clone count as the old upfront
/// `cte_data.tuples.clone()` when fully consumed, but no second resident
/// copy, nothing cloned for rows a LIMIT/TopK never pulls, and N references
/// to the same CTE share one materialization.
enum ScanTuples {
    Owned(Vec<Tuple>),
    Shared(Arc<Vec<Tuple>>),
}

impl ScanTuples {
    fn len(&self) -> usize {
        match self {
            Self::Owned(tuples) => tuples.len(),
            Self::Shared(tuples) => tuples.len(),
        }
    }

    /// Serve the tuple at `index`: move it out of an owned source, clone it
    /// from a shared one.
    fn serve(&mut self, index: usize) -> Option<Tuple> {
        match self {
            Self::Owned(tuples) => tuples.get_mut(index).map(std::mem::take),
            Self::Shared(tuples) => tuples.get(index).cloned(),
        }
    }
}

/// Table scan operator
///
/// Reads tuples from a table.
pub struct ScanOperator {
    table_name: String,
    schema: Arc<Schema>,
    projection: Option<Vec<usize>>,
    projection_move_max_index: Option<usize>,
    tuples: ScanTuples,
    current_index: usize,
    timeout_ctx: Option<TimeoutContext>,
    #[allow(dead_code)]
    parameters: Vec<crate::Value>,
}

impl ScanOperator {
    pub fn new(
        table_name: String,
        schema: Arc<Schema>,
        projection: Option<Vec<usize>>,
        tuples: Vec<Tuple>,
        parameters: Vec<crate::Value>,
    ) -> Self {
        Self::with_source(table_name, schema, projection, ScanTuples::Owned(tuples), parameters)
    }

    /// Construct over an `Arc`-shared materialization without deep-cloning it
    /// (R3.5 item 5; used for CTE references).
    pub(super) fn new_shared(
        table_name: String,
        schema: Arc<Schema>,
        projection: Option<Vec<usize>>,
        tuples: Arc<Vec<Tuple>>,
        parameters: Vec<crate::Value>,
    ) -> Self {
        Self::with_source(table_name, schema, projection, ScanTuples::Shared(tuples), parameters)
    }

    fn with_source(
        table_name: String,
        schema: Arc<Schema>,
        projection: Option<Vec<usize>>,
        tuples: ScanTuples,
        parameters: Vec<crate::Value>,
    ) -> Self {
        let projection_move_max_index = projection
            .as_deref()
            .and_then(|indices| projection_move_max_index(indices, schema.columns.len()));
        Self {
            table_name,
            schema,
            projection,
            projection_move_max_index,
            tuples,
            current_index: 0,
            timeout_ctx: None,
            parameters,
        }
    }

    pub fn with_timeout(mut self, timeout_ctx: Option<TimeoutContext>) -> Self {
        self.timeout_ctx = timeout_ctx;
        self
    }
}

impl PhysicalOperator for ScanOperator {
    fn next(&mut self) -> Result<Option<Tuple>> {
        // Check timeout before processing
        if let Some(ref ctx) = self.timeout_ctx {
            ctx.check_timeout()?;
        }

        if self.current_index >= self.tuples.len() {
            return Ok(None);
        }

        let mut tuple = self
            .tuples
            .serve(self.current_index)
            .ok_or_else(|| Error::query_execution("Scan index out of bounds"))?;
        self.current_index += 1;

        // Apply projection if specified
        if let Some(indices) = &self.projection {
            let projected_values = if self
                .projection_move_max_index
                .is_some_and(|max_idx| max_idx < tuple.values.len())
            {
                let mut values = Vec::with_capacity(indices.len());
                for &idx in indices {
                    values.push(std::mem::replace(&mut tuple.values[idx], Value::Null));
                }
                values
            } else {
                indices.iter().filter_map(|&i| tuple.get(i).cloned()).collect()
            };
            let mut projected_tuple = Tuple::new(projected_values);
            // Preserve row_id through projection for DML operations
            projected_tuple.row_id = tuple.row_id;
            Ok(Some(projected_tuple))
        } else {
            Ok(Some(tuple))
        }
    }

    fn schema(&self) -> Arc<Schema> {
        if let Some(indices) = &self.projection {
            let columns: Vec<_> = indices
                .iter()
                .filter_map(|&i| self.schema.columns.get(i).cloned())
                .collect();
            Arc::new(Schema { columns })
        } else {
            self.schema.clone()
        }
    }
}

fn projection_move_max_index(indices: &[usize], schema_len: usize) -> Option<usize> {
    let mut max_idx: Option<usize> = None;
    for (pos, &idx) in indices.iter().enumerate() {
        if idx >= schema_len || indices[..pos].contains(&idx) {
            return None;
        }
        max_idx = Some(max_idx.map_or(idx, |max| max.max(idx)));
    }
    max_idx
}

/// Vector similarity search operator (k-NN search using HNSW index)
///
/// Performs efficient nearest neighbor search using HNSW indexes.
/// This operator is used when a query has the pattern:
/// ```sql
/// SELECT * FROM table ORDER BY embedding <-> query_vector LIMIT k
/// ```
pub struct VectorScanOperator {
    table_name: String,
    schema: Arc<Schema>,
    /// Pre-computed k-NN results (row_id, distance)
    results: Vec<(u64, f32)>,
    /// Full tuples from storage
    tuples: Vec<Tuple>,
    /// Current iteration index
    current_index: usize,
    /// Optional pre-filter predicate.  When set, tuples are tested
    /// BEFORE being emitted — callers that want "semantic pre-filter
    /// before the vector search" semantics over-fetch candidates and
    /// let this rejection step drop the ones that don't qualify.
    ///
    /// `None` = no pre-filter (equivalent to the pre-3.17.1 behaviour).
    prefilter: Option<crate::sql::LogicalExpr>,
    /// Cached evaluator used to apply `prefilter` to each tuple.
    /// Built lazily on first `next()` so operator construction stays
    /// cheap.
    evaluator: Option<crate::sql::Evaluator>,
}

impl VectorScanOperator {
    /// Create a new vector scan operator.  No pre-filter.
    pub fn new(table_name: String, schema: Arc<Schema>, results: Vec<(u64, f32)>, tuples: Vec<Tuple>) -> Self {
        Self {
            table_name,
            schema,
            results,
            tuples,
            current_index: 0,
            prefilter: None,
            evaluator: None,
        }
    }

    /// Construct with an optional pre-filter predicate.  The expected
    /// usage pattern is: the caller asks the upstream HNSW search
    /// for `over_fetch_multiplier × k` candidates, hands them to
    /// this operator, and lets `prefilter` drop the ones that fail
    /// the scalar predicate.  Composes cleanly with `LIMIT k`
    /// downstream to guarantee the correct final count.
    pub fn with_prefilter(mut self, predicate: crate::sql::LogicalExpr) -> Self {
        self.prefilter = Some(predicate);
        self
    }

    /// Get the distance for the current tuple (if available).
    #[allow(dead_code)]
    pub fn current_distance(&self) -> Option<f32> {
        if self.current_index > 0 && self.current_index <= self.results.len() {
            self.results.get(self.current_index - 1).map(|r| r.1)
        } else {
            None
        }
    }
}

impl PhysicalOperator for VectorScanOperator {
    fn next(&mut self) -> Result<Option<Tuple>> {
        loop {
            if self.current_index >= self.tuples.len() {
                return Ok(None);
            }
            let tuple = self
                .tuples
                .get(self.current_index)
                .cloned()
                .ok_or_else(|| Error::query_execution("Vector scan index out of bounds"))?;
            self.current_index += 1;
            // Fast path: no pre-filter.
            let Some(pred) = &self.prefilter else {
                return Ok(Some(tuple));
            };
            if self.evaluator.is_none() {
                self.evaluator = Some(crate::sql::Evaluator::new(self.schema.clone()));
            }
            let pass = match self.evaluator.as_ref() {
                Some(ev) => match ev.evaluate(pred, &tuple) {
                    Ok(crate::Value::Boolean(b)) => b,
                    Ok(_) => false,
                    Err(_) => false,
                },
                None => true,
            };
            if pass {
                return Ok(Some(tuple));
            }
            // Otherwise loop — drop the tuple and try the next one.
        }
    }

    fn schema(&self) -> Arc<Schema> {
        self.schema.clone()
    }
}

/// Materialized operator
///
/// Holds pre-computed tuples in memory, useful for system views and subqueries.
/// Similar to ScanOperator but without table_name or projection support.
pub struct MaterializedOperator {
    schema: Arc<Schema>,
    tuples: Vec<Tuple>,
    current_index: usize,
}

impl MaterializedOperator {
    /// Create a new materialized operator with pre-computed tuples
    pub fn new(tuples: Vec<Tuple>, schema: Arc<Schema>) -> Self {
        Self {
            schema,
            tuples,
            current_index: 0,
        }
    }
}

impl PhysicalOperator for MaterializedOperator {
    fn next(&mut self) -> Result<Option<Tuple>> {
        if self.current_index >= self.tuples.len() {
            return Ok(None);
        }

        let tuple = std::mem::take(
            self.tuples
                .get_mut(self.current_index)
                .ok_or_else(|| Error::query_execution("Materialized index out of bounds"))?,
        );
        self.current_index += 1;

        Ok(Some(tuple))
    }

    fn schema(&self) -> Arc<Schema> {
        self.schema.clone()
    }
}

/// Handle Scan logical plan node
pub(super) fn handle_scan(executor: &Executor, plan: &LogicalPlan) -> Result<Box<dyn PhysicalOperator>> {
    if let LogicalPlan::Scan {
        table_name,
        alias,
        schema: _plan_schema,
        projection,
        as_of,
    } = plan
    {
        // Use alias for column source_table (for JOIN disambiguation), fallback to table_name
        let source_name = alias.as_ref().unwrap_or(table_name);
        // First, check if this table name is a CTE reference
        if let Some(cte_data) = executor.get_cte(table_name) {
            // Return the materialized CTE data
            let mut schema_with_source = (*cte_data.schema).clone();
            for col in &mut schema_with_source.columns {
                col.source_table = Some(source_name.clone());
                col.source_table_name = Some(table_name.clone());
            }

            // R3.5 item 5: serve the Arc-shared materialization — no deep
            // clone of the whole CTE result per reference.
            return Ok(Box::new(
                ScanOperator::new_shared(
                    table_name.clone(),
                    Arc::new(schema_with_source),
                    projection.clone(),
                    Arc::clone(&cte_data.tuples),
                    executor.parameters().to_vec(),
                )
                .with_timeout(executor.timeout_ctx()),
            ));
        }

        // KanttBan #22 (v3.31.0): system-view source (pg_namespace,
        // pg_class, pg_attribute, …). The planner rewrites
        // `pg_catalog.<view>` → `<view>` and emits Scan; we materialise
        // the rows from the Phase 3 registry here so Project / Filter /
        // Join compose on top exactly like a user table.
        use crate::sql::phase3::SystemViewRegistry;
        let registry = SystemViewRegistry::shared();
        if registry.is_system_view(table_name) {
            let storage = executor
                .storage()
                .ok_or_else(|| Error::query_execution("system view requires storage context".to_string()))?;
            let mut schema = registry
                .get_schema(table_name)
                .cloned()
                .unwrap_or_else(|| Schema { columns: vec![] });
            for col in &mut schema.columns {
                col.source_table = Some(source_name.clone());
                col.source_table_name = Some(table_name.clone());
            }
            let tuples = registry.execute(table_name, storage)?;
            return Ok(Box::new(
                ScanOperator::new(
                    table_name.clone(),
                    Arc::new(schema),
                    projection.clone(),
                    tuples,
                    executor.parameters().to_vec(),
                )
                .with_timeout(executor.timeout_ctx()),
            ));
        }

        // Fetch actual schema from storage and scan table
        let (actual_schema, tuples) = if let Some(storage) = executor.storage() {
            let catalog = storage.catalog();
            let mv_catalog = storage.mv_catalog();

            // First check if it's a materialized view
            // We need to do this first because MVs are stored in __mv_<name> tables
            let (schema, actual_table_name) = if mv_catalog.view_exists(table_name)? {
                let mv_metadata = mv_catalog.get_view(table_name)?;
                let mv_data_table = crate::storage::MaterializedViewCatalog::mv_data_table_name(table_name);

                // Check if MV data table exists (view has been refreshed)
                if !catalog.table_exists(&mv_data_table)? {
                    return Err(Error::query_execution(format!(
                        "Materialized view '{}' exists but has never been refreshed. Run: REFRESH MATERIALIZED VIEW {}",
                        table_name, table_name
                    )));
                }

                (mv_metadata.schema, mv_data_table)
            } else {
                // Not an MV, try regular table
                match catalog.get_table_schema(table_name) {
                    Ok(schema) => (schema, table_name.clone()),
                    Err(e) => return Err(e),
                }
            };

            // Handle time-travel or transactional queries
            let tuples = if let Some(txn) = executor.transaction() {
                // Transactional scan: read at transaction's snapshot
                let base_tuples = storage.scan_table_at_snapshot(&actual_table_name, txn.snapshot_id())?;

                // Merge with write set from transaction for read-your-own-writes
                txn.merge_with_write_set(&actual_table_name, base_tuples)?
            } else if let Some(as_of_clause) = as_of {
                // P0#1: AS OF / historical queries require version history.
                // With time_travel_enabled=false the commit path writes no
                // versions, so honoring AS OF would silently return current
                // state. Error clearly instead of returning a wrong answer.
                if !storage.time_travel_enabled() {
                    return Err(crate::Error::query_execution(
                        "AS OF / time-travel queries require time_travel_enabled = true",
                    ));
                }
                tracing::debug!(
                    "Time-travel query on table '{}' (actual: '{}') with AS OF clause: {:?}",
                    table_name,
                    actual_table_name,
                    as_of_clause
                );

                let snapshot_mgr = storage.snapshot_manager();

                // Handle VERSIONS BETWEEN separately - returns all versions in range
                if let crate::sql::logical_plan::AsOfClause::VersionsBetween { start, end } = as_of_clause {
                    tracing::debug!("VERSIONS BETWEEN query: start={:?}, end={:?}", start, end);

                    // Resolve start and end to internal LSN timestamps for version lookup
                    let start_ts = snapshot_mgr.resolve_timestamp_for_range(start, true)?;
                    let end_ts = snapshot_mgr.resolve_timestamp_for_range(end, false)?;

                    tracing::debug!("Resolved VERSIONS BETWEEN timestamps: {} to {}", start_ts, end_ts);

                    // R4.3: pin the range start so version GC cannot prune
                    // inside the range while it is being scanned.
                    let _gc_pin = storage.pin_historical_snapshot(start_ts)?;

                    // Scan all versions in range
                    let versions = snapshot_mgr.scan_versions_between(&actual_table_name, start_ts, end_ts)?;

                    tracing::debug!(
                        "VERSIONS BETWEEN scan returned {} versions from table '{}'",
                        versions.len(),
                        table_name
                    );

                    // Convert raw version bytes to tuples (RocksDB handles decompression at block level)
                    let mut tuples = Vec::with_capacity(versions.len());
                    for (row_id, timestamp, value_bytes) in versions {
                        // Deserialize tuple directly (RocksDB LZ4 handles decompression)
                        match bincode::deserialize::<crate::Tuple>(&value_bytes) {
                            Ok(mut tuple) => {
                                tuple.row_id = Some(row_id);
                                tuples.push(tuple);
                            }
                            Err(e) => {
                                tracing::warn!(
                                    "Failed to deserialize version at row_id={}, timestamp={}: {} (data len={})",
                                    row_id,
                                    timestamp,
                                    e,
                                    value_bytes.len()
                                );
                            }
                        }
                    }

                    tuples
                } else {
                    // Regular AS OF query - single point in time
                    // Resolve AS OF clause to snapshot timestamp
                    // Supports: AS OF TIMESTAMP '...', AS OF TRANSACTION <id>, AS OF SCN <id>
                    let snapshot_ts = snapshot_mgr.resolve_as_of(as_of_clause).map_err(|e| {
                        tracing::error!(
                            "Failed to resolve AS OF clause {:?} for table '{}': {}",
                            as_of_clause,
                            table_name,
                            e
                        );
                        e
                    })?;

                    tracing::debug!(
                        "Resolved AS OF clause to snapshot timestamp {} for table '{}'",
                        snapshot_ts,
                        table_name
                    );

                    // R4.3: pin the snapshot so the version GC cannot
                    // advance past it while this statement reads history.
                    let _gc_pin = storage.pin_historical_snapshot(snapshot_ts)?;

                    // Scan at historical snapshot (use actual_table_name for MV support)
                    let result = storage.scan_table_at_snapshot(&actual_table_name, snapshot_ts)?;

                    tracing::debug!(
                        "Time-travel scan returned {} tuples from table '{}' at snapshot {}",
                        result.len(),
                        table_name,
                        snapshot_ts
                    );

                    result
                }
            } else {
                // Normal scan (current data) with branch isolation.
                // Use actual_table_name to support materialized views.
                // Pass pre-fetched schema to avoid duplicate lookup inside scan_table.
                // Issue #1 follow-up: when the executor's needed-column analysis is
                // certain (single regular table, no wildcard/subquery), decode only the
                // leading columns and skip the costly tail.
                let decode_hint = executor.scan_decode_hint_for(table_name);
                if actual_table_name == *table_name {
                    if let Some(columns) = columnar_scan_columns(&schema, projection.as_ref(), decode_hint) {
                        storage.scan_table_branch_aware_with_schema_columnar_columns(
                            &actual_table_name,
                            &schema,
                            &columns,
                        )?
                    } else {
                        match decode_hint {
                            Some(ScanDecodeHint::Prefix(prefix_len)) => storage
                                .scan_table_branch_aware_with_schema_prefix(&actual_table_name, &schema, *prefix_len)?,
                            Some(ScanDecodeHint::Columns(columns)) => storage
                                .scan_table_branch_aware_with_schema_columns(&actual_table_name, &schema, columns)?,
                            _ => storage.scan_table_branch_aware_with_schema(&actual_table_name, &schema)?,
                        }
                    }
                } else {
                    storage.scan_table_branch_aware_with_schema(&actual_table_name, &schema)?
                }
            };

            // Set source_table (alias) and source_table_name (actual) on each column for JOIN disambiguation
            // This allows both `e.name` (alias) and `employees.name` (full name) syntax in queries
            let schema_with_source = Schema {
                columns: schema
                    .columns
                    .into_iter()
                    .map(|mut col| {
                        col.source_table = Some(source_name.clone());
                        col.source_table_name = Some(table_name.clone());
                        col
                    })
                    .collect(),
            };
            (Arc::new(schema_with_source), tuples)
        } else {
            // No storage, use placeholder schema from plan
            (_plan_schema.clone(), Vec::new())
        };

        Ok(Box::new(
            ScanOperator::new(
                table_name.clone(),
                actual_schema,
                projection.clone(),
                tuples,
                executor.parameters().to_vec(),
            )
            .with_timeout(executor.timeout_ctx()),
        ))
    } else {
        Err(Error::query_execution("Expected Scan plan node"))
    }
}

/// Handle FilteredScan logical plan node
///
/// This handles scans with storage-level predicate pushdown, using bloom filters,
/// zone maps, and SIMD-accelerated filtering for improved performance.
pub(super) fn handle_filtered_scan(executor: &Executor, plan: &LogicalPlan) -> Result<Box<dyn PhysicalOperator>> {
    if let LogicalPlan::FilteredScan {
        table_name,
        alias,
        schema: _plan_schema,
        projection,
        predicate,
        as_of,
    } = plan
    {
        // Use alias for column source_table (for JOIN disambiguation), fallback to table_name
        let source_name = alias.as_ref().unwrap_or(table_name);
        let materialized_predicate = predicate
            .as_ref()
            .map(|pred| executor.materialize_subqueries(pred))
            .transpose()?;

        // First, check if this table name is a CTE reference
        if let Some(cte_data) = executor.get_cte(table_name) {
            // Return the materialized CTE data with filter applied
            let mut schema_with_source = (*cte_data.schema).clone();
            for col in &mut schema_with_source.columns {
                col.source_table = Some(source_name.clone());
                col.source_table_name = Some(table_name.clone());
            }

            // R3.5 item 5: filter against the Arc-shared materialization,
            // cloning only the rows that pass the predicate; without a
            // predicate, serve the shared materialization directly.
            let schema_arc = Arc::new(schema_with_source);
            let scan_op: Box<dyn PhysicalOperator> = if let Some(pred) = &materialized_predicate {
                let tuples = filter_shared_tuples_with_evaluator(
                    &cte_data.tuples,
                    schema_arc.clone(),
                    pred,
                    executor.parameters(),
                )?;
                Box::new(
                    ScanOperator::new(
                        table_name.clone(),
                        schema_arc.clone(),
                        projection.clone(),
                        tuples,
                        executor.parameters().to_vec(),
                    )
                    .with_timeout(executor.timeout_ctx()),
                )
            } else {
                Box::new(
                    ScanOperator::new_shared(
                        table_name.clone(),
                        schema_arc.clone(),
                        projection.clone(),
                        Arc::clone(&cte_data.tuples),
                        executor.parameters().to_vec(),
                    )
                    .with_timeout(executor.timeout_ctx()),
                )
            };

            return Ok(scan_op);
        }

        use crate::sql::phase3::SystemViewRegistry;
        let registry = SystemViewRegistry::shared();
        if registry.is_system_view(table_name) {
            let storage = executor
                .storage()
                .ok_or_else(|| Error::query_execution("system view requires storage context".to_string()))?;
            let mut schema = registry
                .get_schema(table_name)
                .cloned()
                .unwrap_or_else(|| Schema { columns: vec![] });
            for col in &mut schema.columns {
                col.source_table = Some(source_name.clone());
                col.source_table_name = Some(table_name.clone());
            }
            let schema_arc = Arc::new(schema);
            let tuples = registry.execute(table_name, storage)?;
            let tuples = if let Some(pred) = &materialized_predicate {
                filter_tuples_with_evaluator(tuples, schema_arc.clone(), pred, executor.parameters())?
            } else {
                tuples
            };
            return Ok(Box::new(
                ScanOperator::new(
                    table_name.clone(),
                    schema_arc,
                    projection.clone(),
                    tuples,
                    executor.parameters().to_vec(),
                )
                .with_timeout(executor.timeout_ctx()),
            ));
        }

        // Fetch actual schema from storage and scan table with filtering
        let mut row_projection_applied = false;
        let (actual_schema, tuples) = if let Some(storage) = executor.storage() {
            let catalog = storage.catalog();
            let mv_catalog = storage.mv_catalog();

            // First check if it's a materialized view
            let (schema, actual_table_name) = if mv_catalog.view_exists(table_name)? {
                let mv_metadata = mv_catalog.get_view(table_name)?;
                let mv_data_table = crate::storage::MaterializedViewCatalog::mv_data_table_name(table_name);

                // Check if MV data table exists (view has been refreshed)
                if !catalog.table_exists(&mv_data_table)? {
                    return Err(Error::query_execution(format!(
                        "Materialized view '{}' exists but has never been refreshed. Run: REFRESH MATERIALIZED VIEW {}",
                        table_name, table_name
                    )));
                }

                (mv_metadata.schema, mv_data_table)
            } else {
                // Not an MV, try regular table
                match catalog.get_table_schema(table_name) {
                    Ok(schema) => (schema, table_name.clone()),
                    Err(e) => return Err(e),
                }
            };

            // Analyze the predicate for storage-level pushdown
            let analyzed_predicates = if let Some(ref pred) = materialized_predicate {
                storage.predicate_pushdown().analyze_predicate(pred, &schema)
            } else {
                Vec::new()
            };
            let storage_predicates_safe = storage_predicates_are_sql_safe(&schema, &analyzed_predicates);
            let pushed_predicates = if storage_predicates_safe {
                analyzed_predicates.as_slice()
            } else {
                &[]
            };

            tracing::debug!(
                "FilteredScan on table '{}': analyzed {} predicates for pushdown",
                table_name,
                analyzed_predicates.len()
            );

            // Handle time-travel or transactional queries with filtered scan
            let tuples = if let Some(txn) = executor.transaction() {
                // Transactional scan: read at transaction's snapshot
                let base_tuples = storage.scan_table_at_snapshot(&actual_table_name, txn.snapshot_id())?;

                // Merge with write set
                let merged_tuples = txn.merge_with_write_set(&actual_table_name, base_tuples)?;

                // Apply storage-level filtering (on the merged set)
                storage.predicate_pushdown().scan_with_pushdown(
                    &actual_table_name,
                    merged_tuples,
                    pushed_predicates,
                    &schema,
                    None,
                )
            } else if let Some(as_of_clause) = as_of {
                // P0#1: AS OF requires version history (see above).
                if !storage.time_travel_enabled() {
                    return Err(crate::Error::query_execution(
                        "AS OF / time-travel queries require time_travel_enabled = true",
                    ));
                }
                tracing::debug!(
                    "Time-travel FilteredScan on table '{}' with AS OF clause: {:?}",
                    table_name,
                    as_of_clause
                );

                // Resolve AS OF clause to snapshot timestamp
                let snapshot_mgr = storage.snapshot_manager();
                let snapshot_ts = snapshot_mgr.resolve_as_of(as_of_clause)?;

                // R4.3: pin the snapshot so the version GC cannot advance
                // past it while this statement reads version history.
                let _gc_pin = storage.pin_historical_snapshot(snapshot_ts)?;

                // Scan at historical snapshot, then apply filtering
                let base_tuples = storage.scan_table_at_snapshot(&actual_table_name, snapshot_ts)?;

                // Apply storage-level filtering
                storage.predicate_pushdown().scan_with_pushdown(
                    &actual_table_name,
                    base_tuples,
                    pushed_predicates,
                    &schema,
                    None, // No limit at storage level
                )
            } else {
                // Normal filtered scan (current data) with branch isolation
                let decode_hint = executor.scan_decode_hint_for(table_name);
                let mut columnar_predicates_applied = false;
                let mut row_predicates_applied = false;
                let base_tuples = if actual_table_name == *table_name {
                    if let Some(columns) = columnar_scan_columns_with_predicates(
                        &schema,
                        projection.as_ref(),
                        decode_hint,
                        pushed_predicates,
                    ) {
                        let apply_columnar_predicates = should_apply_columnar_predicates(pushed_predicates);
                        columnar_predicates_applied = apply_columnar_predicates && !storage.is_branch_active();
                        let pushed_predicates = if apply_columnar_predicates {
                            pushed_predicates
                        } else {
                            &[]
                        };
                        let projected_columnar = if apply_columnar_predicates {
                            projection.as_deref().and_then(|projection| {
                                storage
                                    .scan_table_with_schema_columnar_projected_filtered(
                                        &actual_table_name,
                                        &schema,
                                        projection,
                                        pushed_predicates,
                                    )
                                    .transpose()
                            })
                        } else {
                            None
                        };
                        if let Some(projected) = projected_columnar.transpose()? {
                            columnar_predicates_applied = true;
                            row_projection_applied = true;
                            projected
                        } else {
                            storage.scan_table_branch_aware_with_schema_columnar_columns_filtered(
                                &actual_table_name,
                                &schema,
                                &columns,
                                pushed_predicates,
                            )?
                        }
                    } else if !pushed_predicates.is_empty() {
                        let selected_columns: Option<Vec<usize>> = match decode_hint {
                            Some(ScanDecodeHint::Prefix(prefix_len)) => Some((0..*prefix_len).collect()),
                            Some(ScanDecodeHint::Columns(columns)) => Some(columns.clone()),
                            None => None,
                        };
                        let projected_filtered = if let Some(projection) = projection.as_deref() {
                            storage.scan_table_with_schema_projected_filtered(
                                &actual_table_name,
                                &schema,
                                projection,
                                pushed_predicates,
                            )?
                        } else {
                            None
                        };
                        if let Some(projected) = projected_filtered {
                            row_predicates_applied = true;
                            row_projection_applied = true;
                            projected
                        } else if let Some(tuples) = storage.scan_table_with_schema_columns_filtered(
                            &actual_table_name,
                            &schema,
                            selected_columns.as_deref(),
                            pushed_predicates,
                        )? {
                            row_predicates_applied = true;
                            tuples
                        } else {
                            match decode_hint {
                                Some(ScanDecodeHint::Prefix(prefix_len)) => storage
                                    .scan_table_branch_aware_with_schema_prefix(
                                        &actual_table_name,
                                        &schema,
                                        *prefix_len,
                                    )?,
                                Some(ScanDecodeHint::Columns(columns)) => storage
                                    .scan_table_branch_aware_with_schema_columns(
                                        &actual_table_name,
                                        &schema,
                                        columns,
                                    )?,
                                _ => storage.scan_table_branch_aware_with_schema(&actual_table_name, &schema)?,
                            }
                        }
                    } else {
                        match decode_hint {
                            Some(ScanDecodeHint::Prefix(prefix_len)) => storage
                                .scan_table_branch_aware_with_schema_prefix(&actual_table_name, &schema, *prefix_len)?,
                            Some(ScanDecodeHint::Columns(columns)) => storage
                                .scan_table_branch_aware_with_schema_columns(&actual_table_name, &schema, columns)?,
                            _ => storage.scan_table_branch_aware_with_schema(&actual_table_name, &schema)?,
                        }
                    }
                } else {
                    storage.scan_table_branch_aware_with_schema(&actual_table_name, &schema)?
                };

                // Apply storage-level filtering through predicate pushdown manager
                if columnar_predicates_applied || row_predicates_applied {
                    base_tuples
                } else {
                    storage.predicate_pushdown().scan_with_pushdown(
                        &actual_table_name,
                        base_tuples,
                        pushed_predicates,
                        &schema,
                        None, // No limit at storage level
                    )
                }
            };

            tracing::debug!("FilteredScan returned {} tuples after predicate pushdown", tuples.len());

            // Set source_table (alias) and source_table_name (actual) on each column for JOIN disambiguation
            // This allows both `e.name` (alias) and `employees.name` (full name) syntax in queries
            let scan_columns: Vec<_> = if row_projection_applied {
                projection
                    .as_deref()
                    .unwrap_or(&[])
                    .iter()
                    .filter_map(|&idx| schema.columns.get(idx).cloned())
                    .collect()
            } else {
                schema.columns.clone()
            };
            let schema_with_source = Schema {
                columns: scan_columns
                    .into_iter()
                    .map(|mut col| {
                        col.source_table = Some(source_name.clone());
                        col.source_table_name = Some(table_name.clone());
                        col
                    })
                    .collect(),
            };
            let actual_schema = Arc::new(schema_with_source);
            let tuples = if !storage_predicates_safe {
                if let Some(pred) = &materialized_predicate {
                    filter_tuples_with_evaluator(tuples, actual_schema.clone(), pred, executor.parameters())?
                } else {
                    tuples
                }
            } else {
                tuples
            };
            (actual_schema, tuples)
        } else {
            // No storage, use placeholder schema from plan
            (_plan_schema.clone(), Vec::new())
        };

        Ok(Box::new(
            ScanOperator::new(
                table_name.clone(),
                actual_schema,
                if row_projection_applied {
                    None
                } else {
                    projection.clone()
                },
                tuples,
                executor.parameters().to_vec(),
            )
            .with_timeout(executor.timeout_ctx()),
        ))
    } else {
        Err(Error::query_execution("Expected FilteredScan plan node"))
    }
}

/// Generate series operator
///
/// Produces sequential integer values from start to stop (inclusive),
/// with an optional step value. Implements PostgreSQL's `generate_series` function.
///
/// Examples:
/// - `generate_series(1, 5)` produces: 1, 2, 3, 4, 5
/// - `generate_series(1, 10, 2)` produces: 1, 3, 5, 7, 9
/// - `generate_series(5, 1, -1)` produces: 5, 4, 3, 2, 1
pub struct GenerateSeriesOperator {
    /// Current value in the series
    current: i64,
    /// End value (inclusive)
    stop: i64,
    /// Step increment
    step: i64,
    /// Whether the series has been exhausted
    exhausted: bool,
    /// Output schema
    schema: Arc<Schema>,
}

impl GenerateSeriesOperator {
    /// Create a new generate_series operator
    pub fn new(start: i64, stop: i64, step: i64, schema: Arc<Schema>) -> Self {
        // Series is immediately exhausted if step direction doesn't match range direction
        let exhausted = match step.cmp(&0) {
            std::cmp::Ordering::Equal => true, // Zero step would be infinite loop
            std::cmp::Ordering::Greater => start > stop,
            std::cmp::Ordering::Less => start < stop,
        };

        Self {
            current: start,
            stop,
            step,
            exhausted,
            schema,
        }
    }
}

impl PhysicalOperator for GenerateSeriesOperator {
    fn next(&mut self) -> Result<Option<Tuple>> {
        if self.exhausted {
            return Ok(None);
        }

        let value = self.current;

        // Advance to next value
        self.current = self.current.saturating_add(self.step);

        // Check if we've passed the stop value
        if self.step > 0 && self.current > self.stop {
            self.exhausted = true;
        } else if self.step < 0 && self.current < self.stop {
            self.exhausted = true;
        }

        Ok(Some(Tuple::new(vec![crate::Value::Int8(value)])))
    }

    fn schema(&self) -> Arc<Schema> {
        self.schema.clone()
    }
}

/// Unnest operator
///
/// Expands an array expression into a set of rows.
/// Implements PostgreSQL's `unnest` function.
pub struct UnnestOperator {
    /// Pre-materialized values to return
    values: Vec<crate::Value>,
    /// Current index
    current_index: usize,
    /// Output schema
    schema: Arc<Schema>,
}

impl UnnestOperator {
    /// Create a new unnest operator from pre-evaluated values
    pub fn new(values: Vec<crate::Value>, schema: Arc<Schema>) -> Self {
        Self {
            values,
            current_index: 0,
            schema,
        }
    }
}

impl PhysicalOperator for UnnestOperator {
    fn next(&mut self) -> Result<Option<Tuple>> {
        if self.current_index >= self.values.len() {
            return Ok(None);
        }

        let value = self
            .values
            .get(self.current_index)
            .cloned()
            .ok_or_else(|| Error::query_execution("Unnest index out of bounds"))?;
        self.current_index += 1;

        Ok(Some(Tuple::new(vec![value])))
    }

    fn schema(&self) -> Arc<Schema> {
        self.schema.clone()
    }
}

/// Build a table function schema with source table information
fn build_table_function_schema(col_name: &str, alias: &Option<String>) -> Arc<Schema> {
    let source_name = alias.as_deref().unwrap_or(col_name);
    Arc::new(Schema {
        columns: vec![crate::Column {
            name: col_name.to_string(),
            data_type: crate::DataType::Int8,
            nullable: false,
            primary_key: false,
            source_table: Some(source_name.to_string()),
            source_table_name: Some(col_name.to_string()),
            default_expr: None,
            unique: false,
            storage_mode: crate::ColumnStorageMode::Default,
        }],
    })
}

/// Evaluate a LogicalExpr argument to an i64 value for table functions
fn eval_table_function_arg(expr: &crate::sql::LogicalExpr, params: &[crate::Value]) -> Result<i64> {
    use crate::sql::LogicalExpr;
    match expr {
        LogicalExpr::Literal(crate::Value::Int4(v)) => Ok(i64::from(*v)),
        LogicalExpr::Literal(crate::Value::Int8(v)) => Ok(*v),
        LogicalExpr::Literal(crate::Value::Int2(v)) => Ok(i64::from(*v)),
        LogicalExpr::Literal(crate::Value::Float4(v)) => Ok(*v as i64),
        LogicalExpr::Literal(crate::Value::Float8(v)) => Ok(*v as i64),
        LogicalExpr::UnaryExpr {
            op: crate::sql::UnaryOperator::Minus,
            expr: inner,
        } => {
            let val = eval_table_function_arg(inner, params)?;
            Ok(-val)
        }
        LogicalExpr::Parameter { index } => {
            if *index == 0 || *index > params.len() {
                return Err(Error::query_execution(format!("Parameter ${} out of range", index)));
            }
            // Safety: index validated in range 1..=params.len() above
            #[allow(clippy::indexing_slicing)]
            match &params[*index - 1] {
                crate::Value::Int4(v) => Ok(i64::from(*v)),
                crate::Value::Int8(v) => Ok(*v),
                crate::Value::Int2(v) => Ok(i64::from(*v)),
                other => Err(Error::query_execution(format!(
                    "Expected integer parameter for table function, got {:?}",
                    other
                ))),
            }
        }
        other => Err(Error::query_execution(format!(
            "Table function argument must be a literal integer, got {:?}",
            other
        ))),
    }
}

/// Handle TableFunction logical plan node
pub(super) fn handle_table_function(executor: &Executor, plan: &LogicalPlan) -> Result<Box<dyn PhysicalOperator>> {
    if let LogicalPlan::TableFunction {
        function_name,
        args,
        alias,
    } = plan
    {
        match function_name.as_str() {
            "generate_series" => {
                if args.len() < 2 || args.len() > 3 {
                    return Err(Error::query_execution(
                        "generate_series requires 2 or 3 arguments: generate_series(start, stop[, step])",
                    ));
                }
                let params = executor.parameters();
                let start = eval_table_function_arg(
                    args.first()
                        .ok_or_else(|| Error::query_execution("Missing start argument"))?,
                    params,
                )?;
                let stop = eval_table_function_arg(
                    args.get(1)
                        .ok_or_else(|| Error::query_execution("Missing stop argument"))?,
                    params,
                )?;
                let step = if let Some(step_expr) = args.get(2) {
                    let s = eval_table_function_arg(step_expr, params)?;
                    if s == 0 {
                        return Err(Error::query_execution("generate_series step cannot be zero"));
                    }
                    s
                } else {
                    1
                };

                let schema = build_table_function_schema("generate_series", alias);
                Ok(Box::new(GenerateSeriesOperator::new(start, stop, step, schema)))
            }
            "unnest" => {
                if args.is_empty() {
                    return Err(Error::query_execution("unnest requires at least one argument"));
                }
                // For unnest, we expect array literal expressions
                // Arrays are parsed as Literal(Value::Array(...)) by the planner
                let mut values = Vec::new();
                for arg in args {
                    match arg {
                        crate::sql::LogicalExpr::Literal(crate::Value::Array(arr)) => {
                            values.extend(arr.iter().cloned());
                        }
                        crate::sql::LogicalExpr::Literal(v) => {
                            // Single literal value treated as single-element array
                            values.push(v.clone());
                        }
                        _ => {
                            return Err(Error::query_execution("UNNEST argument must be an array expression"));
                        }
                    }
                }

                let schema = build_table_function_schema("unnest", alias);
                Ok(Box::new(UnnestOperator::new(values, schema)))
            }
            _ => Err(Error::query_execution(format!(
                "Unknown table function: {}",
                function_name
            ))),
        }
    } else {
        Err(Error::query_execution("Expected TableFunction plan node"))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::Column;
    use crate::DataType;
    use crate::Value;

    fn test_schema() -> Arc<Schema> {
        Arc::new(Schema {
            columns: vec![
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
                    name: "k".to_string(),
                    data_type: DataType::Text,
                    nullable: true,
                    primary_key: false,
                    source_table: None,
                    source_table_name: None,
                    default_expr: None,
                    unique: false,
                    storage_mode: crate::ColumnStorageMode::Default,
                },
                Column {
                    name: "payload".to_string(),
                    data_type: DataType::Text,
                    nullable: true,
                    primary_key: false,
                    source_table: None,
                    source_table_name: None,
                    default_expr: None,
                    unique: false,
                    storage_mode: crate::ColumnStorageMode::Default,
                },
                Column {
                    name: "note".to_string(),
                    data_type: DataType::Text,
                    nullable: true,
                    primary_key: false,
                    source_table: None,
                    source_table_name: None,
                    default_expr: None,
                    unique: false,
                    storage_mode: crate::ColumnStorageMode::Default,
                },
            ],
        })
    }

    fn id_eq_seven() -> LogicalExpr {
        LogicalExpr::BinaryExpr {
            left: Box::new(LogicalExpr::Column {
                table: None,
                name: "id".to_string(),
            }),
            op: crate::sql::logical_plan::BinaryOperator::Eq,
            right: Box::new(LogicalExpr::Literal(Value::Int4(7))),
        }
    }

    fn count_star() -> LogicalExpr {
        LogicalExpr::AggregateFunction {
            fun: crate::sql::logical_plan::AggregateFunction::Count,
            args: vec![LogicalExpr::Wildcard],
            distinct: false,
        }
    }

    fn col(table: &str, name: &str) -> LogicalExpr {
        LogicalExpr::Column {
            table: Some(table.to_string()),
            name: name.to_string(),
        }
    }

    fn eq(left: LogicalExpr, right: LogicalExpr) -> LogicalExpr {
        LogicalExpr::BinaryExpr {
            left: Box::new(left),
            op: crate::sql::logical_plan::BinaryOperator::Eq,
            right: Box::new(right),
        }
    }

    fn scan_with_alias(table_name: &str, alias: &str) -> LogicalPlan {
        LogicalPlan::Scan {
            table_name: table_name.to_string(),
            alias: Some(alias.to_string()),
            schema: test_schema(),
            projection: None,
            as_of: None,
        }
    }

    #[test]
    fn test_scan_operator_empty() {
        let schema = Arc::new(Schema {
            columns: vec![Column {
                name: "id".to_string(),
                data_type: DataType::Int4,
                nullable: false,
                primary_key: true,
                source_table: None,
                source_table_name: None,
                default_expr: None,
                unique: false,
                storage_mode: crate::ColumnStorageMode::Default,
            }],
        });

        let mut scan = ScanOperator::new("test".to_string(), schema.clone(), None, Vec::new(), Vec::new());
        assert!(scan.next().expect("Failed to execute scan").is_none());
    }

    #[test]
    fn filtered_scan_prefix_hint_includes_projection_and_predicate() {
        let schema = test_schema();
        let plan = LogicalPlan::FilteredScan {
            table_name: "w".to_string(),
            alias: None,
            schema,
            projection: Some(vec![1]),
            predicate: Some(id_eq_seven()),
            as_of: None,
        };

        assert_eq!(compute_scan_prefix_hint(&plan), Some(("w".to_string(), 2)));
    }

    #[test]
    fn filtered_scan_prefix_hint_widens_for_tail_projection() {
        let schema = test_schema();
        let plan = LogicalPlan::FilteredScan {
            table_name: "w".to_string(),
            alias: None,
            schema,
            projection: Some(vec![3]),
            predicate: Some(id_eq_seven()),
            as_of: None,
        };

        assert_eq!(compute_scan_prefix_hint(&plan), Some(("w".to_string(), 4)));
    }

    #[test]
    fn scan_prefix_hint_includes_scan_projection() {
        let schema = test_schema();
        let plan = LogicalPlan::Scan {
            table_name: "w".to_string(),
            alias: None,
            schema,
            projection: Some(vec![2]),
            as_of: None,
        };

        assert_eq!(compute_scan_prefix_hint(&plan), Some(("w".to_string(), 3)));
    }

    #[test]
    fn prefix_decode_gate_requires_meaningful_suffix_skip() {
        assert!(should_use_prefix_decode(0, 5));
        assert!(should_use_prefix_decode(2, 4));
        assert!(!should_use_prefix_decode(4, 5));
        assert!(!should_use_prefix_decode(5, 5));
    }

    #[test]
    fn selected_decode_hint_handles_sparse_later_columns() {
        let schema = test_schema();
        let plan = LogicalPlan::Aggregate {
            input: Box::new(LogicalPlan::Scan {
                table_name: "w".to_string(),
                alias: None,
                schema,
                projection: None,
                as_of: None,
            }),
            group_by: vec![LogicalExpr::Column {
                table: None,
                name: "note".to_string(),
            }],
            aggr_exprs: vec![LogicalExpr::AggregateFunction {
                fun: crate::sql::logical_plan::AggregateFunction::Sum,
                args: vec![LogicalExpr::Column {
                    table: None,
                    name: "payload".to_string(),
                }],
                distinct: false,
            }],
            having: None,
        };

        assert_eq!(
            compute_scan_decode_hint(&plan),
            Some(("w".to_string(), ScanDecodeHint::Columns(vec![2, 3])))
        );
    }

    #[test]
    fn selected_decode_hint_allows_two_skipped_columns() {
        assert!(should_use_selected_decode(&[0, 1, 3], 5));
    }

    #[test]
    fn join_decode_hints_resolve_qualified_columns_per_table() {
        let plan = LogicalPlan::Project {
            input: Box::new(LogicalPlan::Join {
                left: Box::new(scan_with_alias("left_table", "l")),
                right: Box::new(scan_with_alias("right_table", "r")),
                join_type: crate::sql::JoinType::Inner,
                on: Some(eq(col("l", "id"), col("r", "id"))),
                lateral: false,
            }),
            exprs: vec![col("l", "k"), col("r", "note")],
            aliases: vec!["k".to_string(), "note".to_string()],
            distinct: false,
            distinct_on: None,
        };

        assert_eq!(
            compute_scan_decode_hints(&plan),
            vec![
                ("left_table".to_string(), ScanDecodeHint::Prefix(2)),
                ("right_table".to_string(), ScanDecodeHint::Columns(vec![0, 3])),
            ]
        );
    }

    #[test]
    fn join_decode_hints_reject_ambiguous_unqualified_columns() {
        let plan = LogicalPlan::Project {
            input: Box::new(LogicalPlan::Join {
                left: Box::new(scan_with_alias("left_table", "l")),
                right: Box::new(scan_with_alias("right_table", "r")),
                join_type: crate::sql::JoinType::Inner,
                on: Some(eq(col("l", "id"), col("r", "id"))),
                lateral: false,
            }),
            exprs: vec![LogicalExpr::Column {
                table: None,
                name: "id".to_string(),
            }],
            aliases: vec!["id".to_string()],
            distinct: false,
            distinct_on: None,
        };

        assert!(compute_scan_decode_hints(&plan).is_empty());
    }

    #[test]
    fn join_decode_hints_reject_self_join_by_table_name() {
        let plan = LogicalPlan::Project {
            input: Box::new(LogicalPlan::Join {
                left: Box::new(scan_with_alias("same_table", "l")),
                right: Box::new(scan_with_alias("same_table", "r")),
                join_type: crate::sql::JoinType::Inner,
                on: Some(eq(col("l", "id"), col("r", "id"))),
                lateral: false,
            }),
            exprs: vec![col("l", "k")],
            aliases: vec!["k".to_string()],
            distinct: false,
            distinct_on: None,
        };

        assert!(compute_scan_decode_hints(&plan).is_empty());
    }

    #[test]
    fn root_filtered_scan_without_projection_needs_full_row() {
        let schema = test_schema();
        let plan = LogicalPlan::FilteredScan {
            table_name: "w".to_string(),
            alias: None,
            schema,
            projection: None,
            predicate: Some(id_eq_seven()),
            as_of: None,
        };

        assert_eq!(compute_scan_prefix_hint(&plan), Some(("w".to_string(), 4)));
    }

    #[test]
    fn project_over_filter_keeps_prefix_narrow() {
        let schema = test_schema();
        let plan = LogicalPlan::Project {
            input: Box::new(LogicalPlan::Filter {
                input: Box::new(LogicalPlan::Scan {
                    table_name: "w".to_string(),
                    alias: None,
                    schema,
                    projection: None,
                    as_of: None,
                }),
                predicate: id_eq_seven(),
            }),
            exprs: vec![LogicalExpr::Column {
                table: None,
                name: "k".to_string(),
            }],
            aliases: vec!["k".to_string()],
            distinct: false,
            distinct_on: None,
        };

        assert_eq!(compute_scan_prefix_hint(&plan), Some(("w".to_string(), 2)));
    }

    #[test]
    fn count_star_without_filter_needs_no_columns() {
        let schema = test_schema();
        let plan = LogicalPlan::Aggregate {
            input: Box::new(LogicalPlan::Scan {
                table_name: "w".to_string(),
                alias: None,
                schema,
                projection: None,
                as_of: None,
            }),
            group_by: vec![],
            aggr_exprs: vec![count_star()],
            having: None,
        };

        assert_eq!(compute_scan_prefix_hint(&plan), Some(("w".to_string(), 0)));
    }

    #[test]
    fn count_star_filter_uses_predicate_columns_only() {
        let schema = test_schema();
        let plan = LogicalPlan::Aggregate {
            input: Box::new(LogicalPlan::Filter {
                input: Box::new(LogicalPlan::Scan {
                    table_name: "w".to_string(),
                    alias: None,
                    schema,
                    projection: None,
                    as_of: None,
                }),
                predicate: id_eq_seven(),
            }),
            group_by: vec![],
            aggr_exprs: vec![count_star()],
            having: None,
        };

        assert_eq!(compute_scan_prefix_hint(&plan), Some(("w".to_string(), 1)));
    }
}
