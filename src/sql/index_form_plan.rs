//! Planning of the index forms `CREATE INDEX` can ask for beyond one column:
//! a multi-column key, a `WHERE` predicate (partial index), and what an
//! expression key needs recorded. Called from the planner's `CREATE INDEX` arm;
//! the options built here are what the executor builds the index from and what
//! the durable definition keeps (see `crate::storage::index_form`).

use sqlparser::ast::{CreateIndex, Expr};

use super::logical_plan::IndexOption;
use super::Planner;
use crate::storage::{index_family, IndexFamily};
use crate::{Error, Result};

/// The table columns `expr` reads, in first-use order.
fn referenced_columns(expr: &Expr) -> Vec<String> {
    use std::ops::ControlFlow;
    let mut referenced: Vec<String> = Vec::new();
    let _ = sqlparser::ast::visit_expressions(expr, |e| {
        let name = match e {
            Expr::Identifier(ident) => Some(Planner::normalize_ident(ident)),
            Expr::CompoundIdentifier(parts) => parts.last().map(Planner::normalize_ident),
            _ => None,
        };
        if let Some(name) = name {
            if !referenced.contains(&name) {
                referenced.push(name);
            }
        }
        ControlFlow::<()>::Continue(())
    });
    referenced
}

/// Add to `options` what the statement's key list and `WHERE` clause ask for:
///
/// * the full column list of an ordinary multi-column index (a UNIQUE one
///   already carries its own), so every column is indexed, not the leading one;
/// * the predicate of a partial index.
///
/// `method` is the `USING` access method as written (`None`: the default), and
/// `has_expression` says whether a key part is an expression (the caller has
/// then recorded the key as `IndexOption::Expression`).
pub fn add_form_options(
    planner: &Planner<'_>,
    create_index: &CreateIndex,
    method: Option<&str>,
    has_expression: bool,
    options: &mut Vec<IndexOption>,
) -> Result<()> {
    let tree_method = index_family(method) == Some(IndexFamily::Art);

    if tree_method && !has_expression && !create_index.unique && create_index.columns.len() > 1 {
        let mut columns = Vec::with_capacity(create_index.columns.len());
        for col in &create_index.columns {
            match &col.expr {
                Expr::Identifier(ident) => columns.push(Planner::normalize_ident(ident)),
                _ => return Err(Error::query_execution("Column name expected in CREATE INDEX")),
            }
        }
        options.push(IndexOption::UniqueColumns(columns));
    }

    if let Some(predicate) = &create_index.predicate {
        if !tree_method {
            return Err(Error::query_execution(format!(
                "a partial index (CREATE INDEX … WHERE) USING {} {}; partial indexes are btree indexes",
                method.unwrap_or("?"),
                crate::error::UNSUPPORTED_STATEMENT_KIND_MARKER
            )));
        }
        // Planned once here so a predicate that does not plan fails the
        // statement in the planner's own words.
        planner.expr_to_logical(predicate)?;
        // Kept without redundant outer parentheses, so `WHERE (p)` (as a dump
        // spells it) and `WHERE p` record the same text.
        let mut bare = predicate;
        while let Expr::Nested(inner) = bare {
            bare = inner;
        }
        options.push(IndexOption::Predicate {
            sql: bare.to_string(),
            columns: referenced_columns(bare),
        });
    }
    Ok(())
}
