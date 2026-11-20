//! `hdb_dictionary`: the dictionary of the dictionary.
//!
//! One row per name the dictionary answers to: every registered system view,
//! and every alias with the canonical view it reads.

use crate::sql::phase3::system_views::{SystemViewRegistry, SystemViewSchema};
use crate::{Column, ColumnStorageMode, DataType, Schema, Tuple, Value};

/// The canonical name of the dictionary's own listing.
pub const HDB_DICTIONARY: &str = "hdb_dictionary";
/// The canonical name of the compiled-features view.
pub const HDB_FEATURES: &str = "hdb_features";

/// Which naming family a dictionary name belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DictFamily {
    /// A canonical `hdb_*` view.
    Hdb,
    /// A PostgreSQL catalog name (`pg_*`).
    PostgreSql,
    /// A SQL-standard `information_schema.*` name.
    InformationSchema,
    /// A SQLite name (`sqlite_*`).
    Sqlite,
    /// A MySQL name (`mysql.*`, `performance_schema.*`).
    Mysql,
    /// A name of an earlier release that is none of the above.
    Legacy,
}

impl DictFamily {
    /// The value of the `family` column.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Hdb => "hdb",
            Self::PostgreSql => "postgresql",
            Self::InformationSchema => "information_schema",
            Self::Sqlite => "sqlite",
            Self::Mysql => "mysql",
            Self::Legacy => "legacy",
        }
    }
}

/// `pg_*` names of earlier releases that PostgreSQL does not have.
const LEGACY_PG_NAMES: &[&str] = &[
    "pg_advisory_locks",
    "pg_branch_stats",
    "pg_compare_branches",
    "pg_control_checkpoint",
    "pg_database_branches",
    "pg_mv_staleness",
    "pg_replication_metrics",
    "pg_replication_primary",
    "pg_replication_standbys",
    "pg_replication_status",
    "pg_vector_index_stats",
];

/// The family of a dictionary name.
pub fn family_of(name: &str) -> DictFamily {
    if name.starts_with(super::reserved::DICTIONARY_PREFIX) {
        DictFamily::Hdb
    } else if name.starts_with("information_schema.") {
        DictFamily::InformationSchema
    } else if name.starts_with("mysql.") || name.starts_with("performance_schema.") {
        DictFamily::Mysql
    } else if name.starts_with("sqlite_") {
        DictFamily::Sqlite
    } else if name.starts_with("pg_") && !LEGACY_PG_NAMES.contains(&name) {
        DictFamily::PostgreSql
    } else {
        DictFamily::Legacy
    }
}

fn column(name: &str, data_type: DataType, nullable: bool) -> Column {
    Column {
        name: name.to_string(),
        data_type,
        nullable,
        primary_key: false,
        source_table: None,
        source_table_name: None,
        default_expr: None,
        unique: false,
        storage_mode: ColumnStorageMode::Default,
    }
}

/// Register the canonical views this module owns.
pub(crate) fn register(registry: &mut SystemViewRegistry) {
    registry.register_view(SystemViewSchema {
        name: HDB_DICTIONARY.to_string(),
        schema: Schema {
            columns: vec![
                column("name", DataType::Text, false),
                column("kind", DataType::Text, false),
                column("canonical", DataType::Text, true),
                column("family", DataType::Text, false),
                column("description", DataType::Text, false),
            ],
        },
        description: "Every system view and alias: its kind, the canonical view an alias reads, and its naming family"
            .to_string(),
    });
    registry.register_view(SystemViewSchema {
        name: HDB_FEATURES.to_string(),
        schema: Schema {
            columns: vec![
                column("name", DataType::Text, false),
                column("compiled", DataType::Boolean, false),
                column("source", DataType::Text, false),
            ],
        },
        description: "The Cargo features compiled into this binary, and what each provides".to_string(),
    });
}

/// The rows of `hdb_dictionary`, ordered by name.
pub(crate) fn dictionary_rows(registry: &SystemViewRegistry) -> Vec<Tuple> {
    let mut rows: Vec<(String, &'static str, Option<String>, String)> = registry
        .described_views()
        .into_iter()
        .map(|(name, description)| {
            let description = if description.trim().is_empty() {
                "System view".to_string()
            } else {
                description.to_string()
            };
            (name.to_string(), "view", None, description)
        })
        .collect();
    for alias in super::alias::ALIASES {
        rows.push((
            alias.alias.to_string(),
            "alias",
            Some(alias.canonical.to_string()),
            format!("Alias of {}", alias.canonical),
        ));
    }
    rows.sort_by_key(|row| row.0.clone());
    rows.into_iter()
        .map(|(name, kind, canonical, description)| {
            let family = family_of(&name).as_str();
            Tuple::new(vec![
                Value::String(name),
                Value::String(kind.to_string()),
                canonical.map_or(Value::Null, Value::String),
                Value::String(family.to_string()),
                Value::String(description),
            ])
        })
        .collect()
}

/// The rows of `hdb_features`.
pub(crate) fn features_rows() -> Vec<Tuple> {
    crate::sql::features::features()
        .into_iter()
        .map(|feature| {
            Tuple::new(vec![
                Value::String(feature.name.to_string()),
                Value::Boolean(feature.compiled),
                Value::String(feature.source.to_string()),
            ])
        })
        .collect()
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
    fn families() {
        for (name, family) in [
            ("hdb_dictionary", DictFamily::Hdb),
            ("hdb_code_languages", DictFamily::Hdb),
            ("pg_class", DictFamily::PostgreSql),
            ("pg_stat_activity", DictFamily::PostgreSql),
            ("pg_database_branches", DictFamily::Legacy),
            ("information_schema.tables", DictFamily::InformationSchema),
            ("sqlite_master", DictFamily::Sqlite),
            ("mysql.user", DictFamily::Mysql),
            ("heliosdb_config", DictFamily::Legacy),
        ] {
            assert_eq!(family_of(name), family, "{name}");
        }
    }

    #[test]
    fn the_dictionary_lists_every_view_and_alias_once() {
        let registry = SystemViewRegistry::shared();
        let rows = dictionary_rows(registry);
        let text = |value: &Value| match value {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        };
        let names: Vec<String> = rows.iter().map(|row| text(&row.values[0])).collect();
        let mut sorted = names.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(names, sorted, "ordered by name, no name twice");
        assert_eq!(
            rows.len(),
            registry.list_views().len() + super::super::alias::ALIASES.len()
        );
        for view in registry.list_views() {
            assert!(names.iter().any(|name| name == view), "{view} is missing");
        }
        for row in &rows {
            assert_eq!(row.values.len(), 5);
            assert!(!text(&row.values[4]).is_empty());
        }
    }
}
