//! SQL parsing and planning
//!
//! This module integrates sqlparser-rs for SQL parsing and provides
//! the logical plan structures.

pub mod compiled; // RAG-native compiled query plan cache (idea 4)
pub mod constraints;
pub mod evaluator;
pub mod executor;
pub mod functions;
pub mod logical_plan;
pub(crate) mod normalize;
pub mod numeric_special; // PG NaN/±Infinity semantics for String-backed Value::Numeric
pub mod parser;
pub mod planner;
pub mod procedural;
pub mod query_cache;
pub mod sequences;
pub mod settings;
pub mod sqlite_compat;
pub mod system_tables;
pub mod system_views;
pub mod triggers;
pub mod type_inference; // Named-counter store for CREATE SEQUENCE / nextval / currval / setval

// EXPLAIN modules (Week 7-8)
pub mod explain;
pub mod explain_advanced;
pub mod explain_api;
pub mod explain_distributed;
pub mod explain_integrations;
pub mod explain_interactive;
pub mod explain_optimizer;
pub mod explain_options;
pub mod explain_production;
pub mod explain_storage;
pub mod explain_transaction;
pub mod explain_webui;

// Phase 3: SQL Extensions for branching, time-travel, and MVs
pub mod phase3;

pub use constraints::{
    CheckConstraint, ConstraintEnforcement, ConstraintOperation, ConstraintRef, DeferredConstraintTracker,
    ForeignKeyConstraint, ForeignKeyValidator, LockFreeValidation, LockFreeValidationQueue, PendingConstraintCheck,
    ReferentialAction, TableConstraints, UniqueConstraint,
};
pub use evaluator::Evaluator;
pub use executor::Executor;
pub use logical_plan::{
    AggregateFunction, AsOfClause, BinaryOperator, JoinType, LogicalExpr, LogicalPlan, TriggerCharacteristics,
    TriggerType, UnaryOperator,
};
pub use parser::Parser;
pub use planner::Planner;
pub use settings::{parse_setting_value, SessionSettings, SettingValue};
pub use system_tables::{ProtocolType, SessionInfo, SessionRegistry, SessionState, SystemTables};
pub use system_views::{SystemView, SystemViewRegistry, ViewCategory};
pub use triggers::{
    DeferralMode, DeferredTriggerTracker, PendingTriggerExecution, TransitionTables, TriggerAction, TriggerContext,
    TriggerDefinition, TriggerPersistence, TriggerRegistry, TriggerRowContext, MAX_TRIGGER_DEPTH,
};
pub use type_inference::TypeInference;

// Phase 3 exports
pub use phase3::{BranchingParser, MaterializedViewParser, TimeTravelParser};

// Procedural language exports
pub use procedural::{
    ExceptionCondition, ExceptionHandler, ExecutionContext, FetchDirection, ParameterMode, ProceduralBlock,
    ProceduralDialect, ProceduralExecutor, ProceduralParser, ProceduralStatement, RaiseLevel, ReturnType,
    RoutineDefinition, RoutineParameter, Variable, VariableDeclaration, VariableScope, Volatility,
};

// Function registry exports
pub use functions::{FunctionRegistry, StoredFunction, StoredProcedure};

// Query cache exports
pub use query_cache::{CacheKey, CacheStats, CachedResult, QueryCache};

// EXPLAIN options and storage features exports
pub use explain_options::{ExplainFormatOption, ExplainOptions};
pub use explain_storage::{
    format_storage_features_text, BloomFilterEffectiveness, BloomFilterReport, ColumnStatisticsReport,
    ColumnStorageReport, ColumnarReport, CompressionReport, IndexReport, StatisticsReport, StorageFeatureCollector,
    StorageFeatureReport, StorageModeDetails, ZoneMapEffectiveness, ZoneMapReport,
};
