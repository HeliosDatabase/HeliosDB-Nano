//! One statement of a routine body, run for the routine's interpreter.
//!
//! The interpreter hands over a statement text. Beside it
//! (`sql::procedural::host_channel`) come the values of the routine variables
//! the statement names, as its parameters `$1…`; and back goes what the
//! statement did: its row count and the names of the columns it returned,
//! which is what `FOUND`, `GET DIAGNOSTICS` and record variables are made of.

use crate::sql::procedural::host_channel as channel;
use crate::sql::procedural::runtime::BLOCK_SAVEPOINT_PREFIX;
use crate::sql::sql_scan;
use crate::{starts_with_icase, storage, EmbeddedDatabase, Error, Result, Value};

/// What the first words of a body statement say it is.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// `SELECT …` / `WITH …`: read through the query path, as before.
    Query,
    /// `INSERT`, `UPDATE` or `DELETE`.
    Dml,
    Other,
}

fn kind_of(sql: &str) -> Kind {
    if starts_with_icase(sql, "SELECT") || starts_with_icase(sql, "WITH") {
        Kind::Query
    } else if starts_with_icase(sql, "INSERT") || starts_with_icase(sql, "UPDATE") || starts_with_icase(sql, "DELETE") {
        Kind::Dml
    } else {
        Kind::Other
    }
}

fn has_returning(sql: &str) -> bool {
    let toks = sql_scan::tokens(sql);
    toks.iter().any(|t| t.depth == 0 && t.is_word(sql, "RETURNING"))
}

/// The savepoint statement of an `EXCEPTION` block: `(verb, name)`.
fn block_savepoint(sql: &str) -> Option<(&'static str, String)> {
    if !sql.contains(BLOCK_SAVEPOINT_PREFIX) {
        return None;
    }
    let toks = sql_scan::tokens(sql);
    let name = toks.last().filter(|t| t.text(sql).starts_with(BLOCK_SAVEPOINT_PREFIX))?;
    let first = toks.first()?;
    let verb = if first.is_word(sql, "SAVEPOINT") {
        "SAVEPOINT"
    } else if first.is_word(sql, "ROLLBACK") {
        "ROLLBACK"
    } else if first.is_word(sql, "RELEASE") {
        "RELEASE"
    } else {
        return None;
    };
    Some((verb, name.text(sql).to_string()))
}

fn cannot_bind() -> Error {
    Error::sql_state(
        "0A000",
        "a routine variable can be named in a query or an INSERT, UPDATE or DELETE of the routine; \
         in any other statement write it with the `$` sigil",
    )
}

fn values_of(rows: Vec<crate::Tuple>) -> Vec<Vec<Value>> {
    rows.into_iter().map(|row| row.values).collect()
}

impl EmbeddedDatabase {
    fn routine_plan(&self, sql: &str) -> Result<crate::sql::LogicalPlan> {
        let (statement, _) = self.parse_cached(sql)?;
        let catalog = self.storage.catalog();
        crate::sql::Planner::with_catalog(&catalog)
            .with_sql(sql.to_string())
            .with_current_schema(self.current_schema())
            .with_search_path(self.current_search_path())
            .statement_to_plan(statement)
    }

    /// Run one statement of a routine body inside the transaction the routine
    /// was called in.
    pub(crate) fn run_routine_body_statement(
        &self,
        body_sql: &str,
        txn: &storage::Transaction,
    ) -> Result<Vec<Vec<Value>>> {
        let params = channel::take_params();
        let trimmed = body_sql.trim();
        if let Some((verb, name)) = block_savepoint(trimmed) {
            match verb {
                "SAVEPOINT" => self.savepoint_establish(txn, &name),
                "ROLLBACK" => self.savepoint_rollback_to(txn, &name)?,
                _ => Self::savepoint_release(txn, &name)?,
            }
            return Ok(Vec::new());
        }
        // `SET search_path = …` (and the other settings) in a body is a
        // statement: the planner has no arm for it, the setting interceptor does.
        if (starts_with_icase(trimmed, "SET ") || starts_with_icase(trimmed, "RESET "))
            && params.is_empty()
            && self.try_handle_db_setting_statement_with_schema(trimmed)?.is_some()
        {
            return Ok(Vec::new());
        }
        match kind_of(trimmed) {
            Kind::Query => {
                let plan = self.routine_plan(trimmed)?;
                let columns = plan.schema().columns.iter().map(|c| c.name.clone()).collect();
                let rows = self.query_plan_with_params(&plan, &params, Some(txn))?;
                channel::report(rows.len() as u64, columns);
                Ok(values_of(rows))
            }
            // With bound variables or a RETURNING list the statement runs as a
            // plan with its parameters and gives its rows back.
            Kind::Dml if !params.is_empty() || has_returning(trimmed) => {
                let plan = self.routine_plan(trimmed)?;
                let (count, rows) = self.execute_plan_with_params(&plan, &params, Some(txn))?;
                self.invalidate_result_cache();
                channel::report(count, Vec::new());
                Ok(values_of(rows))
            }
            _ if !params.is_empty() => Err(cannot_bind()),
            _ => {
                let count = self.execute_in_transaction_no_fast_path(trimmed, txn)?;
                channel::report(count, Vec::new());
                Ok(Vec::new())
            }
        }
    }

    /// Run one statement of a routine body when the routine was called
    /// outside any transaction this path can borrow: the statement is its own.
    pub(crate) fn run_routine_body_statement_autocommit(&self, body_sql: &str) -> Result<Vec<Vec<Value>>> {
        let params = channel::take_params();
        let trimmed = body_sql.trim();
        if block_savepoint(trimmed).is_some() {
            return Err(Error::sql_state(
                "25P01",
                "no transaction to mark the start of an EXCEPTION block in",
            ));
        }
        match kind_of(trimmed) {
            Kind::Query => {
                let (rows, schema) = if params.is_empty() {
                    self.query_with_schema(body_sql)?
                } else {
                    self.query_params_with_schema(body_sql, &params)?
                };
                let columns = schema.columns.iter().map(|c| c.name.clone()).collect();
                channel::report(rows.len() as u64, columns);
                Ok(values_of(rows))
            }
            Kind::Dml if !params.is_empty() || has_returning(trimmed) => {
                let (count, rows) = self.execute_params_returning(body_sql, &params)?;
                channel::report(count, Vec::new());
                Ok(values_of(rows))
            }
            _ if !params.is_empty() => Err(cannot_bind()),
            _ => {
                let count = self.execute(body_sql)?;
                channel::report(count, Vec::new());
                Ok(Vec::new())
            }
        }
    }
}
