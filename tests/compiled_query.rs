//! Compiled query plan cache integration tests.
//!
//! RAG-native (idea 4).

use heliosdb_nano::sql::compiled::{parse_execute, parse_prepare_compiled, try_handle_compiled, CompiledPlanCache};

#[test]
fn prepare_then_execute_skips_reparsing() {
    let cache = CompiledPlanCache::new();
    let p = cache.compile("hot1", "SELECT 1 AS one").expect("compile");
    assert_eq!(p.execution_count, 0);
    let p1 = cache.execute("hot1").expect("execute");
    let p2 = cache.execute("hot1").expect("execute");
    assert_eq!(p1.execution_count, 1);
    assert_eq!(p2.execution_count, 2);
    // Same statement Arc -> proves we're returning the cached AST.
    assert!(std::sync::Arc::ptr_eq(&p1.statement, &p2.statement));
}

#[test]
fn parse_helpers_round_trip() {
    let (n, s) = parse_prepare_compiled("PREPARE COMPILED q AS SELECT 1").expect("matched prepare");
    assert_eq!(n, "q");
    assert_eq!(s, "SELECT 1");

    assert_eq!(parse_execute("EXECUTE q1(1)").as_deref(), Some("q1"));
    assert!(parse_execute("SELECT 1").is_none());
}

#[test]
fn try_handle_compiled_dispatches() {
    let cache = CompiledPlanCache::new();
    assert!(try_handle_compiled(&cache, "PREPARE COMPILED a AS SELECT 1")
        .unwrap()
        .is_some());
    assert!(try_handle_compiled(&cache, "EXECUTE a").unwrap().is_some());
    assert!(try_handle_compiled(&cache, "SELECT 1").unwrap().is_none());
    assert!(try_handle_compiled(&cache, "EXECUTE missing").is_err());
}

#[test]
fn lru_evicts_oldest_when_full() {
    let cache = CompiledPlanCache::with_capacity(2);
    cache.compile("a", "SELECT 1").unwrap();
    cache.compile("b", "SELECT 2").unwrap();
    cache.compile("c", "SELECT 3").unwrap();
    assert!(cache.peek("a").is_none(), "oldest entry evicted");
    assert!(cache.peek("b").is_some());
    assert!(cache.peek("c").is_some());
}

#[test]
fn snapshot_orders_by_executions() {
    let cache = CompiledPlanCache::new();
    cache.compile("cold", "SELECT 1").unwrap();
    cache.compile("warm", "SELECT 2").unwrap();
    for _ in 0..3 {
        cache.execute("warm").unwrap();
    }
    let snap = cache.snapshot();
    assert_eq!(snap[0].0, "warm");
    assert_eq!(snap[0].1, 3);
}
