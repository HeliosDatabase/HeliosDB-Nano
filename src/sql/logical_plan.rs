//! Logical query plan structures
//!
//! This module defines the logical plan representation for SQL queries.
//! The logical plan is a tree of operators that represents the semantics
//! of a query without specifying how it should be executed.

use crate::{Schema, DataType, Value};
use serde::{Serialize, Deserialize};
use std::sync::Arc;

use super::explain_options::ExplainOptions;

/// Helper module for Arc<Schema> serialization
mod arc_schema_serde {
    use super::*;
    use serde::{Deserializer, Serializer};

    pub fn serialize<S>(schema: &Arc<Schema>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        schema.as_ref().serialize(serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Arc<Schema>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let schema = Schema::deserialize(deserializer)?;
        Ok(Arc::new(schema))
    }
}

/// Trigger timing specification (BEFORE, AFTER, INSTEAD OF)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TriggerTiming {
    /// Fire before the operation
    Before,
    /// Fire after the operation
    After,
    /// Fire instead of the operation (views/tables only)
    InsteadOf,
}

/// Trigger event (INSERT, UPDATE, DELETE, TRUNCATE)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TriggerEvent {
    /// INSERT event
    Insert,
    /// UPDATE event (optionally restricted to columns)
    Update(Option<Vec<String>>),
    /// DELETE event
    Delete,
    /// TRUNCATE event (PostgreSQL: FOR EACH STATEMENT only)
    Truncate,
}

/// Trigger for each clause (ROW or STATEMENT)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TriggerFor {
    /// Fire once per affected row
    Row,
    /// Fire once per statement
    Statement,
}

/// Transition table reference for REFERENCING clause
/// Used in statement-level triggers to access all affected rows as a virtual table
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TransitionTable {
    /// OLD TABLE AS alias_name - access old row values (UPDATE/DELETE)
    OldTable { alias: String },
    /// NEW TABLE AS alias_name - access new row values (INSERT/UPDATE)
    NewTable { alias: String },
}

/// Trigger type: Regular or Constraint
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum TriggerType {
    /// Regular trigger (default)
    #[default]
    Regular,
    /// Constraint trigger - always deferrable, fires at commit
    Constraint,
}

/// Trigger execution characteristics for deferred execution
/// PostgreSQL-compatible DEFERRABLE and INITIALLY DEFERRED/IMMEDIATE
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct TriggerCharacteristics {
    /// Whether the trigger can be deferred (DEFERRABLE vs NOT DEFERRABLE)
    pub deferrable: bool,
    /// Whether the trigger starts deferred (INITIALLY DEFERRED vs INITIALLY IMMEDIATE)
    pub initially_deferred: bool,
}

impl TriggerCharacteristics {
    /// Create default non-deferrable trigger characteristics
    pub fn new() -> Self {
        Self::default()
    }

    /// Create deferrable trigger that starts immediate
    pub fn deferrable() -> Self {
        Self {
            deferrable: true,
            initially_deferred: false,
        }
    }

    /// Create deferrable trigger that starts deferred
    pub fn deferrable_initially_deferred() -> Self {
        Self {
            deferrable: true,
            initially_deferred: true,
        }
    }
}

/// Logical plan node
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum LogicalPlan {
    /// Scan a table
    Scan {
        /// Table name
        table_name: String,
        /// Table alias (for JOIN disambiguation)
        alias: Option<String>,
        /// Schema of the table
        #[serde(with = "arc_schema_serde")]
        schema: Arc<Schema>,
        /// Optional projection (column indices)
        projection: Option<Vec<usize>>,
        /// Time-travel AS OF clause
        as_of: Option<AsOfClause>,
    },

    /// Scan with storage-level predicate pushdown
    /// This combines scanning and filtering at the storage layer for efficiency
    FilteredScan {
        /// Table name
        table_name: String,
        /// Table alias (for JOIN disambiguation)
        alias: Option<String>,
        /// Schema of the table
        #[serde(with = "arc_schema_serde")]
        schema: Arc<Schema>,
        /// Optional projection (column indices)
        projection: Option<Vec<usize>>,
        /// Predicate pushed down to storage layer
        predicate: Option<LogicalExpr>,
        /// Time-travel AS OF clause
        as_of: Option<AsOfClause>,
    },

    /// Filter rows based on a predicate
    Filter {
        /// Input plan
        input: Box<LogicalPlan>,
        /// Filter predicate
        predicate: LogicalExpr,
    },

    /// Project columns
    Project {
        /// Input plan
        input: Box<LogicalPlan>,
        /// Expressions to project
        exprs: Vec<LogicalExpr>,
        /// Aliases for projected columns
        aliases: Vec<String>,
        /// Whether to deduplicate results (DISTINCT)
        distinct: bool,
        /// DISTINCT ON expressions (PostgreSQL extension)
        distinct_on: Option<Vec<LogicalExpr>>,
    },

    /// Aggregate
    Aggregate {
        /// Input plan
        input: Box<LogicalPlan>,
        /// Group by expressions
        group_by: Vec<LogicalExpr>,
        /// Aggregate expressions
        aggr_exprs: Vec<LogicalExpr>,
        /// HAVING clause (filter on aggregated results)
        having: Option<LogicalExpr>,
    },

    /// Join two plans
    Join {
        /// Left input
        left: Box<LogicalPlan>,
        /// Right input
        right: Box<LogicalPlan>,
        /// Join type
        join_type: JoinType,
        /// Join condition
        on: Option<LogicalExpr>,
        /// LATERAL join - right side can reference left side columns
        #[serde(default)]
        lateral: bool,
    },

    /// Sort
    Sort {
        /// Input plan
        input: Box<LogicalPlan>,
        /// Sort expressions
        exprs: Vec<LogicalExpr>,
        /// Ascending or descending for each expression
        asc: Vec<bool>,
    },

    /// Limit
    Limit {
        /// Input plan
        input: Box<LogicalPlan>,
        /// Number of rows to return
        limit: usize,
        /// Number of rows to skip
        offset: usize,
    },

    /// UNION - combine results from two queries
    Union {
        /// Left input plan
        left: Box<LogicalPlan>,
        /// Right input plan
        right: Box<LogicalPlan>,
        /// If true, keep duplicates (UNION ALL)
        all: bool,
    },

    /// INTERSECT - rows that appear in both queries
    Intersect {
        /// Left input plan
        left: Box<LogicalPlan>,
        /// Right input plan
        right: Box<LogicalPlan>,
        /// If true, keep duplicates (INTERSECT ALL)
        all: bool,
    },

    /// EXCEPT - rows in left that don't appear in right
    Except {
        /// Left input plan
        left: Box<LogicalPlan>,
        /// Right input plan
        right: Box<LogicalPlan>,
        /// If true, keep duplicates (EXCEPT ALL)
        all: bool,
    },

    /// Insert values
    Insert {
        /// Table name
        table_name: String,
        /// Column names (if specified)
        columns: Option<Vec<String>>,
        /// Values to insert
        values: Vec<Vec<LogicalExpr>>,
        /// RETURNING clause (column names to return)
        returning: Option<Vec<String>>,
    },

    /// Create table
    CreateTable {
        /// Table name
        name: String,
        /// Column definitions
        columns: Vec<ColumnDef>,
        /// If table already exists, do nothing
        if_not_exists: bool,
        /// Table constraints (FK, CHECK, UNIQUE)
        #[serde(default)]
        constraints: Vec<TableConstraint>,
    },

    /// Drop table
    DropTable {
        /// Table name
        name: String,
        /// If table doesn't exist, do nothing
        if_exists: bool,
    },

    /// Truncate table (remove all rows)
    Truncate {
        /// Table name
        table_name: String,
    },

    /// Update rows
    Update {
        /// Table name
        table_name: String,
        /// Assignments (column name, value expression)
        assignments: Vec<(String, LogicalExpr)>,
        /// Optional WHERE clause
        selection: Option<LogicalExpr>,
        /// RETURNING clause (column names to return)
        returning: Option<Vec<String>>,
    },

    /// Delete rows
    Delete {
        /// Table name
        table_name: String,
        /// Optional WHERE clause
        selection: Option<LogicalExpr>,
        /// RETURNING clause (column names to return)
        returning: Option<Vec<String>>,
    },

    /// Create index
    CreateIndex {
        /// Index name
        name: String,
        /// Table name
        table_name: String,
        /// Column name
        column_name: String,
        /// Index type (e.g., "hnsw")
        index_type: Option<String>,
        /// Index options (e.g., quantization, pq_subquantizers, etc.)
        options: Vec<IndexOption>,
        /// If index already exists, do nothing
        if_not_exists: bool,
    },

    /// Alter column storage mode
    /// Changes per-column storage mode (Dictionary, Content-Addressed, Columnar)
    AlterColumnStorage {
        /// Table name
        table_name: String,
        /// Column name
        column_name: String,
        /// New storage mode
        storage_mode: crate::ColumnStorageMode,
    },

    // === ALTER TABLE operations ===

    /// Add a column to an existing table
    AlterTableAddColumn {
        /// Table name
        table_name: String,
        /// Column definition
        column_def: ColumnDef,
        /// IF NOT EXISTS
        if_not_exists: bool,
    },

    /// Drop a column from an existing table
    AlterTableDropColumn {
        /// Table name
        table_name: String,
        /// Column name to drop
        column_name: String,
        /// IF EXISTS
        if_exists: bool,
        /// CASCADE
        cascade: bool,
    },

    /// Rename a column in an existing table
    AlterTableRenameColumn {
        /// Table name
        table_name: String,
        /// Old column name
        old_column_name: String,
        /// New column name
        new_column_name: String,
    },

    /// Rename a table
    AlterTableRename {
        /// Current table name
        table_name: String,
        /// New table name
        new_table_name: String,
    },

    // === Phase 3: Database Branching ===

    /// Create a database branch
    CreateBranch {
        /// Branch name
        branch_name: String,
        /// Parent branch (None = CURRENT)
        parent: Option<String>,
        /// Creation point
        as_of: AsOfClause,
        /// Branch options
        options: Vec<BranchOption>,
    },

    /// Drop a database branch
    DropBranch {
        /// Branch name
        branch_name: String,
        /// If branch doesn't exist, do nothing
        if_exists: bool,
    },

    /// Merge a branch into another
    MergeBranch {
        /// Source branch
        source: String,
        /// Target branch
        target: String,
        /// Merge options
        options: Vec<MergeOption>,
    },

    /// Switch to a database branch
    UseBranch {
        /// Branch name to switch to
        branch_name: String,
    },

    /// List all database branches
    ShowBranches,

    // === Phase 3: Materialized Views ===

    /// Create materialized view
    CreateMaterializedView {
        /// View name
        name: String,
        /// Query definition
        query: Box<LogicalPlan>,
        /// Materialized view options
        options: Vec<MaterializedViewOption>,
        /// If view already exists, do nothing
        if_not_exists: bool,
    },

    /// Refresh materialized view
    RefreshMaterializedView {
        /// View name
        name: String,
        /// Concurrent refresh (doesn't block reads)
        concurrent: bool,
        /// Use incremental refresh (apply deltas instead of full recompute)
        incremental: bool,
    },

    /// Drop materialized view
    DropMaterializedView {
        /// View name
        name: String,
        /// If view doesn't exist, do nothing
        if_exists: bool,
    },

    /// Alter materialized view options
    AlterMaterializedView {
        /// View name
        name: String,
        /// Options to set (key-value pairs)
        options: std::collections::HashMap<String, String>,
    },

    // === Regular Views (non-materialized) ===

    /// Create a regular view (virtual table)
    CreateView {
        /// View name
        name: String,
        /// Query definition (stored as SQL string for expansion)
        query_sql: String,
        /// If view already exists, do nothing
        if_not_exists: bool,
        /// OR REPLACE - replace existing view
        or_replace: bool,
    },

    /// Drop a regular view
    DropView {
        /// View name
        name: String,
        /// If view doesn't exist, do nothing
        if_exists: bool,
    },

    // === Phase 3: System Views ===

    /// Query a system view (e.g., pg_database_branches())
    SystemView {
        /// View name
        name: String,
        /// Arguments (for function-like views)
        args: Vec<LogicalExpr>,
    },

    /// Common Table Expression (WITH clause)
    With {
        /// CTE definitions (name -> query plan -> optional column aliases)
        /// Column aliases rename the output columns (e.g., `nums(n)` renames column to `n`)
        ctes: Vec<(String, Box<LogicalPlan>, Option<Vec<String>>)>,
        /// Main query plan
        query: Box<LogicalPlan>,
        /// Whether this is WITH RECURSIVE
        recursive: bool,
    },

    /// Create a trigger
    CreateTrigger {
        /// Trigger name
        name: String,
        /// Table name
        table_name: String,
        /// Trigger timing (BEFORE, AFTER, INSTEAD OF)
        timing: TriggerTiming,
        /// Trigger events (INSERT, UPDATE, DELETE)
        events: Vec<TriggerEvent>,
        /// For each row or statement
        for_each: TriggerFor,
        /// Optional WHEN clause condition
        when_condition: Option<Box<LogicalExpr>>,
        /// Trigger body statements
        body: Vec<LogicalPlan>,
        /// If trigger already exists, do nothing
        if_not_exists: bool,
        /// REFERENCING clause: transition table aliases for statement-level triggers
        referencing: Vec<TransitionTable>,
        /// DEFERRABLE characteristics (DEFERRABLE, INITIALLY DEFERRED/IMMEDIATE)
        characteristics: TriggerCharacteristics,
        /// Trigger type (Regular or Constraint)
        trigger_type: TriggerType,
        /// Referenced constraint name (for CONSTRAINT triggers with FROM clause)
        from_constraint: Option<String>,
    },

    /// Drop a trigger
    DropTrigger {
        /// Trigger name
        name: String,
        /// Table name (optional for some databases)
        table_name: Option<String>,
        /// If trigger doesn't exist, do nothing
        if_exists: bool,
    },

    /// Explain a query plan
    Explain {
        /// The plan to explain
        input: Box<LogicalPlan>,
        /// EXPLAIN options (FORMAT, ANALYZE, VERBOSE, STORAGE, AI, etc.)
        options: ExplainOptions,
    },

    // === Transaction Control ===

    /// Start a transaction
    StartTransaction,

    /// Commit the current transaction
    Commit,

    /// Rollback the current transaction
    Rollback,

    /// Create a savepoint within a transaction
    Savepoint {
        /// Savepoint name
        name: String,
    },

    /// Release (commit) a savepoint
    ReleaseSavepoint {
        /// Savepoint name
        name: String,
    },

    /// Rollback to a savepoint
    RollbackToSavepoint {
        /// Savepoint name
        name: String,
    },

    // === Prepared Statements ===

    /// Prepare a statement for later execution
    Prepare {
        /// Statement name
        name: String,
        /// Parameter data types (optional)
        param_types: Vec<DataType>,
        /// The statement to prepare
        statement: Box<LogicalPlan>,
    },

    /// Execute a prepared statement
    Execute {
        /// Statement name
        name: String,
        /// Parameter values
        parameters: Vec<LogicalExpr>,
    },

    /// Deallocate (drop) a prepared statement
    Deallocate {
        /// Statement name (None = ALL)
        name: Option<String>,
    },

    /// SET CONSTRAINTS - change deferral mode for constraints/triggers
    SetConstraints {
        /// Constraint/trigger names (empty = ALL)
        names: Vec<String>,
        /// Whether to defer until commit
        deferred: bool,
    },

    // === Procedural ===

    /// Create a stored function
    CreateFunction {
        /// Function name
        name: String,
        /// Replace if exists
        or_replace: bool,
        /// Function parameters
        params: Vec<FunctionParam>,
        /// Return type
        return_type: Option<crate::types::DataType>,
        /// Function body (procedural code)
        body: String,
        /// Language (plpgsql, sql, etc.)
        language: String,
        /// Volatility (IMMUTABLE, STABLE, VOLATILE)
        volatility: Option<String>,
    },

    /// Create a stored procedure
    CreateProcedure {
        /// Procedure name
        name: String,
        /// Replace if exists
        or_replace: bool,
        /// Procedure parameters
        params: Vec<FunctionParam>,
        /// Procedure body (procedural code)
        body: String,
        /// Language (plpgsql, sql, etc.)
        language: String,
    },

    /// Drop a function
    DropFunction {
        /// Function name
        name: String,
        /// If exists clause
        if_exists: bool,
    },

    /// Drop a procedure
    DropProcedure {
        /// Procedure name
        name: String,
        /// If exists clause
        if_exists: bool,
    },

    /// Call a stored procedure
    Call {
        /// Procedure name
        name: String,
        /// Arguments
        args: Vec<LogicalExpr>,
    },

    // === Utility ===

    /// Dual scan for SELECT without FROM (like Oracle's DUAL)
    /// Returns a single row with no columns for expression evaluation
    DualScan,

    // === HA Operations ===

    /// Controlled switchover to a target standby node
    /// Example: SELECT helios_switchover('node-uuid')
    #[cfg(feature = "ha-tier1")]
    Switchover {
        /// Target node ID (UUID) to promote to primary
        target_node: String,
    },

    /// Check switchover preconditions before executing
    /// Example: SELECT helios_switchover_check('node-uuid')
    #[cfg(feature = "ha-tier1")]
    SwitchoverCheck {
        /// Target node ID (UUID) to check
        target_node: String,
    },

    /// Show HA cluster status (primary, standbys, lag, etc.)
    /// Example: SELECT * FROM helios_cluster_status()
    #[cfg(feature = "ha-tier1")]
    ClusterStatus,

    /// Set or remove a node alias
    /// Example: SET NODE ALIAS 'my-standby' FOR 'node-uuid'
    /// Example: SET NODE ALIAS NULL FOR 'node-uuid' (removes alias)
    #[cfg(feature = "ha-tier1")]
    SetNodeAlias {
        /// Node identifier (UUID or existing alias)
        node_id: String,
        /// New alias (None to remove)
        alias: Option<String>,
    },

    /// Show detailed cluster topology with health information
    /// Example: SHOW TOPOLOGY
    #[cfg(feature = "ha-tier1")]
    ShowTopology,
}

/// Function/Procedure parameter
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FunctionParam {
    /// Parameter name
    pub name: String,
    /// Parameter data type
    pub data_type: crate::types::DataType,
    /// Parameter mode (IN, OUT, INOUT)
    pub mode: ParamMode,
    /// Default value expression
    pub default: Option<LogicalExpr>,
}

/// Parameter mode
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ParamMode {
    In,
    Out,
    InOut,
}

/// Logical expression
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum LogicalExpr {
    /// Column reference
    Column {
        /// Optional table alias or name for disambiguation in JOINs
        table: Option<String>,
        /// Column name
        name: String,
    },

    /// Literal value
    Literal(Value),

    /// Binary expression (a + b, a = b, etc.)
    BinaryExpr {
        /// Left expression
        left: Box<LogicalExpr>,
        /// Operator
        op: BinaryOperator,
        /// Right expression
        right: Box<LogicalExpr>,
    },

    /// Unary expression (NOT a, -a, etc.)
    UnaryExpr {
        /// Operator
        op: UnaryOperator,
        /// Expression
        expr: Box<LogicalExpr>,
    },

    /// Aggregate function
    AggregateFunction {
        /// Function name
        fun: AggregateFunction,
        /// Arguments
        args: Vec<LogicalExpr>,
        /// DISTINCT
        distinct: bool,
    },

    /// Scalar function
    ScalarFunction {
        /// Function name
        fun: String,
        /// Arguments
        args: Vec<LogicalExpr>,
    },

    /// CASE expression
    Case {
        /// Optional base expression for CASE expr WHEN ...
        expr: Option<Box<LogicalExpr>>,
        /// WHEN conditions and results
        when_then: Vec<(LogicalExpr, LogicalExpr)>,
        /// ELSE result
        else_result: Option<Box<LogicalExpr>>,
    },

    /// CAST expression
    Cast {
        /// Expression to cast
        expr: Box<LogicalExpr>,
        /// Target data type
        data_type: DataType,
    },

    /// IS NULL / IS NOT NULL
    IsNull {
        /// Expression to check
        expr: Box<LogicalExpr>,
        /// True for IS NULL, false for IS NOT NULL
        is_null: bool,
    },

    /// BETWEEN
    Between {
        /// Expression to test
        expr: Box<LogicalExpr>,
        /// Lower bound
        low: Box<LogicalExpr>,
        /// Upper bound
        high: Box<LogicalExpr>,
        /// True for BETWEEN, false for NOT BETWEEN
        negated: bool,
    },

    /// IN list
    InList {
        /// Expression to test
        expr: Box<LogicalExpr>,
        /// List of values
        list: Vec<LogicalExpr>,
        /// True for IN, false for NOT IN
        negated: bool,
    },

    /// IN subquery: expr IN (SELECT ...)
    InSubquery {
        /// Expression to test
        expr: Box<LogicalExpr>,
        /// Subquery that returns a single column
        subquery: Box<LogicalPlan>,
        /// True for NOT IN, false for IN
        negated: bool,
    },

    /// EXISTS subquery: EXISTS (SELECT ...)
    Exists {
        /// Subquery to check for existence
        subquery: Box<LogicalPlan>,
        /// True for NOT EXISTS, false for EXISTS
        negated: bool,
    },

    /// Wildcard (SELECT *)
    Wildcard,

    /// Parameter placeholder ($1, $2, etc.)
    Parameter {
        /// Parameter index (1-based, matching PostgreSQL convention)
        index: usize,
    },

    /// NEW context variable (trigger context - inserted/updated row)
    /// Used in trigger bodies to access the new row values
    /// Valid for INSERT and UPDATE triggers
    NewRow {
        /// Column name to access from NEW row
        column: String,
    },

    /// OLD context variable (trigger context - deleted/updated row)
    /// Used in trigger bodies to access the old row values
    /// Valid for UPDATE and DELETE triggers
    OldRow {
        /// Column name to access from OLD row
        column: String,
    },

    /// Array subscript operator: arr[index]
    ArraySubscript {
        /// Array expression
        array: Box<LogicalExpr>,
        /// Index expression (1-based for PostgreSQL compatibility)
        index: Box<LogicalExpr>,
    },

    /// Window function: func(args) OVER (PARTITION BY ... ORDER BY ...)
    WindowFunction {
        /// Window function type
        fun: WindowFunctionType,
        /// Arguments to the function
        args: Vec<LogicalExpr>,
        /// PARTITION BY columns
        partition_by: Vec<LogicalExpr>,
        /// ORDER BY expressions and directions (expr, ascending)
        order_by: Vec<(LogicalExpr, bool)>,
        /// Window frame specification
        frame: Option<WindowFrame>,
    },
}

/// Window function type
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WindowFunctionType {
    /// ROW_NUMBER() - sequential row number within partition
    RowNumber,
    /// RANK() - rank with gaps for ties
    Rank,
    /// DENSE_RANK() - rank without gaps for ties
    DenseRank,
    /// PERCENT_RANK() - relative rank (0 to 1)
    PercentRank,
    /// CUME_DIST() - cumulative distribution
    CumeDist,
    /// NTILE(n) - divide into n buckets
    Ntile,
    /// LAG(expr, offset, default) - value from previous row
    Lag,
    /// LEAD(expr, offset, default) - value from next row
    Lead,
    /// FIRST_VALUE(expr) - first value in window frame
    FirstValue,
    /// LAST_VALUE(expr) - last value in window frame
    LastValue,
    /// NTH_VALUE(expr, n) - nth value in window frame
    NthValue,
    /// Aggregate function used as window function (SUM, AVG, etc.)
    Aggregate(AggregateFunction),
}

/// Window frame specification
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowFrame {
    /// Frame type (ROWS, RANGE, GROUPS)
    pub frame_type: WindowFrameType,
    /// Frame start bound
    pub start: WindowFrameBound,
    /// Frame end bound (None means CURRENT ROW for RANGE/ROWS BETWEEN)
    pub end: Option<WindowFrameBound>,
}

/// Window frame type
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WindowFrameType {
    /// ROWS - physical rows
    Rows,
    /// RANGE - logical value ranges
    Range,
    /// GROUPS - peer groups (PostgreSQL 11+)
    Groups,
}

/// Window frame bound
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WindowFrameBound {
    /// UNBOUNDED PRECEDING
    UnboundedPreceding,
    /// n PRECEDING
    Preceding(u64),
    /// CURRENT ROW
    CurrentRow,
    /// n FOLLOWING
    Following(u64),
    /// UNBOUNDED FOLLOWING
    UnboundedFollowing,
}

/// Binary operator
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BinaryOperator {
    // Arithmetic
    Plus,
    Minus,
    Multiply,
    Divide,
    Modulo,

    // Comparison
    Eq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,

    // Logical
    And,
    Or,

    // String pattern matching
    Like,
    NotLike,
    /// Case-insensitive LIKE
    ILike,
    /// Case-insensitive NOT LIKE
    NotILike,
    /// Regular expression match (POSIX ~)
    RegexMatch,
    /// Case-insensitive regex match (~*)
    RegexIMatch,
    /// Negated regex match (!~)
    NotRegexMatch,
    /// Negated case-insensitive regex match (!~*)
    NotRegexIMatch,
    /// SQL standard SIMILAR TO
    SimilarTo,
    /// Negated SIMILAR TO
    NotSimilarTo,

    // Vector similarity operators (pgvector compatible)
    /// L2 distance (Euclidean): <->
    VectorL2Distance,
    /// Cosine distance: <=>
    VectorCosineDistance,
    /// Inner product (dot product): <#>
    VectorInnerProduct,

    // JSONB operators (PostgreSQL compatible)
    /// Get JSON object field as JSON: ->
    JsonGet,
    /// Get JSON object field as text: ->>
    JsonGetText,
    /// Contains JSON value: @>
    JsonContains,
    /// JSON value is contained in: <@
    JsonContainedBy,
    /// Key/element exists: ?
    JsonExists,
    /// Any key/element exists: ?|
    JsonExistsAny,
    /// All keys/elements exist: ?&
    JsonExistsAll,

    // Array operators (PostgreSQL compatible)
    /// Array concatenation: ||
    ArrayConcat,
}

/// Unary operator
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum UnaryOperator {
    Not,
    Minus,
    Plus,
}

/// Aggregate function
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AggregateFunction {
    Count,
    Sum,
    Avg,
    Min,
    Max,
    JsonAgg,
    /// ARRAY_AGG - collect values into an array
    ArrayAgg,
    /// STRING_AGG(value, delimiter) - concatenate strings with delimiter
    StringAgg { delimiter: String },
}

/// Join type
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum JoinType {
    Inner,
    Left,
    Right,
    Full,
    Cross,
}

/// Column definition for CREATE TABLE
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ColumnDef {
    /// Column name
    pub name: String,
    /// Data type
    pub data_type: DataType,
    /// NOT NULL constraint
    pub not_null: bool,
    /// PRIMARY KEY constraint
    pub primary_key: bool,
    /// UNIQUE constraint
    pub unique: bool,
    /// DEFAULT value
    pub default: Option<LogicalExpr>,
    /// Storage mode for per-column optimization
    #[serde(default)]
    pub storage_mode: crate::ColumnStorageMode,
}

/// Table-level constraint for CREATE TABLE
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TableConstraint {
    /// PRIMARY KEY constraint
    PrimaryKey {
        /// Constraint name (optional)
        name: Option<String>,
        /// Columns forming the primary key
        columns: Vec<String>,
    },
    /// UNIQUE constraint
    Unique {
        /// Constraint name (optional)
        name: Option<String>,
        /// Columns that must be unique together
        columns: Vec<String>,
    },
    /// FOREIGN KEY constraint
    ForeignKey {
        /// Constraint name (optional)
        name: Option<String>,
        /// Foreign key columns in this table
        columns: Vec<String>,
        /// Referenced table name
        references_table: String,
        /// Referenced columns
        references_columns: Vec<String>,
        /// ON DELETE action
        on_delete: Option<ReferentialAction>,
        /// ON UPDATE action
        on_update: Option<ReferentialAction>,
        /// Whether constraint is deferrable
        deferrable: bool,
        /// If deferrable, whether initially deferred
        initially_deferred: bool,
    },
    /// CHECK constraint
    Check {
        /// Constraint name (optional)
        name: Option<String>,
        /// Expression that must evaluate to true
        expression: LogicalExpr,
    },
}

/// Referential action for foreign key ON DELETE / ON UPDATE
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReferentialAction {
    NoAction,
    Restrict,
    Cascade,
    SetNull,
    SetDefault,
}

impl Default for ReferentialAction {
    fn default() -> Self {
        ReferentialAction::NoAction
    }
}

// === Phase 3: Supporting Types ===

/// Time-travel AS OF clause
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum AsOfClause {
    /// AS OF NOW - current time
    Now,
    /// AS OF TIMESTAMP '2025-11-15 06:00:00'
    Timestamp(String),
    /// AS OF TRANSACTION 987654
    Transaction(u64),
    /// AS OF SCN 123456789 (System Change Number)
    Scn(u64),
    /// AS OF COMMIT 'abc123def' (Git commit SHA)
    Commit(String),
    /// VERSIONS BETWEEN start AND end - returns all versions in range
    VersionsBetween {
        /// Start timestamp (inclusive)
        start: Box<AsOfClause>,
        /// End timestamp (inclusive)
        end: Box<AsOfClause>,
    },
}

/// Index creation options
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum IndexOption {
    /// quantization = 'product'
    Quantization(QuantizationType),
    /// pq_subquantizers = 8
    PqSubquantizers(usize),
    /// pq_centroids = 256
    PqCentroids(usize),
    /// m = 16 (HNSW M parameter)
    HnswM(usize),
    /// ef_construction = 200
    EfConstruction(usize),
    /// sharding_strategy = 'hash'
    ShardingStrategy(String),
    /// shard_count = 16
    ShardCount(usize),
}

/// Quantization type for vector indexes
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum QuantizationType {
    /// No quantization
    None,
    /// Scalar quantization
    Scalar,
    /// Product quantization
    Product,
    /// Auto-detect based on index size
    Auto,
}

/// Branch creation options
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum BranchOption {
    /// replication_factor = 3
    ReplicationFactor(usize),
    /// region = 'us-west'
    Region(String),
    /// metadata key-value pairs
    Metadata(String, String),
}

/// Branch merge options
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum MergeOption {
    /// conflict_resolution = 'branch_wins'
    ConflictResolution(ConflictResolution),
    /// delete_branch_after = true
    DeleteBranchAfter(bool),
}

/// Conflict resolution strategy
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConflictResolution {
    /// Source branch wins
    BranchWins,
    /// Target branch wins
    TargetWins,
    /// Fail on conflict
    Fail,
}

/// Materialized view options
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum MaterializedViewOption {
    /// auto_refresh = true
    AutoRefresh(bool),
    /// threshold_table_size = '1GB'
    ThresholdTableSize(String),
    /// threshold_dml_rate = 100 (DML operations per minute)
    ThresholdDmlRate(usize),
    /// max_cpu_percent = 15
    MaxCpuPercent(f32),
    /// lazy_update = true
    LazyUpdate(bool),
    /// lazy_catchup_window = '1 hour'
    LazyCatchupWindow(String),
    /// distribution = 'hash(user_id)'
    Distribution(String),
    /// replication_factor = 3
    ReplicationFactor(usize),
}

/// Diff level for branch/time-travel comparison
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum DiffLevel {
    /// Schema-only comparison (tables, columns, indexes)
    #[default]
    SchemaOnly,
    /// Schema plus sampled data comparison
    Sampled {
        /// Number of sample rows to compare
        sample_size: usize,
    },
    /// Full data comparison for all rows
    Full,
}

impl std::fmt::Display for DiffLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DiffLevel::SchemaOnly => write!(f, "SCHEMA ONLY"),
            DiffLevel::Sampled { sample_size } => write!(f, "SAMPLED ({})", sample_size),
            DiffLevel::Full => write!(f, "FULL"),
        }
    }
}

impl LogicalPlan {
    /// Get the variant name for tracing/debugging
    pub fn plan_type_name(&self) -> &'static str {
        match self {
            Self::Scan { .. } => "Scan",
            Self::FilteredScan { .. } => "FilteredScan",
            Self::Filter { .. } => "Filter",
            Self::Project { .. } => "Project",
            Self::Aggregate { .. } => "Aggregate",
            Self::Sort { .. } => "Sort",
            Self::Limit { .. } => "Limit",
            Self::Join { .. } => "Join",
            Self::Insert { .. } => "Insert",
            Self::Update { .. } => "Update",
            Self::Delete { .. } => "Delete",
            Self::CreateTable { .. } => "CreateTable",
            Self::DropTable { .. } => "DropTable",
            Self::CreateIndex { .. } => "CreateIndex",
            Self::Explain { .. } => "Explain",
            Self::Union { .. } => "Union",
            Self::Intersect { .. } => "Intersect",
            Self::Except { .. } => "Except",
            Self::With { .. } => "CTE",
            Self::Truncate { .. } => "Truncate",
            Self::CreateBranch { .. } => "CreateBranch",
            Self::MergeBranch { .. } => "MergeBranch",
            Self::SystemView { .. } => "SystemView",
            _ => "Other",
        }
    }

    /// Get the schema of this plan's output
    pub fn schema(&self) -> Arc<Schema> {
        match self {
            LogicalPlan::Scan { schema, projection, .. } => {
                if let Some(indices) = projection {
                    let columns: Vec<_> = indices.iter()
                        .filter_map(|&i| schema.columns.get(i).cloned())
                        .collect();
                    Arc::new(Schema { columns })
                } else {
                    schema.clone()
                }
            }
            LogicalPlan::FilteredScan { schema, projection, .. } => {
                if let Some(indices) = projection {
                    let columns: Vec<_> = indices.iter()
                        .filter_map(|&i| schema.columns.get(i).cloned())
                        .collect();
                    Arc::new(Schema { columns })
                } else {
                    schema.clone()
                }
            }
            LogicalPlan::Filter { input, .. } => input.schema(),
            LogicalPlan::Project { input, exprs, aliases, .. } => {
                use crate::sql::type_inference::TypeInference;
                let input_schema = input.schema();
                let columns = aliases.iter()
                    .zip(exprs.iter())
                    .map(|(alias, expr)| {
                        // Use the new to_column method for complete type + nullability inference
                        expr.to_column(alias.clone(), &input_schema)
                    })
                    .collect();
                Arc::new(Schema { columns })
            }
            LogicalPlan::Aggregate { input, group_by, aggr_exprs, .. } => {
                use crate::sql::type_inference::TypeInference;
                let input_schema = input.schema();
                let mut columns = Vec::new();

                // Add GROUP BY columns with complete type and nullability inference
                for (i, expr) in group_by.iter().enumerate() {
                    columns.push(expr.to_column(format!("group_{}", i), &input_schema));
                }

                // Add aggregate columns with complete type and nullability inference
                for (i, expr) in aggr_exprs.iter().enumerate() {
                    columns.push(expr.to_column(format!("agg_{}", i), &input_schema));
                }

                Arc::new(Schema { columns })
            }
            LogicalPlan::Sort { input, .. } => input.schema(),
            LogicalPlan::Limit { input, .. } => input.schema(),
            // Set operations use left schema (both sides must have compatible schemas)
            LogicalPlan::Union { left, .. } => left.schema(),
            LogicalPlan::Intersect { left, .. } => left.schema(),
            LogicalPlan::Except { left, .. } => left.schema(),
            LogicalPlan::Join { left, right, .. } => {
                // Combine schemas from left and right
                let mut columns = left.schema().columns.clone();
                columns.extend(right.schema().columns.clone());
                Arc::new(Schema { columns })
            }
            LogicalPlan::Insert { .. } => {
                // Insert doesn't have output schema
                Arc::new(Schema { columns: vec![] })
            }
            LogicalPlan::CreateTable { .. } => {
                Arc::new(Schema { columns: vec![] })
            }
            LogicalPlan::DropTable { .. } => {
                Arc::new(Schema { columns: vec![] })
            }
            LogicalPlan::Truncate { .. } => {
                // Truncate doesn't have output schema
                Arc::new(Schema { columns: vec![] })
            }
            LogicalPlan::Update { .. } => {
                // Update doesn't have output schema
                Arc::new(Schema { columns: vec![] })
            }
            LogicalPlan::Delete { .. } => {
                // Delete doesn't have output schema
                Arc::new(Schema { columns: vec![] })
            }
            LogicalPlan::CreateIndex { .. } => {
                // CreateIndex doesn't have output schema
                Arc::new(Schema { columns: vec![] })
            }
            LogicalPlan::AlterColumnStorage { .. } => {
                // AlterColumnStorage doesn't have output schema
                Arc::new(Schema { columns: vec![] })
            }
            LogicalPlan::AlterTableAddColumn { .. } => {
                // ALTER TABLE ADD COLUMN doesn't have output schema
                Arc::new(Schema { columns: vec![] })
            }
            LogicalPlan::AlterTableDropColumn { .. } => {
                // ALTER TABLE DROP COLUMN doesn't have output schema
                Arc::new(Schema { columns: vec![] })
            }
            LogicalPlan::AlterTableRenameColumn { .. } => {
                // ALTER TABLE RENAME COLUMN doesn't have output schema
                Arc::new(Schema { columns: vec![] })
            }
            LogicalPlan::AlterTableRename { .. } => {
                // ALTER TABLE RENAME doesn't have output schema
                Arc::new(Schema { columns: vec![] })
            }
            LogicalPlan::CreateBranch { .. } => {
                // CreateBranch doesn't have output schema
                Arc::new(Schema { columns: vec![] })
            }
            LogicalPlan::DropBranch { .. } => {
                // DropBranch doesn't have output schema
                Arc::new(Schema { columns: vec![] })
            }
            LogicalPlan::MergeBranch { .. } => {
                // MergeBranch doesn't have output schema
                Arc::new(Schema { columns: vec![] })
            }
            LogicalPlan::UseBranch { .. } => {
                // UseBranch doesn't have output schema
                Arc::new(Schema { columns: vec![] })
            }
            LogicalPlan::ShowBranches => {
                // ShowBranches returns a table with branch information
                use crate::DataType;
                Arc::new(Schema {
                    columns: vec![
                        crate::Column {
                            name: "branch_name".to_string(),
                            data_type: DataType::Text,
                            nullable: false,
                            primary_key: false,
                            source_table: None,
                            source_table_name: None,
                            default_expr: None,
                            unique: false,
                            storage_mode: Default::default(),
                        },
                        crate::Column {
                            name: "branch_id".to_string(),
                            data_type: DataType::Int8,
                            nullable: false,
                            primary_key: false,
                            source_table: None,
                            source_table_name: None,
                            default_expr: None,
                            unique: false,
                            storage_mode: Default::default(),
                        },
                        crate::Column {
                            name: "parent_branch".to_string(),
                            data_type: DataType::Text,
                            nullable: true,
                            primary_key: false,
                            source_table: None,
                            source_table_name: None,
                            default_expr: None,
                            unique: false,
                            storage_mode: Default::default(),
                        },
                        crate::Column {
                            name: "created_at".to_string(),
                            data_type: DataType::Timestamp,
                            nullable: false,
                            primary_key: false,
                            source_table: None,
                            source_table_name: None,
                            default_expr: None,
                            unique: false,
                            storage_mode: Default::default(),
                        },
                        crate::Column {
                            name: "state".to_string(),
                            data_type: DataType::Text,
                            nullable: false,
                            primary_key: false,
                            source_table: None,
                            source_table_name: None,
                            default_expr: None,
                            unique: false,
                            storage_mode: Default::default(),
                        },
                    ],
                })
            }
            LogicalPlan::CreateMaterializedView { .. } => {
                // CreateMaterializedView doesn't have output schema
                Arc::new(Schema { columns: vec![] })
            }
            LogicalPlan::RefreshMaterializedView { .. } => {
                // RefreshMaterializedView doesn't have output schema
                Arc::new(Schema { columns: vec![] })
            }
            LogicalPlan::DropMaterializedView { .. } => {
                // DropMaterializedView doesn't have output schema
                Arc::new(Schema { columns: vec![] })
            }
            LogicalPlan::AlterMaterializedView { .. } => {
                // AlterMaterializedView doesn't have output schema
                Arc::new(Schema { columns: vec![] })
            }
            LogicalPlan::SystemView { name, .. } => {
                // System views have predefined schemas from the registry
                use crate::sql::phase3::SystemViewRegistry;
                let registry = SystemViewRegistry::new();
                if let Some(schema) = registry.get_schema(name) {
                    Arc::new(schema.clone())
                } else {
                    // Fallback to empty schema if view not found
                    Arc::new(Schema { columns: vec![] })
                }
            }
            LogicalPlan::With { query, .. } => {
                // With clause returns the schema of the inner query
                query.schema()
            }
            LogicalPlan::CreateTrigger { .. } => {
                // CreateTrigger doesn't have output schema
                Arc::new(Schema { columns: vec![] })
            }
            LogicalPlan::DropTrigger { .. } => {
                // DropTrigger doesn't have output schema
                Arc::new(Schema { columns: vec![] })
            }
            LogicalPlan::Explain { .. } => {
                // EXPLAIN returns a single text column with the query plan
                use crate::DataType;
                Arc::new(Schema {
                    columns: vec![
                        crate::Column {
                            name: "QUERY PLAN".to_string(),
                            data_type: DataType::Text,
                            nullable: false,
                            primary_key: false,
                            source_table: None,
                            source_table_name: None,
                            default_expr: None,
                            unique: false,
                            storage_mode: Default::default(),
                        },
                    ],
                })
            }
            // Transaction control - no output schema
            LogicalPlan::StartTransaction => Arc::new(Schema { columns: vec![] }),
            LogicalPlan::Commit => Arc::new(Schema { columns: vec![] }),
            LogicalPlan::Rollback => Arc::new(Schema { columns: vec![] }),
            LogicalPlan::Savepoint { .. } => Arc::new(Schema { columns: vec![] }),
            LogicalPlan::ReleaseSavepoint { .. } => Arc::new(Schema { columns: vec![] }),
            LogicalPlan::RollbackToSavepoint { .. } => Arc::new(Schema { columns: vec![] }),
            LogicalPlan::SetConstraints { .. } => Arc::new(Schema { columns: vec![] }),
            // Prepared statements - no output schema for DDL
            LogicalPlan::Prepare { .. } => Arc::new(Schema { columns: vec![] }),
            LogicalPlan::Execute { .. } => Arc::new(Schema { columns: vec![] }),
            LogicalPlan::Deallocate { .. } => Arc::new(Schema { columns: vec![] }),
            // Procedural statements - no output schema for DDL
            LogicalPlan::CreateFunction { .. } => Arc::new(Schema { columns: vec![] }),
            LogicalPlan::CreateProcedure { .. } => Arc::new(Schema { columns: vec![] }),
            LogicalPlan::DropFunction { .. } => Arc::new(Schema { columns: vec![] }),
            LogicalPlan::DropProcedure { .. } => Arc::new(Schema { columns: vec![] }),
            LogicalPlan::Call { .. } => Arc::new(Schema { columns: vec![] }),
            // DualScan - empty schema (single row, no columns)
            // Used as input for SELECT without FROM, expressions are evaluated in Project
            LogicalPlan::DualScan => Arc::new(Schema { columns: vec![] }),

            // Regular Views
            LogicalPlan::CreateView { .. } => {
                // CreateView doesn't have output schema
                Arc::new(Schema { columns: vec![] })
            }
            LogicalPlan::DropView { .. } => {
                // DropView doesn't have output schema
                Arc::new(Schema { columns: vec![] })
            }

            // HA Operations
            #[cfg(feature = "ha-tier1")]
            LogicalPlan::Switchover { .. } => {
                // Returns status message
                Arc::new(Schema {
                    columns: vec![
                        Column::new("result", DataType::Text),
                    ],
                })
            }
            #[cfg(feature = "ha-tier1")]
            LogicalPlan::SwitchoverCheck { .. } => {
                // Returns check results
                Arc::new(Schema {
                    columns: vec![
                        Column::new("can_proceed", DataType::Boolean),
                        Column::new("target_healthy", DataType::Boolean),
                        Column::new("target_lsn", DataType::Int8),
                        Column::new("primary_lsn", DataType::Int8),
                        Column::new("lag_bytes", DataType::Int8),
                        Column::new("warnings", DataType::Text),
                        Column::new("blockers", DataType::Text),
                    ],
                })
            }
            #[cfg(feature = "ha-tier1")]
            LogicalPlan::ClusterStatus => {
                // Returns cluster status table
                Arc::new(Schema {
                    columns: vec![
                        Column::new("node_id", DataType::Text),
                        Column::new("role", DataType::Text),
                        Column::new("address", DataType::Text),
                        Column::new("is_healthy", DataType::Boolean),
                        Column::new("lsn", DataType::Int8),
                        Column::new("lag_ms", DataType::Int8),
                        Column::new("priority", DataType::Int4),
                    ],
                })
            }
            #[cfg(feature = "ha-tier1")]
            LogicalPlan::SetNodeAlias { .. } => {
                // Returns confirmation message
                Arc::new(Schema {
                    columns: vec![
                        Column::new("result", DataType::Text),
                    ],
                })
            }
            #[cfg(feature = "ha-tier1")]
            LogicalPlan::ShowTopology => {
                // Returns detailed topology table
                Arc::new(Schema {
                    columns: vec![
                        Column::new("node_id", DataType::Text),
                        Column::new("alias", DataType::Text),
                        Column::new("role", DataType::Text),
                        Column::new("client_addr", DataType::Text),
                        Column::new("replication_addr", DataType::Text),
                        Column::new("healthy", DataType::Boolean),
                        Column::new("health_msg", DataType::Text),
                        Column::new("last_seen_secs", DataType::Int8),
                        Column::new("lsn", DataType::Int8),
                        Column::new("lag_ms", DataType::Int8),
                        Column::new("priority", DataType::Int4),
                        Column::new("weight", DataType::Int4),
                    ],
                })
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn test_scan_schema() {
        let schema = Arc::new(Schema {
            columns: vec![
                crate::Column {
                    name: "id".to_string(),
                    data_type: DataType::Int4,
                    nullable: false,
                    primary_key: false,
                    source_table: None,
                    source_table_name: None,
                    default_expr: None,
                    unique: false,
                    storage_mode: Default::default(),
                },
                crate::Column {
                    name: "name".to_string(),
                    data_type: DataType::Text,
                    nullable: true,
                    primary_key: false,
                    source_table: None,
                    source_table_name: None,
                    default_expr: None,
                    unique: false,
                    storage_mode: Default::default(),
                },
            ],
        });

        let plan = LogicalPlan::Scan {
            table_name: "users".to_string(),
            alias: None,
            schema: schema.clone(),
            projection: None,
            as_of: None,
        };

        assert_eq!(plan.schema().columns.len(), 2);
    }

    #[test]
    fn test_scan_with_projection() {
        let schema = Arc::new(Schema {
            columns: vec![
                crate::Column {
                    name: "id".to_string(),
                    data_type: DataType::Int4,
                    nullable: false,
                    primary_key: false,
                    source_table: None,
                    source_table_name: None,
                    default_expr: None,
                    unique: false,
                    storage_mode: Default::default(),
                },
                crate::Column {
                    name: "name".to_string(),
                    data_type: DataType::Text,
                    nullable: true,
                    primary_key: false,
                    source_table: None,
                    source_table_name: None,
                    default_expr: None,
                    unique: false,
                    storage_mode: Default::default(),
                },
            ],
        });

        let plan = LogicalPlan::Scan {
            table_name: "users".to_string(),
            alias: None,
            schema: schema.clone(),
            projection: Some(vec![0]), // Only id column
            as_of: None,
        };

        assert_eq!(plan.schema().columns.len(), 1);
        assert_eq!(plan.schema().columns[0].name, "id");
    }

    #[test]
    fn test_scan_with_time_travel() {
        let schema = Arc::new(Schema {
            columns: vec![
                crate::Column {
                    name: "id".to_string(),
                    data_type: DataType::Int4,
                    nullable: false,
                    primary_key: false,
                    source_table: None,
                    source_table_name: None,
                    default_expr: None,
                    unique: false,
                    storage_mode: Default::default(),
                },
            ],
        });

        let plan = LogicalPlan::Scan {
            table_name: "orders".to_string(),
            alias: None,
            schema: schema.clone(),
            projection: None,
            as_of: Some(AsOfClause::Timestamp("2025-11-15 06:00:00".to_string())),
        };

        assert_eq!(plan.schema().columns.len(), 1);
    }
}
