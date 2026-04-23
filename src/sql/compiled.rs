//! Compiled query-plan cache.
//!
//! RAG-native (idea 4 of the integration plan).
//!
//! Caches the *output of parsing* (a `sqlparser::ast::Statement`) so
//! subsequent executions of the same SQL skip the lexer+parser pass.
//! This is orthogonal to the result cache in
//! [`crate::sql::query_cache`] -- compiled plans always re-execute,
//! but they save the parser cost on every call.
//!
//! ## Surface
//!
//! Two entry points:
//!
//! 1. `CompiledPlanCache::compile(name, sql)` -- parses once, stashes
//!    the AST under `name`.
//! 2. `CompiledPlanCache::execute(name)` -- returns the cached AST so
//!    the executor can plan + run it.
//!
//! Plans are interned in an `lru::LruCache` so the unbounded growth
//! that prepared-statement caches typically suffer never bites.
//!
//! ## Interaction with parameters
//!
//! For now the cached plan stores the parsed statement *with*
//! placeholder text intact (`$1`, `?`, ...). The executor is
//! responsible for substituting parameters at execute time -- exactly
//! the same path it takes for `EXECUTE` of a prepared statement.

use std::num::NonZeroUsize;
use std::sync::Arc;

use lru::LruCache;
use parking_lot::Mutex;
use sqlparser::ast::Statement;

use crate::sql::Parser;
use crate::{Error, Result};

/// A single cached, parser-output plan.
#[derive(Debug, Clone)]
pub struct CompiledPlan {
    /// User-supplied name for the plan.
    pub name: String,
    /// Original SQL text -- kept for diagnostics / EXPLAIN output.
    pub sql: String,
    /// Parsed AST, ready for the planner / executor.
    pub statement: Arc<Statement>,
    /// Number of times this plan has been executed.
    pub execution_count: u64,
}

/// LRU-bounded cache of compiled plans keyed by name.
pub struct CompiledPlanCache {
    inner: Mutex<LruCache<String, CompiledPlan>>,
    parser: Parser,
}

impl CompiledPlanCache {
    /// Default capacity (number of distinct plans the cache holds).
    pub const DEFAULT_CAPACITY: usize = 256;

    /// Create a new cache with the default capacity.
    #[must_use]
    pub fn new() -> Self {
        Self::with_capacity(Self::DEFAULT_CAPACITY)
    }

    /// Create a new cache with the given capacity (in plans).
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        let cap = NonZeroUsize::new(capacity.max(1)).unwrap_or(
            // Safety: 1 is non-zero.
            #[allow(clippy::unwrap_used)]
            NonZeroUsize::new(1).unwrap(),
        );
        Self {
            inner: Mutex::new(LruCache::new(cap)),
            parser: Parser::new(),
        }
    }

    /// Parse `sql` and store the AST under `name`. Returns the new plan.
    pub fn compile(&self, name: impl Into<String>, sql: impl Into<String>) -> Result<CompiledPlan> {
        let name = name.into();
        let sql = sql.into();
        let statement = self.parser.parse_one(&sql)?;
        let plan = CompiledPlan {
            name: name.clone(),
            sql,
            statement: Arc::new(statement),
            execution_count: 0,
        };
        self.inner.lock().put(name, plan.clone());
        Ok(plan)
    }

    /// Look up a compiled plan by name and bump its execution counter.
    ///
    /// Returns `None` if the plan was never compiled or has been evicted
    /// from the LRU.
    pub fn execute(&self, name: &str) -> Option<CompiledPlan> {
        let mut guard = self.inner.lock();
        let plan = guard.get_mut(name)?;
        plan.execution_count = plan.execution_count.saturating_add(1);
        Some(plan.clone())
    }

    /// Look up without bumping the execution counter (for inspection).
    pub fn peek(&self, name: &str) -> Option<CompiledPlan> {
        self.inner.lock().peek(name).cloned()
    }

    /// Drop the named plan from the cache. Returns `true` if it existed.
    pub fn drop_plan(&self, name: &str) -> bool {
        self.inner.lock().pop(name).is_some()
    }

    /// Number of plans currently held.
    pub fn len(&self) -> usize {
        self.inner.lock().len()
    }

    /// `true` if the cache holds no plans.
    pub fn is_empty(&self) -> bool {
        self.inner.lock().is_empty()
    }

    /// Clear every plan.
    pub fn clear(&self) {
        self.inner.lock().clear();
    }

    /// Snapshot of `(name, execution_count, sql)` for every cached plan,
    /// sorted by execution count descending (i.e. hottest first).
    pub fn snapshot(&self) -> Vec<(String, u64, String)> {
        let guard = self.inner.lock();
        let mut out: Vec<_> = guard
            .iter()
            .map(|(name, plan)| (name.clone(), plan.execution_count, plan.sql.clone()))
            .collect();
        out.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        out
    }
}

impl Default for CompiledPlanCache {
    fn default() -> Self {
        Self::new()
    }
}

// -- SQL surface helpers --------------------------------------------------

/// Recognise `PREPARE COMPILED <name> AS <sql>` (case-insensitive on the
/// keywords) and return `(name, inner_sql)` if the input matches.
///
/// This is a lightweight pre-parser: when it returns `Some`, the caller
/// should compile the inner SQL via [`CompiledPlanCache::compile`]; when
/// it returns `None`, the input is just plain SQL.
#[must_use]
pub fn parse_prepare_compiled(sql: &str) -> Option<(String, String)> {
    let trimmed = sql.trim().trim_end_matches(';').trim();
    let upper: String = trimmed.chars().take(16).collect::<String>().to_ascii_uppercase();
    if !upper.starts_with("PREPARE COMPILED") {
        return None;
    }
    let rest = trimmed[16..].trim_start();
    // <name> AS <inner>
    let as_idx = rest.to_ascii_uppercase().find(" AS ")?;
    let name = rest[..as_idx].trim().to_string();
    let inner = rest[as_idx + 4..].trim().to_string();
    if name.is_empty() || inner.is_empty() {
        return None;
    }
    Some((name, inner))
}

/// Recognise `EXECUTE <name>` (parameters parsed elsewhere) and return
/// the plan name. Returns `None` for non-matching input.
#[must_use]
pub fn parse_execute(sql: &str) -> Option<String> {
    let trimmed = sql.trim().trim_end_matches(';').trim();
    let mut iter = trimmed.split_ascii_whitespace();
    let kw = iter.next()?;
    if !kw.eq_ignore_ascii_case("EXECUTE") {
        return None;
    }
    let name = iter.next()?;
    // Strip trailing `(...)` if present so `EXECUTE foo(1,2)` parses.
    let name = name.split('(').next().unwrap_or(name).trim().to_string();
    if name.is_empty() {
        return None;
    }
    Some(name)
}

/// Convenience facade combining [`parse_prepare_compiled`],
/// [`parse_execute`], and a [`CompiledPlanCache`].
///
/// Returns:
/// - `Ok(Some(plan))` on a successful PREPARE/EXECUTE match.
/// - `Ok(None)`        when the input is plain SQL the caller should run.
/// - `Err`             on parse failures.
pub fn try_handle_compiled(cache: &CompiledPlanCache, sql: &str) -> Result<Option<CompiledPlan>> {
    if let Some((name, inner)) = parse_prepare_compiled(sql) {
        let plan = cache.compile(name, inner)?;
        return Ok(Some(plan));
    }
    if let Some(name) = parse_execute(sql) {
        let plan = cache
            .execute(&name)
            .ok_or_else(|| Error::sql_parse(format!("compiled plan '{name}' not found")))?;
        return Ok(Some(plan));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_prepare_compiled_matches_canonical_form() {
        let (name, sql) = parse_prepare_compiled("PREPARE COMPILED top_users AS SELECT * FROM users").expect("matches");
        assert_eq!(name, "top_users");
        assert_eq!(sql, "SELECT * FROM users");
    }

    #[test]
    fn parse_prepare_compiled_case_insensitive() {
        let (name, sql) = parse_prepare_compiled("prepare compiled q1 As select 1").expect("matches");
        assert_eq!(name, "q1");
        assert_eq!(sql, "select 1");
    }

    #[test]
    fn parse_prepare_compiled_rejects_plain_sql() {
        assert!(parse_prepare_compiled("SELECT 1").is_none());
        assert!(parse_prepare_compiled("PREPARE foo AS SELECT 1").is_none());
        assert!(parse_prepare_compiled("PREPARE COMPILED   AS SELECT 1").is_none());
        assert!(parse_prepare_compiled("PREPARE COMPILED foo AS").is_none());
    }

    #[test]
    fn parse_execute_strips_parens() {
        assert_eq!(parse_execute("EXECUTE q1").as_deref(), Some("q1"));
        assert_eq!(parse_execute("execute q1(1, 2)").as_deref(), Some("q1"));
        assert_eq!(parse_execute("EXECUTE  q1 ;").as_deref(), Some("q1"));
        assert!(parse_execute("EXECUTE").is_none());
        assert!(parse_execute("SELECT 1").is_none());
    }

    #[test]
    fn cache_compile_and_execute_roundtrip() {
        let cache = CompiledPlanCache::with_capacity(8);
        let plan = cache.compile("hot1", "SELECT 1").expect("compile");
        assert_eq!(plan.execution_count, 0);
        assert_eq!(cache.len(), 1);

        let p1 = cache.execute("hot1").expect("execute");
        let p2 = cache.execute("hot1").expect("execute");
        assert_eq!(p1.execution_count, 1);
        assert_eq!(p2.execution_count, 2);
    }

    #[test]
    fn cache_unknown_plan_returns_none() {
        let cache = CompiledPlanCache::new();
        assert!(cache.execute("nope").is_none());
    }

    #[test]
    fn cache_lru_eviction_kicks_in() {
        let cache = CompiledPlanCache::with_capacity(2);
        let _ = cache.compile("a", "SELECT 1").unwrap();
        let _ = cache.compile("b", "SELECT 2").unwrap();
        let _ = cache.compile("c", "SELECT 3").unwrap();
        assert_eq!(cache.len(), 2);
        // 'a' should have been evicted (LRU).
        assert!(cache.peek("a").is_none());
        assert!(cache.peek("b").is_some());
        assert!(cache.peek("c").is_some());
    }

    #[test]
    fn cache_drop_and_clear() {
        let cache = CompiledPlanCache::new();
        cache.compile("x", "SELECT 1").unwrap();
        cache.compile("y", "SELECT 2").unwrap();
        assert!(cache.drop_plan("x"));
        assert!(!cache.drop_plan("x"));
        assert_eq!(cache.len(), 1);
        cache.clear();
        assert!(cache.is_empty());
    }

    #[test]
    fn snapshot_orders_by_execution_count() {
        let cache = CompiledPlanCache::new();
        cache.compile("a", "SELECT 1").unwrap();
        cache.compile("b", "SELECT 2").unwrap();
        for _ in 0..5 {
            cache.execute("b").unwrap();
        }
        cache.execute("a").unwrap();
        let snap = cache.snapshot();
        assert_eq!(snap[0].0, "b");
        assert_eq!(snap[0].1, 5);
        assert_eq!(snap[1].0, "a");
        assert_eq!(snap[1].1, 1);
    }

    #[test]
    fn try_handle_compiled_compiles_then_executes() {
        let cache = CompiledPlanCache::new();
        let p = try_handle_compiled(&cache, "PREPARE COMPILED q AS SELECT 1")
            .expect("ok")
            .expect("matched");
        assert_eq!(p.name, "q");
        let p2 = try_handle_compiled(&cache, "EXECUTE q").expect("ok").expect("matched");
        assert_eq!(p2.execution_count, 1);
        // Plain SQL passes through.
        assert!(try_handle_compiled(&cache, "SELECT 42").unwrap().is_none());
    }

    #[test]
    fn try_handle_compiled_errors_on_unknown_execute() {
        let cache = CompiledPlanCache::new();
        let r = try_handle_compiled(&cache, "EXECUTE missing");
        assert!(r.is_err());
    }
}
