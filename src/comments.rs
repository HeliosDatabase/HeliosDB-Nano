//! `COMMENT ON <object> IS '…'`: stored, read back, and carried by dumps.
//!
//! The statement is read here, by its words (sqlparser 0.53 knows the TABLE and
//! COLUMN forms only). The text is kept by the catalog (`meta:comment:` records,
//! `storage::catalog_comments`); `IS NULL` removes it. `pg_description`,
//! `obj_description()`, `col_description()` and `shobj_description()` answer from
//! those records; a dump writes one `COMMENT ON` statement per record.
//!
//! As before, a comment on a table or a column needs its target to exist (unless
//! `IF EXISTS` is written), and every other form is taken as written: scripts
//! from other databases comment on objects this one does not have (an extension,
//! a constraint under another name).

use crate::sql::pieces::pieces;
use crate::storage::StoredComment;
use crate::{sql, EmbeddedDatabase, Error, Result, Value};

/// Object kinds of more than one word, longest first.
const LONG_KINDS: &[&[&str]] = &[
    &["TEXT", "SEARCH", "CONFIGURATION"],
    &["TEXT", "SEARCH", "DICTIONARY"],
    &["TEXT", "SEARCH", "PARSER"],
    &["TEXT", "SEARCH", "TEMPLATE"],
    &["FOREIGN", "DATA", "WRAPPER"],
    &["MATERIALIZED", "VIEW"],
    &["FOREIGN", "TABLE"],
    &["EVENT", "TRIGGER"],
    &["ACCESS", "METHOD"],
    &["LARGE", "OBJECT"],
    &["OPERATOR", "CLASS"],
    &["OPERATOR", "FAMILY"],
    &["PROCEDURAL", "LANGUAGE"],
];

/// One `COMMENT ON` statement, read.
#[derive(Debug, PartialEq, Eq)]
struct CommentStatement {
    /// The kind in upper case, words separated by one space.
    kind: String,
    /// `IF EXISTS` was written after the kind.
    if_exists: bool,
    /// The object's name as written (for `CONSTRAINT`: the constraint's).
    name: String,
    /// `ON table` of a `CONSTRAINT`, `TRIGGER`, `POLICY` or `RULE` comment.
    on_table: Option<String>,
    /// The text after `IS`, as written (`NULL` or a string literal).
    value: String,
}

/// Read `sql` as `COMMENT ON kind [IF EXISTS] name [ON table] IS value`. `None`
/// when it is another statement.
fn read_comment(sql: &str) -> Option<Result<CommentStatement>> {
    let trimmed = sql.trim().trim_end_matches(';').trim_end();
    let parts = pieces(trimmed);
    let text = |index: usize| {
        parts
            .get(index)
            .and_then(|piece| trimmed.get(piece.start..piece.end))
            .unwrap_or_default()
    };
    let is = |index: usize, word: &str| {
        parts.get(index).is_some_and(|piece| piece.word && piece.depth == 0) && text(index).eq_ignore_ascii_case(word)
    };
    if !(is(0, "COMMENT") && is(1, "ON")) {
        return None;
    }
    let syntax = || Error::sql_parse("COMMENT ON needs an object kind, a name and IS '…' or IS NULL");
    let mut at = 2usize;
    let kind_words = LONG_KINDS
        .iter()
        .find(|words| words.iter().enumerate().all(|(offset, word)| is(at + offset, word)))
        .map_or(1, |words| words.len());
    if !parts.get(at).is_some_and(|piece| piece.word) {
        return Some(Err(syntax()));
    }
    let kind = (at..at + kind_words)
        .map(|index| text(index).to_ascii_uppercase())
        .collect::<Vec<_>>()
        .join(" ");
    at += kind_words;
    let if_exists = is(at, "IF") && is(at + 1, "EXISTS");
    if if_exists {
        at += 2;
    }
    // The last `IS` outside quotes and parentheses separates the name and the text.
    let Some(is_at) = (at..parts.len()).rev().find(|index| is(*index, "IS")) else {
        return Some(Err(syntax()));
    };
    let span = |from: usize, to: usize| -> String {
        match (parts.get(from), to.checked_sub(1).and_then(|last| parts.get(last))) {
            (Some(first), Some(last)) if from < to => {
                trimmed.get(first.start..last.end).unwrap_or_default().to_string()
            }
            _ => String::new(),
        }
    };
    let on_at = (at + 1..is_at).find(|index| is(*index, "ON"));
    let (name, on_table) = match on_at {
        Some(on_at) => (span(at, on_at), Some(span(on_at + 1, is_at))),
        None => (span(at, is_at), None),
    };
    let value = span(is_at + 1, parts.len());
    if name.is_empty() || value.is_empty() || on_table.as_ref().is_some_and(String::is_empty) {
        return Some(Err(syntax()));
    }
    Some(Ok(CommentStatement {
        kind,
        if_exists,
        name,
        on_table,
        value,
    }))
}

/// The catalog kind of a statement's kind words.
fn stored_kind(kind: &str) -> String {
    match kind {
        "ROUTINE" | "AGGREGATE" => "function".to_string(),
        "DOMAIN" => "type".to_string(),
        other => other.to_ascii_lowercase().replace(' ', "_"),
    }
}

impl EmbeddedDatabase {
    /// `COMMENT ON …`: `Ok(None)` when `sql` is another statement.
    pub(crate) fn comment_statement(&self, sql: &str) -> Result<Option<u64>> {
        let statement = match read_comment(sql) {
            None => return Ok(None),
            Some(statement) => statement?,
        };
        let text = self.comment_text(&statement.value)?;
        let catalog = self.storage.catalog();
        let planner = sql::Planner::with_catalog(&catalog)
            .with_current_schema(self.current_schema())
            .with_search_path(self.current_search_path());
        let last_ident = |name: &str| -> Result<String> {
            Self::utility_object_name(name)?
                .0
                .last()
                .map(sql::Planner::normalize_ident)
                .ok_or_else(|| Error::sql_parse(format!("invalid name \"{name}\"")))
        };
        let kind = stored_kind(&statement.kind);
        let name = match statement.kind.as_str() {
            "TABLE" => {
                let table = planner.resolve_table_ref(&Self::utility_object_name(&statement.name)?);
                if let Err(missing) = catalog.get_table_schema(&table) {
                    if statement.if_exists {
                        return Ok(Some(0));
                    }
                    return Err(missing);
                }
                table
            }
            "COLUMN" => {
                let mut path = Self::utility_object_name(&statement.name)?.0;
                let column = path
                    .pop()
                    .filter(|_| !path.is_empty())
                    .ok_or_else(|| Error::query_execution("COMMENT ON COLUMN needs table.column"))?;
                let table = planner.resolve_table_ref(&sqlparser::ast::ObjectName(path));
                let schema = match catalog.get_table_schema(&table) {
                    Ok(schema) => schema,
                    Err(_) if statement.if_exists => return Ok(Some(0)),
                    Err(missing) => return Err(missing),
                };
                let wanted = sql::Planner::normalize_ident(&column);
                if !schema.columns.iter().any(|known| known.name == wanted) {
                    if statement.if_exists {
                        return Ok(Some(0));
                    }
                    return Err(sql::scope::undefined_column(Some(&table), &wanted));
                }
                format!("{table}:{wanted}")
            }
            "VIEW" | "MATERIALIZED VIEW" | "FOREIGN TABLE" => {
                planner.resolve_table_ref(&Self::utility_object_name(&statement.name)?)
            }
            "FUNCTION" | "PROCEDURE" | "ROUTINE" | "AGGREGATE" => {
                // `f(integer, text)`: the name; argument types tell nothing apart here.
                let bare = statement.name.split('(').next().unwrap_or(&statement.name).trim();
                last_ident(bare)?
            }
            "CONSTRAINT" | "TRIGGER" | "POLICY" | "RULE" => {
                let constraint = last_ident(&statement.name)?;
                match &statement.on_table {
                    Some(table) => {
                        let table = table.strip_prefix("DOMAIN ").unwrap_or(table);
                        format!(
                            "{}:{}",
                            planner.resolve_table_ref(&Self::utility_object_name(table)?),
                            constraint
                        )
                    }
                    None => constraint,
                }
            }
            // An index, a schema, a sequence, a type, an extension, a role, …: its
            // name, folded as an identifier is; else as written.
            _ => last_ident(&statement.name).unwrap_or_else(|_| statement.name.trim().to_string()),
        };
        catalog.set_comment(&kind, &name, text.as_deref())?;
        // A cached answer of `pg_description` or `obj_description()` is stale now.
        self.invalidate_plan_cache();
        Ok(Some(0))
    }

    /// The text of `IS value`: `None` for `NULL`. An empty string removes the
    /// comment too, as in PostgreSQL.
    fn comment_text(&self, value: &str) -> Result<Option<String>> {
        if value.trim().eq_ignore_ascii_case("NULL") {
            return Ok(None);
        }
        let not_text = || Error::sql_parse("COMMENT ON … IS needs a string literal or NULL");
        let parsed = sql::Parser::new().parse_one(&format!("SELECT {value}"))?;
        let sqlparser::ast::Statement::Query(query) = parsed else {
            return Err(not_text());
        };
        let sqlparser::ast::SetExpr::Select(select) = *query.body else {
            return Err(not_text());
        };
        let [sqlparser::ast::SelectItem::UnnamedExpr(expr)] = select.projection.as_slice() else {
            return Err(not_text());
        };
        let catalog = self.storage.catalog();
        let literal = sql::Planner::with_catalog(&catalog).expr_to_logical(expr)?;
        match literal {
            sql::LogicalExpr::Literal(Value::Null) => Ok(None),
            sql::LogicalExpr::Literal(Value::String(text)) if text.is_empty() => Ok(None),
            sql::LogicalExpr::Literal(Value::String(text)) => Ok(Some(text)),
            _ => Err(not_text()),
        }
    }
}

/// One stored comment as the statement that makes it again, for a dump.
fn comment_sql(comment: &StoredComment) -> String {
    use crate::storage::dump::sql_text::{quote_ident, quote_literal, quote_table_name};
    let kind = comment.kind.to_ascii_uppercase().replace('_', " ");
    let target = match (comment.kind.as_str(), comment.name.split_once(':')) {
        ("column", Some((table, column))) => format!("{}.{}", quote_table_name(table), quote_ident(column)),
        ("constraint" | "trigger" | "policy" | "rule", Some((table, name))) => {
            format!("{} ON {}", quote_ident(name), quote_table_name(table))
        }
        ("table" | "view" | "materialized_view" | "foreign_table", _) => quote_table_name(&comment.name),
        _ => quote_ident(&comment.name),
    };
    format!("COMMENT ON {kind} {target} IS {};", quote_literal(&comment.text))
}

/// Every stored comment as a `COMMENT ON` statement, tables before their
/// columns, for a dump; each with the table it belongs to, if it belongs to one.
pub(crate) fn comment_dump_entries(
    storage: &crate::storage::StorageEngine,
) -> Result<Vec<crate::storage::dump::CommentDump>> {
    let mut comments = storage.catalog().list_comments()?;
    let rank = |kind: &str| match kind {
        "schema" => 0,
        "table" => 1,
        "column" => 2,
        "constraint" => 3,
        "index" => 4,
        "view" => 5,
        _ => 6,
    };
    comments.sort_by(|a, b| {
        rank(&a.kind)
            .cmp(&rank(&b.kind))
            .then(a.kind.cmp(&b.kind))
            .then(a.name.cmp(&b.name))
    });
    Ok(comments
        .iter()
        .map(|comment| crate::storage::dump::CommentDump {
            table: match comment.kind.as_str() {
                "table" => Some(comment.name.clone()),
                "column" | "constraint" => comment.name.split_once(':').map(|(table, _)| table.to_string()),
                _ => None,
            },
            statement: comment_sql(comment),
        })
        .collect())
}

/// `obj_description(oid [, catalog])`, `col_description(table oid, column number)`
/// and `shobj_description(oid, catalog)`: the stored comment, or NULL.
pub(crate) fn description_function(function: &str, args: &[Value]) -> Result<Value> {
    use crate::sql::phase3::system_views::comment_views::{
        descriptions, PG_CLASS_OID, PG_CONSTRAINT_CLASS_OID, PG_NAMESPACE_OID,
    };
    let as_oid = |value: Option<&Value>| match value {
        Some(Value::Int2(v)) => Some(i32::from(*v)),
        Some(Value::Int4(v)) => Some(*v),
        Some(Value::Int8(v)) => i32::try_from(*v).ok(),
        Some(Value::String(text)) => text.trim().parse::<i32>().ok(),
        _ => None,
    };
    let Some(oid) = as_oid(args.first()) else {
        return Ok(Value::Null);
    };
    let Some(storage) = crate::sql::sequences::persist_handle() else {
        return Ok(Value::Null);
    };
    let rows = descriptions(&storage)?;
    let found = match function {
        "col_description" => {
            let Some(column) = as_oid(args.get(1)) else {
                return Ok(Value::Null);
            };
            rows.into_iter()
                .find(|row| row.objoid == oid && row.classoid == PG_CLASS_OID && row.objsubid == column)
        }
        _ => {
            let class = match args.get(1) {
                Some(Value::String(catalog)) => match catalog.trim().trim_start_matches("pg_catalog.") {
                    "pg_class" => Some(PG_CLASS_OID),
                    "pg_namespace" => Some(PG_NAMESPACE_OID),
                    "pg_constraint" => Some(PG_CONSTRAINT_CLASS_OID),
                    _ => return Ok(Value::Null),
                },
                _ => None,
            };
            rows.into_iter()
                .find(|row| row.objoid == oid && row.objsubid == 0 && class.is_none_or(|class| row.classoid == class))
        }
    };
    Ok(found.map_or(Value::Null, |row| Value::String(row.description)))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn read(sql: &str) -> CommentStatement {
        read_comment(sql).unwrap().unwrap()
    }

    #[test]
    fn the_statement_is_read_by_its_words() {
        let table = read("COMMENT ON TABLE app.orders IS 'placed orders';");
        assert_eq!(
            (table.kind.as_str(), table.name.as_str(), table.value.as_str()),
            ("TABLE", "app.orders", "'placed orders'")
        );
        let column = read("comment on column orders.\"Is Paid\" is 'what IS it'");
        assert_eq!(
            (column.kind.as_str(), column.name.as_str(), column.value.as_str()),
            ("COLUMN", "orders.\"Is Paid\"", "'what IS it'")
        );
        let constraint = read("COMMENT ON CONSTRAINT orders_pkey ON orders IS NULL");
        assert_eq!(constraint.name, "orders_pkey");
        assert_eq!(constraint.on_table.as_deref(), Some("orders"));
        assert_eq!(constraint.value, "NULL");
        let view = read("COMMENT ON MATERIALIZED VIEW IF EXISTS totals IS 'x'");
        assert_eq!((view.kind.as_str(), view.if_exists), ("MATERIALIZED VIEW", true));
        let function = read("COMMENT ON FUNCTION f(integer) IS 'f'");
        assert_eq!(function.name, "f(integer)");
        assert!(read_comment("COMMENTON TABLE t IS 'x'").is_none());
        assert!(read_comment("SELECT 'COMMENT ON TABLE t IS x'").is_none());
        assert!(read_comment("COMMENT ON TABLE t").unwrap().is_err());
        assert!(read_comment("COMMENT").is_none());
    }

    #[test]
    fn a_comment_is_written_back_as_its_statement() {
        let comment = |kind: &str, name: &str, text: &str| StoredComment {
            kind: kind.to_string(),
            name: name.to_string(),
            text: text.to_string(),
        };
        assert_eq!(
            comment_sql(&comment("table", "orders", "it's ours")),
            "COMMENT ON TABLE \"orders\" IS 'it''s ours';"
        );
        assert_eq!(
            comment_sql(&comment("column", "orders:Total", "sum")),
            "COMMENT ON COLUMN \"orders\".\"Total\" IS 'sum';"
        );
        assert_eq!(
            comment_sql(&comment("constraint", "orders:orders_pkey", "key")),
            "COMMENT ON CONSTRAINT \"orders_pkey\" ON \"orders\" IS 'key';"
        );
        assert_eq!(
            comment_sql(&comment("materialized_view", "totals", "t")),
            "COMMENT ON MATERIALIZED VIEW \"totals\" IS 't';"
        );
    }
}
