//! Process-scoped bridge that lets the session-less expression evaluator invoke
//! a user-defined function.
//!
//! # Why a process-scoped handle and not a constructor argument
//!
//! `Evaluator` is built at dozens of sites as `Evaluator::new(schema)` with no
//! database handle at all (the procedural runtime builds one from an EMPTY
//! schema, the constant folder builds one with no storage, every physical
//! operator builds one per plan node). Threading an `Arc<FunctionRegistry>` plus
//! a re-entrant SQL executor through all of them would touch every one of those
//! call sites and change a very hot constructor.
//!
//! This module follows the precedent the repo already set for exactly this
//! problem: [`crate::sql::sequences`]'s `install_persistence` / `persist_handle`
//! pair, which is how `nextval`/`setval` reach durable storage from inside the
//! same evaluator. A `Weak` is stored at DB open and upgraded only when the work
//! actually happens, so nothing is added to the per-statement hot path.
//!
//! # Hot-path contract (READ BEFORE MOVING THIS CALL)
//!
//! [`resolve`] is consulted from ONE place: the TERMINAL `_` arm of
//! `Evaluator::evaluate_scalar_function`, i.e. only after every built-in
//! function name has already failed to match. A built-in call never touches this
//! module — no lock, no upgrade, no map lookup. Moving the lookup ahead of the
//! built-in dispatch would put a mutex + `Weak::upgrade` + `RwLock` read on the
//! single hottest expression path in the engine; the perf gate
//! (`benches/public/ci_perf_smoke.sh`) exists to catch that.
//!
//! # Ownership / no Arc cycle
//!
//! [`install`] stores a `Weak<UdfBridge>`. The single strong reference lives in
//! the `EmbeddedDatabase::udf_bridge` field of the database that installed it,
//! and `EmbeddedDatabase::clone_for_trigger` deliberately leaves that field
//! EMPTY on the clones it mints. That asymmetry is load-bearing: the bridge's
//! `exec` closure captures a `clone_for_trigger()` handle, so a clone that
//! carried the bridge would close the loop `Arc<UdfBridge> -> handle ->
//! Arc<UdfBridge>`, the strong count would never reach zero, `StorageEngine`'s
//! `Drop` would never run and the data dir would stay locked. This repo has
//! shipped precisely that bug once (the SIGTERM/Drop incident); do not "tidy"
//! the clone into copying every field.
//!
//! # Several databases in one process
//!
//! `sequences::PERSIST` keeps a single last-open-wins slot, which is fine there
//! because sequence state is durable and keyed by name. It is NOT fine here:
//! `cargo test` opens dozens of `EmbeddedDatabase`s concurrently in one process,
//! and a single slot would let database B's open silently steal resolution from
//! a query still running against database A — a flaky "Unknown scalar function"
//! for a function that plainly exists.
//!
//! So this module keeps a pruned list of installed bridges and resolves BY
//! FUNCTION NAME, newest first: [`resolve`] hands back the most recently
//! installed live bridge whose registry actually contains the name. A database
//! that never defined the function is skipped, so a name unknown everywhere
//! still produces the ordinary "Unknown scalar function" error.
//!
//! Residual, documented: if TWO live databases in one process define the same
//! function name, the more recently opened one wins, and the body then runs
//! against THAT database. Same class of process-global caveat `sequences`
//! carries; resolving per calling handle needs a database reference inside
//! `Evaluator`, which is the invasive change this design exists to avoid.

use std::cell::Cell;
use std::sync::{Arc, OnceLock, Weak};

use parking_lot::Mutex;

use super::functions::FunctionRegistry;
use crate::{Error, Result, Value};

/// Re-entrant SQL executor used to run a routine body.
///
/// Contract (identical to the closure `EmbeddedDatabase::execute_call_plan`
/// hands to `execute_procedure`): a `SELECT`/`WITH` statement returns its rows
/// as `Vec<Vec<Value>>`; anything else is executed for effect and returns an
/// empty row set.
pub type UdfSqlExecutor = Box<dyn Fn(&str) -> Result<Vec<Vec<Value>>> + Send + Sync>;

/// Everything the evaluator needs to run a user-defined function.
pub struct UdfBridge {
    /// The registry the calling database registered its functions into.
    pub registry: Arc<FunctionRegistry>,
    /// Re-entrant executor for the function body.
    pub exec: UdfSqlExecutor,
    /// Maximum nested UDF invocations on one thread. Sourced from
    /// `[session] udf_max_call_depth` (see `crate::config::SessionConfig`), NOT
    /// a hardcoded constant.
    pub max_call_depth: u32,
}

/// Every installed bridge, oldest first. `Weak`, so an entry never keeps a
/// database (and therefore a `StorageEngine`, and therefore a data-dir lock)
/// alive; dead entries are pruned on the next install.
static BRIDGES: OnceLock<Mutex<Vec<Weak<UdfBridge>>>> = OnceLock::new();

fn bridges() -> &'static Mutex<Vec<Weak<UdfBridge>>> {
    BRIDGES.get_or_init(|| Mutex::new(Vec::new()))
}

/// Install a bridge for this process. Called once per `EmbeddedDatabase` open,
/// next to `sequences::install_persistence`. Takes `&Arc` and downgrades, so the
/// caller keeps the only strong reference.
pub fn install(bridge: &Arc<UdfBridge>) {
    let mut guard = bridges().lock();
    guard.retain(|w| w.strong_count() > 0);
    guard.push(Arc::downgrade(bridge));
}

/// The most recently installed live bridge whose registry defines `name`.
///
/// `None` means no open database in this process has such a function — the
/// caller then reports its ordinary "unknown function" error. Called ONLY from
/// the terminal arm of scalar dispatch; see the module header.
pub fn resolve(name: &str) -> Option<Arc<UdfBridge>> {
    let guard = bridges().lock();
    for weak in guard.iter().rev() {
        if let Some(bridge) = weak.upgrade() {
            if bridge.registry.function_exists(name) {
                return Some(bridge);
            }
        }
    }
    None
}

thread_local! {
    /// Nested UDF invocations on THIS thread. A function body re-enters the
    /// planner and evaluator, so a self-recursive body would otherwise recurse
    /// until the thread's stack is exhausted (an abort, not an error).
    static CALL_DEPTH: Cell<u32> = const { Cell::new(0) };
}

/// Restores the depth counter on every exit path, including the `?` early
/// returns inside the interpreter and a panic unwinding through it.
struct DepthGuard;

impl Drop for DepthGuard {
    fn drop(&mut self) {
        CALL_DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
    }
}

/// Current nesting depth. Test/diagnostic helper; the guard keeps it at 0
/// outside a UDF call.
pub fn current_call_depth() -> u32 {
    CALL_DEPTH.with(|d| d.get())
}

/// Invoke user-defined function `name` with already-evaluated `args`.
///
/// This is the production caller `FunctionRegistry::execute_function` never had:
/// the interpreter itself (`LANGUAGE sql` bodies via `execute_sql_function`,
/// `LANGUAGE plpgsql` bodies via `execute_plpgsql_function`) is unchanged and is
/// NOT reimplemented here.
///
/// Errors are always loud: an unknown name, a bad argument count, an
/// unsupported language, a body statement that fails, or the depth limit below
/// all surface to the caller.
pub fn call_scalar(bridge: &UdfBridge, name: &str, args: &[Value]) -> Result<Value> {
    let depth = CALL_DEPTH.with(|d| d.get());
    if depth >= bridge.max_call_depth {
        return Err(Error::query_execution(format!(
            "Function '{}' was NOT executed: UDF call depth limit ({}) exceeded (recursive \
             function?). Raise `[session] udf_max_call_depth` in config.toml if the nesting is \
             legitimate.",
            name, bridge.max_call_depth
        )));
    }
    CALL_DEPTH.with(|d| d.set(depth.saturating_add(1)));
    let _guard = DepthGuard;

    bridge.registry.execute_function(name, args, |sql| (bridge.exec)(sql))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::sql::functions::StoredFunction;
    use crate::sql::logical_plan::{FunctionParam, ParamMode};
    use crate::DataType;

    fn double_fn() -> StoredFunction {
        StoredFunction {
            name: "ub_double".to_string(),
            or_replace: false,
            params: vec![FunctionParam {
                name: "x".to_string(),
                data_type: DataType::Int4,
                mode: ParamMode::In,
                default: None,
            }],
            return_type: Some(DataType::Int4),
            body: "SELECT $1 * 2".to_string(),
            language: "sql".to_string(),
            volatility: None,
            created_at: 0,
        }
    }

    #[test]
    fn call_scalar_runs_the_body_and_restores_depth() {
        let registry = Arc::new(FunctionRegistry::new());
        registry.register_function(double_fn()).unwrap();
        let bridge = UdfBridge {
            registry,
            exec: Box::new(|sql: &str| {
                assert!(sql.contains("21"), "argument must be interpolated: {sql}");
                Ok(vec![vec![Value::Int4(42)]])
            }),
            max_call_depth: 4,
        };

        assert_eq!(current_call_depth(), 0);
        let out = call_scalar(&bridge, "ub_double", &[Value::Int4(21)]).unwrap();
        assert_eq!(out, Value::Int4(42));
        assert_eq!(current_call_depth(), 0, "the guard must restore the depth");
    }

    #[test]
    fn depth_limit_is_enforced_and_names_the_limit() {
        let registry = Arc::new(FunctionRegistry::new());
        registry.register_function(double_fn()).unwrap();
        let body_runs = Arc::new(Mutex::new(0_u32));
        let seen = Arc::clone(&body_runs);
        let bridge = UdfBridge {
            registry,
            exec: Box::new(move |_sql: &str| {
                *seen.lock() += 1;
                Ok(vec![vec![Value::Int4(1)]])
            }),
            max_call_depth: 2,
        };

        // Simulate being two frames deep already: the next call must be refused.
        CALL_DEPTH.with(|d| d.set(2));
        let err = call_scalar(&bridge, "ub_double", &[Value::Int4(1)]).unwrap_err();
        CALL_DEPTH.with(|d| d.set(0));
        let msg = err.to_string();
        assert!(msg.contains("depth limit (2)"), "message must name the limit: {msg}");
        assert!(msg.contains("ub_double"), "message must name the function: {msg}");
        assert_eq!(*body_runs.lock(), 0, "the body must not have run");
    }

    #[test]
    fn resolve_skips_bridges_that_do_not_define_the_name() {
        // Two live bridges: only the OLDER one defines `ub_double`. Resolution
        // must find it rather than stopping at the newest bridge — that is what
        // keeps parallel tests (each opening its own database) from stealing each
        // other's function resolution.
        let with_fn = Arc::new(FunctionRegistry::new());
        with_fn.register_function(double_fn()).unwrap();
        let first = Arc::new(UdfBridge {
            registry: with_fn,
            exec: Box::new(|_sql: &str| Ok(vec![vec![Value::Int4(1)]])),
            max_call_depth: 4,
        });
        let second = Arc::new(UdfBridge {
            registry: Arc::new(FunctionRegistry::new()),
            exec: Box::new(|_sql: &str| Ok(vec![])),
            max_call_depth: 4,
        });
        install(&first);
        install(&second);

        let found = resolve("ub_double").expect("the older bridge defines it");
        assert!(found.registry.function_exists("ub_double"));
        assert!(resolve("ub_defined_nowhere").is_none());

        // `found` is itself a strong reference — it must go before the bridge can
        // die, which is exactly the ownership rule the database relies on.
        drop(found);
        drop(second);
        drop(first);
        // Dropped bridges must never be resolvable again.
        assert!(resolve("ub_double").is_none());
    }

    #[test]
    fn unknown_function_is_loud() {
        let bridge = UdfBridge {
            registry: Arc::new(FunctionRegistry::new()),
            exec: Box::new(|_sql: &str| Ok(vec![])),
            max_call_depth: 4,
        };
        let err = call_scalar(&bridge, "ub_missing", &[]).unwrap_err().to_string();
        assert!(err.contains("does not exist"), "got: {err}");
    }
}
