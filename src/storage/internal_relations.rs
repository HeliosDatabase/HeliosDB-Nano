//! The one place that knows which catalogued tables belong to the engine.
//!
//! A table in the catalog is the user's unless its name carries one of the
//! prefixes below. Every listing asks [`is_internal_relation`] and then decides
//! for itself whether that kind is hidden, shown under another name, or shown
//! as it is; no listing tests a prefix on its own.

/// Prefix of the table that stores the rows of a materialized view.
pub const MATERIALIZED_VIEW_DATA_PREFIX: &str = "__mv_";
/// Prefix of every engine-owned table.
pub const ENGINE_TABLE_PREFIX: &str = "_hdb_";
/// Prefix of the code-graph tables (`_hdb_code.<table>` in dotted form).
pub const CODE_GRAPH_PREFIX: &str = "_hdb_code_";
/// Prefix of the knowledge-graph tables (`_hdb_graph.<table>` in dotted form).
pub const GRAPH_RAG_PREFIX: &str = "_hdb_graph_";
/// Prefix earlier releases reserved for bookkeeping tables.
pub const LEGACY_BOOKKEEPING_PREFIX: &str = "helios_";

/// Why a catalogued table is not an ordinary user table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InternalKind {
    /// `__mv_<view>`: the rows of the materialized view `<view>`.
    MaterializedViewData,
    /// `_hdb_code_*`: owned by the code-graph feature.
    CodeGraph,
    /// `_hdb_graph_*`: owned by the knowledge-graph feature.
    GraphRag,
    /// Any other `_hdb_*` table.
    EngineTable,
    /// A table in a session's private `pg_temp_<n>` schema.
    SessionTemp,
    /// `helios_*`: the bookkeeping prefix of earlier releases.
    LegacyBookkeeping,
}

impl InternalKind {
    /// The Cargo feature that owns tables of this kind, if one does.
    pub fn owning_feature(self) -> Option<&'static str> {
        match self {
            Self::CodeGraph => Some("code-graph"),
            Self::GraphRag => Some("graph-rag"),
            _ => None,
        }
    }

    /// Whether the feature that owns tables of this kind is compiled into
    /// this binary. `false` for a kind no feature owns.
    pub fn owner_is_compiled_in(self) -> bool {
        match self {
            Self::CodeGraph => cfg!(feature = "code-graph"),
            Self::GraphRag => cfg!(feature = "graph-rag"),
            _ => false,
        }
    }

    /// Storage that backs another object (a materialized view) or bookkeeping
    /// no user created: never listed as a table of the user's.
    pub fn is_hidden_storage(self) -> bool {
        matches!(self, Self::MaterializedViewData | Self::LegacyBookkeeping)
    }

    /// An `_hdb_*` table of any owner.
    pub fn is_engine_table(self) -> bool {
        matches!(self, Self::CodeGraph | Self::GraphRag | Self::EngineTable)
    }
}

/// The table part of a storage key (`schema.table`, or the bare name for a
/// table of `public`).
fn bare_name(key: &str) -> &str {
    key.rsplit_once('.').map_or(key, |(_, bare)| bare)
}

/// Classify a catalogued table by its storage key. `None` is a user table.
///
/// A user table whose name merely resembles a prefix is a user table:
/// `mv_orders` is not the data table of a materialized view (`__mv_orders`
/// is), and `hdb_notes` is not an engine table (`_hdb_notes` is).
pub fn is_internal_relation(name: &str) -> Option<InternalKind> {
    if crate::sql::temp_tables::is_temp_key(name) {
        return Some(InternalKind::SessionTemp);
    }
    // A materialized view in a schema keeps its data under `__mv_<schema>.<view>`,
    // so the whole key is tested as well as its last part.
    let bare = bare_name(name);
    if name.starts_with(MATERIALIZED_VIEW_DATA_PREFIX) || bare.starts_with(MATERIALIZED_VIEW_DATA_PREFIX) {
        return Some(InternalKind::MaterializedViewData);
    }
    if bare.starts_with(CODE_GRAPH_PREFIX) {
        return Some(InternalKind::CodeGraph);
    }
    if bare.starts_with(GRAPH_RAG_PREFIX) {
        return Some(InternalKind::GraphRag);
    }
    if bare.starts_with(ENGINE_TABLE_PREFIX) {
        return Some(InternalKind::EngineTable);
    }
    if bare.starts_with(LEGACY_BOOKKEEPING_PREFIX) {
        return Some(InternalKind::LegacyBookkeeping);
    }
    None
}

/// Whether `name` is storage the user did not create as a table: the rows of a
/// materialized view, or bookkeeping of an earlier release.
pub fn is_hidden_storage_table(name: &str) -> bool {
    is_internal_relation(name).is_some_and(InternalKind::is_hidden_storage)
}

/// The materialized view whose rows `name` stores, when `name` is such a table.
pub fn materialized_view_of(name: &str) -> Option<&str> {
    name.strip_prefix(MATERIALIZED_VIEW_DATA_PREFIX)
}

/// The schema an engine table is reported under and its name there:
/// `_hdb_code_symbols` is `symbols` in `_hdb_code`. `None` for any other name.
pub fn engine_schema_and_name(name: &str) -> Option<(&'static str, &str)> {
    if let Some(rest) = name.strip_prefix(CODE_GRAPH_PREFIX) {
        return Some(("_hdb_code", rest));
    }
    if let Some(rest) = name.strip_prefix(GRAPH_RAG_PREFIX) {
        return Some(("_hdb_graph", rest));
    }
    None
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
    fn user_tables_that_resemble_a_prefix_are_user_tables() {
        for name in [
            "mv_x",
            "mv_",
            "_mv_x",
            "hdb_notes",
            "orders",
            "app.mv_x",
            "my__mv_x",
            "x_hdb_code_y",
            "heliosdb",
        ] {
            assert_eq!(is_internal_relation(name), None, "{name}");
        }
    }

    #[test]
    fn engine_owned_names_are_classified() {
        assert_eq!(is_internal_relation("__mv_x"), Some(InternalKind::MaterializedViewData));
        assert_eq!(
            is_internal_relation("__mv_app.x"),
            Some(InternalKind::MaterializedViewData)
        );
        assert_eq!(
            is_internal_relation("app.__mv_x"),
            Some(InternalKind::MaterializedViewData)
        );
        assert_eq!(is_internal_relation("_hdb_code_symbols"), Some(InternalKind::CodeGraph));
        assert_eq!(is_internal_relation("_hdb_graph_nodes"), Some(InternalKind::GraphRag));
        assert_eq!(is_internal_relation("_hdb_other"), Some(InternalKind::EngineTable));
        assert_eq!(
            is_internal_relation("helios_sessions"),
            Some(InternalKind::LegacyBookkeeping)
        );
        assert_eq!(is_internal_relation("pg_temp_7.t"), Some(InternalKind::SessionTemp));
        assert_eq!(
            is_internal_relation("pg_temp_7.__mv_x"),
            Some(InternalKind::SessionTemp)
        );
    }

    #[test]
    fn kinds_answer_what_listings_ask() {
        assert!(InternalKind::MaterializedViewData.is_hidden_storage());
        assert!(InternalKind::LegacyBookkeeping.is_hidden_storage());
        assert!(!InternalKind::CodeGraph.is_hidden_storage());
        assert!(InternalKind::CodeGraph.is_engine_table() && InternalKind::EngineTable.is_engine_table());
        assert_eq!(InternalKind::GraphRag.owning_feature(), Some("graph-rag"));
        assert_eq!(InternalKind::EngineTable.owning_feature(), None);
        assert!(!InternalKind::EngineTable.owner_is_compiled_in());
        assert_eq!(
            InternalKind::CodeGraph.owner_is_compiled_in(),
            cfg!(feature = "code-graph")
        );
        assert_eq!(materialized_view_of("__mv_x"), Some("x"));
        assert_eq!(materialized_view_of("mv_x"), None);
        assert_eq!(
            engine_schema_and_name("_hdb_code_symbols"),
            Some(("_hdb_code", "symbols"))
        );
        assert_eq!(
            engine_schema_and_name("_hdb_graph_nodes"),
            Some(("_hdb_graph", "nodes"))
        );
        assert_eq!(engine_schema_and_name("_hdb_other"), None);
    }
}
