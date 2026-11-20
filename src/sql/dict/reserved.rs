//! Names the system dictionary keeps for itself, and tables the engine owns.
//!
//! Three rules live here, each with one emitter and one marker text that the
//! PostgreSQL wire maps to a SQLSTATE:
//!
//! * A table or view cannot be created with, or renamed to, a name that starts
//!   with `hdb_` or that is the name of a system view (42939). The planner
//!   resolves a system view before a table, so such a table would be created
//!   and then never read.
//! * A table with such a name that already exists (made by an earlier release)
//!   keeps being read: [`names_dictionary_view`] lets the table win, and
//!   [`warn_about_shadowing_tables`] says so once, at open.
//! * An engine-owned `_hdb_code_*` / `_hdb_graph_*` table cannot be dropped,
//!   renamed or altered by a statement while the feature that owns it is
//!   compiled into the binary (42501). "In use" is decided at compile time on
//!   purpose: a binary with the feature creates, fills and migrates these
//!   tables behind the user's back at any moment, so there is no later point
//!   at which a user's DDL on them is safe; a binary without the feature never
//!   touches them, so there they are ordinary tables the user may remove. The
//!   engine's own statements run inside [`engine_ddl`].

use std::cell::Cell;

use crate::storage::internal_relations::is_internal_relation;
use crate::storage::{Catalog, StorageEngine};
use crate::{Error, Result};

/// The prefix of every canonical dictionary view.
pub const DICTIONARY_PREFIX: &str = "hdb_";

/// Marker of the reserved-name refusal (SQLSTATE 42939).
pub const RESERVED_NAME_MARKER: &str = "is reserved for the system dictionary";

/// Marker of the engine-owned-table refusal (SQLSTATE 42501).
pub const ENGINE_OWNED_MARKER: &str = "is owned by the engine";

/// The last part of a storage key (`schema.table`, bare under `public`).
fn bare_name(key: &str) -> &str {
    key.rsplit_once('.').map_or(key, |(_, bare)| bare)
}

/// Whether `key` (a resolved storage key) is a name the dictionary keeps: it
/// starts with `hdb_`, or it is a system view or an alias of one, in any
/// schema.
pub fn is_reserved_relation_name(key: &str) -> bool {
    let bare = bare_name(key);
    bare.starts_with(DICTIONARY_PREFIX) || super::is_dictionary_name(key) || super::is_dictionary_name(bare)
}

/// Refuse to create or rename a relation to a reserved name.
pub fn refuse_reserved_name(key: &str) -> Result<()> {
    if !is_reserved_relation_name(key) {
        return Ok(());
    }
    Err(Error::query_execution(format!(
        "the name \"{key}\" {RESERVED_NAME_MARKER}: a name that starts with hdb_, or that names a system view, \
         cannot be given to a table or a view"
    )))
}

thread_local! {
    /// Nesting depth of [`engine_ddl`] on this thread.
    static ENGINE_DDL_DEPTH: Cell<u32> = const { Cell::new(0) };
}

struct EngineDdlGuard;

impl EngineDdlGuard {
    fn enter() -> Self {
        ENGINE_DDL_DEPTH.with(|depth| depth.set(depth.get().saturating_add(1)));
        Self
    }
}

impl Drop for EngineDdlGuard {
    fn drop(&mut self) {
        ENGINE_DDL_DEPTH.with(|depth| depth.set(depth.get().saturating_sub(1)));
    }
}

/// Run the engine's own DDL on a table it owns. Statements planned on this
/// thread inside `body` are not subject to the engine-owned-table refusal.
#[doc(hidden)]
pub fn engine_ddl<R>(body: impl FnOnce() -> R) -> R {
    let _guard = EngineDdlGuard::enter();
    body()
}

fn inside_engine_ddl() -> bool {
    ENGINE_DDL_DEPTH.with(|depth| depth.get() > 0)
}

/// The refusal of `statement` on the table `key`, when `key` is an engine
/// table whose owning feature is in use. `owner_in_use` is passed in so the
/// rule can be tested in a binary compiled either way.
pub fn engine_table_refusal(key: &str, statement: &str, owner_in_use: bool) -> Option<Error> {
    let kind = is_internal_relation(key)?;
    let feature = kind.owning_feature()?;
    if !owner_in_use {
        return None;
    }
    Some(Error::query_execution(format!(
        "{statement} refused: \"{key}\" {ENGINE_OWNED_MARKER} (feature {feature}) and cannot be dropped, renamed \
         or altered by a statement"
    )))
}

/// Refuse a user's `statement` (`DROP TABLE`, `ALTER TABLE`) on an engine
/// table whose owning feature is compiled into this binary.
pub fn refuse_engine_table_ddl(key: &str, statement: &str) -> Result<()> {
    if inside_engine_ddl() {
        return Ok(());
    }
    let owner_in_use = is_internal_relation(key).is_some_and(|kind| kind.owner_is_compiled_in());
    match engine_table_refusal(key, statement, owner_in_use) {
        Some(refusal) => Err(refusal),
        None => Ok(()),
    }
}

/// Whether `name` is read as a dictionary view (a registered system view or an
/// alias of one). It is, unless a user table of that exact name exists: a
/// database made by an earlier release keeps reading its own table.
///
/// The table lookup happens only for a name the registry knows, so a query
/// over ordinary tables pays nothing for it.
pub fn names_dictionary_view(name: &str, catalog: Option<&Catalog<'_>>) -> bool {
    if !super::is_dictionary_name(name) {
        return false;
    }
    match catalog {
        Some(catalog) => !catalog.table_exists(name).unwrap_or(false),
        None => true,
    }
}

/// [`names_dictionary_view`] for a caller that holds the storage engine.
pub fn names_dictionary_view_in(name: &str, storage: Option<&StorageEngine>) -> bool {
    match storage {
        Some(storage) => names_dictionary_view(name, Some(&storage.catalog())),
        None => names_dictionary_view(name, None),
    }
}

/// The user tables of `storage` whose name is also a dictionary view's.
pub fn shadowing_tables(storage: &StorageEngine) -> Result<Vec<String>> {
    Ok(storage
        .catalog()
        .list_tables()?
        .into_iter()
        .filter(|table| super::is_dictionary_name(table))
        .collect())
}

/// Log one WARNING per user table that hides a dictionary view. Called at open.
pub(crate) fn warn_about_shadowing_tables(storage: &StorageEngine) {
    match shadowing_tables(storage) {
        Ok(tables) => {
            for table in tables {
                tracing::warn!(
                    "table \"{table}\" has the name of a system view: statements read the table, and the \
                     system view of that name is hidden until the table is renamed or dropped"
                );
            }
        }
        Err(error) => tracing::warn!("could not check for tables named after system views: {error}"),
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
    use crate::{Column, DataType, EmbeddedDatabase, Schema};

    #[test]
    fn reserved_names() {
        for name in [
            "hdb_x",
            "hdb_",
            "app.hdb_x",
            "pg_class",
            "app.pg_class",
            "information_schema.tables",
            "sqlite_master",
            "hdb_dictionary",
            "heliosdb_features",
        ] {
            assert!(is_reserved_relation_name(name), "{name}");
            let message = refuse_reserved_name(name).expect_err(name).to_string();
            assert!(message.contains(RESERVED_NAME_MARKER), "{message}");
        }
        for name in [
            "hdbx",
            "my_hdb_t",
            "_hdb_code_files",
            "pg_class_backup",
            "tables",
            "app.tables",
            "HDB_x",
        ] {
            assert!(!is_reserved_relation_name(name), "{name}");
            refuse_reserved_name(name).unwrap();
        }
    }

    #[test]
    fn engine_tables_are_refused_only_while_their_feature_is_in_use() {
        for table in ["_hdb_code_symbols", "_hdb_graph_nodes"] {
            let message = engine_table_refusal(table, "DROP TABLE", true)
                .expect("refused")
                .to_string();
            assert!(
                message.contains(ENGINE_OWNED_MARKER) && message.contains(table),
                "{message}"
            );
            assert!(engine_table_refusal(table, "DROP TABLE", false).is_none());
        }
        // No feature owns these.
        for table in ["_hdb_other", "__mv_x", "orders", "hdb_code_symbols"] {
            assert!(engine_table_refusal(table, "ALTER TABLE", true).is_none(), "{table}");
        }
        // The engine's own statements pass.
        let inside = engine_ddl(|| refuse_engine_table_ddl("_hdb_code_symbols", "ALTER TABLE"));
        inside.unwrap();
        assert_eq!(
            refuse_engine_table_ddl("_hdb_code_symbols", "ALTER TABLE").is_err(),
            cfg!(feature = "code-graph")
        );
    }

    #[test]
    fn a_user_table_with_a_view_name_is_found_and_wins() {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        let catalog = db.storage.catalog();
        assert!(names_dictionary_view("pg_inherits", Some(&catalog)));
        assert!(names_dictionary_view("pg_inherits", None));
        assert!(!names_dictionary_view("orders", Some(&catalog)));
        assert!(shadowing_tables(&db.storage).unwrap().is_empty());

        catalog
            .create_table("pg_inherits", Schema::new(vec![Column::new("id", DataType::Int4)]))
            .unwrap();
        assert_eq!(shadowing_tables(&db.storage).unwrap(), vec!["pg_inherits".to_string()]);
        assert!(!names_dictionary_view("pg_inherits", Some(&catalog)));
        assert!(!names_dictionary_view_in("pg_inherits", Some(&db.storage)));
        assert!(names_dictionary_view_in("pg_class", Some(&db.storage)));
    }
}
