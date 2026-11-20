//! The foundation of the system dictionary.
//!
//! The dictionary is the set of system views a statement can read: canonical
//! views named `hdb_*`, and the PostgreSQL, `information_schema`, SQLite,
//! MySQL and earlier-release names that answer over them. This module holds
//! what every such view shares:
//!
//! * [`context`]: what a producer may read ([`DictContext`]).
//! * [`oid`]: object identifiers derived from names, the same in every view.
//! * [`reserved`]: the names the dictionary keeps for itself and the tables
//!   the engine owns.
//! * [`alias`]: other names for a canonical view ([`DictAlias`]).
//! * [`dictionary`]: `hdb_dictionary`, the listing of all of the above.
//! * [`routines`], [`tables`]: the producers of `pg_proc` and `pg_tables`.
//!
//! The registry of views itself is `sql::phase3::system_views`.

pub mod context;
pub mod oid;
pub mod reserved;
pub mod routines;
pub mod tables;

pub use context::DictContext;

/// Whether `name` is a name of the dictionary: a registered system view or an
/// alias of one.
pub fn is_dictionary_name(name: &str) -> bool {
    crate::sql::phase3::SystemViewRegistry::shared().is_system_view(name)
}
