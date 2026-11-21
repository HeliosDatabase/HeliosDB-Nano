//! Procedural Language AST
//!
//! Unified AST for procedural SQL dialects (PL/pgSQL, T-SQL, PL/SQL, DB2 PL).
//! This module provides a common representation that all dialects can be translated to.

use crate::sql::LogicalExpr;
use crate::DataType;
use serde::{Deserialize, Serialize};

/// A procedural expression (a condition, an assigned value, a loop bound, a
/// DECLARE default, a RETURN value), kept as the SQL text the routine was
/// written with.
///
/// It is evaluated at run time as `SELECT <text>`, after the `$`-sigil
/// interpolation every body statement already goes through (`$name` from the
/// variable scope, `$N` from the call's arguments). The parser used to wrap
/// this text in a string literal that the runtime then took at face value, so
/// every IF condition was false and `x := 1 + 1` stored the text `1 + 1`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlExpr {
    /// The expression as written (trimmed).
    pub text: String,
}

/// The expression text `GET DIAGNOSTICS target = ROW_COUNT` assigns from:
/// the row count of the routine's last statement. Not SQL: no body can write it.
pub const ROW_COUNT_EXPR: &str = "\u{1}row_count";

impl PlExpr {
    /// An expression with the given SQL text.
    pub fn new(text: impl Into<String>) -> Self {
        Self { text: text.into() }
    }
}

/// Procedural SQL dialect
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProceduralDialect {
    /// PostgreSQL PL/pgSQL
    PlPgSql,
    /// Microsoft T-SQL
    TSql,
    /// Oracle PL/SQL
    PlSql,
    /// IBM DB2 SQL PL
    Db2Pl,
    /// Auto-detected (based on syntax patterns)
    Auto,
}

impl Default for ProceduralDialect {
    fn default() -> Self {
        ProceduralDialect::Auto
    }
}

impl std::fmt::Display for ProceduralDialect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProceduralDialect::PlPgSql => write!(f, "PL/pgSQL"),
            ProceduralDialect::TSql => write!(f, "T-SQL"),
            ProceduralDialect::PlSql => write!(f, "PL/SQL"),
            ProceduralDialect::Db2Pl => write!(f, "DB2 SQL PL"),
            ProceduralDialect::Auto => write!(f, "Auto"),
        }
    }
}

/// A procedural code block (BEGIN...END)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProceduralBlock {
    /// Optional label for the block
    pub label: Option<String>,
    /// Variable declarations (DECLARE section)
    pub declarations: Vec<VariableDeclaration>,
    /// Statements in the block
    pub statements: Vec<ProceduralStatement>,
    /// Exception handlers
    pub exception_handlers: Vec<ExceptionHandler>,
}

impl ProceduralBlock {
    pub fn new() -> Self {
        Self {
            label: None,
            declarations: Vec::new(),
            statements: Vec::new(),
            exception_handlers: Vec::new(),
        }
    }
}

impl Default for ProceduralBlock {
    fn default() -> Self {
        Self::new()
    }
}

/// Variable declaration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VariableDeclaration {
    /// Variable name
    pub name: String,
    /// Data type (optional for some dialects)
    pub data_type: Option<DataType>,
    /// Default/initial value
    pub default: Option<PlExpr>,
    /// Whether this is a constant
    pub is_constant: bool,
    /// NOT NULL constraint
    pub not_null: bool,
}

/// Procedural statement
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ProceduralStatement {
    /// Variable assignment (SET, :=, =)
    Assignment { target: String, value: PlExpr },

    /// IF statement
    If {
        condition: PlExpr,
        then_block: Vec<ProceduralStatement>,
        elsif_branches: Vec<(PlExpr, Vec<ProceduralStatement>)>,
        else_block: Option<Vec<ProceduralStatement>>,
    },

    /// CASE statement (searched)
    Case {
        when_branches: Vec<(PlExpr, Vec<ProceduralStatement>)>,
        else_block: Option<Vec<ProceduralStatement>>,
    },

    /// CASE statement (simple)
    SimpleCase {
        operand: PlExpr,
        when_branches: Vec<(PlExpr, Vec<ProceduralStatement>)>,
        else_block: Option<Vec<ProceduralStatement>>,
    },

    /// LOOP statement (infinite)
    Loop {
        label: Option<String>,
        body: Vec<ProceduralStatement>,
    },

    /// WHILE loop
    While {
        label: Option<String>,
        condition: PlExpr,
        body: Vec<ProceduralStatement>,
    },

    /// FOR loop (numeric range)
    ForNumeric {
        label: Option<String>,
        variable: String,
        lower_bound: PlExpr,
        upper_bound: PlExpr,
        step: Option<PlExpr>,
        reverse: bool,
        body: Vec<ProceduralStatement>,
    },

    /// FOR loop (cursor/query result)
    ForQuery {
        label: Option<String>,
        record_variable: String,
        query: String,
        body: Vec<ProceduralStatement>,
    },

    /// EXIT (break out of loop)
    Exit {
        label: Option<String>,
        when_condition: Option<PlExpr>,
    },

    /// CONTINUE (next iteration)
    Continue {
        label: Option<String>,
        when_condition: Option<PlExpr>,
    },

    /// RETURN statement
    Return { value: Option<PlExpr> },

    /// RETURN NEXT (for set-returning functions)
    ReturnNext { value: LogicalExpr },

    /// RETURN QUERY (for set-returning functions)
    ReturnQuery { query: String },

    /// `RAISE [level] 'format', args… [USING option = expr, …]`,
    /// `RAISE [level] condition_name | SQLSTATE 'code' [USING …]`, or a bare
    /// `RAISE` (re-raise the error being handled).
    Raise {
        level: RaiseLevel,
        /// The SQLSTATE given by a condition name or `SQLSTATE 'code'`.
        condition: Option<String>,
        /// The format string; each `%` takes the next argument.
        format: Option<String>,
        args: Vec<PlExpr>,
        /// `USING` options by upper-case name: MESSAGE, DETAIL, HINT, ERRCODE.
        options: Vec<(String, PlExpr)>,
    },

    /// Execute SQL statement
    Execute { sql: String, into_variables: Vec<String> },

    /// `EXECUTE command [INTO [STRICT] targets] [USING values]`: the command
    /// is computed, then run with the USING values bound as `$1`, `$2`, ….
    ExecuteDynamic {
        sql_expression: PlExpr,
        into_variables: Vec<String>,
        using_parameters: Vec<PlExpr>,
        strict: bool,
    },

    /// Nested block
    Block(ProceduralBlock),

    /// NULL statement (no-op)
    Null,

    /// PRINT (T-SQL)
    Print { message: LogicalExpr },

    /// SELECT INTO (fetch into variables)
    SelectInto {
        query: String,
        variables: Vec<String>,
        /// `INTO STRICT`: exactly one row, or an error.
        strict: bool,
    },

    /// OPEN cursor
    OpenCursor { cursor_name: String, query: Option<String> },

    /// FETCH from cursor
    FetchCursor {
        cursor_name: String,
        into_variables: Vec<String>,
        direction: FetchDirection,
    },

    /// CLOSE cursor
    CloseCursor { cursor_name: String },

    /// CALL procedure
    Call {
        procedure_name: String,
        arguments: Vec<LogicalExpr>,
    },
}

/// RAISE severity level
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RaiseLevel {
    Debug,
    Log,
    Info,
    Notice,
    Warning,
    Exception,
}

impl Default for RaiseLevel {
    fn default() -> Self {
        RaiseLevel::Exception
    }
}

/// Cursor fetch direction
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FetchDirection {
    Next,
    Prior,
    First,
    Last,
    Absolute(i64),
    Relative(i64),
}

impl Default for FetchDirection {
    fn default() -> Self {
        FetchDirection::Next
    }
}

/// Exception handler
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExceptionHandler {
    /// Conditions to catch (e.g., "no_data_found", "others", "SQLSTATE '23505'")
    pub conditions: Vec<ExceptionCondition>,
    /// Handler body
    pub body: Vec<ProceduralStatement>,
}

/// Exception condition
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ExceptionCondition {
    /// Named condition (e.g., "no_data_found", "division_by_zero")
    Named(String),
    /// SQLSTATE code
    SqlState(String),
    /// OTHERS (catch-all)
    Others,
}

/// Function/Procedure definition
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutineDefinition {
    /// Name of the routine
    pub name: String,
    /// Schema/owner (optional)
    pub schema: Option<String>,
    /// Parameters
    pub parameters: Vec<RoutineParameter>,
    /// Return type (None for procedures)
    pub return_type: Option<ReturnType>,
    /// Language (PLPGSQL, SQL, etc.)
    pub language: String,
    /// Volatility (VOLATILE, STABLE, IMMUTABLE)
    pub volatility: Volatility,
    /// Whether it's a SECURITY DEFINER
    pub security_definer: bool,
    /// Procedure/Function body
    pub body: ProceduralBlock,
    /// Original dialect
    pub dialect: ProceduralDialect,
}

/// Routine parameter
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutineParameter {
    /// Parameter name
    pub name: String,
    /// Data type
    pub data_type: DataType,
    /// Parameter mode (IN, OUT, INOUT)
    pub mode: ParameterMode,
    /// Default value
    pub default: Option<LogicalExpr>,
}

/// Parameter mode
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ParameterMode {
    #[default]
    In,
    Out,
    InOut,
}

/// Return type for functions
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ReturnType {
    /// Single value
    Scalar(DataType),
    /// TABLE (set of rows)
    Table { columns: Vec<(String, DataType)> },
    /// SETOF (set of single type)
    SetOf(DataType),
    /// VOID
    Void,
}

/// Function volatility
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Volatility {
    #[default]
    Volatile,
    Stable,
    Immutable,
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn test_procedural_block() {
        let mut block = ProceduralBlock::new();
        block.label = Some("outer_block".to_string());
        block.declarations.push(VariableDeclaration {
            name: "counter".to_string(),
            data_type: Some(DataType::Int4),
            default: None,
            is_constant: false,
            not_null: false,
        });
        block.statements.push(ProceduralStatement::Null);

        assert_eq!(block.label, Some("outer_block".to_string()));
        assert_eq!(block.declarations.len(), 1);
        assert_eq!(block.statements.len(), 1);
    }

    #[test]
    fn test_dialect_display() {
        assert_eq!(ProceduralDialect::PlPgSql.to_string(), "PL/pgSQL");
        assert_eq!(ProceduralDialect::TSql.to_string(), "T-SQL");
        assert_eq!(ProceduralDialect::PlSql.to_string(), "PL/SQL");
        assert_eq!(ProceduralDialect::Db2Pl.to_string(), "DB2 SQL PL");
    }

    #[test]
    fn test_variable_declaration() {
        let decl = VariableDeclaration {
            name: "my_var".to_string(),
            data_type: Some(DataType::Text),
            default: None,
            is_constant: true,
            not_null: true,
        };

        assert!(decl.is_constant);
        assert!(decl.not_null);
    }
}
