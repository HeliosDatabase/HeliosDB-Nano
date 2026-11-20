//! Aliases: other names for a canonical dictionary view.
//!
//! An alias is not a second implementation. It is a name, the canonical view
//! it reads, and for each of its columns the expression over the canonical
//! view's columns that produces it. The planner expands an alias into that
//! projection over a scan of the canonical view, exactly as it expands a
//! stored view, so an alias filters, joins, aggregates and describes like any
//! other relation and can never disagree with its canonical view.

use std::collections::HashMap;
use std::sync::LazyLock;

use sqlparser::ast::{ObjectName, TableAlias};

use crate::sql::logical_plan::LogicalPlan;
use crate::sql::planner::Planner;
use crate::storage::Catalog;
use crate::{Column, Result, Schema};

/// One alias of a canonical dictionary view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DictAlias {
    /// The alias, as the planner resolves it (`pg_x`, `information_schema.x`).
    pub alias: &'static str,
    /// The canonical view the alias reads.
    pub canonical: &'static str,
    /// The alias's columns in order: its column name, and the SQL expression
    /// over the canonical view's columns that yields it.
    pub columns: &'static [(&'static str, &'static str)],
}

/// Every alias. The first entry proves the mechanism; the full map of
/// PostgreSQL, SQLite, MySQL and legacy names is added on top of it.
pub static ALIASES: &[DictAlias] = &[DictAlias {
    alias: "heliosdb_features",
    canonical: "hdb_features",
    columns: &[("name", "name"), ("compiled", "compiled"), ("source", "source")],
}];

static BY_NAME: LazyLock<HashMap<&'static str, &'static DictAlias>> =
    LazyLock::new(|| ALIASES.iter().map(|alias| (alias.alias, alias)).collect());

/// The alias called `name`, if `name` is one.
pub fn alias_for(name: &str) -> Option<&'static DictAlias> {
    BY_NAME.get(name).copied()
}

/// A double-quoted SQL identifier.
fn quoted(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

/// A relation name, each dotted part quoted.
fn quoted_relation(name: &str) -> String {
    name.split('.').map(quoted).collect::<Vec<_>>().join(".")
}

impl DictAlias {
    /// The query the alias stands for.
    pub fn select_sql(&self) -> String {
        let columns: Vec<String> = self
            .columns
            .iter()
            .map(|(column, expression)| format!("{expression} AS {}", quoted(column)))
            .collect();
        format!("SELECT {} FROM {}", columns.join(", "), quoted_relation(self.canonical))
    }

    /// The alias's columns, typed, when every one of them is a plain column of
    /// the canonical view. `None` when a column is a computed expression: its
    /// type is then only known to the planner.
    pub fn schema(&self) -> Option<Schema> {
        let canonical = crate::sql::phase3::SystemViewRegistry::shared().get_schema(self.canonical)?;
        let mut columns = Vec::with_capacity(self.columns.len());
        for (name, expression) in self.columns {
            let source = canonical.columns.iter().find(|column| column.name == *expression)?;
            let mut column: Column = source.clone();
            column.name = (*name).to_string();
            columns.push(column);
        }
        Some(Schema { columns })
    }
}

/// The columns of the dictionary relation `name`: a registered view's, or an
/// alias's when they can be typed without planning.
pub fn dictionary_schema(name: &str) -> Option<Schema> {
    crate::sql::phase3::SystemViewRegistry::shared()
        .get_schema(name)
        .cloned()
        .or_else(|| alias_for(name).and_then(DictAlias::schema))
}

impl Planner<'_> {
    /// Plan a reference to a dictionary alias: the alias's projection over a
    /// scan of its canonical view, known by the name the statement uses for
    /// it. `Ok(None)` when `table_name` is not an alias, or when a user table
    /// of that name exists and therefore wins.
    pub(crate) fn dictionary_alias_to_plan(
        &self,
        table_name: &str,
        written_name: &ObjectName,
        table_alias: Option<&TableAlias>,
        catalog: Option<&Catalog<'_>>,
    ) -> Result<Option<LogicalPlan>> {
        let Some(dict_alias) = alias_for(table_name) else {
            return Ok(None);
        };
        if !super::reserved::names_dictionary_view(table_name, catalog) {
            return Ok(None);
        }
        let statement = crate::sql::Parser::new().parse_one(&dict_alias.select_sql())?;
        let planner = match catalog {
            Some(catalog) => Planner::with_catalog(catalog),
            None => Planner::new(),
        };
        let plan = planner.statement_to_plan(statement)?;
        let known_as = match table_alias {
            Some(alias) => Self::normalize_ident(&alias.name),
            None => written_name
                .0
                .last()
                .map_or_else(|| table_name.to_string(), Self::normalize_ident),
        };
        let stamped = Self::stamp_derived_plan(plan, &known_as, &[])?;
        // The projection names each of its columns: no wildcard is involved.
        self.set_written_select_list_of_factor(true);
        Ok(Some(stamped))
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::assert_is_empty
)]
mod tests {
    use super::*;

    #[test]
    fn every_alias_reads_a_registered_view_and_is_not_one_itself() {
        let registry = crate::sql::phase3::SystemViewRegistry::shared();
        let mut seen = std::collections::HashSet::new();
        for alias in ALIASES {
            assert!(seen.insert(alias.alias), "{} is listed twice", alias.alias);
            assert!(
                registry.is_system_view(alias.canonical),
                "{} reads {}, which is not registered",
                alias.alias,
                alias.canonical
            );
            assert!(
                !registry.is_system_view(alias.alias),
                "{} is both an alias and a view",
                alias.alias
            );
            assert!(
                alias_for(alias.canonical).is_none(),
                "an alias of an alias: {}",
                alias.alias
            );
            assert!(!alias.columns.is_empty());
            // The alias's query parses.
            crate::sql::Parser::new().parse_one(&alias.select_sql()).unwrap();
        }
    }

    #[test]
    fn the_first_alias() {
        let alias = alias_for("heliosdb_features").expect("alias");
        assert_eq!(alias.canonical, "hdb_features");
        assert_eq!(
            alias.select_sql(),
            "SELECT name AS \"name\", compiled AS \"compiled\", source AS \"source\" FROM \"hdb_features\""
        );
        let schema = alias.schema().expect("plain columns are typed");
        let canonical = dictionary_schema("hdb_features").expect("canonical");
        assert_eq!(
            schema
                .columns
                .iter()
                .map(|c| (&c.name, &c.data_type))
                .collect::<Vec<_>>(),
            canonical
                .columns
                .iter()
                .map(|c| (&c.name, &c.data_type))
                .collect::<Vec<_>>()
        );
        assert!(alias_for("hdb_features").is_none());
        assert!(dictionary_schema("heliosdb_features").is_some());
        assert!(dictionary_schema("no_such_view").is_none());
        assert_eq!(
            quoted_relation("information_schema.tables"),
            "\"information_schema\".\"tables\""
        );
    }
}
