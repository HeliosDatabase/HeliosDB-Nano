//! Applying the policies: the plan rewrite for reads and the row checks for
//! writes.
//!
//! Reads. A read policy on table T becomes part of T's scan leaf, as the scan's
//! own predicate, which the executor evaluates against the full base row
//! before any projection. [`Rewriter`] walks the whole plan (an exhaustive
//! match, so a new plan node cannot be forgotten) and every query embedded in
//! an expression. A view is expanded by the planner before this runs, so the
//! tables under it are filtered as the reader. A materialized view's stored
//! rows are filtered by its base tables' policies when [`super::derived`] can
//! prove that equivalent, and refused otherwise.
//!
//! Writes. [`RlsWriteGuard`] is built once per statement: `USING` filters the
//! rows an UPDATE or DELETE can see, `WITH CHECK` (or `USING` when there is
//! none) must hold for every row an INSERT or UPDATE writes.
//!
//! A policy that cannot be parsed or applied fails the statement. Nothing here
//! falls back to "no policy".

use std::cell::RefCell;
use std::sync::Arc;

use super::decide::{Decision, RlsIdentity, INFINITE_RECURSION};
use super::derived;
use super::record::{PolicyCommand, PolicyRecord};
use crate::sql::{self, BinaryOperator, LogicalExpr, LogicalPlan};
use crate::storage::StorageEngine;
use crate::{EmbeddedDatabase, Error, Result, Schema, Tuple, Value};

/// Which clause of a policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Side {
    /// `USING`: the rows that exist and may be seen.
    Using,
    /// `WITH CHECK`: the rows that may be written; `USING` when absent.
    Check,
}

/// The predicate the policies of one table come to, for one command.
pub(crate) struct TablePredicate {
    pub expr: LogicalExpr,
    /// The policies it was built from, for a message.
    pub policies: Vec<String>,
    /// The table's own switch applies them (not only a tenant binding).
    pub by_table_switch: bool,
}

fn and(left: LogicalExpr, right: LogicalExpr) -> LogicalExpr {
    LogicalExpr::BinaryExpr {
        left: Box::new(left),
        op: BinaryOperator::And,
        right: Box::new(right),
    }
}

fn or(left: LogicalExpr, right: LogicalExpr) -> LogicalExpr {
    LogicalExpr::BinaryExpr {
        left: Box::new(left),
        op: BinaryOperator::Or,
        right: Box::new(right),
    }
}

fn quoted(table: &str) -> String {
    let quote = |part: &str| format!("\"{}\"", part.replace('"', "\"\""));
    let (schema, name) = sql::Planner::split_schema_key(table);
    if schema == "public" {
        quote(&name)
    } else {
        format!("{}.{}", quote(&schema), quote(&name))
    }
}

/// The predicate of `SELECT … FROM table WHERE <predicate>`, as planned.
fn planned_predicate(plan: &LogicalPlan) -> Option<LogicalExpr> {
    match plan {
        LogicalPlan::Filter { input, predicate } => Some(match planned_predicate(input) {
            Some(below) => and(below, predicate.clone()),
            None => predicate.clone(),
        }),
        LogicalPlan::FilteredScan { predicate, .. } => predicate.clone(),
        LogicalPlan::Project { input, .. } => planned_predicate(input),
        _ => None,
    }
}

/// Drop the table's own name from the columns of a policy expression
/// (`docs.owner` -> `owner`): the predicate runs on the table's scan, where
/// the table may be known by an alias.
fn unqualify(expr: LogicalExpr, table: &str) -> LogicalExpr {
    let (_, bare) = sql::Planner::split_schema_key(table);
    map_columns(expr, &|qualifier, name| LogicalExpr::Column {
        table: qualifier.filter(|q| q != table && *q != bare),
        name,
    })
}

/// Rebuild `expr` with every column reference outside a subquery replaced by
/// `column(qualifier, name)`.
fn map_columns(expr: LogicalExpr, column: &dyn Fn(Option<String>, String) -> LogicalExpr) -> LogicalExpr {
    use LogicalExpr as E;
    let boxed = |e: Box<LogicalExpr>| Box::new(map_columns(*e, column));
    let each = |es: Vec<LogicalExpr>| es.into_iter().map(|e| map_columns(e, column)).collect::<Vec<_>>();
    match expr {
        E::Column { table, name } => column(table, name),
        E::InSubquery {
            expr,
            subquery,
            negated,
        } => E::InSubquery {
            expr: boxed(expr),
            subquery,
            negated,
        },
        E::BinaryExpr { left, op, right } => E::BinaryExpr {
            left: boxed(left),
            op,
            right: boxed(right),
        },
        E::UnaryExpr { op, expr } => E::UnaryExpr { op, expr: boxed(expr) },
        E::AggregateFunction { fun, args, distinct } => E::AggregateFunction {
            fun,
            args: each(args),
            distinct,
        },
        E::ScalarFunction { fun, args } => E::ScalarFunction { fun, args: each(args) },
        E::Case {
            expr,
            when_then,
            else_result,
        } => E::Case {
            expr: expr.map(boxed),
            when_then: when_then
                .into_iter()
                .map(|(w, t)| (map_columns(w, column), map_columns(t, column)))
                .collect(),
            else_result: else_result.map(boxed),
        },
        E::Cast { expr, data_type } => E::Cast {
            expr: boxed(expr),
            data_type,
        },
        E::IsNull { expr, is_null } => E::IsNull {
            expr: boxed(expr),
            is_null,
        },
        E::Between {
            expr,
            low,
            high,
            negated,
        } => E::Between {
            expr: boxed(expr),
            low: boxed(low),
            high: boxed(high),
            negated,
        },
        E::InList { expr, list, negated } => E::InList {
            expr: boxed(expr),
            list: each(list),
            negated,
        },
        E::InSet { expr, values, negated } => E::InSet {
            expr: boxed(expr),
            values,
            negated,
        },
        E::ArraySubscript { array, index } => E::ArraySubscript {
            array: boxed(array),
            index: boxed(index),
        },
        E::Tuple { items } => E::Tuple { items: each(items) },
        E::WindowFunction {
            fun,
            args,
            partition_by,
            order_by,
            frame,
        } => E::WindowFunction {
            fun,
            args: each(args),
            partition_by: each(partition_by),
            order_by: order_by.into_iter().map(|(e, asc)| (map_columns(e, column), asc)).collect(),
            frame,
        },
        other @ (E::ScalarSubquery { .. }
        | E::Exists { .. }
        | E::Literal(_)
        | E::DefaultValue
        | E::Wildcard
        | E::Parameter { .. }
        | E::NewRow { .. }
        | E::OldRow { .. }
        | E::BoundColumn { .. }) => other,
    }
}

/// A policy expression (`USING` / `WITH CHECK` text) as a plan expression over
/// the rows of `table`.
///
/// The expressions the tenant API has always taken (comparisons, AND / OR /
/// NOT, IS NULL, function calls) are lowered as before. Anything else
/// (parentheses, IN lists, casts, subqueries, …) is planned as the WHERE
/// clause of a query on the table, so it means what it means in a query.
pub(crate) fn compile_policy_expr(storage: &StorageEngine, table: &str, text: &str) -> Result<LogicalExpr> {
    let simple = crate::tenant::RLSExpressionEvaluator::new(Arc::new(Schema { columns: vec![] }), None).parse(text);
    let expr = match simple {
        Ok(expr) => expr,
        Err(simple_error) => match plan_policy_expr(storage, table, text) {
            Ok(expr) => expr,
            // Text that is not SQL at all keeps the error it always had.
            Err(_) if simple_error.to_string().contains("Failed to parse RLS expression") => {
                return Err(simple_error)
            }
            Err(e) => return Err(e),
        },
    };
    Ok(unqualify(expr, table))
}

/// [`compile_policy_expr`] through the planner only: what `CREATE POLICY`
/// checks an expression with, so a column that does not exist is refused when
/// the policy is written.
pub(crate) fn plan_policy_expr(storage: &StorageEngine, table: &str, text: &str) -> Result<LogicalExpr> {
    let sql_text = format!("SELECT 1 FROM {} WHERE {}", quoted(table), text);
    let statement = sql::Parser::new().parse_one(&sql_text)?;
    let catalog = storage.catalog();
    let plan = sql::Planner::with_catalog(&catalog).statement_to_plan(statement)?;
    planned_predicate(&plan)
        .map(|expr| unqualify(expr, table))
        .ok_or_else(|| Error::query_execution(format!("policy expression \"{text}\" is not a row condition")))
}

/// Per-statement, pre-compiled row-level security for one (table, command)
/// pair. Built ONCE before a DML row loop and reused for every row; never
/// build one inside a row loop.
///
/// Which clause applies to which command is PostgreSQL's asymmetry:
///
/// | command | `USING` (the row as it is) | `WITH CHECK` (the row written) |
/// |---|---|---|
/// | INSERT | n/a: there is no existing row | error on violation |
/// | UPDATE | filters: an invisible row is silently not updated | error on violation |
/// | DELETE | filters: an invisible row is silently not deleted | n/a: no new row |
pub(crate) struct RlsWriteGuard {
    /// The table, for the error message.
    table_name: String,
    /// `None` for INSERT.
    using: Option<LogicalExpr>,
    /// `None` for DELETE.
    with_check: Option<LogicalExpr>,
    evaluator: sql::Evaluator,
}

impl EmbeddedDatabase {
    /// Who the statement running on this thread is, for row-level security.
    pub(crate) fn rls_identity(&self) -> RlsIdentity {
        self.tenant_manager.rls_identity()
    }

    /// R1.2: the ONE place a read plan is filtered for row-level security
    /// before execution. `Cow::Borrowed` when nothing can apply, so the common
    /// case pays no clone.
    ///
    /// FAIL-CLOSED: the `?` propagates a policy error to the caller, so a read
    /// that cannot be filtered returns `Err` and never rows. Do NOT add a
    /// fallback here or at any call site. Pinned by
    /// `rls_parse_error_on_read_fails_closed` in `tests/rls_read_parity_tests.rs`.
    pub(crate) fn rls_filtered_plan<'p>(&self, plan: &'p LogicalPlan) -> Result<std::borrow::Cow<'p, LogicalPlan>> {
        if !self.has_effective_tenant_context() {
            return Ok(std::borrow::Cow::Borrowed(plan));
        }
        Ok(std::borrow::Cow::Owned(self.apply_rls_to_plan_recursive(plan.clone())?))
    }

    /// Apply the read policies to a query plan.
    pub(crate) fn apply_rls_to_plan(&self, plan: LogicalPlan) -> Result<LogicalPlan> {
        // Nothing can apply (the common case): skip the walk.
        if !self.has_effective_tenant_context() {
            return Ok(plan);
        }
        self.apply_rls_to_plan_recursive(plan)
    }

    /// [`Self::apply_rls_to_plan`] without the early exit.
    pub(crate) fn apply_rls_to_plan_recursive(&self, plan: LogicalPlan) -> Result<LogicalPlan> {
        Rewriter::new(self).apply_rls_to_plan_recursive(plan)
    }

    /// Apply the read policies to every query embedded in `expr` (scalar, IN
    /// and EXISTS subqueries, at any depth). Used for expressions evaluated
    /// outside a plan rewrite, such as UPDATE … SET.
    pub(crate) fn apply_rls_to_expr(&self, expr: LogicalExpr) -> Result<LogicalExpr> {
        Rewriter::new(self).apply_rls_to_expr(expr)
    }

    /// v05.6: an UPDATE / DELETE predicate with a subquery, with the read
    /// policies applied to every subquery's plan, as a SELECT gets them.
    /// `None` when there is nothing to change, so the common path keeps the
    /// planned predicate.
    pub(crate) fn rls_dml_where(&self, selection: Option<&LogicalExpr>) -> Result<Option<LogicalExpr>> {
        match selection {
            Some(expr) if self.has_effective_tenant_context() && sql::Executor::expr_contains_subquery(expr) => {
                Ok(Some(self.apply_rls_to_expr(expr.clone())?))
            }
            _ => Ok(None),
        }
    }

    /// `row_security` (PostgreSQL's setting; `SET row_security = off` is the
    /// first line of every pg_dump script). Called only once a policy is known
    /// to apply to `table_name` for this statement:
    ///
    /// * on (the default): `Ok(true)`, the policy filters / checks as always;
    /// * off, for a session that bypasses row-level security (no login
    ///   identity, a bootstrap role, SUPERUSER or BYPASSRLS): `Ok(false)`, the
    ///   policy is not applied;
    /// * off, for any other role: PostgreSQL's 42501 "query would be affected
    ///   by row-level security policy". Never silently filtered rows, and
    ///   never silently unfiltered ones.
    fn row_security_applies(&self, table_name: &str) -> Result<bool> {
        if sql::params::row_security_enabled() {
            return Ok(true);
        }
        if self.session_bypasses_rls() {
            return Ok(false);
        }
        // `ConstraintViolation` with "row-level security" in the text is what
        // the wire reports as 42501, like the WITH CHECK refusal.
        Err(Error::ConstraintViolation(format!(
            "query would be affected by row-level security policy for table \"{table_name}\""
        )))
    }

    /// Does the role running this statement bypass row-level security?
    fn session_bypasses_rls(&self) -> bool {
        match crate::session_login_user_tls() {
            None => true,
            Some(login) => self.tenant_manager.policy_store().role_is_exempt(&login),
        }
    }

    /// The predicate the policies of `table` come to for `command`, or `None`
    /// when none applies to this identity.
    ///
    /// Permissive policies are OR'd (none: nothing passes), restrictive ones
    /// are AND'd with that. A policy without the clause asked for passes
    /// every row, as in PostgreSQL.
    pub(crate) fn rls_table_predicate(
        &self,
        table: &str,
        command: PolicyCommand,
        side: Side,
        identity: &RlsIdentity,
    ) -> Result<Option<TablePredicate>> {
        let Decision::Apply(applicable) = self.tenant_manager.policy_store().decide(table, command, identity)? else {
            return Ok(None);
        };
        if !self.row_security_applies(table)? {
            return Ok(None);
        }
        let compile = |policy: &PolicyRecord| -> Result<LogicalExpr> {
            let text = match side {
                Side::Using => policy.using.as_deref(),
                Side::Check => policy.with_check.as_deref().or(policy.using.as_deref()),
            };
            match text {
                Some(text) => applicable
                    .entry
                    .compiled(text, || compile_policy_expr(&self.storage, table, text)),
                None => Ok(LogicalExpr::Literal(Value::Boolean(true))),
            }
        };
        let mut passes: Option<LogicalExpr> = None;
        for policy in &applicable.permissive {
            let expr = compile(policy)?;
            passes = Some(match passes {
                Some(so_far) => or(so_far, expr),
                None => expr,
            });
        }
        let mut expr = passes.unwrap_or(LogicalExpr::Literal(Value::Boolean(false)));
        for policy in &applicable.restrictive {
            expr = and(expr, compile(policy)?);
        }
        Ok(Some(TablePredicate {
            expr,
            policies: applicable.policy_names(),
            by_table_switch: applicable.by_table_switch,
        }))
    }

    /// Resolve and compile the policies applying to `table_name` / `cmd`
    /// (`"INSERT"` | `"UPDATE"` | `"DELETE"`), once per statement. Shared by
    /// both DML executor families. `Ok(None)` when nothing applies.
    ///
    /// On a table with row security enabled, the rows an UPDATE or DELETE can
    /// see are also limited by the SELECT policies (PostgreSQL applies them
    /// whenever the statement reads the row, which a WHERE clause, a SET
    /// expression or RETURNING does; here they always apply).
    ///
    /// A subquery in a policy is run once, when the guard is built, against
    /// committed data; one that refers to the row being written is refused.
    pub(crate) fn build_rls_write_guard(
        &self,
        table_name: &str,
        cmd: &str,
        schema: &Schema,
    ) -> Result<Option<RlsWriteGuard>> {
        if !self.has_effective_tenant_context() {
            return Ok(None);
        }
        let Some(command) = PolicyCommand::from_keyword(cmd) else {
            return Ok(None);
        };
        let identity = self.rls_identity();
        let rewriter = Rewriter::with_identity(self, identity);
        let identity = &rewriter.identity;
        let using = match command {
            PolicyCommand::Insert => None,
            _ => match self.rls_table_predicate(table_name, command, Side::Using, identity)? {
                Some(found) if found.by_table_switch => {
                    let select = self.rls_table_predicate(table_name, PolicyCommand::Select, Side::Using, identity)?;
                    Some(match select {
                        Some(select) => and(found.expr, select.expr),
                        None => found.expr,
                    })
                }
                found => found.map(|found| found.expr),
            },
        };
        let with_check = match command {
            PolicyCommand::Delete => None,
            _ => self
                .rls_table_predicate(table_name, command, Side::Check, identity)?
                .map(|found| found.expr),
        };
        if using.is_none() && with_check.is_none() {
            return Ok(None);
        }
        let prepare = |expr: LogicalExpr| -> Result<LogicalExpr> {
            if !sql::Executor::expr_contains_subquery(&expr) {
                return Ok(expr);
            }
            let expr = rewriter.expanded(table_name, expr)?;
            if Self::dml_predicate_is_correlated(&expr, schema, table_name) {
                return Err(Error::query_execution(format!(
                    "a policy on \"{table_name}\" with a subquery that refers to the row being written {}",
                    super::UNSUPPORTED_MARKER
                )));
            }
            let probe = Tuple::new(vec![Value::Null; schema.columns.len()]);
            self.materialize_scalar_subqueries_for_row(&expr, &probe, schema, table_name, &[], None)
        };
        Ok(Some(RlsWriteGuard {
            table_name: table_name.to_string(),
            using: using.map(&prepare).transpose()?,
            with_check: with_check.map(&prepare).transpose()?,
            evaluator: sql::Evaluator::new(Arc::new(schema.clone())),
        }))
    }

    /// UPDATE / DELETE: does this existing row pass `USING`?
    ///
    /// A `false` answer is a **filter**, never an error: PostgreSQL silently
    /// leaves rows failing `USING` untouched. Anything but TRUE (FALSE, NULL)
    /// hides the row. `Ok(true)` when `guard` is `None`.
    pub(crate) fn rls_row_visible(guard: Option<&RlsWriteGuard>, tuple: &Tuple) -> Result<bool> {
        let Some(guard) = guard else { return Ok(true) };
        let Some(using) = &guard.using else {
            return Ok(true);
        };
        Ok(matches!(guard.evaluator.evaluate(using, tuple)?, Value::Boolean(true)))
    }

    /// INSERT (the new row), UPDATE (the row after assignment) and the DO
    /// UPDATE leg of `ON CONFLICT`: does this row pass `WITH CHECK`?
    ///
    /// **Errors** on violation, never filters. The message is PostgreSQL's;
    /// its "row-level security" is also what the wire keys SQLSTATE 42501 on.
    pub(crate) fn enforce_rls_with_check(guard: Option<&RlsWriteGuard>, tuple: &Tuple) -> Result<()> {
        let Some(guard) = guard else { return Ok(()) };
        let Some(with_check) = &guard.with_check else {
            return Ok(());
        };
        if matches!(guard.evaluator.evaluate(with_check, tuple)?, Value::Boolean(true)) {
            Ok(())
        } else {
            Err(Error::constraint_violation(format!(
                "new row violates row-level security policy for table \"{}\"",
                guard.table_name
            )))
        }
    }
}

/// One rewrite, for one identity.
pub(crate) struct Rewriter<'a> {
    db: &'a EmbeddedDatabase,
    identity: RlsIdentity,
    /// The tables whose policies are being expanded, outermost first: a
    /// policy whose subquery reads its own table would never end.
    expanding: RefCell<Vec<String>>,
}

impl<'a> Rewriter<'a> {
    pub(crate) fn new(db: &'a EmbeddedDatabase) -> Self {
        Self::with_identity(db, db.rls_identity())
    }

    fn with_identity(db: &'a EmbeddedDatabase, identity: RlsIdentity) -> Self {
        Self {
            db,
            identity,
            expanding: RefCell::new(Vec::new()),
        }
    }

    /// `expr` (a predicate of `table`'s policies) with the read policies
    /// applied to the tables its subqueries read.
    fn expanded(&self, table: &str, expr: LogicalExpr) -> Result<LogicalExpr> {
        if !sql::Executor::expr_contains_subquery(&expr) {
            return Ok(expr);
        }
        if self.expanding.borrow().iter().any(|t| t == table) {
            return Err(Error::query_execution(format!("{INFINITE_RECURSION} \"{table}\"")));
        }
        self.expanding.borrow_mut().push(table.to_string());
        let result = self.apply_rls_to_expr(expr);
        self.expanding.borrow_mut().pop();
        result
    }

    /// THE one place a SELECT policy becomes a plan predicate: `Some` iff
    /// row-level security applies to a read of `table_name`. Both scan-leaf
    /// arms route through this, so a read policy has one meaning:
    ///
    /// > A read policy on table T attaches to T's scan leaf as a scan-level
    /// > predicate (`FilteredScan.predicate`), which the executor evaluates
    /// > against the full base-table row BEFORE any projection is applied.
    ///
    /// FAIL-CLOSED: a policy that will not compile propagates `Err`.
    fn rls_read_predicate(&self, table_name: &str) -> Result<Option<LogicalExpr>> {
        if self.identity.exempt_everywhere() {
            return Ok(None);
        }
        match self
            .db
            .rls_table_predicate(table_name, PolicyCommand::Select, Side::Using, &self.identity)?
        {
            Some(found) => Ok(Some(self.expanded(table_name, found.expr)?)),
            None => self.materialized_view_predicate(table_name),
        }
    }

    /// What reading the materialized view `name` needs: the policies of its
    /// base tables, applied to its stored rows. `None` when `name` is no
    /// materialized view or no policy of a base table applies. An error when
    /// one applies and cannot be applied to the stored rows.
    fn materialized_view_predicate(&self, name: &str) -> Result<Option<LogicalExpr>> {
        let store = self.db.tenant_manager.policy_store();
        if !store.active() {
            return Ok(None);
        }
        let Some(view) = store.materialized_view_behind(name) else {
            return Ok(None);
        };
        let mut chain = Vec::new();
        let requirements = self.materialized_view_requirements(&view, &mut chain)?;
        Ok(requirements.into_iter().map(|r| r.expr).reduce(and))
    }

    fn materialized_view_requirements(&self, view: &str, chain: &mut Vec<String>) -> Result<Vec<Requirement>> {
        if chain.iter().any(|v| v == view) {
            return Err(Error::query_execution(format!(
                "materialized view \"{view}\" is defined over itself"
            )));
        }
        let store = self.db.tenant_manager.policy_store();
        let metadata = self.db.storage.mv_catalog().get_view(view)?;
        let plan: LogicalPlan = bincode::deserialize(&metadata.query_plan_bytes).map_err(|e| {
            Error::storage(format!(
                "the definition of materialized view \"{view}\" cannot be read: {e}"
            ))
        })?;
        let mut kept = Vec::new();
        for relation in derived::scanned_tables(&plan) {
            let inner = if let Some(inner_view) = store.materialized_view_behind(&relation) {
                chain.push(view.to_string());
                let inner = self.materialized_view_requirements(&inner_view, chain);
                chain.pop();
                inner?
            } else {
                match self
                    .db
                    .rls_table_predicate(&relation, PolicyCommand::Select, Side::Using, &self.identity)?
                {
                    Some(found) => vec![Requirement {
                        table: relation.clone(),
                        policies: found.policies,
                        expr: self.expanded(&relation, found.expr)?,
                    }],
                    None => Vec::new(),
                }
            };
            for requirement in inner {
                let proof = if self.requirement_is_correlated(&requirement) {
                    Err(derived::Refusal::CorrelatedPolicy)
                } else {
                    derived::prove(&plan, &relation, &derived::policy_columns(&requirement.expr))
                };
                if let Err(refusal) = proof {
                    return Err(self.materialized_view_refusal(view, &requirement, &refusal));
                }
                kept.push(requirement);
            }
        }
        Ok(kept)
    }

    fn requirement_is_correlated(&self, requirement: &Requirement) -> bool {
        if !sql::Executor::expr_contains_subquery(&requirement.expr) {
            return false;
        }
        match self.db.storage.catalog().get_table_schema(&requirement.table) {
            Ok(schema) => EmbeddedDatabase::dml_predicate_is_correlated(&requirement.expr, &schema, &requirement.table),
            // Cannot tell: refuse.
            Err(_) => true,
        }
    }

    /// SQLSTATE 42501 ("row-level security" in a constraint violation),
    /// naming the view, the table, the policy and the column.
    fn materialized_view_refusal(&self, view: &str, requirement: &Requirement, refusal: &derived::Refusal) -> Error {
        let policies = if requirement.policies.is_empty() {
            "the default-deny row-level security".to_string()
        } else {
            let names: Vec<String> = requirement.policies.iter().map(|p| format!("\"{p}\"")).collect();
            format!("row-level security policy {}", names.join(", "))
        };
        let role = self.identity.role.as_deref().unwrap_or("the current session");
        Error::ConstraintViolation(format!(
            "cannot read materialized view \"{view}\" as {role}: {policies} of table \"{}\" cannot be applied to its stored rows: {refusal}",
            requirement.table
        ))
    }

    /// Recursively apply RLS to all Scan operators in a plan
    pub(crate) fn apply_rls_to_plan_recursive(&self, plan: sql::LogicalPlan) -> Result<sql::LogicalPlan> {
        match plan {
            sql::LogicalPlan::Scan {
                table_name,
                alias,
                schema,
                projection,
                as_of,
            } => {
                // Emit a FilteredScan carrying the policy as the scan's own
                // predicate rather than a Filter wrapped ABOVE the scan. The
                // projection is PRESERVED: `FilteredScan` evaluates its
                // predicate against the full base-table schema before rows are
                // projected (`handle_filtered_scan`, and the index fast paths
                // that re-filter with the full-width schema), so a policy on a
                // column outside the projection resolves. `Scan::schema()` and
                // `FilteredScan::schema()` are the same function, so nothing
                // above the leaf sees a different arity or column set.
                if let Some(rls_predicate) = self.rls_read_predicate(&table_name)? {
                    return Ok(sql::LogicalPlan::FilteredScan {
                        table_name,
                        alias,
                        schema,
                        projection,
                        predicate: Some(rls_predicate),
                        as_of,
                    });
                }

                // No RLS, return as-is
                Ok(sql::LogicalPlan::Scan {
                    table_name,
                    alias,
                    schema,
                    projection,
                    as_of,
                })
            }

            sql::LogicalPlan::Filter { input, predicate } => Ok(sql::LogicalPlan::Filter {
                input: self.apply_rls_to_boxed_plan(input)?,
                predicate: self.apply_rls_to_expr(predicate)?,
            }),

            sql::LogicalPlan::Project {
                input,
                exprs,
                aliases,
                distinct,
                distinct_on,
                source_alias,
            } => Ok(sql::LogicalPlan::Project {
                input: self.apply_rls_to_boxed_plan(input)?,
                exprs: self.apply_rls_to_exprs(exprs)?,
                aliases,
                distinct,
                distinct_on: distinct_on.map(|d| self.apply_rls_to_exprs(d)).transpose()?,
                source_alias,
            }),

            sql::LogicalPlan::Aggregate {
                input,
                group_by,
                aggr_exprs,
                having,
            } => Ok(sql::LogicalPlan::Aggregate {
                input: self.apply_rls_to_boxed_plan(input)?,
                group_by: self.apply_rls_to_exprs(group_by)?,
                aggr_exprs: self.apply_rls_to_exprs(aggr_exprs)?,
                having: self.apply_rls_to_opt_expr(having)?,
            }),

            sql::LogicalPlan::Join {
                left,
                right,
                join_type,
                on,
                lateral,
            } => Ok(sql::LogicalPlan::Join {
                left: self.apply_rls_to_boxed_plan(left)?,
                right: self.apply_rls_to_boxed_plan(right)?,
                join_type,
                on: self.apply_rls_to_opt_expr(on)?,
                lateral,
            }),

            sql::LogicalPlan::Sort { input, exprs, asc } => Ok(sql::LogicalPlan::Sort {
                input: self.apply_rls_to_boxed_plan(input)?,
                exprs: self.apply_rls_to_exprs(exprs)?,
                asc,
            }),

            sql::LogicalPlan::Limit {
                input,
                limit,
                offset,
                limit_param,
                offset_param,
            } => Ok(sql::LogicalPlan::Limit {
                input: self.apply_rls_to_boxed_plan(input)?,
                limit,
                offset,
                limit_param,
                offset_param,
            }),

            // Handle FilteredScan - inject RLS filter into the existing predicate
            sql::LogicalPlan::FilteredScan {
                table_name,
                alias,
                schema,
                projection,
                predicate,
                as_of,
            } => {
                // A pushed-down predicate may itself hold a subquery over a
                // protected table (`WHERE id IN (SELECT … FROM docs)`).
                let predicate = self.apply_rls_to_opt_expr(predicate)?;
                // Same rule, same parse (see `rls_read_predicate`): the policy
                // becomes part of this scan's own predicate. AND-merged with
                // any predicate already pushed here by the optimizer.
                if let Some(rls_predicate) = self.rls_read_predicate(&table_name)? {
                    // Combine existing predicate with RLS predicate using AND
                    let combined_predicate = if let Some(existing) = predicate {
                        Some(sql::LogicalExpr::BinaryExpr {
                            left: Box::new(existing),
                            op: sql::BinaryOperator::And,
                            right: Box::new(rls_predicate),
                        })
                    } else {
                        Some(rls_predicate)
                    };

                    return Ok(sql::LogicalPlan::FilteredScan {
                        table_name,
                        alias,
                        schema,
                        projection,
                        predicate: combined_predicate,
                        as_of,
                    });
                }

                // No RLS, return as-is
                Ok(sql::LogicalPlan::FilteredScan {
                    table_name,
                    alias,
                    schema,
                    projection,
                    predicate,
                    as_of,
                })
            }

            // Set operations: a policy applies to the relation on either side
            // (v05.6; before, both inputs were returned unfiltered).
            sql::LogicalPlan::Union { left, right, all } => Ok(sql::LogicalPlan::Union {
                left: self.apply_rls_to_boxed_plan(left)?,
                right: self.apply_rls_to_boxed_plan(right)?,
                all,
            }),
            sql::LogicalPlan::Intersect { left, right, all } => Ok(sql::LogicalPlan::Intersect {
                left: self.apply_rls_to_boxed_plan(left)?,
                right: self.apply_rls_to_boxed_plan(right)?,
                all,
            }),
            sql::LogicalPlan::Except { left, right, all } => Ok(sql::LogicalPlan::Except {
                left: self.apply_rls_to_boxed_plan(left)?,
                right: self.apply_rls_to_boxed_plan(right)?,
                all,
            }),

            // Every CTE body and the main query.
            sql::LogicalPlan::With { ctes, query, recursive } => Ok(sql::LogicalPlan::With {
                ctes: ctes
                    .into_iter()
                    .map(|(name, plan, columns)| Ok((name, self.apply_rls_to_boxed_plan(plan)?, columns)))
                    .collect::<Result<Vec<_>>>()?,
                query: self.apply_rls_to_boxed_plan(query)?,
                recursive,
            }),

            // EXPLAIN ANALYZE runs its input; plain EXPLAIN shows the plan that
            // would run, which is the filtered one.
            sql::LogicalPlan::Explain { input, options } => Ok(sql::LogicalPlan::Explain {
                input: self.apply_rls_to_boxed_plan(input)?,
                options,
            }),

            sql::LogicalPlan::TableFunction {
                function_name,
                args,
                alias,
                column_alias,
            } => Ok(sql::LogicalPlan::TableFunction {
                function_name,
                args: self.apply_rls_to_exprs(args)?,
                alias,
                column_alias,
            }),

            // Not reached nested (their sources are rewritten at the call sites),
            // given arms so the match stays exhaustive and a direct call is
            // still correct.
            sql::LogicalPlan::InsertSelect {
                table_name,
                columns,
                source,
                returning,
                on_conflict,
            } => Ok(sql::LogicalPlan::InsertSelect {
                table_name,
                columns,
                source: self.apply_rls_to_boxed_plan(source)?,
                returning,
                on_conflict,
            }),
            sql::LogicalPlan::CreateTableAs {
                name,
                column_names,
                if_not_exists,
                query,
                with_data,
            } => Ok(sql::LogicalPlan::CreateTableAs {
                name,
                column_names,
                if_not_exists,
                query: self.apply_rls_to_boxed_plan(query)?,
                with_data,
            }),

            // Nodes that read no table through this rewrite. Listed by name, with
            // no wildcard, so a new plan-bearing variant fails to compile until
            // it is classified. Notes on the less obvious members:
            //   * Update / Delete / Insert: read policies on their own target
            //     are write guards (`build_rls_write_guard`); the subqueries in
            //     UPDATE … SET are rewritten in the Update executor arms.
            //   * Prepare: the stored statement is rewritten when EXECUTE runs it
            //     under the executing session's context, never baked in here.
            //   * CreateView / CreateMaterializedView / CreateTrigger: stored
            //     definitions; a view is filtered when it is expanded in a read,
            //     a materialized view's stored rows are filtered when read
            //     (`materialized_view_predicate`).
            plan @ (sql::LogicalPlan::Insert { .. }
            | sql::LogicalPlan::CreateTable { .. }
            | sql::LogicalPlan::DropTable { .. }
            | sql::LogicalPlan::DropMulti { .. }
            | sql::LogicalPlan::Truncate { .. }
            | sql::LogicalPlan::Update { .. }
            | sql::LogicalPlan::Delete { .. }
            | sql::LogicalPlan::CreateIndex { .. }
            | sql::LogicalPlan::CreateSequence { .. }
            | sql::LogicalPlan::DropSequence { .. }
            | sql::LogicalPlan::AlterSequence(_)
            | sql::LogicalPlan::CreateEnumType { .. }
            | sql::LogicalPlan::DropEnumType { .. }
            | sql::LogicalPlan::CreateSchema { .. }
            | sql::LogicalPlan::DropSchema { .. }
            | sql::LogicalPlan::CreateExtension { .. }
            | sql::LogicalPlan::CreateDatabase { .. }
            | sql::LogicalPlan::DropDatabase { .. }
            | sql::LogicalPlan::DropExtension { .. }
            | sql::LogicalPlan::AlterColumnStorage { .. }
            | sql::LogicalPlan::AlterTableAddColumn { .. }
            | sql::LogicalPlan::AlterTableDropColumn { .. }
            | sql::LogicalPlan::AlterTableRenameColumn { .. }
            | sql::LogicalPlan::AlterTableAlterColumnNullability { .. }
            | sql::LogicalPlan::AlterTableRename { .. }
            | sql::LogicalPlan::AlterTableSetSchema { .. }
            | sql::LogicalPlan::AlterTableAddForeignKey { .. }
            | sql::LogicalPlan::AlterTableAlterConstraintEnforcement { .. }
            | sql::LogicalPlan::AlterTableDropConstraint { .. }
            | sql::LogicalPlan::AlterTableMulti { .. }
            | sql::LogicalPlan::CreateBranch { .. }
            | sql::LogicalPlan::DropBranch { .. }
            | sql::LogicalPlan::MergeBranch { .. }
            | sql::LogicalPlan::UseBranch { .. }
            | sql::LogicalPlan::ShowBranches
            | sql::LogicalPlan::CreateMaterializedView { .. }
            | sql::LogicalPlan::RefreshMaterializedView { .. }
            | sql::LogicalPlan::DropMaterializedView { .. }
            | sql::LogicalPlan::AlterMaterializedView { .. }
            | sql::LogicalPlan::CreateView { .. }
            | sql::LogicalPlan::DropView { .. }
            | sql::LogicalPlan::SystemView { .. }
            | sql::LogicalPlan::CreateTrigger { .. }
            | sql::LogicalPlan::DropTrigger { .. }
            | sql::LogicalPlan::StartTransaction
            | sql::LogicalPlan::Commit
            | sql::LogicalPlan::Rollback
            | sql::LogicalPlan::Savepoint { .. }
            | sql::LogicalPlan::ReleaseSavepoint { .. }
            | sql::LogicalPlan::RollbackToSavepoint { .. }
            | sql::LogicalPlan::Prepare { .. }
            | sql::LogicalPlan::Execute { .. }
            | sql::LogicalPlan::Deallocate { .. }
            | sql::LogicalPlan::SetConstraints { .. }
            | sql::LogicalPlan::CreateFunction { .. }
            | sql::LogicalPlan::CreateProcedure { .. }
            | sql::LogicalPlan::DropFunction { .. }
            | sql::LogicalPlan::DropProcedure { .. }
            | sql::LogicalPlan::Call { .. }
            | sql::LogicalPlan::DualScan
            | sql::LogicalPlan::Noop
            | sql::LogicalPlan::DropRole { .. }
            | sql::LogicalPlan::CreateRole { .. }
            | sql::LogicalPlan::AlterRole { .. }
            | sql::LogicalPlan::GrantPrivileges { .. }
            | sql::LogicalPlan::RevokePrivileges { .. }
            | sql::LogicalPlan::DropIndex { .. }
            | sql::LogicalPlan::AlterTableAddUnique { .. }
            | sql::LogicalPlan::AlterTableAddPrimaryKey { .. }
            | sql::LogicalPlan::AlterTableAddCheck { .. }
            | sql::LogicalPlan::DropTableCascade { .. }
            | sql::LogicalPlan::Notification(_)
            | sql::LogicalPlan::RowSecurity(_)) => Ok(plan),

            #[cfg(feature = "ha-tier1")]
            plan @ (sql::LogicalPlan::Switchover { .. }
            | sql::LogicalPlan::SwitchoverCheck { .. }
            | sql::LogicalPlan::ClusterStatus
            | sql::LogicalPlan::SetNodeAlias { .. }
            | sql::LogicalPlan::ShowTopology) => Ok(plan),
        }
    }

    /// Apply read policies to every plan embedded in `expr` (scalar, IN and
    /// EXISTS subqueries, at any depth). The owning counterpart of the
    /// executor's read-only `walk_expr_subplans`: the expression tree is rebuilt
    /// unchanged except that each embedded plan goes through
    /// [`apply_rls_to_plan_recursive`](Self::apply_rls_to_plan_recursive), so a
    /// policy still becomes a predicate only in
    /// [`rls_read_predicate`](Self::rls_read_predicate) and a parse error still
    /// fails the statement.
    ///
    /// The subquery executors run the plan held in the expression, they do not
    /// plan again from text, so rewriting the embedded plan is enough.
    ///
    /// Callers check `has_effective_tenant_context()` first (this walk is
    /// skipped entirely without one). Used by the plan rewrite for every
    /// expression-bearing node and by UPDATE … SET, whose assignment
    /// expressions are evaluated outside the plan rewrite.
    pub(crate) fn apply_rls_to_expr(&self, expr: sql::LogicalExpr) -> Result<sql::LogicalExpr> {
        use sql::LogicalExpr as E;
        Ok(match expr {
            E::ScalarSubquery { subquery } => E::ScalarSubquery {
                subquery: Box::new(self.apply_rls_to_plan_recursive(*subquery)?),
            },
            E::InSubquery {
                expr,
                subquery,
                negated,
            } => E::InSubquery {
                expr: self.apply_rls_to_boxed_expr(expr)?,
                subquery: Box::new(self.apply_rls_to_plan_recursive(*subquery)?),
                negated,
            },
            E::Exists { subquery, negated } => E::Exists {
                subquery: Box::new(self.apply_rls_to_plan_recursive(*subquery)?),
                negated,
            },
            E::BinaryExpr { left, op, right } => E::BinaryExpr {
                left: self.apply_rls_to_boxed_expr(left)?,
                op,
                right: self.apply_rls_to_boxed_expr(right)?,
            },
            E::UnaryExpr { op, expr } => E::UnaryExpr {
                op,
                expr: self.apply_rls_to_boxed_expr(expr)?,
            },
            E::AggregateFunction { fun, args, distinct } => E::AggregateFunction {
                fun,
                args: self.apply_rls_to_exprs(args)?,
                distinct,
            },
            E::ScalarFunction { fun, args } => E::ScalarFunction {
                fun,
                args: self.apply_rls_to_exprs(args)?,
            },
            E::Case {
                expr,
                when_then,
                else_result,
            } => E::Case {
                expr: match expr {
                    Some(e) => Some(self.apply_rls_to_boxed_expr(e)?),
                    None => None,
                },
                when_then: when_then
                    .into_iter()
                    .map(|(w, t)| Ok((self.apply_rls_to_expr(w)?, self.apply_rls_to_expr(t)?)))
                    .collect::<Result<Vec<_>>>()?,
                else_result: match else_result {
                    Some(e) => Some(self.apply_rls_to_boxed_expr(e)?),
                    None => None,
                },
            },
            E::Cast { expr, data_type } => E::Cast {
                expr: self.apply_rls_to_boxed_expr(expr)?,
                data_type,
            },
            E::IsNull { expr, is_null } => E::IsNull {
                expr: self.apply_rls_to_boxed_expr(expr)?,
                is_null,
            },
            E::Between {
                expr,
                low,
                high,
                negated,
            } => E::Between {
                expr: self.apply_rls_to_boxed_expr(expr)?,
                low: self.apply_rls_to_boxed_expr(low)?,
                high: self.apply_rls_to_boxed_expr(high)?,
                negated,
            },
            E::InList { expr, list, negated } => E::InList {
                expr: self.apply_rls_to_boxed_expr(expr)?,
                list: self.apply_rls_to_exprs(list)?,
                negated,
            },
            E::InSet { expr, values, negated } => E::InSet {
                expr: self.apply_rls_to_boxed_expr(expr)?,
                values,
                negated,
            },
            E::ArraySubscript { array, index } => E::ArraySubscript {
                array: self.apply_rls_to_boxed_expr(array)?,
                index: self.apply_rls_to_boxed_expr(index)?,
            },
            E::Tuple { items } => E::Tuple {
                items: self.apply_rls_to_exprs(items)?,
            },
            E::WindowFunction {
                fun,
                args,
                partition_by,
                order_by,
                frame,
            } => E::WindowFunction {
                fun,
                args: self.apply_rls_to_exprs(args)?,
                partition_by: self.apply_rls_to_exprs(partition_by)?,
                order_by: order_by
                    .into_iter()
                    .map(|(e, asc)| Ok((self.apply_rls_to_expr(e)?, asc)))
                    .collect::<Result<Vec<_>>>()?,
                frame,
            },
            // Leaves: no child expression, no embedded plan. Listed by name (no
            // wildcard) so a new expression variant must be classified here.
            leaf @ (E::Column { .. }
            | E::Literal(_)
            | E::DefaultValue
            | E::Wildcard
            | E::Parameter { .. }
            | E::NewRow { .. }
            | E::OldRow { .. }
            | E::BoundColumn { .. }) => leaf,
        })
    }

    fn apply_rls_to_boxed_expr(&self, expr: Box<sql::LogicalExpr>) -> Result<Box<sql::LogicalExpr>> {
        Ok(Box::new(self.apply_rls_to_expr(*expr)?))
    }

    fn apply_rls_to_exprs(&self, exprs: Vec<sql::LogicalExpr>) -> Result<Vec<sql::LogicalExpr>> {
        exprs.into_iter().map(|e| self.apply_rls_to_expr(e)).collect()
    }

    fn apply_rls_to_opt_expr(&self, expr: Option<sql::LogicalExpr>) -> Result<Option<sql::LogicalExpr>> {
        expr.map(|e| self.apply_rls_to_expr(e)).transpose()
    }

    fn apply_rls_to_boxed_plan(&self, plan: Box<sql::LogicalPlan>) -> Result<Box<sql::LogicalPlan>> {
        Ok(Box::new(self.apply_rls_to_plan_recursive(*plan)?))
    }
}

/// A base table's policies, as something a materialized view over it must
/// satisfy.
struct Requirement {
    /// The base table.
    table: String,
    /// The policies, for a message.
    policies: Vec<String>,
    /// Their predicate, over the base table's columns.
    expr: LogicalExpr,
}
