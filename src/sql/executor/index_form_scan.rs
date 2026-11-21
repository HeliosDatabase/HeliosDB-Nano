//! Reads through multi-column, partial and expression indexes.
//!
//! The single-column paths in `scan` answer `col = v`, `col IN (…)` and a range
//! on one indexed column. This module answers what a wider index can:
//!
//! * equality on a leading run of the index's key parts (all of them: a point
//!   probe; some of them: a bounded scan of the keys that begin with them);
//! * the same, plus a range on the next key part;
//! * a key part that is an expression, when the query's predicate holds the
//!   same expression;
//! * a partial index, when the query's predicate implies the index's;
//! * `ORDER BY` on the key parts with a `LIMIT`, by walking the index in order.
//!
//! Every row an index hands back is checked against the whole predicate again,
//! so an index only ever narrows what is read. Detection ([`form_index_lookup`],
//! [`ordered_detect`]) is shared with `EXPLAIN`, which names the index.

use std::sync::Arc;

use super::scan::{self, ScanOperator};
use super::{Executor, LimitOperator, PhysicalOperator, ProjectOperator};
use crate::sql::{BinaryOperator, LogicalExpr, LogicalPlan};
use crate::storage::art_manager::IndexShapeView;
use crate::storage::index_form::{canonical_value, class, normalize_expr, value_class, FormPart};
use crate::storage::{ArtIndexManager, ArtIndexType, RowId, StorageEngine};
use crate::{Column, DataType, Result, Schema, Tuple, Value};

/// How the chosen index is read.
#[derive(Debug, Clone)]
enum Access {
    /// One key.
    Point(Vec<u8>),
    /// The keys between two bounds (`(bound, inclusive)`, `None`: unbounded).
    Range {
        lower: Option<(Vec<u8>, bool)>,
        upper: Option<(Vec<u8>, bool)>,
    },
}

/// A read through a multi-column, partial or expression index.
#[derive(Debug, Clone)]
pub(super) struct FormScanSpec {
    /// The index read.
    pub(super) index_name: String,
    /// What it is asked for, in words (`a = 1 AND b >= 5`).
    pub(super) display: String,
    access: Access,
    /// Is the index partial (its entry count is then no measure of the table)?
    partial: bool,
    /// Preference among candidate indexes: key parts bound by equality, then a
    /// range, then a predicate.
    score: (usize, usize, usize),
}

/// One key part of a candidate index, as the query side needs it.
enum Part<'a> {
    /// A table column, with its declared type.
    Column(&'a Column),
    /// An expression, with the classes of the values it has produced.
    Expr { expr: &'a LogicalExpr, classes: u8 },
}

/// The key parts of `shape`; `None` when a column does not resolve.
fn parts_of<'a>(shape: &'a IndexShapeView, schema: &'a Schema) -> Option<Vec<Part<'a>>> {
    match &shape.form {
        Some(form) => form
            .parts()
            .iter()
            .enumerate()
            .map(|(i, part)| match part {
                FormPart::Column { name, .. } => schema.get_column(name).map(Part::Column),
                FormPart::Expr { expr, .. } => Some(Part::Expr {
                    expr,
                    classes: form.part_classes(i),
                }),
            })
            .collect(),
        None => shape
            .columns
            .iter()
            .map(|name| schema.get_column(name).map(Part::Column))
            .collect(),
    }
}

/// Is `expr` (normalised) this key part?
fn is_part(part: &Part<'_>, expr: &LogicalExpr) -> bool {
    match part {
        Part::Column(column) => {
            matches!(expr, LogicalExpr::Column { name, .. } if name.eq_ignore_ascii_case(&column.name))
        }
        Part::Expr { expr: own, .. } => *own == expr,
    }
}

/// The constant a normalised expression is: a literal, possibly under casts.
fn constant_of(expr: &LogicalExpr) -> Option<&Value> {
    match expr {
        LogicalExpr::Literal(value) => Some(value),
        LogicalExpr::Cast { expr, .. } => constant_of(expr),
        _ => None,
    }
}

/// The value an equality on `part` probes with, in the form the index stores.
/// `None`: this constant cannot be looked up in the index (it has no exact key),
/// and the index is not used for it.
fn equality_key_value(part: &Part<'_>, raw: &Value) -> Option<Value> {
    if matches!(raw, Value::Null) {
        return None;
    }
    match part {
        Part::Column(column) => {
            let value = super::coerce_literal_to_column_type(raw.clone(), &column.data_type)?;
            match value {
                // No exact key: a NUMERIC's key keeps its scale, a float has
                // two zeros.
                Value::Null | Value::Numeric(_) | Value::Float4(_) | Value::Float8(_) => None,
                other => Some(other),
            }
        }
        Part::Expr { classes, .. } => {
            let value = canonical_value(raw.clone());
            let own = value_class(&value);
            let exact = matches!(own, class::INT | class::TEXT | class::BOOL | class::UUID);
            // Every value the expression has produced is of the constant's
            // class, so equality is equality of keys.
            (exact && (classes & !own) == 0).then_some(value)
        }
    }
}

/// The value a range bound on `part` scans from or to. `None`: the part's keys
/// are not in value order for this constant.
fn range_key_value(part: &Part<'_>, raw: &Value) -> Option<Value> {
    if matches!(raw, Value::Null) {
        return None;
    }
    match part {
        Part::Column(column) => {
            if !scan::range_scannable_type(&column.data_type) {
                return None;
            }
            super::coerce_literal_to_column_type(raw.clone(), &column.data_type).filter(|v| !matches!(v, Value::Null))
        }
        Part::Expr { classes, .. } => {
            let value = canonical_value(raw.clone());
            let own = value_class(&value);
            (matches!(own, class::INT | class::TEXT) && (classes & !own) == 0).then_some(value)
        }
    }
}

/// A comparison of an expression with a constant, read either way round:
/// `(expression, operator as if the expression were on the left, constant)`.
fn as_comparison(expr: &LogicalExpr) -> Option<(&LogicalExpr, BinaryOperator, &Value)> {
    let LogicalExpr::BinaryExpr { left, op, right } = expr else {
        return None;
    };
    if !matches!(
        op,
        BinaryOperator::Eq
            | BinaryOperator::NotEq
            | BinaryOperator::Lt
            | BinaryOperator::LtEq
            | BinaryOperator::Gt
            | BinaryOperator::GtEq
    ) {
        return None;
    }
    if let Some(value) = constant_of(right) {
        return Some((left.as_ref(), *op, value));
    }
    let value = constant_of(left)?;
    let flipped = match op {
        BinaryOperator::Lt => BinaryOperator::Gt,
        BinaryOperator::LtEq => BinaryOperator::GtEq,
        BinaryOperator::Gt => BinaryOperator::Lt,
        BinaryOperator::GtEq => BinaryOperator::LtEq,
        other => *other,
    };
    Some((right.as_ref(), flipped, value))
}

fn flatten<'a>(expr: &'a LogicalExpr, out: &mut Vec<&'a LogicalExpr>) {
    if let LogicalExpr::BinaryExpr {
        left,
        op: BinaryOperator::And,
        right,
    } = expr
    {
        flatten(left, out);
        flatten(right, out);
    } else {
        out.push(expr);
    }
}

/// The whole numbers `query` allows `subject` to be, as `(lower, upper)` bounds
/// (`(value, inclusive)`); `None` when `query` says nothing of that kind.
#[allow(clippy::type_complexity)]
fn integer_bounds(query: &LogicalExpr, subject: &LogicalExpr) -> Option<(Option<(i64, bool)>, Option<(i64, bool)>)> {
    if let LogicalExpr::Between {
        expr,
        low,
        high,
        negated: false,
    } = query
    {
        if expr.as_ref() != subject {
            return None;
        }
        let (Some(Value::Int8(low)), Some(Value::Int8(high))) = (constant_of(low), constant_of(high)) else {
            return None;
        };
        return Some((Some((*low, true)), Some((*high, true))));
    }
    let (expr, op, value) = as_comparison(query)?;
    if expr != subject {
        return None;
    }
    let Value::Int8(v) = value else {
        return None;
    };
    Some(match op {
        BinaryOperator::Eq => (Some((*v, true)), Some((*v, true))),
        BinaryOperator::Gt => (Some((*v, false)), None),
        BinaryOperator::GtEq => (Some((*v, true)), None),
        BinaryOperator::Lt => (None, Some((*v, false))),
        BinaryOperator::LtEq => (None, Some((*v, true))),
        _ => return None,
    })
}

/// Does the query conjunct `query` imply the index-predicate conjunct `wanted`?
///
/// Covered: the same expression (an equality either way round); `x IS NOT NULL`
/// from any comparison of `x` with a constant, a `BETWEEN` or an `IN` list; a
/// boolean column from `column = true` and the reverse; a comparison of `x` with
/// a whole number from a tighter comparison of `x` with whole numbers.
fn conjunct_implies(query: &LogicalExpr, wanted: &LogicalExpr) -> bool {
    if query == wanted {
        return true;
    }
    if let (
        LogicalExpr::BinaryExpr {
            left: ql,
            op: BinaryOperator::Eq,
            right: qr,
        },
        LogicalExpr::BinaryExpr {
            left: wl,
            op: BinaryOperator::Eq,
            right: wr,
        },
    ) = (query, wanted)
    {
        if ql == wr && qr == wl {
            return true;
        }
    }
    match wanted {
        LogicalExpr::IsNull {
            expr: subject,
            is_null: false,
        } => {
            if let Some((expr, _, value)) = as_comparison(query) {
                return expr == subject.as_ref() && !matches!(value, Value::Null);
            }
            match query {
                LogicalExpr::Between {
                    expr, negated: false, ..
                }
                | LogicalExpr::InList {
                    expr, negated: false, ..
                } => expr == subject,
                _ => false,
            }
        }
        LogicalExpr::Column { .. } => {
            matches!(as_comparison(query), Some((expr, BinaryOperator::Eq, Value::Boolean(true))) if expr == wanted)
        }
        _ => {
            let Some((subject, op, value)) = as_comparison(wanted) else {
                return false;
            };
            if matches!((op, value), (BinaryOperator::Eq, Value::Boolean(true))) && query == subject {
                return true;
            }
            let Value::Int8(c) = value else {
                return false;
            };
            let Some((lower, upper)) = integer_bounds(query, subject) else {
                return false;
            };
            match op {
                BinaryOperator::Gt => lower.is_some_and(|(v, inclusive)| v > *c || (v == *c && !inclusive)),
                BinaryOperator::GtEq => lower.is_some_and(|(v, _)| v >= *c),
                BinaryOperator::Lt => upper.is_some_and(|(v, inclusive)| v < *c || (v == *c && !inclusive)),
                BinaryOperator::LtEq => upper.is_some_and(|(v, _)| v <= *c),
                BinaryOperator::Eq => lower == Some((*c, true)) && upper == Some((*c, true)),
                BinaryOperator::NotEq => {
                    lower.is_some_and(|(v, inclusive)| v > *c || (v == *c && !inclusive))
                        || upper.is_some_and(|(v, inclusive)| v < *c || (v == *c && !inclusive))
                }
                _ => false,
            }
        }
    }
}

/// Does the query (its conjuncts, normalised) imply the index predicate?
fn predicate_implied(conjuncts: &[LogicalExpr], index_predicate: &LogicalExpr) -> bool {
    let mut wanted = Vec::new();
    flatten(index_predicate, &mut wanted);
    wanted
        .iter()
        .all(|wanted| conjuncts.iter().any(|query| conjunct_implies(query, wanted)))
}

/// The query's predicate as normalised conjuncts.
fn normalized_conjuncts(predicate: &LogicalExpr, parameters: &[Value]) -> Vec<LogicalExpr> {
    let mut raw = Vec::new();
    flatten(predicate, &mut raw);
    raw.into_iter().map(|c| normalize_expr(c, parameters)).collect()
}

/// The constant an equality conjunct gives `part`, as the index stores it.
fn equality_for(part: &Part<'_>, conjuncts: &[LogicalExpr]) -> Option<Value> {
    conjuncts.iter().find_map(|conjunct| {
        let (expr, op, value) = as_comparison(conjunct)?;
        if op != BinaryOperator::Eq || !is_part(part, expr) {
            return None;
        }
        equality_key_value(part, value)
    })
}

/// The tightest range the conjuncts give `part`: `(lower, upper)`, each
/// `(value, inclusive)`. `None` when they give none, or a bound has no key.
#[allow(clippy::type_complexity)]
fn range_for(part: &Part<'_>, conjuncts: &[LogicalExpr]) -> Option<(Option<(Value, bool)>, Option<(Value, bool)>)> {
    let mut lowers: Vec<(Value, bool)> = Vec::new();
    let mut uppers: Vec<(Value, bool)> = Vec::new();
    for conjunct in conjuncts {
        if let LogicalExpr::Between {
            expr,
            low,
            high,
            negated: false,
        } = conjunct
        {
            if !is_part(part, expr) {
                continue;
            }
            let (Some(low), Some(high)) = (constant_of(low), constant_of(high)) else {
                continue;
            };
            lowers.push((range_key_value(part, low)?, true));
            uppers.push((range_key_value(part, high)?, true));
            continue;
        }
        let Some((expr, op, value)) = as_comparison(conjunct) else {
            continue;
        };
        if !is_part(part, expr) {
            continue;
        }
        match op {
            BinaryOperator::Gt => lowers.push((range_key_value(part, value)?, false)),
            BinaryOperator::GtEq => lowers.push((range_key_value(part, value)?, true)),
            BinaryOperator::Lt => uppers.push((range_key_value(part, value)?, false)),
            BinaryOperator::LtEq => uppers.push((range_key_value(part, value)?, true)),
            _ => {}
        }
    }
    if lowers.is_empty() && uppers.is_empty() {
        return None;
    }
    // Order-preserving encodings: the tightest bound is the one whose key sorts
    // highest (lower) or lowest (upper).
    let key = |v: &Value| ArtIndexManager::encode_key_from_values(std::iter::once(v));
    let lower = lowers.into_iter().max_by_key(|bound| key(&bound.0));
    let upper = uppers.into_iter().min_by_key(|bound| key(&bound.0));
    Some((lower, upper))
}

/// May a reader take rows from this index when `bound` of its key parts are
/// fixed by equality? A PRIMARY KEY / UNIQUE index has no entry for a row with a
/// NULL in its key, so every part the query does not fix must be a column that
/// cannot be NULL.
fn holds_every_matching_row(shape: &IndexShapeView, parts: &[Part<'_>], bound: usize) -> bool {
    if !matches!(shape.index_type, ArtIndexType::PrimaryKey | ArtIndexType::Unique) {
        return true;
    }
    parts
        .iter()
        .skip(bound)
        .all(|part| matches!(part, Part::Column(column) if !column.nullable || column.primary_key))
}

fn part_label(shape: &IndexShapeView, index: usize) -> String {
    shape.columns.get(index).cloned().unwrap_or_else(|| "?".to_string())
}

/// What one index can do for the query; `None` when nothing.
fn spec_for_index(shape: &IndexShapeView, schema: &Schema, conjuncts: &[LogicalExpr]) -> Option<FormScanSpec> {
    let form_predicate = shape.form.as_ref().and_then(|form| form.predicate());
    if let Some(index_predicate) = form_predicate {
        if !predicate_implied(conjuncts, index_predicate) {
            return None;
        }
    }
    let parts = parts_of(shape, schema)?;
    let total = parts.len();
    let mut bound: Vec<Value> = Vec::new();
    let mut shown: Vec<String> = Vec::new();
    for (i, part) in parts.iter().enumerate() {
        let Some(value) = equality_for(part, conjuncts) else {
            break;
        };
        shown.push(format!("{} = {}", part_label(shape, i), value));
        bound.push(value);
    }
    let partial = form_predicate.is_some();
    let suffix = shape
        .form
        .as_ref()
        .and_then(|form| form.predicate_sql())
        .map(|sql| format!("index predicate: {sql}"));
    let display = |mut shown: Vec<String>| {
        if let Some(suffix) = &suffix {
            shown.push(suffix.clone());
        }
        shown.join(" AND ")
    };

    if bound.len() == total {
        return Some(FormScanSpec {
            index_name: shape.name.clone(),
            display: display(shown),
            access: Access::Point(ArtIndexManager::encode_key_prefix(&bound, total)),
            partial,
            score: (total, 0, usize::from(partial)),
        });
    }

    let range = parts.get(bound.len()).and_then(|part| range_for(part, conjuncts));
    if bound.is_empty() && range.is_none() && !partial {
        return None;
    }
    if !holds_every_matching_row(shape, &parts, bound.len()) {
        return None;
    }
    let prefix = ArtIndexManager::encode_key_prefix(&bound, total);
    let (lower, upper) = match &range {
        Some((low, high)) => {
            let label = part_label(shape, bound.len());
            let with = |value: &Value| {
                let mut values = bound.clone();
                values.push(value.clone());
                ArtIndexManager::encode_key_prefix(&values, total)
            };
            if let Some((value, inclusive)) = low {
                shown.push(format!("{} {} {}", label, if *inclusive { ">=" } else { ">" }, value));
            }
            if let Some((value, inclusive)) = high {
                shown.push(format!("{} {} {}", label, if *inclusive { "<=" } else { "<" }, value));
            }
            // Bounds are a superset on purpose: every row read is checked
            // against the predicate again, so an inclusive bound is never wrong.
            let lower = match low {
                Some((value, _)) => Some((with(value), true)),
                None if bound.is_empty() => None,
                None => Some((prefix.clone(), true)),
            };
            let upper = match high {
                Some((value, _)) if total == 1 => Some((with(value), true)),
                Some((value, _)) => {
                    let mut key = with(value);
                    key.push(1);
                    Some((key, false))
                }
                None if bound.is_empty() => None,
                None => {
                    let mut key = prefix.clone();
                    key.push(1);
                    Some((key, false))
                }
            };
            (lower, upper)
        }
        None if bound.is_empty() => (None, None),
        None => {
            let mut end = prefix.clone();
            end.push(1);
            (Some((prefix, true)), Some((end, false)))
        }
    };
    Some(FormScanSpec {
        index_name: shape.name.clone(),
        display: display(shown),
        access: Access::Range { lower, upper },
        partial,
        score: (bound.len(), usize::from(range.is_some()), usize::from(partial)),
    })
}

/// Is this index one of the forms this module reads (the single-column paths
/// read an index over one plain column)?
fn is_wide(shape: &IndexShapeView) -> bool {
    shape.form.is_some() || shape.columns.len() > 1
}

/// The best read of `predicate` over `table_name` through a multi-column,
/// partial or expression index; `None` when no such index helps. Pure
/// detection: nothing is fetched.
pub(super) fn form_index_lookup(
    storage: &StorageEngine,
    table_name: &str,
    schema: &Schema,
    predicate: &LogicalExpr,
    parameters: &[Value],
) -> Option<FormScanSpec> {
    let shapes = storage.art_indexes().table_index_shapes(table_name);
    if !shapes.iter().any(is_wide) {
        return None;
    }
    let conjuncts = normalized_conjuncts(predicate, parameters);
    let mut best: Option<FormScanSpec> = None;
    for shape in shapes.iter().filter(|shape| is_wide(shape)) {
        let Some(spec) = spec_for_index(shape, schema, &conjuncts) else {
            continue;
        };
        // Name order breaks ties (`table_index_shapes` is sorted).
        if best.as_ref().is_none_or(|current| spec.score > current.score) {
            best = Some(spec);
        }
    }
    best
}

/// The row ids `spec` reads, or `None` when reading them through the index
/// would cost more than a scan (more than a quarter of the table, beyond a
/// handful of rows) and the scan is taken instead. Shared with `EXPLAIN`.
pub(super) fn form_scan_row_ids(storage: &StorageEngine, table_name: &str, spec: &FormScanSpec) -> Option<Vec<RowId>> {
    const ALWAYS: usize = 64;
    let art = storage.art_indexes();
    match &spec.access {
        Access::Point(key) => Some(art.index_get_all(&spec.index_name, key)),
        Access::Range { lower, upper } => {
            let total = if spec.partial {
                // A partial index's own size says nothing of the table's. The
                // primary key's entry count is the cheap measure; this is a
                // cost guard, not an answer.
                match art.pk_index_len(table_name) {
                    Some(rows) => rows,
                    None => storage.count_table_rows(table_name).ok()?,
                }
            } else {
                usize::try_from(art.index_entry_count(&spec.index_name)?).ok()?
            };
            let cap = (total / 4).max(ALWAYS).saturating_add(1);
            let pairs = art.index_range_scan(
                &spec.index_name,
                lower.as_ref().map(|(key, inclusive)| (key.as_slice(), *inclusive)),
                upper.as_ref().map(|(key, inclusive)| (key.as_slice(), *inclusive)),
                Some(cap),
            );
            if pairs.len() >= cap {
                return None;
            }
            Some(pairs.into_iter().map(|(_, row_id)| row_id).collect())
        }
    }
}

/// The gates every index read passes: a plain table on the main branch, read
/// outside a transaction that has to see its own writes, with no transaction
/// anywhere holding a key of it out of the indexes.
pub(super) fn gates_pass(executor: &Executor, storage: &StorageEngine, table_name: &str) -> Result<bool> {
    if storage.is_branch_active()
        || executor.txn_forces_slow_reads_for_table(table_name)
        || executor.get_cte(table_name).is_some()
        || storage.cached_table_kind(table_name)? != crate::storage::TableKind::Table
        || !scan::index_miss_is_authoritative(storage, table_name)
    {
        return Ok(false);
    }
    Ok(true)
}

/// Serve a `Filter` / `FilteredScan` over a base table through a multi-column,
/// partial or expression index. `None`: no such read applies.
pub(super) fn try_form_index_for_scan(
    executor: &Executor,
    input: &LogicalPlan,
    predicate: &LogicalExpr,
) -> Result<Option<Box<dyn PhysicalOperator>>> {
    if scan::index_range_fast_path_disabled() {
        return Ok(None);
    }
    let Some(storage) = executor.storage() else {
        return Ok(None);
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
    let Some(spec) = form_index_lookup(storage, table_name, schema.as_ref(), predicate, executor.parameters()) else {
        return Ok(None);
    };
    if !gates_pass(executor, storage, table_name)? {
        return Ok(None);
    }
    let source_name = alias.as_ref().unwrap_or(table_name);
    let (materialized_predicate, actual_schema) =
        scan::materialize_scan_predicate(executor, predicate, schema.as_ref(), source_name, table_name)?;
    let Some(mut row_ids) = form_scan_row_ids(storage, table_name, &spec) else {
        return Ok(None);
    };
    // Row-id order: storage keys are in row-id order, so the fetches are near
    // sequential. This read promises no order.
    row_ids.sort_unstable();
    row_ids.dedup();
    let mut tuples = Vec::with_capacity(row_ids.len());
    for row_id in row_ids {
        if let Some(tuple) = storage.get_row_by_id(table_name, row_id, schema.as_ref())? {
            tuples.push(tuple);
        }
    }
    let actual_schema =
        actual_schema.unwrap_or_else(|| Arc::new(scan::schema_with_source(schema.as_ref(), source_name, table_name)));
    let tuples = scan::filter_scan_candidates(executor, tuples, actual_schema.clone(), &materialized_predicate)?;
    tracing::debug!(
        "index scan: '{}' on {} ({}) served {} rows",
        spec.index_name,
        table_name,
        spec.display,
        tuples.len()
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

/// `EXPLAIN`'s view of [`try_form_index_for_scan`]: the read it would make.
pub(super) fn explain_spec(
    executor: &Executor,
    table_name: &str,
    schema: &Schema,
    predicate: &LogicalExpr,
) -> Option<FormScanSpec> {
    if scan::index_range_fast_path_disabled() {
        return None;
    }
    let storage = executor.storage()?;
    let spec = form_index_lookup(storage, table_name, schema, predicate, executor.parameters())?;
    if !gates_pass(executor, storage, table_name).unwrap_or(false) {
        return None;
    }
    form_scan_row_ids(storage, table_name, &spec)?;
    Some(spec)
}

// ───────────────────────────────────────────────────────────────────────────
// ORDER BY key parts … LIMIT k: the index walked in order, no sort.
// ───────────────────────────────────────────────────────────────────────────

/// An `ORDER BY … LIMIT` served by walking an index in key order.
pub(super) struct OrderedSpec<'p> {
    table_name: &'p String,
    alias: &'p Option<String>,
    schema: &'p Arc<Schema>,
    projection: &'p Option<Vec<usize>>,
    predicate: Option<&'p LogicalExpr>,
    #[allow(clippy::type_complexity)]
    project_wrap: Option<(Vec<LogicalExpr>, Vec<String>, bool, Option<Vec<LogicalExpr>>)>,
    /// The index walked.
    pub(super) index_name: String,
    /// The sort keys, in words.
    pub(super) order_display: String,
    /// Where the walk starts and ends.
    lower: Option<(Vec<u8>, bool)>,
    upper: Option<(Vec<u8>, bool)>,
    /// The key parts whose value in a row read must be an ordinary one (not
    /// NULL, not an empty string) for key order to be sort order.
    checked_parts: Vec<usize>,
    entry_count: usize,
}

impl OrderedSpec<'_> {
    /// The table the walk reads.
    pub(super) fn table_name(&self) -> &str {
        self.table_name
    }
}

/// Is key order sort order for this part, and must the rows read be checked?
/// `Some(false)`: a column that cannot be NULL, of a type whose keys sort as
/// its values do. `Some(true)`: a part that may be NULL but whose NULL (and
/// empty-string) keys come first in the index, so a walk from the front that
/// meets none has proved there are none. `None`: no such guarantee.
fn ordered_part_rule(part: &Part<'_>, index: usize, total: usize, walk_from_front: bool) -> Option<bool> {
    match part {
        Part::Column(column) => {
            let text = matches!(
                column.data_type,
                DataType::Text | DataType::Varchar(_) | DataType::Char(_)
            );
            let whole = matches!(column.data_type, DataType::Int2 | DataType::Int4 | DataType::Int8);
            if !text && !whole {
                return None;
            }
            if !column.nullable || column.primary_key {
                return Some(false);
            }
            let at_front = index == 0 && walk_from_front;
            ((text && at_front) || (whole && at_front && total == 1)).then_some(true)
        }
        Part::Expr { classes, .. } => {
            let at_front = index == 0 && walk_from_front;
            let text = *classes == class::TEXT;
            let whole = *classes == class::INT;
            ((text && at_front) || (whole && at_front && total == 1)).then_some(true)
        }
    }
}

/// Decide, without reading a row, whether `ORDER BY … LIMIT` (the `Limit`
/// node's input) is served by walking a multi-column or expression index.
pub(super) fn ordered_detect<'p>(executor: &Executor, input: &'p LogicalPlan) -> Result<Option<OrderedSpec<'p>>> {
    if scan::index_range_fast_path_disabled() {
        return Ok(None);
    }
    let Some(storage) = executor.storage() else {
        return Ok(None);
    };
    let (sort_exprs, sort_asc, sort_input, project_wrap) = match input {
        LogicalPlan::Sort {
            input: inner,
            exprs,
            asc,
        } => (exprs, asc, inner.as_ref(), None),
        LogicalPlan::Project {
            input: inner,
            exprs: p_exprs,
            aliases,
            distinct,
            distinct_on,
            source_alias: None,
            ..
        } => match inner.as_ref() {
            LogicalPlan::Sort {
                input: inner2,
                exprs,
                asc,
            } => (
                exprs,
                asc,
                inner2.as_ref(),
                Some((p_exprs.clone(), aliases.clone(), *distinct, distinct_on.clone())),
            ),
            _ => return Ok(None),
        },
        _ => return Ok(None),
    };
    if sort_exprs.is_empty() || sort_exprs.len() != sort_asc.len() || sort_asc.iter().any(|asc| !asc) {
        return Ok(None);
    }
    if let Some((_, _, distinct, distinct_on)) = &project_wrap {
        if *distinct || distinct_on.is_some() {
            return Ok(None);
        }
    }
    let (table_name, alias, schema, projection, as_of, predicate) = match sort_input {
        LogicalPlan::Scan {
            table_name,
            alias,
            schema,
            projection,
            as_of,
        } => (table_name, alias, schema, projection, as_of, None),
        LogicalPlan::FilteredScan {
            table_name,
            alias,
            schema,
            projection,
            predicate,
            as_of,
        } => (table_name, alias, schema, projection, as_of, predicate.as_ref()),
        LogicalPlan::Filter { input, predicate } => match input.as_ref() {
            LogicalPlan::Scan {
                table_name,
                alias,
                schema,
                projection,
                as_of,
            } => (table_name, alias, schema, projection, as_of, Some(predicate)),
            _ => return Ok(None),
        },
        _ => return Ok(None),
    };
    if as_of.is_some() {
        return Ok(None);
    }
    let art = storage.art_indexes();
    let shapes = art.table_index_shapes(table_name);
    if !shapes.iter().any(is_wide) {
        return Ok(None);
    }
    let parameters = executor.parameters();
    let sort_keys: Vec<LogicalExpr> = sort_exprs.iter().map(|e| normalize_expr(e, parameters)).collect();
    let conjuncts = predicate
        .map(|p| normalized_conjuncts(p, parameters))
        .unwrap_or_default();

    for shape in shapes.iter().filter(|shape| is_wide(shape)) {
        // A partial index does not hold every row the sort has to order.
        if shape.form.as_ref().is_some_and(|form| form.predicate().is_some()) {
            continue;
        }
        let Some(parts) = parts_of(shape, schema.as_ref()) else {
            continue;
        };
        let total = parts.len();
        // The leading key parts the predicate fixes: the walk stays inside them.
        let mut bound: Vec<Value> = Vec::new();
        for part in &parts {
            match equality_for(part, &conjuncts) {
                Some(value) => bound.push(value),
                None => break,
            }
        }
        if bound.len() == total {
            continue;
        }
        // The sort keys are a run of key parts that starts at or before the
        // first part the predicate leaves open.
        let Some(start) = (0..=bound.len()).find(|start| {
            sort_keys.len() <= total - start
                && sort_keys
                    .iter()
                    .enumerate()
                    .all(|(i, key)| parts.get(start + i).is_some_and(|part| is_part(part, key)))
        }) else {
            continue;
        };
        let last = start + sort_keys.len();
        if last <= bound.len() {
            // Every sort key is fixed by the predicate: nothing to order.
            continue;
        }
        let walk_from_front = bound.is_empty();
        let mut checked_parts = Vec::new();
        let mut in_order = true;
        for (index, part) in parts.iter().enumerate().take(last).skip(bound.len()) {
            match ordered_part_rule(part, index, total, walk_from_front) {
                Some(true) => checked_parts.push(index),
                Some(false) => {}
                None => {
                    in_order = false;
                    break;
                }
            }
        }
        if !in_order {
            continue;
        }
        if !gates_pass(executor, storage, table_name)? {
            return Ok(None);
        }
        // The index must hold every row: one left out would vanish from the answer.
        let Some(entry_count) = art.index_entry_count(&shape.name).and_then(|n| usize::try_from(n).ok()) else {
            continue;
        };
        if entry_count != storage.count_table_rows(table_name)? {
            continue;
        }
        let (lower, upper) = if bound.is_empty() {
            (None, None)
        } else {
            let prefix = ArtIndexManager::encode_key_prefix(&bound, total);
            let mut end = prefix.clone();
            end.push(1);
            (Some((prefix, true)), Some((end, false)))
        };
        let order_display = (start..last)
            .map(|i| part_label(shape, i))
            .collect::<Vec<_>>()
            .join(", ");
        return Ok(Some(OrderedSpec {
            table_name,
            alias,
            schema,
            projection,
            predicate,
            project_wrap,
            index_name: shape.name.clone(),
            order_display,
            lower,
            upper,
            checked_parts,
            entry_count,
        }));
    }
    Ok(None)
}

/// Is `value` one whose key does not sort where the value does (NULL, an empty
/// string, a string holding a NUL)?
fn sorts_out_of_place(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::String(s) => s.is_empty() || s.contains('\0'),
        _ => false,
    }
}

/// `ORDER BY key parts ASC LIMIT k` by walking the index in key order: no sort,
/// and only as many rows read as the limit needs. Detection is
/// [`ordered_detect`]; `None` hands the query to the ordinary sort.
pub(super) fn try_ordered_topk(
    executor: &Executor,
    input: &LogicalPlan,
    limit: usize,
    offset: usize,
) -> Result<Option<Box<dyn PhysicalOperator>>> {
    let Some(spec) = ordered_detect(executor, input)? else {
        return Ok(None);
    };
    let Some(storage) = executor.storage() else {
        return Ok(None);
    };
    let art = storage.art_indexes();
    let Some(shape) = art
        .table_index_shapes(spec.table_name)
        .into_iter()
        .find(|shape| shape.name == spec.index_name)
    else {
        return Ok(None);
    };
    let source_name = spec.alias.as_ref().unwrap_or(spec.table_name);
    let actual_schema = Arc::new(scan::schema_with_source(
        spec.schema.as_ref(),
        source_name,
        spec.table_name,
    ));
    if spec
        .predicate
        .is_some_and(|predicate| executor.expr_has_correlated_subquery(predicate, &actual_schema))
    {
        return Ok(None);
    }
    let materialized = spec.predicate.map(|p| executor.materialize_subqueries(p)).transpose()?;
    let residual = materialized.as_ref().map(|predicate| {
        let evaluator = crate::sql::Evaluator::with_parameters(actual_schema.clone(), executor.parameters().to_vec());
        let bound = evaluator.bind(predicate.clone());
        (evaluator, bound)
    });
    // The value of key part `index` in a row read.
    let part_value = |index: usize, tuple: &Tuple| -> Option<Value> {
        match &shape.form {
            Some(form) => form
                .key_for_tuple(spec.schema.as_ref(), tuple)
                .and_then(|key| key.get(index).cloned()),
            None => shape
                .columns
                .get(index)
                .and_then(|name| spec.schema.get_column_index(name))
                .and_then(|i| tuple.values.get(i).cloned()),
        }
    };

    let k_target = limit.saturating_add(offset);
    let total = spec.entry_count;
    let mut fetch_k = k_target.saturating_mul(2).max(k_target.saturating_add(8)).min(total);
    let mut ordered: Vec<Tuple>;
    loop {
        ordered = Vec::with_capacity(k_target.min(fetch_k));
        let pairs = art.index_range_scan(
            &spec.index_name,
            spec.lower.as_ref().map(|(key, inclusive)| (key.as_slice(), *inclusive)),
            spec.upper.as_ref().map(|(key, inclusive)| (key.as_slice(), *inclusive)),
            Some(fetch_k),
        );
        let exhausted = pairs.len() < fetch_k;
        for (_, row_id) in &pairs {
            let Some(tuple) = storage.get_row_by_id(spec.table_name, *row_id, spec.schema.as_ref())? else {
                continue;
            };
            // Before the predicate: the proof that no such key exists further
            // on rests on the walk having met none from its start.
            for index in &spec.checked_parts {
                if part_value(*index, &tuple).as_ref().is_none_or(sorts_out_of_place) {
                    return Ok(None);
                }
            }
            if let Some((evaluator, predicate)) = &residual {
                match evaluator.evaluate(predicate, &tuple)? {
                    Value::Boolean(true) => {}
                    Value::Boolean(false) | Value::Null => continue,
                    other => {
                        return Err(crate::Error::query_execution(format!(
                            "Filter predicate must evaluate to boolean, got: {:?}",
                            other
                        )));
                    }
                }
            }
            ordered.push(tuple);
            if ordered.len() >= k_target {
                break;
            }
        }
        if exhausted || fetch_k >= total || ordered.len() >= k_target {
            break;
        }
        fetch_k = fetch_k.saturating_mul(4).min(total);
    }
    ordered.truncate(k_target);
    tracing::debug!(
        "ordered index scan: '{}' on {} (ORDER BY {}) served {} of {} requested rows (no sort)",
        spec.index_name,
        spec.table_name,
        spec.order_display,
        ordered.len(),
        k_target
    );

    let scan_op: Box<dyn PhysicalOperator> = Box::new(
        ScanOperator::new(
            spec.table_name.clone(),
            actual_schema,
            spec.projection.clone(),
            ordered,
            executor.parameters().to_vec(),
        )
        .with_timeout(executor.timeout_ctx()),
    );
    let after_project: Box<dyn PhysicalOperator> = match spec.project_wrap {
        Some((exprs, aliases, _, _)) => {
            let materialised: Vec<LogicalExpr> = exprs
                .iter()
                .map(|e| executor.materialize_subqueries(e))
                .collect::<Result<Vec<_>>>()?;
            Box::new(
                ProjectOperator::new(scan_op, materialised, aliases, false, executor.parameters().to_vec())
                    .with_timeout(executor.timeout_ctx()),
            )
        }
        None => scan_op,
    };
    Ok(Some(Box::new(
        LimitOperator::new(after_project, limit, offset).with_timeout(executor.timeout_ctx()),
    )))
}
