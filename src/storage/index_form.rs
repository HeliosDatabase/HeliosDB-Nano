//! Index forms: what a secondary index holds beyond a list of columns.
//!
//! An ordinary index keys every row of its table on one or more columns. Two
//! forms change that, and both are described here so that index maintenance
//! ([`super::ArtIndexManager`]) and index selection (the executor) read ONE
//! definition:
//!
//! * a **partial** index (`CREATE INDEX … WHERE predicate`) holds only the rows
//!   for which its predicate is true;
//! * an **expression** index (`CREATE INDEX … (lower(email))`) keys a row on the
//!   value an expression computes from it.
//!
//! A plain multi-column index needs no form: its key is its column list.
//!
//! The durable definition keeps the SQL text (`IndexOption::Expression.key`,
//! `IndexOption::Predicate.sql`); [`IndexForm::compile`] turns it into the
//! expressions evaluated on every write and compared with a query's predicate.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

use crate::sql::logical_plan::IndexOption;
use crate::sql::LogicalExpr;
use crate::{Column, Error, Result, Schema, Tuple, Value};

/// The words every refusal of a non-immutable index expression or predicate
/// carries. The PostgreSQL wire maps them to SQLSTATE `42P17`.
pub const NOT_IMMUTABLE_MARKER: &str = "must be marked IMMUTABLE";

/// The words the refusal of an index form this build cannot keep carries, with
/// [`crate::error::UNSUPPORTED_STATEMENT_KIND_MARKER`] (SQLSTATE `0A000`).
pub const FORM_UNSUPPORTED_PREFIX: &str = "this index form";

/// Value classes an expression key part has produced so far (a bit set).
pub mod class {
    /// A whole number (`smallint`, `integer`, `bigint`), stored as `bigint`.
    pub const INT: u8 = 1;
    /// Text.
    pub const TEXT: u8 = 2;
    /// A boolean.
    pub const BOOL: u8 = 4;
    /// A UUID.
    pub const UUID: u8 = 8;
    /// Anything else: kept and compared byte for byte, never probed by a query.
    pub const OTHER: u8 = 16;
}

/// One key part of a form index.
#[derive(Debug)]
pub enum FormPart {
    /// A table column.
    Column {
        /// The column's name.
        name: String,
        /// Its position among [`IndexForm::referenced_columns`].
        slot: usize,
    },
    /// An expression over the row.
    Expr {
        /// The normalised expression ([`normalize_expr`]).
        expr: LogicalExpr,
        /// The classes of the values it has produced ([`class`]).
        seen: AtomicU8,
    },
}

/// The compiled form of one index. Shared by the index's registry entry and by
/// every reader that asks what the index can answer.
#[derive(Debug)]
pub struct IndexForm {
    parts: Vec<FormPart>,
    part_sql: Vec<String>,
    predicate: Option<LogicalExpr>,
    predicate_sql: Option<String>,
    referenced: Vec<String>,
    schema: Arc<Schema>,
}

/// What a durable index definition asks for, read from its options.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexShape {
    /// `CREATE UNIQUE INDEX`.
    pub unique: bool,
    /// The key parts as SQL text: column names, or expressions.
    pub parts: Vec<String>,
    /// True when at least one part is an expression (the parts then come from
    /// the definition's key text and are parsed, not taken as names).
    pub expression: bool,
    /// The `WHERE` predicate as SQL text.
    pub predicate: Option<String>,
    /// The access method the definition names (`btree` when none).
    pub method: String,
}

impl IndexShape {
    /// Read the shape of a definition: its recorded column, its type tag and its
    /// options.
    pub fn of(column_name: &str, index_type: Option<&str>, options: &[IndexOption]) -> Self {
        let unique = options.iter().any(|o| matches!(o, IndexOption::Unique(true)));
        let predicate = options.iter().find_map(|o| match o {
            IndexOption::Predicate { sql, .. } => Some(sql.clone()),
            _ => None,
        });
        let expression = options.iter().find_map(|o| match o {
            IndexOption::Expression { method, key, .. } => Some((method.clone(), key.clone())),
            _ => None,
        });
        if let Some((method, key)) = expression {
            return Self {
                unique,
                parts: vec![key],
                expression: true,
                predicate,
                method,
            };
        }
        let columns = options
            .iter()
            .find_map(|o| match o {
                IndexOption::UniqueColumns(columns) if !columns.is_empty() => Some(columns.clone()),
                _ => None,
            })
            .unwrap_or_else(|| vec![column_name.to_string()]);
        Self {
            unique,
            parts: columns,
            expression: false,
            predicate,
            method: index_type.unwrap_or("btree").to_string(),
        }
    }

    /// Does this shape need a compiled [`IndexForm`] (a predicate or an
    /// expression)? A plain column list does not.
    pub fn needs_form(&self) -> bool {
        self.expression || self.predicate.is_some()
    }

    /// Is the access method one the ordered tree serves (`btree`, `hash`, `art`)?
    pub fn is_tree_method(&self) -> bool {
        matches!(self.method.as_str(), "btree" | "hash" | "art")
    }
}

/// Does a definition with this type tag and these options have a tree behind
/// it? True for the ordinary tags, and for an expression index whose access
/// method is a tree's (`btree`, `hash`); false for a vector index and for an
/// index that is only recorded (`gin`, `gist`, an expression index `USING gin`).
pub fn is_tree_definition(index_type: Option<&str>, options: &[IndexOption]) -> bool {
    match index_type {
        Some("expression") => IndexShape::of("", index_type, options).is_tree_method(),
        other => super::index_family(other) == Some(super::IndexFamily::Art),
    }
}

/// [`super::index_family`] of a whole definition: an expression index over a
/// tree method belongs to the tree family, whatever its tag says.
pub fn definition_family(definition: &super::PersistedIndexDefinition) -> Option<super::IndexFamily> {
    if is_tree_definition(definition.index_type.as_deref(), &definition.options) {
        return Some(super::IndexFamily::Art);
    }
    super::index_family(definition.index_type.as_deref())
}

/// Register the tree of a durable definition that is more than one plain column
/// or a plain UNIQUE key: an ordinary multi-column index, a partial index, an
/// expression index. The tree is registered empty, under the recorded name; the
/// caller backfills it. `Ok(false)`: the definition is none of these, and the
/// caller registers it as it always did.
///
/// An expression index recorded by a release that did not build one comes back
/// through here too: its key text is compiled, and from then on it is kept.
pub(crate) fn register_shaped_definition(
    catalog: &super::Catalog<'_>,
    storage: &super::StorageEngine,
    index_name: &str,
    definition: &super::PersistedIndexDefinition,
) -> Result<bool> {
    let shape = IndexShape::of(
        &definition.column_name,
        definition.index_type.as_deref(),
        &definition.options,
    );
    if !is_tree_definition(definition.index_type.as_deref(), &definition.options) {
        return Ok(false);
    }
    if !shape.needs_form() && (shape.unique || shape.parts.len() == 1) {
        return Ok(false);
    }
    let art = storage.art_indexes();
    let table = definition.table_name.as_str();
    let schema = catalog.get_table_schema(table)?;
    if shape.needs_form() {
        let form = Arc::new(IndexForm::compile(catalog, table, &schema, &shape)?);
        catalog.evict_mint_squatting_persisted_name(index_name, definition, form.part_names(), shape.unique);
        if !art.index_exists(index_name) {
            art.create_form_index(index_name, table, shape.unique, form)
                .map_err(|e| Error::storage(format!("index '{index_name}': {e}")))?;
        }
    } else {
        for column in &shape.parts {
            if schema.get_column(column).is_none() {
                return Err(Error::storage(format!(
                    "index '{index_name}': column '{column}' not found in table '{table}'"
                )));
            }
        }
        catalog.evict_mint_squatting_persisted_name(index_name, definition, &shape.parts, false);
        if !art.index_exists(index_name) {
            art.create_manual_index(index_name, table, &shape.parts)
                .map_err(|e| Error::storage(format!("index '{index_name}': {e}")))?;
        }
    }
    Ok(true)
}

/// The UPDATE-side check of the partial and expression UNIQUE indexes of
/// `table`: refuse (23505) a row whose new image takes a key another row holds
/// or another row of the same statement moved onto, after waiting for any
/// transaction that took that key out and has not ended.
///
/// `seen` is the statement's record of the keys its rows moved onto. Main only:
/// a branch's rows are in no index, and its keys are judged when it is merged.
pub(crate) fn enforce_form_unique_on_update(
    storage: &super::StorageEngine,
    table: &str,
    schema: &Schema,
    old_tuple: &Tuple,
    new_tuple: &Tuple,
    seen: &mut Vec<(Vec<String>, Vec<Value>, Option<u64>)>,
    txn: Option<&super::Transaction>,
) -> Result<()> {
    let art = storage.art_indexes();
    if !art.has_form_unique_index(table) {
        return Ok(());
    }
    let row_id = new_tuple.row_id.or(old_tuple.row_id);
    let own = txn.map_or(u64::MAX, super::Transaction::transaction_id);
    loop {
        match art.form_unique_check_for_update(table, schema, old_tuple, new_tuple, row_id, own, seen) {
            super::art_manager::FormUniqueCheck::Free => return Ok(()),
            super::art_manager::FormUniqueCheck::Taken(index) => {
                return Err(Error::constraint_violation(format!(
                    "Duplicate key value violates UNIQUE constraint \"{index}\""
                )));
            }
            super::art_manager::FormUniqueCheck::Held(hold) => storage.wait_out_key_hold(table, &hold, txn)?,
        }
    }
}

/// How a tree-backed index definition reads as SQL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexDefinitionText {
    /// `CREATE UNIQUE INDEX`.
    pub unique: bool,
    /// The access method (`btree`, `hash`).
    pub method: String,
    /// The key list as it goes between the parentheses.
    pub key: String,
    /// The `WHERE` predicate, without the keyword.
    pub predicate: Option<String>,
}

impl IndexDefinitionText {
    /// Read a definition. `quote` spells a column name.
    pub fn of(definition: &super::PersistedIndexDefinition, quote: &dyn Fn(&str) -> String) -> Self {
        let tag = definition.index_type.as_deref();
        let shape = IndexShape::of(&definition.column_name, tag, &definition.options);
        let key = if shape.expression {
            shape.parts.join(", ")
        } else {
            shape.parts.iter().map(|c| quote(c)).collect::<Vec<_>>().join(", ")
        };
        let method = if shape.expression {
            shape.method.clone()
        } else if tag == Some("hash") {
            "hash".to_string()
        } else {
            "btree".to_string()
        };
        Self {
            unique: shape.unique,
            method,
            key,
            predicate: shape.predicate,
        }
    }

    /// ` WHERE (predicate)`, or nothing.
    pub fn where_clause(&self) -> String {
        self.predicate
            .as_ref()
            .map(|predicate| format!(" WHERE ({predicate})"))
            .unwrap_or_default()
    }
}

/// Parse one SQL expression.
fn parse_expr(sql: &str) -> Result<sqlparser::ast::Expr> {
    let dialect = sqlparser::dialect::PostgreSqlDialect {};
    let mut parser = sqlparser::parser::Parser::new(&dialect)
        .try_with_sql(sql)
        .map_err(|e| Error::query_execution(format!("index expression '{sql}' does not parse: {e}")))?;
    let expr = parser
        .parse_expr()
        .map_err(|e| Error::query_execution(format!("index expression '{sql}' does not parse: {e}")))?;
    if parser.peek_token().token != sqlparser::tokenizer::Token::EOF {
        return Err(Error::query_execution(format!(
            "index expression '{sql}' does not parse: unexpected text after the expression"
        )));
    }
    Ok(expr)
}

/// Parse a comma-separated key list (`lower(email), (a + b), c`).
fn parse_key_list(sql: &str) -> Result<Vec<sqlparser::ast::Expr>> {
    let dialect = sqlparser::dialect::PostgreSqlDialect {};
    let mut parser = sqlparser::parser::Parser::new(&dialect)
        .try_with_sql(sql)
        .map_err(|e| Error::query_execution(format!("index key '{sql}' does not parse: {e}")))?;
    let exprs = parser
        .parse_comma_separated(|parser| parser.parse_expr())
        .map_err(|e| Error::query_execution(format!("index key '{sql}' does not parse: {e}")))?;
    if parser.peek_token().token != sqlparser::tokenizer::Token::EOF {
        return Err(Error::query_execution(format!(
            "index key '{sql}' does not parse: unexpected text after the key"
        )));
    }
    Ok(exprs)
}

/// Functions whose result is not fixed by their arguments: an index over one
/// would hold values no later evaluation agrees with.
const NOT_IMMUTABLE_FUNCTIONS: &[&str] = &[
    "now",
    "current_timestamp",
    "current_date",
    "current_time",
    "localtime",
    "localtimestamp",
    "clock_timestamp",
    "statement_timestamp",
    "transaction_timestamp",
    "timeofday",
    "random",
    "setseed",
    "rand",
    "gen_random_uuid",
    "uuid_generate_v4",
    "uuid_generate_v1",
    "nextval",
    "currval",
    "lastval",
    "setval",
    "last_insert_id",
    "last_insert_rowid",
    "txid_current",
    "pg_backend_pid",
    "connection_id",
    "current_user",
    "session_user",
    "user",
    "current_role",
    "current_schema",
    "current_schemas",
    "current_database",
    "current_catalog",
    "current_setting",
    "set_config",
    "current_tenant",
    "current_branch",
    "version",
    "pg_sleep",
    "sleep",
    "unix_timestamp",
    "utc_timestamp",
    "systimestamp",
    "sysdatetime",
    "getutcdate",
    "current_tenant_id",
    "current_user_id",
    "pg_current_wal_lsn",
    "pg_current_wal_insert_lsn",
    "pg_current_wal_flush_lsn",
    "pg_current_xact_id",
    "predict",
    "infer",
    "generate",
    "sysdate",
    "curdate",
    "curtime",
    "getdate",
    "changes",
    "total_changes",
];

/// Call `each` on every node of `expr`, parents first.
pub fn visit_expr(expr: &LogicalExpr, each: &mut dyn FnMut(&LogicalExpr)) {
    each(expr);
    match expr {
        LogicalExpr::BinaryExpr { left, right, .. } => {
            visit_expr(left, each);
            visit_expr(right, each);
        }
        LogicalExpr::UnaryExpr { expr, .. } | LogicalExpr::Cast { expr, .. } | LogicalExpr::IsNull { expr, .. } => {
            visit_expr(expr, each);
        }
        LogicalExpr::AggregateFunction { args, .. } | LogicalExpr::ScalarFunction { args, .. } => {
            for arg in args {
                visit_expr(arg, each);
            }
        }
        LogicalExpr::Case {
            expr,
            when_then,
            else_result,
        } => {
            if let Some(expr) = expr {
                visit_expr(expr, each);
            }
            for (when, then) in when_then {
                visit_expr(when, each);
                visit_expr(then, each);
            }
            if let Some(else_result) = else_result {
                visit_expr(else_result, each);
            }
        }
        LogicalExpr::Between { expr, low, high, .. } => {
            visit_expr(expr, each);
            visit_expr(low, each);
            visit_expr(high, each);
        }
        LogicalExpr::InList { expr, list, .. } => {
            visit_expr(expr, each);
            for item in list {
                visit_expr(item, each);
            }
        }
        LogicalExpr::InSet { expr, .. } | LogicalExpr::InSubquery { expr, .. } => visit_expr(expr, each),
        LogicalExpr::ArraySubscript { array, index } => {
            visit_expr(array, each);
            visit_expr(index, each);
        }
        LogicalExpr::Tuple { items } => {
            for item in items {
                visit_expr(item, each);
            }
        }
        LogicalExpr::WindowFunction {
            args,
            partition_by,
            order_by,
            ..
        } => {
            for arg in args.iter().chain(partition_by) {
                visit_expr(arg, each);
            }
            for (key, _) in order_by {
                visit_expr(key, each);
            }
        }
        LogicalExpr::Column { .. }
        | LogicalExpr::Literal(_)
        | LogicalExpr::ScalarSubquery { .. }
        | LogicalExpr::Exists { .. }
        | LogicalExpr::DefaultValue
        | LogicalExpr::Wildcard
        | LogicalExpr::Parameter { .. }
        | LogicalExpr::NewRow { .. }
        | LogicalExpr::OldRow { .. }
        | LogicalExpr::BoundColumn { .. } => {}
    }
}

/// A function's name as two spellings of one call share it: lower case,
/// without the `pg_catalog.` qualifier.
fn function_name(fun: &str) -> String {
    let lower = fun.to_ascii_lowercase();
    match lower.strip_prefix("pg_catalog.") {
        Some(bare) => bare.to_string(),
        None => lower,
    }
}

fn boxed(expr: &LogicalExpr, parameters: &[Value]) -> Box<LogicalExpr> {
    Box::new(normalize_expr(expr, parameters))
}

/// The one spelling two expressions are compared in: column references lose
/// their table qualifier, function names are lower case, whole-number literals
/// are `bigint`, and a `$n` parameter is the value bound to it (a cached plan
/// carries the literals of the statement it was planned from as parameters).
pub fn normalize_expr(expr: &LogicalExpr, parameters: &[Value]) -> LogicalExpr {
    match expr {
        LogicalExpr::Column { name, .. } | LogicalExpr::BoundColumn { name, .. } => LogicalExpr::Column {
            table: None,
            name: name.clone(),
        },
        LogicalExpr::Literal(value) => LogicalExpr::Literal(canonical_value(value.clone())),
        LogicalExpr::Parameter { index } => match index.checked_sub(1).and_then(|i| parameters.get(i)) {
            Some(value) => LogicalExpr::Literal(canonical_value(value.clone())),
            None => expr.clone(),
        },
        LogicalExpr::BinaryExpr { left, op, right } => LogicalExpr::BinaryExpr {
            left: boxed(left, parameters),
            op: *op,
            right: boxed(right, parameters),
        },
        LogicalExpr::UnaryExpr { op, expr } => LogicalExpr::UnaryExpr {
            op: *op,
            expr: boxed(expr, parameters),
        },
        LogicalExpr::ScalarFunction { fun, args } => LogicalExpr::ScalarFunction {
            fun: function_name(fun),
            args: args.iter().map(|arg| normalize_expr(arg, parameters)).collect(),
        },
        LogicalExpr::Case {
            expr,
            when_then,
            else_result,
        } => LogicalExpr::Case {
            expr: expr.as_ref().map(|e| boxed(e, parameters)),
            when_then: when_then
                .iter()
                .map(|(when, then)| (normalize_expr(when, parameters), normalize_expr(then, parameters)))
                .collect(),
            else_result: else_result.as_ref().map(|e| boxed(e, parameters)),
        },
        LogicalExpr::Cast { expr, data_type } => LogicalExpr::Cast {
            expr: boxed(expr, parameters),
            data_type: data_type.clone(),
        },
        LogicalExpr::IsNull { expr, is_null } => LogicalExpr::IsNull {
            expr: boxed(expr, parameters),
            is_null: *is_null,
        },
        LogicalExpr::Between {
            expr,
            low,
            high,
            negated,
        } => LogicalExpr::Between {
            expr: boxed(expr, parameters),
            low: boxed(low, parameters),
            high: boxed(high, parameters),
            negated: *negated,
        },
        LogicalExpr::InList { expr, list, negated } => LogicalExpr::InList {
            expr: boxed(expr, parameters),
            list: list.iter().map(|item| normalize_expr(item, parameters)).collect(),
            negated: *negated,
        },
        LogicalExpr::ArraySubscript { array, index } => LogicalExpr::ArraySubscript {
            array: boxed(array, parameters),
            index: boxed(index, parameters),
        },
        LogicalExpr::Tuple { items } => LogicalExpr::Tuple {
            items: items.iter().map(|item| normalize_expr(item, parameters)).collect(),
        },
        other => other.clone(),
    }
}

/// The stored spelling of an expression key value: every whole number is a
/// `bigint`, so the key does not depend on which integer width the expression
/// happened to compute in.
pub fn canonical_value(value: Value) -> Value {
    match value {
        Value::Int2(v) => Value::Int8(i64::from(v)),
        Value::Int4(v) => Value::Int8(i64::from(v)),
        other => other,
    }
}

/// The class ([`class`]) of a canonical value; `0` for NULL.
pub(crate) fn value_class(value: &Value) -> u8 {
    match value {
        Value::Null => 0,
        Value::Int2(_) | Value::Int4(_) | Value::Int8(_) => class::INT,
        Value::String(_) => class::TEXT,
        Value::Boolean(_) => class::BOOL,
        Value::Uuid(_) => class::UUID,
        _ => class::OTHER,
    }
}

/// Is `name` (lower case) a user-defined function of this database?
fn is_user_defined(name: &str) -> bool {
    let base = name.strip_prefix("public.").unwrap_or(name);
    !base.contains('.') && matches!(crate::sql::udf_bridge::resolve_owned(base), Ok(Some(_)))
}

/// Refuse what an index expression or predicate may not hold: a subquery, an
/// aggregate or window function, a parameter, a function whose result is not
/// fixed by its arguments. `what` is `expression` or `predicate`.
fn check_immutable(expr: &LogicalExpr, what: &str) -> Result<()> {
    let mut refusal: Option<Error> = None;
    visit_expr(expr, &mut |node| {
        if refusal.is_some() {
            return;
        }
        refusal = match node {
            LogicalExpr::ScalarSubquery { .. } | LogicalExpr::InSubquery { .. } | LogicalExpr::Exists { .. } => {
                Some(Error::query_execution(format!(
                    "cannot use subquery in index {what}: an index {what} {NOT_IMMUTABLE_MARKER}"
                )))
            }
            LogicalExpr::AggregateFunction { .. } | LogicalExpr::WindowFunction { .. } => {
                Some(Error::query_execution(format!(
                    "aggregate and window functions are not allowed in an index {what}: an index {what} \
                     {NOT_IMMUTABLE_MARKER}"
                )))
            }
            LogicalExpr::Parameter { .. }
            | LogicalExpr::DefaultValue
            | LogicalExpr::Wildcard
            | LogicalExpr::NewRow { .. }
            | LogicalExpr::OldRow { .. } => Some(Error::query_execution(format!(
                "an index {what} can read only the row's columns and constants: an index {what} \
                 {NOT_IMMUTABLE_MARKER}"
            ))),
            LogicalExpr::ScalarFunction { fun, .. }
                if NOT_IMMUTABLE_FUNCTIONS.contains(&function_name(fun).as_str()) =>
            {
                Some(Error::query_execution(format!(
                    "functions in index {what} {NOT_IMMUTABLE_MARKER}: {}() is not",
                    function_name(fun)
                )))
            }
            // A user-defined function runs SQL of its own and can be replaced:
            // nothing says its result is fixed by its arguments.
            LogicalExpr::ScalarFunction { fun, .. } if is_user_defined(&function_name(fun)) => {
                Some(Error::query_execution(format!(
                    "functions in index {what} {NOT_IMMUTABLE_MARKER}: the user-defined function {}() is not",
                    function_name(fun)
                )))
            }
            _ => None,
        };
    });
    match refusal {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

/// The table columns `expr` reads, in first-use order, appended to `into`.
fn collect_columns(expr: &LogicalExpr, into: &mut Vec<String>) {
    visit_expr(expr, &mut |node| {
        if let LogicalExpr::Column { name, .. } | LogicalExpr::BoundColumn { name, .. } = node {
            if !into.iter().any(|known| known == name) {
                into.push(name.clone());
            }
        }
    });
}

impl IndexForm {
    /// Compile the form `shape` describes for a table with `table_schema`.
    ///
    /// Fails when a key part or the predicate does not parse, reads a column the
    /// table does not have, or is not immutable.
    pub fn compile(
        catalog: &super::Catalog<'_>,
        table_name: &str,
        table_schema: &Schema,
        shape: &IndexShape,
    ) -> Result<Self> {
        let planner = crate::sql::Planner::with_catalog(catalog);
        let mut logical_parts: Vec<(LogicalExpr, String)> = Vec::new();
        if shape.expression {
            for key in &shape.parts {
                for parsed in parse_key_list(key)? {
                    let mut inner = &parsed;
                    while let sqlparser::ast::Expr::Nested(nested) = inner {
                        inner = nested;
                    }
                    let logical = normalize_expr(&planner.convert_expr_to_logical(inner, Some(table_schema))?, &[]);
                    logical_parts.push((logical, inner.to_string()));
                }
            }
        } else {
            for column in &shape.parts {
                logical_parts.push((
                    LogicalExpr::Column {
                        table: None,
                        name: column.clone(),
                    },
                    column.clone(),
                ));
            }
        }
        if logical_parts.is_empty() {
            return Err(Error::query_execution("At least one column required for index"));
        }
        let predicate = match &shape.predicate {
            Some(sql) => {
                let parsed = parse_expr(sql)?;
                Some(normalize_expr(
                    &planner.convert_expr_to_logical(&parsed, Some(table_schema))?,
                    &[],
                ))
            }
            None => None,
        };

        let mut referenced: Vec<String> = Vec::new();
        for (expr, _) in &logical_parts {
            collect_columns(expr, &mut referenced);
            if !matches!(expr, LogicalExpr::Column { .. }) {
                check_immutable(expr, "expression")?;
            }
        }
        if let Some(predicate) = &predicate {
            collect_columns(predicate, &mut referenced);
            check_immutable(predicate, "predicate")?;
        }
        let mut columns: Vec<Column> = Vec::with_capacity(referenced.len());
        for name in &referenced {
            let column = table_schema.get_column(name).ok_or_else(|| {
                Error::query_execution(format!("Column '{}' not found in table '{}'", name, table_name))
            })?;
            if column.storage_mode != crate::ColumnStorageMode::Default {
                return Err(Error::query_execution(format!(
                    "{FORM_UNSUPPORTED_PREFIX} (an expression or partial index) over column \"{}\", which has a \
                     column storage mode, {}",
                    name,
                    crate::error::UNSUPPORTED_STATEMENT_KIND_MARKER
                )));
            }
            let mut column = column.clone();
            // The form's own row has no table qualifier to resolve against.
            column.source_table = None;
            column.source_table_name = None;
            columns.push(column);
        }
        // The names as the table spells them, so a row's values are found by them.
        let referenced: Vec<String> = columns.iter().map(|c| c.name.clone()).collect();

        let mut parts = Vec::with_capacity(logical_parts.len());
        let mut part_sql = Vec::with_capacity(logical_parts.len());
        for (expr, sql) in logical_parts {
            match &expr {
                LogicalExpr::Column { name, .. } => {
                    let slot = columns
                        .iter()
                        .position(|c| c.name.eq_ignore_ascii_case(name))
                        .ok_or_else(|| Error::internal("index column lost while compiling its form"))?;
                    let name = columns
                        .get(slot)
                        .map(|c| c.name.clone())
                        .unwrap_or_else(|| name.clone());
                    part_sql.push(name.clone());
                    parts.push(FormPart::Column { name, slot });
                }
                _ => {
                    part_sql.push(sql);
                    parts.push(FormPart::Expr {
                        expr,
                        seen: AtomicU8::new(0),
                    });
                }
            }
        }
        Ok(Self {
            parts,
            part_sql,
            predicate,
            predicate_sql: shape.predicate.clone(),
            referenced,
            schema: Arc::new(Schema::new(columns)),
        })
    }

    /// The key parts, in key order.
    pub fn parts(&self) -> &[FormPart] {
        &self.parts
    }

    /// The key parts as text, in key order: what the registry lists as the
    /// index's columns.
    pub fn part_names(&self) -> &[String] {
        &self.part_sql
    }

    /// The `WHERE` predicate, normalised.
    pub fn predicate(&self) -> Option<&LogicalExpr> {
        self.predicate.as_ref()
    }

    /// The `WHERE` predicate as the definition spells it.
    pub fn predicate_sql(&self) -> Option<&str> {
        self.predicate_sql.as_deref()
    }

    /// Every table column the key or the predicate reads.
    pub fn referenced_columns(&self) -> &[String] {
        &self.referenced
    }

    /// The column a key part is, with its declared type; `None` for an
    /// expression part.
    pub fn part_column(&self, part: usize) -> Option<&Column> {
        match self.parts.get(part)? {
            FormPart::Column { slot, .. } => self.schema.columns.get(*slot),
            FormPart::Expr { .. } => None,
        }
    }

    /// The key of the row `values` (one per referenced column, in that order).
    /// `Ok(None)`: the predicate is not true for it, so the index has no entry.
    fn key_of(&self, values: Vec<Value>) -> Result<Option<Vec<Value>>> {
        let row = Tuple::new(values);
        let evaluator = crate::sql::Evaluator::new(Arc::clone(&self.schema));
        if let Some(predicate) = &self.predicate {
            if !matches!(evaluator.evaluate(predicate, &row)?, Value::Boolean(true)) {
                return Ok(None);
            }
        }
        let mut key = Vec::with_capacity(self.parts.len());
        for part in &self.parts {
            match part {
                FormPart::Column { slot, .. } => key.push(row.values.get(*slot).cloned().unwrap_or(Value::Null)),
                FormPart::Expr { expr, seen } => {
                    let value = canonical_value(evaluator.evaluate(expr, &row)?);
                    let bit = value_class(&value);
                    if bit != 0 && (seen.load(Ordering::Relaxed) & bit) == 0 {
                        seen.fetch_or(bit, Ordering::Relaxed);
                    }
                    key.push(value);
                }
            }
        }
        Ok(Some(key))
    }

    /// The key of a row given as its table schema and tuple, with the error an
    /// expression raised (a `CREATE INDEX` reports it).
    pub fn strict_key_for_tuple(&self, schema: &Schema, tuple: &Tuple) -> Result<Option<Vec<Value>>> {
        let mut values = Vec::with_capacity(self.referenced.len());
        for name in &self.referenced {
            let Some(value) = schema.get_column_index(name).and_then(|i| tuple.values.get(i)) else {
                return Ok(None);
            };
            values.push(value.clone());
        }
        self.key_of(values)
    }

    /// The key of a row given as its table schema and tuple. `None`: the row has
    /// no entry (its predicate is not true, a column it reads is absent, or an
    /// expression could not be evaluated for it).
    pub fn key_for_tuple(&self, schema: &Schema, tuple: &Tuple) -> Option<Vec<Value>> {
        self.strict_key_for_tuple(schema, tuple).ok().flatten()
    }

    /// [`Self::key_for_tuple`] for a row given as a map from column name to value.
    pub fn key_for_map(&self, row: &HashMap<String, Value>) -> Option<Vec<Value>> {
        let mut values = Vec::with_capacity(self.referenced.len());
        for name in &self.referenced {
            values.push(row.get(name)?.clone());
        }
        self.key_of(values).ok().flatten()
    }

    /// The index was filled with keys this form did not compute (a snapshot):
    /// what its expression parts hold is unknown, so no query probes them.
    pub(crate) fn mark_classes_unknown(&self) {
        for part in &self.parts {
            if let FormPart::Expr { seen, .. } = part {
                seen.fetch_or(class::OTHER, Ordering::Relaxed);
            }
        }
    }

    /// Does the form have a key part that is an expression?
    pub fn has_expression_part(&self) -> bool {
        self.parts.iter().any(|part| matches!(part, FormPart::Expr { .. }))
    }

    /// The classes of the values expression part `part` has produced
    /// ([`class`]); `0` for a column part or one that has produced only NULLs.
    pub(crate) fn part_classes(&self, part: usize) -> u8 {
        match self.parts.get(part) {
            Some(FormPart::Expr { seen, .. }) => seen.load(Ordering::Relaxed),
            _ => 0,
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]
    use super::*;

    #[test]
    fn shape_reads_composite_partial_and_expression_definitions() {
        let plain = IndexShape::of("a", Some("art"), &[]);
        assert_eq!(plain.parts, vec!["a".to_string()]);
        assert!(!plain.needs_form());

        let composite = IndexShape::of(
            "a",
            Some("art"),
            &[IndexOption::UniqueColumns(vec!["a".into(), "b".into()])],
        );
        assert_eq!(composite.parts, vec!["a".to_string(), "b".to_string()]);
        assert!(!composite.unique);
        assert!(!composite.needs_form());

        let partial = IndexShape::of(
            "email",
            Some("art"),
            &[
                IndexOption::Unique(true),
                IndexOption::Predicate {
                    sql: "deleted_at IS NULL".into(),
                    columns: vec!["deleted_at".into()],
                },
            ],
        );
        assert!(partial.unique && partial.needs_form());

        let expression = IndexShape::of(
            "lower(email)",
            Some("expression"),
            &[IndexOption::Expression {
                method: "btree".into(),
                key: "lower(email)".into(),
                columns: vec!["email".into()],
            }],
        );
        assert!(expression.expression && expression.is_tree_method());
    }

    #[test]
    fn normalisation_makes_spellings_equal() {
        let a = LogicalExpr::ScalarFunction {
            fun: "LOWER".into(),
            args: vec![LogicalExpr::Column {
                table: Some("t".into()),
                name: "email".into(),
            }],
        };
        let b = LogicalExpr::ScalarFunction {
            fun: "lower".into(),
            args: vec![LogicalExpr::Column {
                table: None,
                name: "email".into(),
            }],
        };
        assert_eq!(normalize_expr(&a, &[]), normalize_expr(&b, &[]));
        let literal = LogicalExpr::Literal(Value::Int4(7));
        let parameter = LogicalExpr::Parameter { index: 1 };
        assert_eq!(
            normalize_expr(&literal, &[]),
            normalize_expr(&parameter, &[Value::Int8(7)])
        );
    }

    #[test]
    fn volatile_functions_are_refused() {
        let now = LogicalExpr::ScalarFunction {
            fun: "now".into(),
            args: vec![],
        };
        let error = check_immutable(&now, "expression").unwrap_err().to_string();
        assert!(error.contains(NOT_IMMUTABLE_MARKER), "{error}");
        let lower = LogicalExpr::ScalarFunction {
            fun: "lower".into(),
            args: vec![LogicalExpr::Column {
                table: None,
                name: "email".into(),
            }],
        };
        assert!(check_immutable(&lower, "expression").is_ok());
    }
}
