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

pub mod executor;
pub mod aggregation;
pub mod cancellation;

// Re-export executor types (Tuple is re-exported from crate::types)
pub use executor::{
    Executor,
    ScanExecutor, FilterExecutor, ProjectExecutor,
    NestedLoopJoinExecutor, HashJoinExecutor,
    AggregateExecutor, SortExecutor, LimitExecutor,
    PredicateFn, ProjectFn, JoinConditionFn, KeyExtractorFn, GroupKeyFn,
};

// Re-export aggregation types
pub use aggregation::{
    AggregateFunction, AggregateState,
    CountFunction, SumFunction, AvgFunction,
    MinFunction, MaxFunction, StddevFunction, VarianceFunction,
    create_aggregate,
};

// Re-export cancellation types
pub use cancellation::{
    CancellationToken, QueryRegistry, QueryGuard,
    RunningQuery, QueryState,
    start_timeout_checker,
};
