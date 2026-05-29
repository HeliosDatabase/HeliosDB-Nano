//! Query execution (Volcano model)
//!
//! Standard iterator-based execution with pull-based operators.
//!
//! ## Implemented Operators
//!
//! - **Scan**: Table scan with iterator-based row retrieval
//! - **Filter**: WHERE clause evaluation with predicate functions
//! - **Project**: SELECT column projection
//! - **NestedLoopJoin**: Simple join for small datasets
//! - **HashJoin**: Efficient equi-join with hash table
//! - **Aggregate**: GROUP BY with aggregate functions (COUNT, SUM, AVG, MIN, MAX, STDDEV, VARIANCE)
//! - **Sort**: ORDER BY with in-memory sorting
//! - **Limit**: LIMIT/OFFSET row restriction

pub mod aggregation;
pub mod cancellation;
pub mod executor;

// Re-export executor types (Tuple is re-exported from crate::types)
pub use executor::{
    AggregateExecutor, Executor, FilterExecutor, GroupKeyFn, HashJoinExecutor, JoinConditionFn, KeyExtractorFn,
    LimitExecutor, NestedLoopJoinExecutor, PredicateFn, ProjectExecutor, ProjectFn, ScanExecutor, SortExecutor,
};

// Re-export aggregation types
pub use aggregation::{
    create_aggregate, AggregateFunction, AggregateState, AvgFunction, CountFunction, MaxFunction, MinFunction,
    StddevFunction, SumFunction, VarianceFunction,
};

// Re-export cancellation types
pub use cancellation::{start_timeout_checker, CancellationToken, QueryGuard, QueryRegistry, QueryState, RunningQuery};
