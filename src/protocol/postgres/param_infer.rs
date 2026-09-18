//! Extended-protocol parameter TYPE inference (sprinter 6ac716be10ea).
//!
//! ## The defect this exists to close
//!
//! `handle_parse_extended` used to answer `Describe(Statement)` with
//! `vec![0i32; param_count]` — OID 0, "unspecified" — for every `$N` the client
//! did not type at Parse. PostgreSQL never sends 0 there: `exec_describe_
//! statement_message` reports the types parse analysis RESOLVED, and raises
//! `42P18 could not determine data type of parameter $N` when it cannot.
//!
//! OID 0 is not merely uninformative to rust-postgres — it is FATAL.
//! `tokio_postgres::prepare::prepare` resolves every parameter OID through
//! `get_type`; `Type::from_oid(0)` is `None`, so it prepares its own
//! `TYPEINFO_QUERY` (`… FROM pg_catalog.pg_type t … WHERE t.oid = $1`) to look
//! the OID up — and Nano describes THAT `$1` as 0 as well. `typeinfo_statement`
//! only caches the statement AFTER `prepare_rec` returns, so the second lookup
//! re-enters the first: unbounded recursion, client-side stack overflow, and
//! the connection never answers. Two whole test files were `#[ignore]`d on it
//! (`tests/server_mode_integration_test.rs`, `tests/extended_query_param_select.rs`),
//! and it is the reason `vector` result columns are still pinned to `text` on
//! the wire (sprinter 671743292162 / HDB-002).
//!
//! ## The rules
//!
//! PostgreSQL infers a parameter's type from the CONTEXT it appears in. This
//! module reproduces the subset that covers real driver traffic, reading the
//! parsed statement with the relations in scope resolved to their real column
//! types:
//!
//! * `col <cmp> $1` (either side) → the column's type. `cmp` is restricted to
//!   the six comparisons; arithmetic is deliberately NOT included, because
//!   `int_col / $1` is `numeric` in PostgreSQL, not `int4`, and a WRONG OID is
//!   worse than `unknown` (the client then binds in the wrong wire format).
//! * `col IN ($1, $2)` / `col BETWEEN $1 AND $2` → the column's type.
//! * `x LIKE $1` / `ILIKE` → `text`.
//! * `$1::<type>` → the cast's target type, exactly as PostgreSQL resolves it.
//! * `INSERT INTO t (a, b) VALUES ($1, $2)` → the target column at that
//!   POSITION (the full column list in declaration order when none is named).
//! * `UPDATE t SET col = $1` → `col`'s type.
//! * A column of a `pg_catalog` relation that PostgreSQL types `oid` reports
//!   **26**, not the `int4` Nano stores it as. This single rule is what makes
//!   tokio-postgres' own `WHERE t.oid = $1` resolvable (`Type::OID` is a
//!   driver builtin), so its TYPEINFO lookup terminates instead of recursing.
//! * Everything else — a bare `SELECT $1`, a function argument, an operand
//!   whose relation could not be resolved, or a bare column name that resolves
//!   AMBIGUOUSLY to different types in two relations — reports **705**
//!   (`unknown`). That is the case PostgreSQL itself declines to type, and 705
//!   is a driver builtin, so it terminates the lookup without claiming
//!   anything false.
//!
//! ## The invariant
//!
//! Every OID this module may advertise is one `Type::from_oid` resolves
//! LOCALLY in rust-postgres ([`driver_resolvable`]). Advertising anything else
//! would send the driver straight back into the TYPEINFO lookup this item
//! exists to stop — so an inferred type that is not driver-resolvable (a
//! user-band extension OID, say) degrades to 705 rather than being sent.

use crate::Schema;
use sqlparser::ast::{
    Assignment, AssignmentTarget, BinaryOperator, DataType as SqlDataType, Expr, Ident, ObjectName, SetExpr, Statement,
    TableFactor, TableWithJoins, TimezoneInfo, Value as SqlValue, Visit, Visitor,
};
use std::ops::ControlFlow;

/// PostgreSQL's `unknown` pseudo-type — what a parameter reports when nothing
/// in the statement determines its type. `Type::from_oid(705)` is `Some`, so a
/// rust-postgres client resolves it without a server round trip, and
/// `ToSql for str` accepts it (`Type::UNKNOWN` is in its `accepts` list), which
/// is exactly the text-ish binding PostgreSQL would apply.
pub(super) const UNKNOWN_OID: i32 = 705;

/// PostgreSQL's `oid` type.
const OID_OID: i32 = 26;

/// PostgreSQL's `text` type.
const TEXT_OID: i32 = 25;

/// A relation in scope for one Parse, with every spelling a qualified column
/// reference may use to name it (alias, bare name, full storage key).
struct Relation {
    names: Vec<String>,
    schema: Schema,
    /// Resolved from the system-view registry rather than from user storage —
    /// the precondition for the `oid` column rule.
    catalog: bool,
}

impl Relation {
    /// This relation's OID for `column`, or `None` when it has no such column
    /// (or the column's type is not driver-resolvable).
    fn column_oid(&self, column: &str) -> Option<i32> {
        let col = self
            .schema
            .columns
            .iter()
            .find(|c| c.name.as_str() == column)
            .or_else(|| {
                // Identifier folding already ran (`Planner::normalize_ident`:
                // unquoted → lowercase, quoted → verbatim), so an exact hit is
                // the normal case. The case-insensitive retry covers a table
                // created through a surface that stored a different casing, and
                // is REFUSED when it is not unique — guessing between two
                // columns is exactly the "wrong OID" failure mode.
                let mut matches = self
                    .schema
                    .columns
                    .iter()
                    .filter(|c| c.name.eq_ignore_ascii_case(column));
                let first = matches.next()?;
                if matches.next().is_some() {
                    None
                } else {
                    Some(first)
                }
            })?;
        if self.catalog && is_catalog_oid_column(&col.name) {
            return Some(OID_OID);
        }
        let oid = super::handler::datatype_to_oid(&col.data_type);
        driver_resolvable(oid).then_some(oid)
    }
}

/// The relations one Parse can resolve column references against.
struct ParamScope {
    relations: Vec<Relation>,
}

impl ParamScope {
    fn collect<L>(statement: &Statement, lookup: &L) -> Self
    where
        L: Fn(&ObjectName) -> Option<(Schema, bool)>,
    {
        let mut scope = Self { relations: Vec::new() };
        {
            let mut collector = RelationCollector {
                lookup,
                scope: &mut scope,
            };
            let _ = statement.visit(&mut collector);
        }
        // An INSERT target is an `ObjectName` carrying `visit(with =
        // "visit_relation")`, NOT a `TableFactor`, so the collector above never
        // sees it. Push it explicitly — without it `INSERT … ON CONFLICT DO
        // UPDATE SET c = $1 WHERE t.k = $2` has no relation in scope at all.
        if let Statement::Insert(insert) = statement {
            let alias = insert.table_alias.as_ref().map(normalize_ident);
            scope.push(&insert.table_name, alias, lookup);
        }
        scope
    }

    fn push<L>(&mut self, name: &ObjectName, alias: Option<String>, lookup: &L)
    where
        L: Fn(&ObjectName) -> Option<(Schema, bool)>,
    {
        let Some((schema, catalog)) = lookup(name) else {
            return;
        };
        let mut names = Vec::with_capacity(3);
        if let Some(alias) = alias {
            names.push(alias);
        }
        if let Some(last) = name.0.last() {
            names.push(normalize_ident(last));
        }
        names.push(crate::sql::planner::Planner::normalize_object_name(name));
        self.relations.push(Relation { names, schema, catalog });
    }

    /// The relation `name` refers to, matched on any of its spellings.
    fn relation_for(&self, name: &ObjectName) -> Option<&Relation> {
        let key = crate::sql::planner::Planner::normalize_object_name(name);
        self.relations
            .iter()
            .find(|r| r.names.iter().any(|n| n.eq_ignore_ascii_case(&key)))
    }

    /// The OID of the column `parts` names — `[col]` or `[qualifier, col]`.
    ///
    /// A BARE name must resolve UNAMBIGUOUSLY: several relations may carry it,
    /// but they must agree on the type, or the answer is `None` (→ 705).
    /// PostgreSQL raises `42702 ambiguous_column` for the disagreeing case;
    /// declining to type the parameter is the same refusal one round trip
    /// later, and never a wrong OID.
    fn column_oid(&self, parts: &[Ident]) -> Option<i32> {
        let column = normalize_ident(parts.last()?);
        if parts.len() >= 2 {
            let qualifier = normalize_ident(parts.get(parts.len() - 2)?);
            return self
                .relations
                .iter()
                .find(|r| r.names.iter().any(|n| n.eq_ignore_ascii_case(&qualifier)))
                .and_then(|r| r.column_oid(&column));
        }
        let mut found: Option<i32> = None;
        for relation in &self.relations {
            let Some(oid) = relation.column_oid(&column) else {
                continue;
            };
            match found {
                None => found = Some(oid),
                Some(existing) if existing == oid => {}
                Some(_) => return None,
            }
        }
        found
    }

    /// The type of an operand a parameter is being compared against.
    fn operand_oid(&self, expr: &Expr) -> Option<i32> {
        match expr {
            Expr::Identifier(ident) => self.column_oid(std::slice::from_ref(ident)),
            Expr::CompoundIdentifier(parts) => self.column_oid(parts),
            Expr::Nested(inner) => self.operand_oid(inner),
            Expr::Cast { data_type, .. } => cast_target_oid(data_type),
            _ => None,
        }
    }

    /// One expression node's contribution. Called for EVERY `Expr` in the
    /// statement (the visitor walks subqueries too), so each rule matches its
    /// own shape and ignores the rest.
    fn apply_expr(&self, expr: &Expr, oids: &mut [Option<i32>]) {
        match expr {
            Expr::BinaryOp { left, op, right } if is_comparison(op) => {
                if let Some(index) = placeholder_index(right) {
                    if let Some(oid) = self.operand_oid(left) {
                        set_oid(oids, index, oid);
                    }
                }
                if let Some(index) = placeholder_index(left) {
                    if let Some(oid) = self.operand_oid(right) {
                        set_oid(oids, index, oid);
                    }
                }
            }
            Expr::InList { expr: probe, list, .. } => {
                if let Some(oid) = self.operand_oid(probe) {
                    for item in list {
                        if let Some(index) = placeholder_index(item) {
                            set_oid(oids, index, oid);
                        }
                    }
                }
            }
            Expr::Between {
                expr: probe, low, high, ..
            } => {
                if let Some(oid) = self.operand_oid(probe) {
                    for bound in [low, high] {
                        if let Some(index) = placeholder_index(bound) {
                            set_oid(oids, index, oid);
                        }
                    }
                }
            }
            // `LIKE` is text-only in PostgreSQL (`text ~~ text`), so the
            // pattern — and the subject, when IT is the parameter — is `text`
            // whatever the other side turns out to be.
            Expr::Like {
                expr: probe, pattern, ..
            }
            | Expr::ILike {
                expr: probe, pattern, ..
            } => {
                for side in [probe, pattern] {
                    if let Some(index) = placeholder_index(side) {
                        set_oid(oids, index, TEXT_OID);
                    }
                }
            }
            Expr::Cast {
                expr: inner, data_type, ..
            } => {
                if let Some(index) = placeholder_index(inner) {
                    if let Some(oid) = cast_target_oid(data_type) {
                        set_oid(oids, index, oid);
                    }
                }
            }
            _ => {}
        }
    }

    /// The DML positions no expression rule can see: an INSERT's VALUES row
    /// (typed by POSITION against the target's column list) and an UPDATE's
    /// `SET col = $1` (typed by the assignment target). Both are bare
    /// placeholders in the AST with no neighbouring column reference, so the
    /// expression visitor alone would leave them `unknown`.
    fn apply_dml_targets(&self, statement: &Statement, oids: &mut [Option<i32>]) {
        match statement {
            Statement::Insert(insert) => self.apply_insert(insert, oids),
            Statement::Update { table, assignments, .. } => self.apply_update(table, assignments, oids),
            _ => {}
        }
    }

    fn apply_insert(&self, insert: &sqlparser::ast::Insert, oids: &mut [Option<i32>]) {
        let Some(target) = self.relation_for(&insert.table_name) else {
            return;
        };
        // `INSERT INTO t VALUES (…)` with no column list fills the columns in
        // declaration order — the same rule the executor's default-fill uses.
        let positions: Vec<Option<i32>> = if insert.columns.is_empty() {
            target
                .schema
                .columns
                .iter()
                .map(|c| {
                    let oid = super::handler::datatype_to_oid(&c.data_type);
                    driver_resolvable(oid).then_some(oid)
                })
                .collect()
        } else {
            insert
                .columns
                .iter()
                .map(|ident| target.column_oid(&normalize_ident(ident)))
                .collect()
        };
        let Some(source) = insert.source.as_ref() else {
            return;
        };
        // Only a literal VALUES list types by position. `INSERT … SELECT`
        // leaves its parameters to the expression rules (they sit inside the
        // source query, where a comparison can still type them).
        let SetExpr::Values(values) = &*source.body else {
            return;
        };
        for row in &values.rows {
            for (position, expr) in row.iter().enumerate() {
                let Some(index) = placeholder_index(expr) else {
                    continue;
                };
                if let Some(Some(oid)) = positions.get(position) {
                    set_oid(oids, index, *oid);
                }
            }
        }
    }

    fn apply_update(&self, table: &TableWithJoins, assignments: &[Assignment], oids: &mut [Option<i32>]) {
        let TableFactor::Table { name, .. } = &table.relation else {
            return;
        };
        let Some(target) = self.relation_for(name) else {
            return;
        };
        for assignment in assignments {
            let Some(index) = placeholder_index(&assignment.value) else {
                continue;
            };
            let AssignmentTarget::ColumnName(column) = &assignment.target else {
                continue;
            };
            let Some(ident) = column.0.last() else {
                continue;
            };
            if let Some(oid) = target.column_oid(&normalize_ident(ident)) {
                set_oid(oids, index, oid);
            }
        }
    }
}

/// Collects every `TableFactor::Table` in the statement — including the ones
/// inside subqueries, CTEs and join trees — into one flat scope.
struct RelationCollector<'a, 'b, L> {
    lookup: &'a L,
    scope: &'b mut ParamScope,
}

impl<L> Visitor for RelationCollector<'_, '_, L>
where
    L: Fn(&ObjectName) -> Option<(Schema, bool)>,
{
    type Break = ();

    fn pre_visit_table_factor(&mut self, table_factor: &TableFactor) -> ControlFlow<()> {
        if let TableFactor::Table { name, alias, .. } = table_factor {
            let alias = alias.as_ref().map(|a| normalize_ident(&a.name));
            self.scope.push(name, alias, self.lookup);
        }
        ControlFlow::Continue(())
    }
}

/// Applies [`ParamScope::apply_expr`] to every expression in the statement.
struct ExprScan<'a, 'b> {
    scope: &'a ParamScope,
    oids: &'b mut [Option<i32>],
}

impl Visitor for ExprScan<'_, '_> {
    type Break = ();

    fn pre_visit_expr(&mut self, expr: &Expr) -> ControlFlow<()> {
        self.scope.apply_expr(expr, self.oids);
        ControlFlow::Continue(())
    }
}

/// THE entry point: one OID per parameter, `705` wherever the statement does
/// not determine a type.
///
/// `lookup` resolves a relation reference to `(schema, is_catalog_relation)`;
/// returning `None` simply means "nothing in scope for this name", and every
/// parameter that depended on it falls back to `unknown`.
pub(super) fn infer_parameter_oids<L>(statement: &Statement, param_count: usize, lookup: &L) -> Vec<i32>
where
    L: Fn(&ObjectName) -> Option<(Schema, bool)>,
{
    if param_count == 0 {
        return Vec::new();
    }
    let scope = ParamScope::collect(statement, lookup);
    let mut oids: Vec<Option<i32>> = vec![None; param_count];
    scope.apply_dml_targets(statement, &mut oids);
    {
        let mut scan = ExprScan {
            scope: &scope,
            oids: &mut oids,
        };
        let _ = statement.visit(&mut scan);
    }
    oids.into_iter().map(|oid| oid.unwrap_or(UNKNOWN_OID)).collect()
}

/// Identifier folding, borrowed from the planner so a Describe resolves the
/// same spelling execution will.
fn normalize_ident(ident: &Ident) -> String {
    crate::sql::planner::Planner::normalize_ident(ident)
}

/// First-writer-wins: the DML positional pass runs before the expression scan,
/// and within the scan the OUTERMOST node wins, which is the most specific
/// context a parameter has.
fn set_oid(oids: &mut [Option<i32>], index: usize, oid: i32) {
    // The invariant (module doc): never advertise an OID the client would have
    // to ask the SERVER about, because that lookup is the recursion this item
    // removes. An unresolvable type degrades to `unknown`.
    if !driver_resolvable(oid) {
        return;
    }
    let Some(position) = index.checked_sub(1) else {
        return;
    };
    if let Some(slot) = oids.get_mut(position) {
        if slot.is_none() {
            *slot = Some(oid);
        }
    }
}

/// `$N` → `N`, through any number of parentheses. Anything else is not a
/// parameter position.
fn placeholder_index(expr: &Expr) -> Option<usize> {
    match expr {
        Expr::Nested(inner) => placeholder_index(inner),
        Expr::Value(SqlValue::Placeholder(text)) => text
            .strip_prefix('$')
            .and_then(|digits| digits.parse::<usize>().ok())
            .filter(|n| *n >= 1),
        _ => None,
    }
}

/// The operators that carry an operand's type across to the parameter.
///
/// Comparisons ONLY. Arithmetic is excluded on purpose: PostgreSQL resolves
/// `int4 / unknown` to `numeric`, and `date + unknown` to `date + integer` OR
/// `date + interval` depending on the literal, so an "obvious" `int4` here
/// would be a WRONG OID — which makes the client bind in the wrong wire
/// format, strictly worse than `unknown`.
fn is_comparison(op: &BinaryOperator) -> bool {
    matches!(
        op,
        BinaryOperator::Eq
            | BinaryOperator::NotEq
            | BinaryOperator::Lt
            | BinaryOperator::LtEq
            | BinaryOperator::Gt
            | BinaryOperator::GtEq
    )
}

/// `$1::<type>` — PostgreSQL types the parameter as the cast's target, and so
/// does this. Only the spellings that map onto a type Nano can actually decode
/// are listed; anything else falls through to `unknown`.
fn cast_target_oid(data_type: &SqlDataType) -> Option<i32> {
    Some(match data_type {
        SqlDataType::Bool | SqlDataType::Boolean => 16,
        SqlDataType::SmallInt(_) | SqlDataType::Int2(_) => 21,
        SqlDataType::Int(_) | SqlDataType::Integer(_) | SqlDataType::Int4(_) => 23,
        SqlDataType::BigInt(_) | SqlDataType::Int8(_) => 20,
        SqlDataType::Real | SqlDataType::Float4 => 700,
        SqlDataType::Double | SqlDataType::DoublePrecision | SqlDataType::Float8 => 701,
        SqlDataType::Numeric(_) | SqlDataType::Decimal(_) | SqlDataType::Dec(_) => 1700,
        SqlDataType::Text => TEXT_OID,
        SqlDataType::Varchar(_) | SqlDataType::CharacterVarying(_) | SqlDataType::CharVarying(_) => 1043,
        SqlDataType::Char(_) | SqlDataType::Character(_) => 1042,
        SqlDataType::Bytea => 17,
        SqlDataType::Date => 1082,
        SqlDataType::Time(_, _) => 1083,
        SqlDataType::Timestamp(_, TimezoneInfo::WithTimeZone | TimezoneInfo::Tz) => 1184,
        SqlDataType::Timestamp(_, _) => 1114,
        SqlDataType::Interval => 1186,
        SqlDataType::Uuid => 2950,
        SqlDataType::JSON => 114,
        SqlDataType::JSONB => 3802,
        _ => return None,
    })
}

/// Column names a `pg_catalog` relation types `oid` in PostgreSQL.
///
/// Nano stores these as `int4` (its `DataType` has no `oid` variant), which is
/// a LIE on the wire for a parameter: `impl FromSql/ToSql for u32` accepts
/// `Type::OID` and nothing else, so a driver that binds an OID against an
/// `int4` parameter fails `WrongType` — and tokio-postgres binds exactly that
/// for `WHERE t.oid = $1` in its TYPEINFO lookup. Reporting 26 here is what
/// makes that lookup work rather than merely terminate.
///
/// The list is PostgreSQL's, narrowed to the catalogue columns drivers
/// actually filter on (`pg_type`, `pg_class`, `pg_attribute`, `pg_range`,
/// `pg_enum`, `pg_constraint`, `pg_index`, `pg_namespace`, `pg_proc`).
fn is_catalog_oid_column(name: &str) -> bool {
    matches!(
        name,
        "oid"
            | "typnamespace"
            | "typowner"
            | "typrelid"
            | "typelem"
            | "typarray"
            | "typbasetype"
            | "typcollation"
            | "relnamespace"
            | "reltype"
            | "reloftype"
            | "relowner"
            | "relam"
            | "reltablespace"
            | "reltoastrelid"
            | "attrelid"
            | "atttypid"
            | "attcollation"
            | "rngtypid"
            | "rngsubtype"
            | "rngmultitypid"
            | "rngcollation"
            | "rngsubopc"
            | "enumtypid"
            | "connamespace"
            | "conrelid"
            | "contypid"
            | "confrelid"
            | "conindid"
            | "indexrelid"
            | "indrelid"
            | "indcollation"
            | "adrelid"
            | "nspowner"
            | "pronamespace"
            | "proowner"
            | "prorettype"
            | "stxrelid"
            | "stxnamespace"
            | "classid"
            | "objid"
            | "refclassid"
            | "refobjid"
    )
}

/// The OIDs `tokio_postgres::types::Type::from_oid` answers WITHOUT asking the
/// server — i.e. the only OIDs it is safe to put in a ParameterDescription.
///
/// This is the whole point of the item: an OID outside this set sends
/// rust-postgres (and with it sqlx and Prisma's query engine) into the
/// server-side TYPEINFO lookup whose own `$1` used to recurse. The set is
/// deliberately narrow — every OID `handler::datatype_to_oid` can produce, plus
/// `oid` (26) and `unknown` (705) — and is pinned end-to-end against the real
/// driver by `tests/param_oid_batch_g5.rs::
/// g5_every_inferable_parameter_oid_is_a_driver_builtin`.
fn driver_resolvable(oid: i32) -> bool {
    matches!(
        oid,
        16      // bool
            | 17    // bytea
            | 18    // char
            | 19    // name
            | 20    // int8
            | 21    // int2
            | 23    // int4
            | 25    // text
            | 26    // oid
            | 114   // json
            | 700   // float4
            | 701   // float8
            | 705   // unknown
            | 1042  // bpchar
            | 1043  // varchar
            | 1082  // date
            | 1083  // time
            | 1114  // timestamp
            | 1184  // timestamptz
            | 1186  // interval
            | 1700  // numeric
            | 2950  // uuid
            | 3614  // tsvector
            | 3615  // tsquery
            | 3802 // jsonb
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::{Column, DataType};

    /// `users(id int4, name text)` plus a catalog-shaped `pg_type(oid int4,
    /// typname text)` — the two relations every rule below needs, with no
    /// database in the way.
    fn lookup(name: &ObjectName) -> Option<(Schema, bool)> {
        let key = crate::sql::planner::Planner::normalize_object_name(name);
        match key.as_str() {
            "users" => Some((
                Schema::new(vec![
                    Column::new("id".to_string(), DataType::Int4),
                    Column::new("name".to_string(), DataType::Text),
                    Column::new("created_at".to_string(), DataType::Timestamp),
                ]),
                false,
            )),
            // `pg_catalog.pg_type` collapses to `pg_type` in
            // `normalize_object_name`, exactly as the real registry keys it.
            "pg_type" => Some((
                Schema::new(vec![
                    Column::new("oid".to_string(), DataType::Int4),
                    Column::new("typname".to_string(), DataType::Text),
                ]),
                true,
            )),
            _ => None,
        }
    }

    fn infer(sql: &str, count: usize) -> Vec<i32> {
        let statement = crate::sql::Parser::new().parse_one(sql).expect("parse");
        infer_parameter_oids(&statement, count, &lookup)
    }

    #[test]
    fn comparison_against_a_column_takes_that_column_type() {
        assert_eq!(infer("SELECT name FROM users WHERE id = $1", 1), vec![23]);
        assert_eq!(infer("SELECT id FROM users WHERE name = $1", 1), vec![25]);
        // Qualified, and with the parameter on the LEFT.
        assert_eq!(infer("SELECT id FROM users u WHERE $1 = u.name", 1), vec![25]);
        assert_eq!(infer(r#"SELECT id FROM "users" WHERE "users"."id" > $1"#, 1), vec![23]);
    }

    #[test]
    fn insert_and_update_positions_resolve() {
        assert_eq!(infer("INSERT INTO users VALUES ($1, $2, $3)", 3), vec![23, 25, 1114]);
        assert_eq!(infer("INSERT INTO users (name, id) VALUES ($1, $2)", 2), vec![25, 23]);
        assert_eq!(
            infer("UPDATE users SET name = $1 WHERE id = $2", 2),
            vec![25, 23],
            "SET takes the assignment target's type, WHERE the comparison's"
        );
        assert_eq!(infer("DELETE FROM users WHERE id = $1", 1), vec![23]);
    }

    #[test]
    fn in_list_between_like_and_cast() {
        assert_eq!(infer("SELECT id FROM users WHERE id IN ($1, $2)", 2), vec![23, 23]);
        assert_eq!(
            infer("SELECT id FROM users WHERE id BETWEEN $1 AND $2", 2),
            vec![23, 23]
        );
        assert_eq!(infer("SELECT id FROM users WHERE name LIKE $1", 1), vec![25]);
        assert_eq!(infer("SELECT $1::bigint", 1), vec![20]);
    }

    /// A catalog `oid` column is 26, NOT the `int4` (23) Nano stores it as —
    /// the rule that makes tokio-postgres' TYPEINFO lookup bind a real `Oid`.
    #[test]
    fn catalog_oid_column_reports_oid_not_int4() {
        assert_eq!(
            infer("SELECT typname FROM pg_catalog.pg_type t WHERE t.oid = $1", 1),
            vec![26]
        );
        assert_eq!(
            infer("SELECT typname FROM pg_catalog.pg_type WHERE typname = $1", 1),
            vec![25],
            "a text column of the same relation is still text"
        );
    }

    /// The cases PostgreSQL itself declines to type.
    #[test]
    fn no_context_reports_unknown_never_a_guess() {
        assert_eq!(infer("SELECT $1", 1), vec![UNKNOWN_OID]);
        assert_eq!(
            infer("SELECT pg_try_advisory_lock($1)", 1),
            vec![UNKNOWN_OID],
            "a function argument carries no resolved signature here"
        );
        assert_eq!(
            infer("SELECT x FROM nosuchtable WHERE x = $1", 1),
            vec![UNKNOWN_OID],
            "an unresolvable relation must not invent a type"
        );
        assert_eq!(
            infer("SELECT id FROM users WHERE id + $1 > 5", 1),
            vec![UNKNOWN_OID],
            "arithmetic is NOT type-preserving in PostgreSQL — 705, not a guess"
        );
        assert_eq!(
            infer("SELECT id FROM users WHERE id = $1", 3),
            vec![23, UNKNOWN_OID, UNKNOWN_OID],
            "parameters the statement never mentions still get a resolvable OID"
        );
    }

    /// Every OID the inference can emit must be one the driver resolves
    /// locally; the guard degrades anything else to `unknown`.
    #[test]
    fn every_emitted_oid_is_driver_resolvable() {
        for oid in infer("INSERT INTO users VALUES ($1, $2, $3)", 3) {
            assert!(driver_resolvable(oid), "{oid} would send the client back to TYPEINFO");
        }
        assert!(driver_resolvable(UNKNOWN_OID));
        assert!(!driver_resolvable(16385), "a user-band extension OID is NOT resolvable");
    }
}
