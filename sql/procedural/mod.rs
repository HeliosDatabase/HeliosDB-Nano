//! Procedural Language Support
//!
//! This module provides support for procedural SQL dialects including:
//! - PL/pgSQL (PostgreSQL)
//! - T-SQL (Microsoft SQL Server)
//! - PL/SQL (Oracle)
//! - DB2 SQL PL (IBM DB2)
//!
//! The implementation uses a unified AST representation that all dialects
//! are translated to, enabling cross-dialect compatibility and execution.

pub mod ast;
pub mod parser;
pub mod runtime;

pub use ast::{
    ExceptionCondition, ExceptionHandler, FetchDirection, ParameterMode, ProceduralBlock,
    ProceduralDialect, ProceduralStatement, RaiseLevel, ReturnType, RoutineDefinition,
    RoutineParameter, VariableDeclaration, Volatility,
};
pub use parser::ProceduralParser;
pub use runtime::{ExecutionContext, ProceduralExecutor, Variable, VariableScope};
