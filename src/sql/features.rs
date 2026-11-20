//! v07.6: what this binary really contains — the `hdb_features` view (alias
//! `heliosdb_features`) and
//! the REPL's `\version` print the same table.
//!
//! `compiled` is `cfg!(feature = …)` for the build; `source` says what the
//! capability is. The deprecated HA names (ha-tier2, ha-tier3, ha-dedup,
//! ha-branch-replication) and `simd` select no code and are listed as such,
//! so nobody builds `--features ha-full` expecting multi-primary or sharding.

/// One capability row.
#[derive(Debug, Clone, Copy)]
pub struct Feature {
    /// The Cargo feature name.
    pub name: &'static str,
    /// Compiled into this binary.
    pub compiled: bool,
    /// What it provides (or that it provides nothing).
    pub source: &'static str,
}

/// Every Cargo feature, in name order.
pub fn features() -> Vec<Feature> {
    let f = |name, compiled, source| Feature { name, compiled, source };
    vec![
        f(
            "code-graph",
            cfg!(feature = "code-graph"),
            "AST symbol graph and lsp_* functions",
        ),
        f(
            "compression",
            cfg!(feature = "compression"),
            "MySQL wire-protocol packet compression",
        ),
        f(
            "encryption",
            cfg!(feature = "encryption"),
            "encryption at rest (AES-256-GCM)",
        ),
        f("fips", cfg!(feature = "fips"), "FIPS crypto provider (aws-lc-rs)"),
        f(
            "graph-rag",
            cfg!(feature = "graph-rag"),
            "knowledge-graph RAG on the code graph",
        ),
        f(
            "ha-ab-testing",
            cfg!(feature = "ha-ab-testing"),
            "branch-based A/B experiment routing",
        ),
        f(
            "ha-branch-replication",
            cfg!(feature = "ha-branch-replication"),
            "deprecated: selects nothing beyond ha-tier1",
        ),
        f(
            "ha-dedup",
            cfg!(feature = "ha-dedup"),
            "deprecated: selects nothing beyond ha-tier1",
        ),
        f(
            "ha-tier1",
            cfg!(feature = "ha-tier1"),
            "physical standby replication (primary / standby)",
        ),
        f(
            "ha-tier2",
            cfg!(feature = "ha-tier2"),
            "deprecated: selects nothing beyond ha-tier1 (no multi-primary)",
        ),
        f(
            "ha-tier3",
            cfg!(feature = "ha-tier3"),
            "deprecated: selects nothing beyond ha-tier1 (no sharding)",
        ),
        f(
            "lock-census",
            cfg!(feature = "lock-census"),
            "read-path lock-contention census",
        ),
        f("mcp-endpoint", cfg!(feature = "mcp-endpoint"), "MCP JSON-RPC endpoint"),
        f("simd", cfg!(feature = "simd"), "deprecated: selects nothing"),
        f(
            "sync-experimental",
            cfg!(feature = "sync-experimental"),
            "not buildable (module disabled)",
        ),
        f(
            "vector-persist",
            cfg!(feature = "vector-persist"),
            "persistent vector index backend",
        ),
        f("vector-search", cfg!(feature = "vector-search"), "HNSW vector search"),
    ]
}
