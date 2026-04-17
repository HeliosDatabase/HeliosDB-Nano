//! SQL parsing and planning
//!
//! This module integrates sqlparser-rs for SQL parsing and provides
//! the logical plan structures.

pub mod parser;
pub mod logical_plan;
pub mod planner;
pub mod executor;
pub mod evaluator;
pub mod type_inference;
pub mod system_tables;
pub mod system_views;
pub mod settings;
pub mod triggers;
pub mod constraints;
pub mod procedural;
pub mod functions;
pub mod query_cache;
pub mod compiled; // RAG-native compiled query plan cache (idea 4)

// EXPLAIN modules (Week 7-8)
pub mod explain;
pub mod explain_options;
pub mod explain_storage;
pub mod explain_transaction;
pub mod explain_distributed;
pub mod explain_optimizer;
pub mod explain_interactive;
pub mod explain_api;
pub mod explain_production;
pub mod explain_integrations;
pub mod explain_advanced;
pub mod explain_webui;

// Phase 3: SQL Extensions for branching, time-travel, and MVs
pub mod phase3;

pub use parser::Parser;
pub use logical_plan::{LogicalPlan, LogicalExpr, AggregateFunction, BinaryOperator, UnaryOperator, JoinType, AsOfClause, TriggerCharacteristics, TriggerType};
pub use planner::Planner;
pub use executor::Executor;
pub use evaluator::Evaluator;
pub use type_inference::TypeInference;
pub use system_tables::{SessionRegistry, SessionInfo, SessionState, ProtocolType, SystemTables};
pub use system_views::{SystemViewRegistry, SystemView, ViewCategory};
pub use settings::{SessionSettings, SettingValue, parse_setting_value};
pub use triggers::{TriggerRegistry, TriggerDefinition, TriggerContext, TriggerPersistence, TriggerRowContext, TriggerAction, TransitionTables, DeferredTriggerTracker, DeferralMode, PendingTriggerExecution, MAX_TRIGGER_DEPTH};
pub use constraints::{
    ForeignKeyConstraint, ReferentialAction, ConstraintEnforcement,
    CheckConstraint, UniqueConstraint, TableConstraints, ConstraintRef,
    ForeignKeyValidator, DeferredConstraintTracker, PendingConstraintCheck,
    ConstraintOperation, LockFreeValidationQueue, LockFreeValidation,
};

// Phase 3 exports
pub use phase3::{
    BranchingParser,
    TimeTravelParser,
    MaterializedViewParser,
};

// Procedural language exports
pub use procedural::{
    ProceduralDialect, ProceduralBlock, ProceduralStatement, VariableDeclaration,
    ProceduralParser, ProceduralExecutor, ExecutionContext, VariableScope, Variable,
    RaiseLevel, FetchDirection, ExceptionHandler, ExceptionCondition,
    RoutineDefinition, RoutineParameter, ParameterMode, ReturnType, Volatility,
};

// Function registry exports
pub use functions::{FunctionRegistry, StoredFunction, StoredProcedure};

// Query cache exports
pub use query_cache::{QueryCache, CacheKey, CachedResult, CacheStats};

// EXPLAIN options and storage features exports
pub use explain_options::{ExplainOptions, ExplainFormatOption};
pub use explain_storage::{
    StorageFeatureReport, StorageFeatureCollector, ColumnStorageReport,
    StorageModeDetails, BloomFilterReport, BloomFilterEffectiveness,
    ZoneMapReport, ZoneMapEffectiveness, CompressionReport, IndexReport,
    StatisticsReport, ColumnStatisticsReport, ColumnarReport,
    format_storage_features_text,
};
