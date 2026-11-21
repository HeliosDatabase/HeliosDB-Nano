//! Planning of `ALTER TABLE … ALTER COLUMN` forms, and the refusal of every
//! `ALTER TABLE` form that has no plan.
//!
//! sqlparser 0.53 reads `SET NOT NULL`, `DROP NOT NULL`, `SET DEFAULT`, `DROP
//! DEFAULT`, `[SET DATA] TYPE … [USING …]` and `ADD GENERATED … AS IDENTITY`. It
//! has no grammar for `DROP IDENTITY`, `SET STATISTICS` and the other PostgreSQL
//! forms, so a statement holding one fails to parse. [`rewrite_unparsed_forms`]
//! (run by the parser only after that failure) turns each such clause into a
//! `SET DEFAULT <marker call>` clause the grammar reads, and
//! [`plan_alter_column`] turns the marker back into the form it stands for: an
//! action, or a refusal (SQLSTATE 0A000) that names the form in SQL words.

use sqlparser::ast::{AlterColumnOperation, AlterTableOperation, Ident};

use super::logical_plan::{AlterColumnAction, LogicalPlan};
use super::pieces::{pieces, Piece};
use super::planner::Planner;
use crate::error::UNSUPPORTED_STATEMENT_KIND_MARKER;
use crate::{Error, Result};

/// `ALTER COLUMN` forms of PostgreSQL the grammar lacks and nothing here
/// implements: the words they start with, and their name in a refusal.
const KNOWN_UNSUPPORTED: &[(&[&str], &str)] = &[
    (&["SET", "COMPRESSION"], "SET COMPRESSION"),
    (&["SET", "GENERATED"], "SET GENERATED"),
    (&["SET", "EXPRESSION"], "SET EXPRESSION"),
    (&["DROP", "EXPRESSION"], "DROP EXPRESSION"),
    (&["RESTART"], "RESTART"),
    (&["RESET"], "RESET ( … )"),
];

/// The marker calls a rewritten clause carries (never evaluated).
const DROP_IDENTITY_MARKER: &str = "helios_alter_column_drop_identity";
const SET_STATISTICS_MARKER: &str = "helios_alter_column_set_statistics";
const UNSUPPORTED_MARKER: &str = "helios_alter_column_unsupported";

/// The refusal of an `ALTER TABLE` form that has no plan: SQLSTATE 0A000, the
/// form in SQL words.
pub(crate) fn unsupported_form(form: &str) -> Error {
    let mut words: String = form.split_whitespace().collect::<Vec<_>>().join(" ");
    if words.chars().count() > 120 {
        words = words.chars().take(120).collect::<String>() + "…";
    }
    Error::query_execution(format!("ALTER TABLE … {words} {UNSUPPORTED_STATEMENT_KIND_MARKER}"))
}

/// The refusal of a parsed `ALTER TABLE` operation that has no plan.
pub(crate) fn unsupported_operation(operation: &AlterTableOperation) -> Error {
    unsupported_form(&operation.to_string())
}

/// The argument text of `marker(...)` when `text` is exactly that call.
fn marker_argument<'t>(text: &'t str, marker: &str) -> Option<&'t str> {
    let rest = text.trim().strip_prefix(marker)?.trim_start();
    Some(rest.strip_prefix('(')?.strip_suffix(')')?.trim())
}

/// One `ALTER [COLUMN] column <operation>` clause as a plan.
pub(crate) fn plan_alter_column(
    planner: &Planner<'_>,
    table_name: String,
    column: &Ident,
    operation: AlterColumnOperation,
) -> Result<LogicalPlan> {
    let column_name = Planner::normalize_ident(column);
    let action = match operation {
        AlterColumnOperation::DropNotNull => {
            return Ok(LogicalPlan::AlterTableAlterColumnNullability {
                table_name,
                column_name,
                nullable: true,
            })
        }
        AlterColumnOperation::SetNotNull => {
            return Ok(LogicalPlan::AlterTableAlterColumnNullability {
                table_name,
                column_name,
                nullable: false,
            })
        }
        AlterColumnOperation::SetDefault { value } => {
            let text = value.to_string();
            if let Some(argument) = marker_argument(&text, DROP_IDENTITY_MARKER) {
                AlterColumnAction::DropIdentity {
                    if_exists: argument == "1",
                }
            } else if let Some(argument) = marker_argument(&text, SET_STATISTICS_MARKER) {
                let target: i32 = argument
                    .parse()
                    .map_err(|_| Error::sql_parse(format!("invalid statistics target \"{argument}\"")))?;
                AlterColumnAction::SetStatistics((target >= 0).then_some(target))
            } else if let Some(argument) = marker_argument(&text, UNSUPPORTED_MARKER) {
                return Err(unsupported_form(&format!(
                    "ALTER COLUMN … {}",
                    argument.trim_matches('\'')
                )));
            } else {
                AlterColumnAction::SetDefault(Box::new(planner.expr_to_logical(&value)?))
            }
        }
        AlterColumnOperation::DropDefault => AlterColumnAction::DropDefault,
        AlterColumnOperation::SetDataType { data_type, using } => AlterColumnAction::SetDataType {
            data_type: planner.sql_data_type_to_data_type(&data_type)?,
            using: using
                .as_ref()
                .map(|expr| planner.expr_to_logical(expr).map(Box::new))
                .transpose()?,
        },
        AlterColumnOperation::AddGenerated { .. } => AlterColumnAction::AddIdentity,
    };
    Ok(LogicalPlan::AlterTableAlterColumn {
        table_name,
        column_name,
        action,
    })
}

/// Rewrite the `ALTER [COLUMN] c <form>` clauses of one `ALTER TABLE` statement
/// whose form sqlparser cannot read (see the module docs). `None` when `sql` is
/// not an `ALTER TABLE` statement or holds no such clause.
pub(crate) fn rewrite_unparsed_forms(sql: &str) -> Option<String> {
    let pieces = pieces(sql);
    let text = |piece: &Piece| sql.get(piece.start..piece.end).unwrap_or_default();
    let is = |index: usize, keyword: &str| {
        pieces
            .get(index)
            .is_some_and(|piece| piece.word && piece.depth == 0 && text(piece).eq_ignore_ascii_case(keyword))
    };
    if !(is(0, "ALTER") && is(1, "TABLE")) {
        return None;
    }
    // (from, to, replacement), in statement order.
    let mut edits: Vec<(usize, usize, String)> = Vec::new();
    let mut index = 2usize;
    while index < pieces.len() {
        if !is(index, "ALTER") {
            index += 1;
            continue;
        }
        let mut at = index + 1;
        if is(at, "COLUMN") {
            at += 1;
        }
        // The column, then the form.
        let form = at + 1;
        let Some(first) = pieces.get(form) else {
            break;
        };
        // The clause ends at the next comma or semicolon outside parentheses.
        let clause_end = pieces
            .iter()
            .enumerate()
            .skip(form)
            .find(|(_, piece)| piece.depth == 0 && !piece.word && matches!(text(piece), "," | ";"))
            .map_or(pieces.len(), |(position, _)| position);
        let last = pieces
            .get(clause_end.saturating_sub(1))
            .map_or(first.end, |piece| piece.end);
        let replacement = if is(form, "DROP") && is(form + 1, "IDENTITY") {
            let if_exists = is(form + 2, "IF") && is(form + 3, "EXISTS");
            Some(format!(
                "SET DEFAULT {DROP_IDENTITY_MARKER}({})",
                if if_exists { 1 } else { 0 }
            ))
        } else if is(form, "SET") && is(form + 1, "STATISTICS") {
            let argument: String = pieces
                .get(form + 2..clause_end)
                .unwrap_or_default()
                .iter()
                .map(text)
                .collect();
            let target = if argument.eq_ignore_ascii_case("DEFAULT") {
                Some(-1)
            } else {
                argument.parse::<i32>().ok()
            };
            target.map(|target| format!("SET DEFAULT {SET_STATISTICS_MARKER}({target})"))
        } else {
            KNOWN_UNSUPPORTED
                .iter()
                .find(|(words, _)| words.iter().enumerate().all(|(offset, word)| is(form + offset, word)))
                .map(|(_, name)| (*name).to_string())
                .or_else(|| {
                    // `SET ( attribute_option = value [, …] )`.
                    (is(form, "SET") && pieces.get(form + 1).is_some_and(|piece| text(piece) == "("))
                        .then(|| "SET ( … )".to_string())
                })
                .map(|name| format!("SET DEFAULT {UNSUPPORTED_MARKER}('{name}')"))
        };
        if let Some(replacement) = replacement {
            edits.push((first.start, last, replacement));
        }
        index = clause_end.max(index + 1);
    }
    if edits.is_empty() {
        return None;
    }
    let mut rewritten = String::with_capacity(sql.len());
    let mut copied = 0usize;
    for (from, to, replacement) in edits {
        rewritten.push_str(sql.get(copied..from)?);
        rewritten.push_str(&replacement);
        copied = to;
    }
    rewritten.push_str(sql.get(copied..)?);
    Some(rewritten)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn forms_the_grammar_lacks_are_rewritten_and_the_rest_left() {
        assert_eq!(
            rewrite_unparsed_forms("ALTER TABLE t ALTER COLUMN id DROP IDENTITY").unwrap(),
            "ALTER TABLE t ALTER COLUMN id SET DEFAULT helios_alter_column_drop_identity(0)"
        );
        assert_eq!(
            rewrite_unparsed_forms("alter table t alter id drop identity if exists;").unwrap(),
            "alter table t alter id SET DEFAULT helios_alter_column_drop_identity(1);"
        );
        assert_eq!(
            rewrite_unparsed_forms("ALTER TABLE t ALTER COLUMN a SET STATISTICS 100, ALTER COLUMN b SET STATISTICS -1")
                .unwrap(),
            "ALTER TABLE t ALTER COLUMN a SET DEFAULT helios_alter_column_set_statistics(100), ALTER COLUMN b SET \
             DEFAULT helios_alter_column_set_statistics(-1)"
        );
        assert_eq!(
            rewrite_unparsed_forms("ALTER TABLE t ALTER COLUMN a SET COMPRESSION lz4").unwrap(),
            "ALTER TABLE t ALTER COLUMN a SET DEFAULT helios_alter_column_unsupported('SET COMPRESSION')"
        );
        assert_eq!(
            rewrite_unparsed_forms("ALTER TABLE t ALTER COLUMN a SET (n_distinct = 5), ADD COLUMN b INT").unwrap(),
            "ALTER TABLE t ALTER COLUMN a SET DEFAULT helios_alter_column_unsupported('SET ( … )'), ADD COLUMN b INT"
        );
        assert!(rewrite_unparsed_forms("ALTER TABLE t ALTER COLUMN a SET DEFAULT 5").is_none());
        assert!(rewrite_unparsed_forms("ALTER TABLE t ADD COLUMN note TEXT DEFAULT 'DROP IDENTITY'").is_none());
        assert!(rewrite_unparsed_forms("SELECT 'ALTER TABLE t ALTER c DROP IDENTITY'").is_none());
    }
}
