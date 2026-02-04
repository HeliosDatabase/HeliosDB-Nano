//! Query executor adapter layer for protocol integration
//!
//! This module provides a trait-based adapter that bridges HeliosDB Full's
//! QueryExecutor interface to HeliosDB Lite's SQL executor.

use crate::{Error, Result, StorageEngine, Tuple};
use crate::sql::{Executor, LogicalPlan, Parser, Planner};
use std::sync::Arc;
use parking_lot::RwLock;

/// Query result set
///
/// Represents the result of executing a query, including the rows
/// and metadata about the execution.
#[derive(Debug, Clone)]
pub struct QueryResult {
    /// The result rows
    pub rows: Vec<Tuple>,
    /// Number of rows affected (for INSERT, UPDATE, DELETE)
    pub rows_affected: usize,
    /// Execution time in milliseconds
    pub execution_time_ms: Option<u64>,
}

impl QueryResult {
    /// Create a new query result
    pub fn new(rows: Vec<Tuple>) -> Self {
        let rows_affected = rows.len();
        Self {
            rows,
            rows_affected,
            execution_time_ms: None,
        }
    }

    /// Create a query result with rows affected
    pub fn with_affected(rows_affected: usize) -> Self {
        Self {
            rows: Vec::new(),
            rows_affected,
            execution_time_ms: None,
        }
    }

    /// Set execution time
    pub fn with_execution_time(mut self, time_ms: u64) -> Self {
        self.execution_time_ms = Some(time_ms);
        self
    }
}

/// Prepared statement
///
/// Represents a parsed and optimized SQL statement that can be executed
/// multiple times with different parameters.
pub struct PreparedStatement {
    /// The SQL text
    sql: String,
    /// The logical plan
    plan: LogicalPlan,
    /// Parameter count
    param_count: usize,
}

impl PreparedStatement {
    /// Create a new prepared statement
    pub fn new(sql: String, plan: LogicalPlan, param_count: usize) -> Self {
        Self {
            sql,
            plan,
            param_count,
        }
    }

    /// Get the SQL text
    pub fn sql(&self) -> &str {
        &self.sql
    }

    /// Get the logical plan
    pub fn plan(&self) -> &LogicalPlan {
        &self.plan
    }

    /// Get parameter count
    pub fn param_count(&self) -> usize {
        self.param_count
    }
}

/// Query executor adapter trait
///
/// Provides a unified interface for query execution that can be implemented
/// by different query engines.
pub trait QueryExecutorAdapter: Send + Sync {
    /// Execute a SQL query and return results
    ///
    /// # Arguments
    /// * `sql` - The SQL query to execute
    ///
    /// # Returns
    /// * `Ok(result)` with query results
    /// * `Err(error)` if an error occurs
    fn execute_query(&self, sql: &str) -> Result<QueryResult>;

    /// Prepare a SQL statement for later execution
    ///
    /// # Arguments
    /// * `sql` - The SQL statement to prepare
    ///
    /// # Returns
    /// * `Ok(statement)` with prepared statement
    /// * `Err(error)` if an error occurs
    fn prepare_statement(&self, sql: &str) -> Result<PreparedStatement>;

    /// Execute a prepared statement with parameters
    ///
    /// # Arguments
    /// * `statement` - The prepared statement
    /// * `params` - Parameter values
    ///
    /// # Returns
    /// * `Ok(result)` with query results
    /// * `Err(error)` if an error occurs
    fn execute_prepared(
        &self,
        statement: &PreparedStatement,
        params: &[Vec<u8>],
    ) -> Result<QueryResult>;

    /// Set query timeout in milliseconds
    ///
    /// # Arguments
    /// * `timeout_ms` - Timeout in milliseconds
    fn set_timeout(&self, timeout_ms: u64);

    /// Get current timeout setting
    fn get_timeout(&self) -> Option<u64>;
}

/// Implementation of QueryExecutorAdapter for HeliosDB Lite
pub struct LiteQueryExecutorAdapter {
    storage: Arc<StorageEngine>,
    timeout_ms: Arc<RwLock<Option<u64>>>,
}

impl LiteQueryExecutorAdapter {
    /// Create a new adapter wrapping the given storage engine
    pub fn new(storage: Arc<StorageEngine>) -> Self {
        Self {
            storage,
            timeout_ms: Arc::new(RwLock::new(None)),
        }
    }

    /// Get a reference to the underlying storage engine
    pub fn storage(&self) -> &Arc<StorageEngine> {
        &self.storage
    }
}

impl QueryExecutorAdapter for LiteQueryExecutorAdapter {
    fn execute_query(&self, sql: &str) -> Result<QueryResult> {
        let start = std::time::Instant::now();

        // Parse SQL
        let parser = Parser::new();
        let statements = parser.parse(sql)?;

        if statements.is_empty() {
            return Err(Error::sql_parse("Empty query"));
        }

        // Create planner and convert to logical plan
        let planner = Planner::new();
        let plan = planner.statement_to_plan(statements[0].clone())?;

        // Check if this is a DDL/DML statement that needs special handling
        let rows = match &plan {
            LogicalPlan::CreateTable { name, columns, if_not_exists, .. } => {
                // Convert columns to Schema
                let schema_columns: Vec<crate::types::Column> = columns.iter().map(|col| {
                    crate::types::Column {
                        name: col.name.clone(),
                        data_type: col.data_type.clone(),
                        nullable: !col.not_null,
                        primary_key: col.primary_key,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: col.storage_mode,
                    }
                }).collect();

                let schema = crate::types::Schema::new(schema_columns);

                // Get catalog
                let catalog = self.storage.catalog();

                // Create table in storage
                if *if_not_exists && catalog.table_exists(name)? {
                    // Table already exists, return empty result
                    vec![]
                } else {
                    catalog.create_table(name, schema)?;
                    vec![]
                }
            }
            LogicalPlan::Insert { table_name, values, .. } => {
                // Convert literal expressions to values and insert
                use crate::sql::logical_plan::LogicalExpr;

                for row_exprs in values {
                    let mut tuple_values = Vec::new();
                    for expr in row_exprs {
                        // Extract literal value
                        match expr {
                            LogicalExpr::Literal(val) => {
                                tuple_values.push(val.clone());
                            }
                            _ => {
                                return Err(Error::query_execution(
                                    "Only literal values are supported in INSERT via adapter"
                                ));
                            }
                        }
                    }
                    let tuple = Tuple::new(tuple_values);

                    // Insert tuple
                    self.storage.insert_tuple(table_name, tuple)?;
                }
                vec![]
            }
            _ => {
                // Regular query execution through executor
                let mut executor = Executor::with_storage(&self.storage);

                // Set timeout if configured
                if let Some(timeout) = *self.timeout_ms.read() {
                    executor = executor.with_timeout(Some(timeout));
                }

                executor.execute(&plan)?
            }
        };

        let execution_time = start.elapsed().as_millis() as u64;

        Ok(QueryResult::new(rows).with_execution_time(execution_time))
    }

    fn prepare_statement(&self, sql: &str) -> Result<PreparedStatement> {
        // Parse SQL
        let parser = Parser::new();
        let statements = parser.parse(sql)?;

        if statements.is_empty() {
            return Err(Error::sql_parse("Empty query"));
        }

        // Create planner and convert to logical plan
        let planner = Planner::new();
        let plan = planner.statement_to_plan(statements[0].clone())?;

        // Count parameters (simple implementation - could be enhanced)
        let param_count = sql.matches('$').count();

        Ok(PreparedStatement::new(sql.to_string(), plan, param_count))
    }

    fn execute_prepared(
        &self,
        statement: &PreparedStatement,
        _params: &[Vec<u8>],
    ) -> Result<QueryResult> {
        let start = std::time::Instant::now();

        // For now, we execute the plan directly
        // In a full implementation, we would bind parameters to the plan
        let mut executor = Executor::with_storage(&self.storage);

        // Set timeout if configured
        if let Some(timeout) = *self.timeout_ms.read() {
            executor = executor.with_timeout(Some(timeout));
        }

        // Execute prepared plan
        let rows = executor.execute(statement.plan())?;

        let execution_time = start.elapsed().as_millis() as u64;

        Ok(QueryResult::new(rows).with_execution_time(execution_time))
    }

    fn set_timeout(&self, timeout_ms: u64) {
        *self.timeout_ms.write() = Some(timeout_ms);
    }

    fn get_timeout(&self) -> Option<u64> {
        *self.timeout_ms.read()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::Config;

    #[test]
    fn test_executor_adapter_create_table() -> Result<()> {
        let config = Config::in_memory();
        let storage = Arc::new(StorageEngine::open_in_memory(&config)?);
        let adapter = LiteQueryExecutorAdapter::new(storage);

        let sql = "CREATE TABLE users (id INT, name TEXT)";
        let result = adapter.execute_query(sql);
        assert!(result.is_ok());
        Ok(())
    }

    #[test]
    fn test_executor_adapter_insert_select() -> Result<()> {
        let config = Config::in_memory();
        let storage = Arc::new(StorageEngine::open_in_memory(&config)?);
        let adapter = LiteQueryExecutorAdapter::new(storage);

        // Create table
        adapter.execute_query("CREATE TABLE users (id INT, name TEXT)")?;

        // Insert data
        adapter.execute_query("INSERT INTO users VALUES (1, 'Alice')")?;
        adapter.execute_query("INSERT INTO users VALUES (2, 'Bob')")?;

        // Select data
        let result = adapter.execute_query("SELECT * FROM users")?;
        assert_eq!(result.rows.len(), 2);
        Ok(())
    }

    #[test]
    fn test_executor_adapter_prepare() -> Result<()> {
        let config = Config::in_memory();
        let storage = Arc::new(StorageEngine::open_in_memory(&config)?);
        let adapter = LiteQueryExecutorAdapter::new(storage);

        // Create table
        adapter.execute_query("CREATE TABLE users (id INT, name TEXT)")?;

        // Prepare statement
        let sql = "SELECT * FROM users WHERE id = $1";
        let stmt = adapter.prepare_statement(sql)?;
        assert_eq!(stmt.param_count(), 1);
        Ok(())
    }

    #[test]
    fn test_executor_adapter_timeout() -> Result<()> {
        let config = Config::in_memory();
        let storage = Arc::new(StorageEngine::open_in_memory(&config)?);
        let adapter = LiteQueryExecutorAdapter::new(storage);

        // Set timeout
        adapter.set_timeout(5000);
        assert_eq!(adapter.get_timeout(), Some(5000));

        // Create table with timeout set
        let result = adapter.execute_query("CREATE TABLE users (id INT, name TEXT)");
        assert!(result.is_ok());
        Ok(())
    }

    #[test]
    fn test_query_result_execution_time() {
        let rows = vec![];
        let result = QueryResult::new(rows).with_execution_time(42);
        assert_eq!(result.execution_time_ms, Some(42));
    }
}
