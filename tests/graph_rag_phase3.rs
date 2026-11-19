//! Phase-3 smoke tests for the graph-rag MVP. Feature flag
//! `graph-rag` implies `code-graph`.

#![cfg(feature = "graph-rag")]

use heliosdb_nano::{
    code_graph::CodeIndexOptions,
    graph_rag::{Direction, GraphRagOptions},
    EmbeddedDatabase, Result,
};

fn setup() -> Result<EmbeddedDatabase> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE src (path TEXT PRIMARY KEY, lang TEXT, content TEXT)")?;
    Ok(db)
}

fn insert(db: &EmbeddedDatabase, path: &str, lang: &str, content: &str) -> Result<()> {
    let p = path.replace('\'', "''");
    let l = lang.replace('\'', "''");
    let c = content.replace('\'', "''");
    db.execute(&format!(
        "INSERT INTO src (path, lang, content) VALUES ('{p}', '{l}', '{c}')"
    ))?;
    Ok(())
}

#[test]
fn project_and_search_finds_symbol() -> Result<()> {
    let db = setup()?;
    insert(
        &db,
        "repo.py",
        "python",
        "def helper():\n    return 42\n\ndef caller():\n    return helper()\n",
    )?;
    db.code_index(CodeIndexOptions::for_table("src"))?;
    // `code_index` mirrors the symbols it indexed into `_hdb_graph_nodes` itself, so
    // both functions are nodes already. An explicit pass is idempotent: it finds
    // nothing new to project and adds no second node for a symbol.
    let symbol_nodes = || -> Result<usize> {
        Ok(db
            .query(
                "SELECT node_id FROM _hdb_graph_nodes WHERE source_ref LIKE 'code_symbol:%'",
                &[],
            )?
            .len())
    };
    let projected = symbol_nodes()?;
    assert!(projected >= 2, "code_index projected {projected} symbol nodes");
    let stats = db.graph_rag_project_symbols()?;
    assert_eq!(stats.code_symbols_projected, 0, "nothing new to project");
    assert_eq!(symbol_nodes()?, projected, "the explicit pass duplicated a node");

    let hits = db.graph_rag_search(&GraphRagOptions {
        seed_text: "helper".into(),
        seed_kinds: vec!["Function".into()],
        hops: 1,
        direction: Direction::Both,
        ..Default::default()
    })?;
    assert!(!hits.is_empty(), "expected at least one hit for 'helper'");
    assert!(hits.iter().any(|h| h.hop_distance == 0));
    Ok(())
}

#[test]
fn empty_seed_text_errors() -> Result<()> {
    let db = setup()?;
    db.graph_rag_project_symbols()?;
    let err = db.graph_rag_search(&GraphRagOptions::default());
    assert!(err.is_err(), "empty seed should error");
    Ok(())
}

#[test]
fn bfs_respects_hops() -> Result<()> {
    // a -> b -> c (both edges CALLS). Seed on "a", hops=1 should
    // return a and b but not c.
    let db = setup()?;
    insert(
        &db,
        "chain.py",
        "python",
        "def a():\n    return b()\n\ndef b():\n    return c()\n\ndef c():\n    return 1\n",
    )?;
    db.code_index(CodeIndexOptions::for_table("src"))?;
    db.graph_rag_project_symbols()?;

    let hits1 = db.graph_rag_search(&GraphRagOptions {
        seed_text: "a".into(),
        seed_kinds: vec!["Function".into()],
        hops: 1,
        ..Default::default()
    })?;
    let names1: Vec<_> = hits1.iter().filter_map(|h| h.title.clone()).collect();
    assert!(names1.iter().any(|t| t == "a"));
    // BFS of hops=1 from "a" should pick up the CALLS edge to b.
    // c is 2 hops away and must not be included at hops=1.
    assert!(!names1.iter().any(|t| t == "c"));

    let hits2 = db.graph_rag_search(&GraphRagOptions {
        seed_text: "a".into(),
        seed_kinds: vec!["Function".into()],
        hops: 2,
        ..Default::default()
    })?;
    let names2: Vec<_> = hits2.iter().filter_map(|h| h.title.clone()).collect();
    // With hops=2, c should show up via the a→b→c chain.
    assert!(names2.iter().any(|t| t == "c"));
    Ok(())
}

/// v09.3: `ensure_tables` is idempotent and, on main, leaves the read indexes
/// graph expansion needs (`from_node`, `to_node`, `source_ref`).
#[test]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]
fn ensure_tables_is_idempotent_and_indexes_exist_on_main() -> Result<()> {
    use heliosdb_nano::Value;
    let db = setup()?;
    heliosdb_nano::graph_rag::ensure_tables(&db)?;
    heliosdb_nano::graph_rag::ensure_tables(&db)?;
    let names: Vec<String> = db
        .query("SELECT indexname FROM pg_indexes", &[])?
        .iter()
        .filter_map(|r| match r.values.first() {
            Some(Value::String(s)) => Some(s.clone()),
            _ => None,
        })
        .collect();
    for want in [
        "idx_hdb_graph_edges_from_node",
        "idx_hdb_graph_edges_to_node",
        "idx_hdb_graph_nodes_source_ref",
    ] {
        assert_eq!(names.iter().filter(|n| *n == want).count(), 1, "{want} in {names:?}");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// v09.8: the projection is a diff
// ---------------------------------------------------------------------------

#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]
mod v09_8 {
    use super::*;
    use heliosdb_nano::Value;

    fn n(db: &EmbeddedDatabase, sql: &str) -> i64 {
        match &db.query(sql, &[]).unwrap()[0].values[0] {
            Value::Int4(v) => i64::from(*v),
            Value::Int8(v) => *v,
            Value::Null => 0,
            other => panic!("not an integer: {other:?}"),
        }
    }

    fn assert_graph_matches_symbols(db: &EmbeddedDatabase) {
        assert_eq!(
            n(
                db,
                "SELECT COUNT(*) FROM _hdb_graph_nodes WHERE source_ref LIKE 'code_symbol:%'"
            ),
            n(db, "SELECT COUNT(*) FROM _hdb_code_symbols")
        );
        // Every code node belongs to a live symbol, and no edge references a missing node.
        let symbols: std::collections::HashSet<i64> = db
            .query("SELECT node_id FROM _hdb_code_symbols", &[])
            .unwrap()
            .iter()
            .map(|r| n_of(&r.values[0]))
            .collect();
        let mut nodes = std::collections::HashSet::new();
        for r in db
            .query("SELECT node_id, source_ref FROM _hdb_graph_nodes", &[])
            .unwrap()
        {
            nodes.insert(n_of(&r.values[0]));
            if let Value::String(sr) = &r.values[1] {
                if let Some(id) = sr.strip_prefix("code_symbol:") {
                    assert!(
                        symbols.contains(&id.parse::<i64>().unwrap()),
                        "ghost node for symbol {id}"
                    );
                }
            }
        }
        for r in db
            .query("SELECT from_node, to_node FROM _hdb_graph_edges", &[])
            .unwrap()
        {
            assert!(
                nodes.contains(&n_of(&r.values[0])) && nodes.contains(&n_of(&r.values[1])),
                "dangling edge"
            );
        }
    }

    fn n_of(v: &Value) -> i64 {
        match v {
            Value::Int4(x) => i64::from(*x),
            Value::Int8(x) => *x,
            other => panic!("not an integer: {other:?}"),
        }
    }

    fn corpus(db: &EmbeddedDatabase) -> Result<()> {
        insert(
            db,
            "a.py",
            "python",
            "def alpha():\n    return beta()\n\ndef beta():\n    return 1\n",
        )?;
        insert(db, "b.py", "python", "def gamma():\n    return alpha()\n")?;
        db.code_index(CodeIndexOptions::for_table("src"))?;
        Ok(())
    }

    #[test]
    fn reindex_after_edit_leaves_one_node_per_symbol() -> Result<()> {
        let db = setup()?;
        corpus(&db)?;
        assert_graph_matches_symbols(&db);
        db.execute("UPDATE src SET content = 'def alpha():\n    return beta()\n\ndef beta():\n    return 2\n\ndef delta():\n    return 0\n' WHERE path = 'a.py'")?;
        let stats = db.code_index(CodeIndexOptions::for_table("src"))?;
        assert_eq!(stats.projection_error, None);
        assert_graph_matches_symbols(&db);
        assert!(!db
            .graph_rag_search(&GraphRagOptions {
                seed_text: "delta".into(),
                ..Default::default()
            })?
            .is_empty());
        Ok(())
    }

    #[test]
    fn deleted_file_nodes_removed() -> Result<()> {
        let db = setup()?;
        corpus(&db)?;
        db.execute("DELETE FROM src WHERE path = 'a.py'")?;
        db.code_index(CodeIndexOptions::for_table("src"))?;
        assert_graph_matches_symbols(&db);
        let hits = db.graph_rag_search(&GraphRagOptions {
            seed_text: "beta".into(),
            ..Default::default()
        })?;
        assert!(hits.is_empty(), "a deleted symbol is no longer a graph hit: {hits:?}");
        Ok(())
    }

    /// A second index run that changes nothing writes no graph row: the
    /// counters of the serial keys do not move and the stats say so.
    #[test]
    fn noop_reindex_writes_no_graph_rows() -> Result<()> {
        let db = setup()?;
        corpus(&db)?;
        let node_max = n(&db, "SELECT MAX(node_id) FROM _hdb_graph_nodes");
        let edge_max = n(&db, "SELECT MAX(edge_id) FROM _hdb_graph_edges");
        let before = db.query(
            "SELECT node_id, node_kind, title, text FROM _hdb_graph_nodes ORDER BY node_id",
            &[],
        )?;
        let stats = db.code_index(CodeIndexOptions::for_table("src"))?;
        assert_eq!(stats.files_parsed, 0);
        assert_eq!(n(&db, "SELECT MAX(node_id) FROM _hdb_graph_nodes"), node_max);
        assert_eq!(n(&db, "SELECT MAX(edge_id) FROM _hdb_graph_edges"), edge_max);
        let after = db.query(
            "SELECT node_id, node_kind, title, text FROM _hdb_graph_nodes ORDER BY node_id",
            &[],
        )?;
        assert_eq!(before.len(), after.len());
        // The explicit pass finds nothing to write either.
        let explicit = db.graph_rag_project_symbols()?;
        assert_eq!(
            (
                explicit.code_symbols_projected,
                explicit.nodes_updated,
                explicit.nodes_deleted,
                explicit.edges_projected,
                explicit.edges_deleted
            ),
            (0, 0, 0, 0, 0)
        );
        assert_eq!(n(&db, "SELECT MAX(node_id) FROM _hdb_graph_nodes"), node_max);
        Ok(())
    }

    /// A store indexed before graph-rag was enabled has symbols and no nodes:
    /// the next run parses nothing but still projects.
    #[test]
    fn first_projection_on_existing_index_still_runs() -> Result<()> {
        let db = setup()?;
        corpus(&db)?;
        db.execute("DELETE FROM _hdb_graph_edges")?;
        db.execute("DELETE FROM _hdb_graph_nodes")?;
        let stats = db.code_index(CodeIndexOptions::for_table("src"))?;
        assert_eq!(stats.files_parsed, 0);
        assert!(
            n(&db, "SELECT COUNT(*) FROM _hdb_graph_nodes") > 0,
            "graph was repopulated"
        );
        assert_graph_matches_symbols(&db);
        Ok(())
    }

    #[test]
    fn doc_chunk_nodes_untouched() -> Result<()> {
        let db = setup()?;
        corpus(&db)?;
        db.execute("INSERT INTO _hdb_graph_nodes (node_kind, source_ref, title, text) VALUES ('DocChunk', 'doc:1', 'readme', 'about alpha')")?;
        db.execute("INSERT INTO _hdb_graph_nodes (node_kind, source_ref, title, text) VALUES ('Email', 'mail:1', 'subject', 'body')")?;
        let doc = n(&db, "SELECT node_id FROM _hdb_graph_nodes WHERE source_ref = 'doc:1'");
        // A MENTIONS edge from the doc to a symbol that is about to vanish.
        let beta = n(&db, "SELECT node_id FROM _hdb_graph_nodes WHERE title LIKE '%beta'");
        db.execute(&format!(
            "INSERT INTO _hdb_graph_edges (from_node, to_node, edge_kind, weight) VALUES ({doc}, {beta}, 'MENTIONS', 1.0)"
        ))?;
        db.execute("DELETE FROM src WHERE path = 'a.py'")?;
        db.code_index(CodeIndexOptions::for_table("src"))?;
        assert_eq!(
            n(
                &db,
                "SELECT COUNT(*) FROM _hdb_graph_nodes WHERE source_ref IN ('doc:1', 'mail:1')"
            ),
            2
        );
        assert_eq!(
            n(
                &db,
                "SELECT COUNT(*) FROM _hdb_graph_edges WHERE edge_kind = 'MENTIONS'"
            ),
            0
        );
        assert_graph_matches_symbols(&db);
        Ok(())
    }

    #[test]
    fn projection_failure_is_reported() -> Result<()> {
        let db = setup()?;
        // A table of the same name with another shape: IF NOT EXISTS keeps it
        // and the projection's INSERT cannot succeed.
        db.execute("CREATE TABLE _hdb_graph_nodes (node_id BIGSERIAL PRIMARY KEY, node_kind TEXT)")?;
        insert(&db, "a.py", "python", "def alpha():\n    return 1\n")?;
        let stats = db.code_index(CodeIndexOptions::for_table("src"))?;
        assert_eq!(stats.files_parsed, 1, "the index itself succeeded");
        assert!(stats.projection_error.is_some(), "the failure is visible: {stats:?}");
        assert_eq!(n(&db, "SELECT COUNT(*) FROM _hdb_code_symbols"), 1);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// v09.9: one frontier per hop
// ---------------------------------------------------------------------------

#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]
mod v09_9 {
    use super::*;
    use heliosdb_nano::graph_rag::{
        graph_rag_expand_with_context, search_statement_count_this_thread, GraphRagHit, WithContextOptions,
    };
    use std::collections::{BTreeMap, BTreeSet, HashMap};

    fn graph() -> EmbeddedDatabase {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        heliosdb_nano::graph_rag::ensure_tables(&db).unwrap();
        db
    }

    fn node(db: &EmbeddedDatabase, title: &str) -> i64 {
        db.execute_params_returning(
            "INSERT INTO _hdb_graph_nodes (node_kind, source_ref, title, text) VALUES ('Function', $1, $2, '') RETURNING node_id",
            &[heliosdb_nano::Value::String(format!("t:{title}")), heliosdb_nano::Value::String(title.into())],
        )
        .unwrap()
        .1[0]
            .values[0]
            .clone()
            .into_i64()
    }

    trait IntoI64 {
        fn into_i64(self) -> i64;
    }
    impl IntoI64 for heliosdb_nano::Value {
        fn into_i64(self) -> i64 {
            match self {
                heliosdb_nano::Value::Int4(n) => i64::from(n),
                heliosdb_nano::Value::Int8(n) => n,
                other => panic!("not an integer: {other:?}"),
            }
        }
    }

    fn edge(db: &EmbeddedDatabase, from: i64, to: i64, kind: &str) {
        db.execute(&format!(
            "INSERT INTO _hdb_graph_edges (from_node, to_node, edge_kind, weight) VALUES ({from}, {to}, '{kind}', 1.0)"
        ))
        .unwrap();
    }

    fn hits(h: &[GraphRagHit]) -> Vec<(i64, u32)> {
        h.iter().map(|x| (x.node_id, x.hop_distance)).collect()
    }

    /// Reference: the per-node BFS the search used to run, over the raw edges,
    /// with no limit truncation.
    fn reference(
        edges: &[(i64, i64, String)],
        seeds: &[i64],
        hops: u32,
        dir: Direction,
        kinds: &[&str],
    ) -> Vec<(i64, u32)> {
        let mut visited: BTreeMap<i64, u32> = seeds.iter().map(|s| (*s, 0)).collect();
        let mut frontier: Vec<i64> = seeds.to_vec();
        for d in 0..hops {
            let mut next = BTreeSet::new();
            for &n in &frontier {
                for (f, t, k) in edges {
                    if !kinds.is_empty() && !kinds.contains(&k.as_str()) {
                        continue;
                    }
                    let peer = match dir {
                        Direction::Out if *f == n => Some(*t),
                        Direction::In if *t == n => Some(*f),
                        Direction::Both if *f == n => Some(*t),
                        Direction::Both if *t == n => Some(*f),
                        _ => None,
                    };
                    if let Some(p) = peer {
                        if p != n && !visited.contains_key(&p) {
                            next.insert(p);
                        }
                    }
                }
            }
            for p in &next {
                visited.insert(*p, d + 1);
            }
            frontier = next.into_iter().collect();
        }
        let mut v: Vec<(i64, u32)> = visited.into_iter().collect();
        v.sort_by_key(|(id, hop)| (*hop, *id));
        v
    }

    #[test]
    fn frontier_equals_per_node_bfs_without_truncation() {
        let db = graph();
        let mut ids = Vec::new();
        for i in 0..40 {
            ids.push(node(
                &db,
                &if i % 13 == 0 {
                    format!("seed-{i}")
                } else {
                    format!("plain-{i}")
                },
            ));
        }
        let mut edges = Vec::new();
        let mut x: u64 = 0x1234_5678;
        for _ in 0..90 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let f = ids[(x % 40) as usize];
            let t = ids[((x >> 20) % 40) as usize];
            let k = ["CALLS", "REFERENCES", "MENTIONS"][((x >> 40) % 3) as usize];
            edge(&db, f, t, k);
            edges.push((f, t, k.to_string()));
        }
        let seeds: Vec<i64> = [0usize, 13, 26, 39].iter().map(|i| ids[*i]).collect();
        for dir in [Direction::Out, Direction::In, Direction::Both] {
            for kinds in [vec![], vec!["CALLS"], vec!["CALLS", "MENTIONS"]] {
                for hops in 0..=3 {
                    let got = db
                        .graph_rag_search(&GraphRagOptions {
                            seed_text: "seed".into(),
                            hops,
                            direction: dir,
                            edge_kinds: kinds.iter().map(|k| (*k).to_string()).collect(),
                            limit: 1000,
                            ..Default::default()
                        })
                        .unwrap();
                    assert_eq!(
                        hits(&got),
                        reference(&edges, &seeds, hops, dir, &kinds),
                        "dir={dir:?} kinds={kinds:?} hops={hops}"
                    );
                }
            }
        }
        // WITH CONTEXT on the same seeds gives the same set.
        let ctx = graph_rag_expand_with_context(
            &db,
            "SELECT node_id FROM _hdb_graph_nodes WHERE title LIKE 'seed%'",
            &WithContextOptions {
                hops: 2,
                limit: Some(1000),
                direction: Some(Direction::Both),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(hits(&ctx), reference(&edges, &seeds, 2, Direction::Both, &[]));
    }

    #[test]
    fn limit_cuts_lowest_node_id_first() {
        let db = graph();
        let seed = node(&db, "seed-star");
        let peers: Vec<i64> = (0..10).map(|i| node(&db, &format!("leaf-{i}"))).collect();
        // Insert the edges in a scrambled order: the cut must not depend on it.
        for i in [7usize, 2, 9, 0, 5, 3, 8, 1, 6, 4] {
            edge(&db, seed, peers[i], "CALLS");
        }
        let got = db
            .graph_rag_search(&GraphRagOptions {
                seed_text: "seed-star".into(),
                hops: 1,
                limit: 4,
                ..Default::default()
            })
            .unwrap();
        let ids: Vec<i64> = got.iter().map(|h| h.node_id).collect();
        let mut want = vec![seed];
        want.extend(&peers[..3]);
        want.sort_unstable();
        let mut got_sorted = ids.clone();
        got_sorted.sort_unstable();
        assert_eq!(got_sorted, want);
    }

    #[test]
    fn expand_limit_is_per_source_node() {
        let db = graph();
        let a = node(&db, "seed-a");
        let b = node(&db, "seed-b");
        let pa: Vec<i64> = (0..5).map(|i| node(&db, &format!("pa-{i}"))).collect();
        let pb: Vec<i64> = (0..5).map(|i| node(&db, &format!("pb-{i}"))).collect();
        for p in &pa {
            edge(&db, a, *p, "CALLS");
        }
        for p in &pb {
            edge(&db, b, *p, "CALLS");
        }
        let got = graph_rag_expand_with_context(
            &db,
            "SELECT node_id FROM _hdb_graph_nodes WHERE title LIKE 'seed%'",
            &WithContextOptions {
                hops: 1,
                expand_limit: Some(2),
                limit: Some(100),
                ..Default::default()
            },
        )
        .unwrap();
        let got_ids: BTreeSet<i64> = got.iter().map(|h| h.node_id).collect();
        let mut want: BTreeSet<i64> = BTreeSet::from([a, b]);
        want.extend(&pa[..2]);
        want.extend(&pb[..2]);
        assert_eq!(got_ids, want);
    }

    #[test]
    fn hop_distance_is_first_reach() {
        let db = graph();
        let s = node(&db, "seed-d");
        let a = node(&db, "a");
        let b = node(&db, "b");
        edge(&db, s, a, "CALLS");
        edge(&db, a, b, "CALLS");
        edge(&db, s, b, "CALLS"); // b is also one hop from the seed
        let got = db
            .graph_rag_search(&GraphRagOptions {
                seed_text: "seed-d".into(),
                hops: 3,
                ..Default::default()
            })
            .unwrap();
        let by: HashMap<i64, u32> = hits(&got).into_iter().collect();
        assert_eq!((by[&s], by[&a], by[&b]), (0, 1, 1));
    }

    #[test]
    fn seed_with_percent_and_underscore_matches_literally() {
        let db = graph();
        let lit = node(&db, "100%_sure");
        let _other = node(&db, "100x_sure");
        let got = db
            .graph_rag_search(&GraphRagOptions {
                seed_text: "100%_".into(),
                hops: 0,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(hits(&got), vec![(lit, 0)]);
    }

    #[test]
    fn statements_per_search_are_bounded_by_hops() {
        let db = graph();
        let mut prev = node(&db, "seed-chain");
        for i in 0..30 {
            let n = node(&db, &format!("c-{i}"));
            edge(&db, prev, n, "CALLS");
            prev = n;
        }
        for hops in 1..=3u32 {
            let before = search_statement_count_this_thread();
            let got = db
                .graph_rag_search(&GraphRagOptions {
                    seed_text: "seed-chain".into(),
                    hops,
                    ..Default::default()
                })
                .unwrap();
            let used = search_statement_count_this_thread() - before;
            assert_eq!(got.len(), hops as usize + 1);
            assert!(used <= 3 * u64::from(hops) + 1, "{used} statements for {hops} hops");
        }
    }
}
