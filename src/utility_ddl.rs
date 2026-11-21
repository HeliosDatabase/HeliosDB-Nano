//! `REINDEX`, `CREATE DOMAIN` / `DROP DOMAIN` and `CREATE TABLESPACE`.
//!
//! sqlparser 0.53 has no grammar for any of them, so each is recognised by its
//! leading words before the parser runs. All three used to be accepted and do
//! nothing.
//!
//! * `REINDEX { INDEX | TABLE | SCHEMA | DATABASE | SYSTEM } name` builds the
//!   named index, or every index of the named table (of every table, for the
//!   last three), again from the table's rows, in a tree of its own, and puts it
//!   in the place of the old one in one step: a reader sees the old entries or
//!   the new ones. The table is locked against writers meanwhile. A PRIMARY KEY
//!   or UNIQUE index whose rows repeat a key is not replaced, and the statement
//!   fails (23505) naming the constraint. A vector index is not rebuilt here
//!   (`REINDEX INDEX` of one is refused, 0A000; `REINDEX TABLE` leaves it as it
//!   is).
//! * `CREATE DOMAIN` is refused (0A000). A column of a domain type would have to
//!   carry the domain's NOT NULL, DEFAULT and CHECK rules on every write path;
//!   accepting the statement without them, as before, left the rules a script
//!   declared enforced by nothing. `DROP DOMAIN IF EXISTS` succeeds (no domain
//!   can exist); `DROP DOMAIN name` reports that the type does not exist.
//! * `CREATE TABLESPACE` is refused (0A000): a database is one data directory.

use crate::error::UNSUPPORTED_STATEMENT_KIND_MARKER;
use crate::{sql, EmbeddedDatabase, Error, Result};

/// The words of `sql` after `prefix` (ASCII case-insensitive, whole words), with
/// the trailing semicolon gone.
fn after_words<'s>(sql: &'s str, prefix: &[&str]) -> Option<&'s str> {
    let mut rest = sql.trim().trim_end_matches(';').trim_end();
    for word in prefix {
        let head = rest.get(..word.len())?;
        if !head.eq_ignore_ascii_case(word) {
            return None;
        }
        let tail = rest.get(word.len()..)?;
        if tail.chars().next().is_some_and(|c| c.is_alphanumeric() || c == '_') {
            return None;
        }
        rest = tail.trim_start();
    }
    Some(rest)
}

/// What a `REINDEX` statement names.
#[derive(Debug, PartialEq, Eq)]
enum ReindexTarget {
    Index(String),
    Table(String),
    Schema(String),
    Everything,
    SystemCatalogs,
}

/// `REINDEX [ ( option [, …] ) ] { INDEX | TABLE | SCHEMA | DATABASE | SYSTEM }
/// [ CONCURRENTLY ] [ name ]`.
fn parse_reindex(sql: &str) -> Result<ReindexTarget> {
    let syntax = || Error::sql_parse("REINDEX needs INDEX, TABLE, SCHEMA, DATABASE or SYSTEM and a name");
    let mut rest = after_words(sql, &["REINDEX"]).ok_or_else(syntax)?;
    if rest.starts_with('(') {
        let close = rest.find(')').ok_or_else(syntax)?;
        rest = rest.get(close + 1..).unwrap_or_default().trim_start();
    }
    let (kind, mut name) = match rest.split_once(char::is_whitespace) {
        Some((kind, name)) => (kind, name.trim()),
        None => (rest, ""),
    };
    if let Some(after) = after_words(name, &["CONCURRENTLY"]) {
        name = after;
    }
    let named = || {
        if name.is_empty() || (name.contains(char::is_whitespace) && !name.contains('"')) {
            Err(syntax())
        } else {
            Ok(name.to_string())
        }
    };
    match kind.to_ascii_uppercase().as_str() {
        "INDEX" => Ok(ReindexTarget::Index(named()?)),
        "TABLE" => Ok(ReindexTarget::Table(named()?)),
        "SCHEMA" => Ok(ReindexTarget::Schema(named()?)),
        "DATABASE" => Ok(ReindexTarget::Everything),
        "SYSTEM" => Ok(ReindexTarget::SystemCatalogs),
        _ => Err(syntax()),
    }
}

impl EmbeddedDatabase {
    /// `name` as written in a statement, as the parser reads an object name.
    pub(crate) fn utility_object_name(name: &str) -> Result<sqlparser::ast::ObjectName> {
        match sql::Parser::new().parse_one(&format!("DROP TABLE {name}"))? {
            sqlparser::ast::Statement::Drop { mut names, .. } if names.len() == 1 => names
                .pop()
                .ok_or_else(|| Error::sql_parse(format!("invalid name \"{name}\""))),
            _ => Err(Error::sql_parse(format!("invalid name \"{name}\""))),
        }
    }

    /// `REINDEX …`: `Ok(None)` when `sql` is another statement.
    pub(crate) fn reindex_statement(&self, sql: &str) -> Result<Option<u64>> {
        if !sql::Parser::is_reindex_statement(sql) {
            return Ok(None);
        }
        let target = parse_reindex(sql)?;
        if self.storage.is_branch_active() {
            return Err(Error::query_execution("REINDEX must run on the main branch"));
        }
        let catalog = self.storage.catalog();
        let planner = sql::Planner::with_catalog(&catalog)
            .with_current_schema(self.current_schema())
            .with_search_path(self.current_search_path());
        let art = self.storage.art_indexes();
        match target {
            ReindexTarget::Index(name) => {
                let object = Self::utility_object_name(&name)?;
                let index = object
                    .0
                    .last()
                    .map(sql::Planner::normalize_ident)
                    .ok_or_else(|| Error::sql_parse("REINDEX INDEX needs an index name"))?;
                let rebuilt = match art.index_kind_and_table(&index) {
                    Some((_, table)) => self.reindex_table(&table, Some(&index))?,
                    None => {
                        return match catalog.get_index_definition(&index)? {
                            Some(definition) => Err(Error::query_execution(format!(
                                "REINDEX INDEX of the {} index \"{}\" {}: drop the index and create it again",
                                definition.index_type.as_deref().unwrap_or("non-tree"),
                                index,
                                UNSUPPORTED_STATEMENT_KIND_MARKER
                            ))),
                            None => Err(Error::query_execution(format!("index \"{index}\" does not exist"))),
                        }
                    }
                };
                Ok(Some(rebuilt))
            }
            ReindexTarget::Table(name) => {
                let table = planner.resolve_table_ref(&Self::utility_object_name(&name)?);
                catalog.get_table_schema(&table)?;
                self.reindex_table(&table, None).map(Some)
            }
            ReindexTarget::Schema(name) => {
                let object = Self::utility_object_name(&name)?;
                let schema = object
                    .0
                    .last()
                    .map(sql::Planner::normalize_ident)
                    .ok_or_else(|| Error::sql_parse("REINDEX SCHEMA needs a schema name"))?;
                let prefix = format!("{schema}.");
                let mut rebuilt = 0u64;
                for table in catalog.list_tables()? {
                    let in_schema = table.starts_with(&prefix) || (schema == "public" && !table.contains('.'));
                    if in_schema {
                        rebuilt += self.reindex_table(&table, None)?;
                    }
                }
                Ok(Some(rebuilt))
            }
            ReindexTarget::Everything => {
                let mut rebuilt = 0u64;
                for table in catalog.list_tables()? {
                    rebuilt += self.reindex_table(&table, None)?;
                }
                Ok(Some(rebuilt))
            }
            // The system catalogs are computed when read: they have no index.
            ReindexTarget::SystemCatalogs => Ok(Some(0)),
        }
    }

    /// Rebuild `only`, or every tree index of `table`, from the table's rows (see
    /// the module docs). Returns the indexes rebuilt.
    fn reindex_table(&self, table: &str, only: Option<&str>) -> Result<u64> {
        let scope = self.storage.table_locks().statement_scope();
        scope.lock_exclusive(self.storage.statement_lock_level(), table, "REINDEX")?;
        // A transaction's staged rows are in the old trees and not in the table's
        // committed rows: a rebuild from the rows would lose their entries.
        if self.storage.has_uncommitted_writes_for_table(table) {
            return Err(Error::ddl_over_staged_rows("REINDEX", table));
        }
        let catalog = self.storage.catalog();
        let schema = catalog.get_table_schema(table)?;
        let rows = self.storage.scan_table_with_schema(table, &schema)?;
        let art = self.storage.art_indexes();
        let mut trees = Vec::new();
        for (name, _, _) in art.list_table_indexes(table) {
            if only.is_some_and(|only| only != name) {
                continue;
            }
            let tree = art.build_index_tree(&name, &schema, &rows).map_err(|e| {
                Error::constraint_violation(format!(
                    "could not reindex \"{name}\" of relation \"{table}\": {e}; the index was left as it was"
                ))
            })?;
            trees.push((name, tree));
        }
        let rebuilt = trees.len() as u64;
        for (name, tree) in trees {
            art.install_index_tree(&name, tree)
                .map_err(|e| Error::query_execution(format!("could not reindex \"{name}\": {e}")))?;
        }
        tracing::info!(
            "REINDEX: {} index(es) of '{}' rebuilt from {} row(s)",
            rebuilt,
            table,
            rows.len()
        );
        Ok(rebuilt)
    }

    /// `CREATE DOMAIN` / `DROP DOMAIN`: `Ok(None)` when `sql` is another statement.
    pub(crate) fn domain_statement(sql: &str) -> Result<Option<u64>> {
        if !sql::Parser::is_domain_ddl_statement(sql) {
            return Ok(None);
        }
        if let Some(rest) = after_words(sql, &["DROP", "DOMAIN"]) {
            if after_words(rest, &["IF", "EXISTS"]).is_some() {
                return Ok(Some(0));
            }
            let name = rest.split_whitespace().next().unwrap_or_default();
            return Err(Error::query_execution(format!(
                "type \"{}\" does not exist",
                name.trim_matches('"')
            )));
        }
        Err(Error::query_execution(format!(
            "CREATE DOMAIN {UNSUPPORTED_STATEMENT_KIND_MARKER}: a column of a domain type would not have the \
             domain's NOT NULL, DEFAULT and CHECK rules enforced. Declare the base type on the column, with the \
             constraints"
        )))
    }

    /// `CREATE TABLESPACE`: `Ok(None)` when `sql` is another statement.
    pub(crate) fn tablespace_statement(sql: &str) -> Result<Option<u64>> {
        if !sql::Parser::is_create_tablespace_statement(sql) {
            return Ok(None);
        }
        Err(Error::query_execution(format!(
            "CREATE TABLESPACE {UNSUPPORTED_STATEMENT_KIND_MARKER}: a database is one data directory and has no \
             tablespaces"
        )))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::Value;

    #[test]
    fn the_reindex_grammar() {
        assert_eq!(
            parse_reindex("REINDEX INDEX t_v_idx").unwrap(),
            ReindexTarget::Index("t_v_idx".into())
        );
        assert_eq!(
            parse_reindex("reindex (verbose) table concurrently app.t;").unwrap(),
            ReindexTarget::Table("app.t".into())
        );
        assert_eq!(
            parse_reindex("REINDEX SCHEMA public").unwrap(),
            ReindexTarget::Schema("public".into())
        );
        assert_eq!(
            parse_reindex("REINDEX DATABASE postgres").unwrap(),
            ReindexTarget::Everything
        );
        assert_eq!(parse_reindex("REINDEX SYSTEM").unwrap(), ReindexTarget::SystemCatalogs);
        assert!(parse_reindex("REINDEX").is_err());
        assert!(parse_reindex("REINDEX TABLE").is_err());
        assert!(parse_reindex("REINDEX SOMETHING t").is_err());
    }

    /// The proof that the statement rebuilds: the index is emptied behind the
    /// engine's back, a lookup through it then finds nothing, and `REINDEX` brings
    /// every row back.
    #[test]
    fn reindex_brings_back_the_entries_an_index_lost() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE t (id INT PRIMARY KEY, v TEXT)").unwrap();
        db.execute("CREATE INDEX t_v_idx ON t (v)").unwrap();
        db.execute("CREATE UNIQUE INDEX t_v_uq ON t (v)").unwrap();
        for i in 0..50 {
            db.execute(&format!("INSERT INTO t VALUES ({i}, 'v{i}')")).unwrap();
        }
        let art = db.storage.art_indexes();
        let entries = |name: &str| art.index_entry_count(name).unwrap();
        let pk = art
            .list_table_indexes("t")
            .into_iter()
            .find(|(_, kind, _)| *kind == crate::storage::ArtIndexType::PrimaryKey)
            .map(|(name, _, _)| name)
            .unwrap();
        assert_eq!(entries("t_v_idx"), 50);
        assert_eq!(entries(&pk), 50);

        art.clear_table_indexes("t");
        assert_eq!(entries("t_v_idx"), 0);
        assert_eq!(entries(&pk), 0);
        // With its unique index empty, the engine would accept a duplicate.
        assert!(!art.unique_key_exists("t", &["v".to_string()], &[Value::String("v7".into())]));

        // One index.
        db.execute("REINDEX INDEX t_v_idx").unwrap();
        assert_eq!(entries("t_v_idx"), 50);
        assert_eq!(entries(&pk), 0, "REINDEX INDEX rebuilds the named index only");
        // The table.
        db.execute_params("REINDEX TABLE t", &[]).unwrap();
        assert_eq!(entries(&pk), 50);
        assert_eq!(entries("t_v_uq"), 50);
        assert!(art.unique_key_exists("t", &["v".to_string()], &[Value::String("v7".into())]));
        let duplicate = db.execute("INSERT INTO t VALUES (100, 'v7')").unwrap_err();
        assert!(
            duplicate.to_string().to_lowercase().contains("unique") || duplicate.to_string().contains("uplicate"),
            "{duplicate}"
        );
        assert_eq!(db.query("SELECT id FROM t WHERE id = 7", &[]).unwrap().len(), 1);
        assert_eq!(db.query("SELECT id FROM t WHERE v = 'v7'", &[]).unwrap().len(), 1);
    }
}
