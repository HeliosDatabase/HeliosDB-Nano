//! Prepared statement and portal management for extended query protocol
//!
//! This module implements storage and execution of prepared statements and portals
//! for the PostgreSQL extended query protocol.

use crate::{Result, Error, Value, Schema, Tuple};
use crate::sql::logical_plan::LogicalPlan;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

/// Prepared statement with optional cached plan
#[derive(Debug, Clone)]
pub struct PreparedStatement {
    /// Statement name (empty string for unnamed)
    pub name: String,
    /// Original SQL query text
    pub query: String,
    /// Parameter type OIDs (0 = infer)
    pub param_types: Vec<i32>,
    /// Result schema (if available)
    pub result_schema: Option<Schema>,
    /// Cached logical plan (avoids re-parsing on each Execute)
    /// The plan contains Parameter nodes that are resolved at execution time
    pub cached_plan: Option<LogicalPlan>,
}

/// Portal (bound statement ready for execution)
#[derive(Debug, Clone)]
pub struct Portal {
    /// Portal name (empty string for unnamed)
    pub name: String,
    /// Statement name this portal is bound to
    pub statement_name: String,
    /// Bound parameter values
    pub params: Vec<Option<Vec<u8>>>,
    /// Parameter formats (0 = text, 1 = binary)
    pub param_formats: Vec<i16>,
    /// Result column formats (0 = text, 1 = binary)
    pub result_formats: Vec<i16>,
    /// Current execution state
    pub state: PortalState,
}

/// Portal execution state
#[derive(Debug, Clone, PartialEq)]
pub enum PortalState {
    /// Portal created, not executed
    Ready,
    /// Portal executing with suspended state
    Suspended {
        /// Number of rows already returned
        rows_returned: usize,
        /// Cached results (if available)
        cached_results: Option<Vec<Tuple>>,
    },
    /// Portal execution complete
    Complete,
}

/// Prepared statement and portal manager
pub struct PreparedStatementManager {
    /// Prepared statements (name → statement)
    statements: Arc<RwLock<HashMap<String, PreparedStatement>>>,
    /// Portals (name → portal)
    portals: Arc<RwLock<HashMap<String, Portal>>>,
    /// Maximum number of statements to cache
    max_statements: usize,
    /// Maximum number of portals
    max_portals: usize,
    /// Statement creation order for LRU eviction
    statement_order: Arc<RwLock<Vec<String>>>,
}

impl PreparedStatementManager {
    /// Create a new prepared statement manager
    pub fn new() -> Self {
        Self::with_capacity(1000, 500) // Increased limits
    }

    /// Create manager with custom capacity limits
    pub fn with_capacity(max_statements: usize, max_portals: usize) -> Self {
        Self {
            statements: Arc::new(RwLock::new(HashMap::new())),
            portals: Arc::new(RwLock::new(HashMap::new())),
            max_statements,
            max_portals,
            statement_order: Arc::new(RwLock::new(Vec::new())),
        }
    }

    /// Store a prepared statement with LRU eviction
    pub fn store_statement(&self, statement: PreparedStatement) -> Result<()> {
        use crate::error::LockResultExt;
        let mut statements = self.statements.write()
            .map_lock_err("Failed to acquire write lock on statements")?;
        let mut order = self.statement_order.write()
            .map_lock_err("Failed to acquire write lock on statement order")?;

        let is_new = !statements.contains_key(&statement.name);

        // If at capacity and this is a new statement, evict oldest
        if is_new && statements.len() >= self.max_statements {
            // Evict oldest non-unnamed statements first (keep unnamed)
            let mut evicted = false;
            for name in order.iter() {
                if !name.is_empty() && statements.contains_key(name) {
                    statements.remove(name);
                    tracing::debug!("Evicted prepared statement '{}' due to capacity limit", name);
                    evicted = true;
                    break;
                }
            }
            // If no named statements to evict, evict unnamed as last resort
            if !evicted && statements.len() >= self.max_statements {
                if let Some(first) = order.first().cloned() {
                    statements.remove(&first);
                    tracing::debug!("Evicted prepared statement '{}' due to capacity limit", first);
                }
            }
            // Clean up order list
            order.retain(|n| statements.contains_key(n));
        }

        // Remove from order if replacing existing
        if !is_new {
            order.retain(|n| n != &statement.name);
        }

        // Add to order (most recent at end)
        order.push(statement.name.clone());
        statements.insert(statement.name.clone(), statement);
        Ok(())
    }

    /// Get a prepared statement
    pub fn get_statement(&self, name: &str) -> Result<Option<PreparedStatement>> {
        use crate::error::LockResultExt;
        let statements = self.statements.read()
            .map_lock_err("Failed to acquire read lock on statements")?;
        Ok(statements.get(name).cloned())
    }

    /// Remove a prepared statement
    pub fn remove_statement(&self, name: &str) -> Result<bool> {
        use crate::error::LockResultExt;
        let mut statements = self.statements.write()
            .map_lock_err("Failed to acquire write lock on statements")?;
        let mut order = self.statement_order.write()
            .map_lock_err("Failed to acquire write lock on statement order")?;

        let removed = statements.remove(name).is_some();
        if removed {
            order.retain(|n| n != name);
        }
        Ok(removed)
    }

    /// Store a portal
    pub fn store_portal(&self, portal: Portal) -> Result<()> {
        use crate::error::LockResultExt;
        let mut portals = self.portals.write()
            .map_lock_err("Failed to acquire write lock on portals")?;

        // Check capacity
        if portals.len() >= self.max_portals && !portals.contains_key(&portal.name) {
            return Err(Error::resource_limit(format!(
                "Maximum number of portals ({}) reached",
                self.max_portals
            )));
        }

        portals.insert(portal.name.clone(), portal);
        Ok(())
    }

    /// Get a portal
    pub fn get_portal(&self, name: &str) -> Result<Option<Portal>> {
        use crate::error::LockResultExt;
        let portals = self.portals.read()
            .map_lock_err("Failed to acquire read lock on portals")?;
        Ok(portals.get(name).cloned())
    }

    /// Update portal state
    pub fn update_portal_state(&self, name: &str, state: PortalState) -> Result<()> {
        use crate::error::LockResultExt;
        let mut portals = self.portals.write()
            .map_lock_err("Failed to acquire write lock on portals")?;

        if let Some(portal) = portals.get_mut(name) {
            portal.state = state;
            Ok(())
        } else {
            Err(Error::query_execution(format!("Portal '{}' not found", name)))
        }
    }

    /// Remove a portal
    pub fn remove_portal(&self, name: &str) -> Result<bool> {
        use crate::error::LockResultExt;
        let mut portals = self.portals.write()
            .map_lock_err("Failed to acquire write lock on portals")?;
        Ok(portals.remove(name).is_some())
    }

    /// Clear all statements and portals
    pub fn clear_all(&self) -> Result<()> {
        use crate::error::LockResultExt;
        let mut statements = self.statements.write()
            .map_lock_err("Failed to acquire write lock on statements")?;
        let mut portals = self.portals.write()
            .map_lock_err("Failed to acquire write lock on portals")?;
        let mut order = self.statement_order.write()
            .map_lock_err("Failed to acquire write lock on statement order")?;

        statements.clear();
        portals.clear();
        order.clear();
        Ok(())
    }

    /// Get statement count
    pub fn statement_count(&self) -> Result<usize> {
        use crate::error::LockResultExt;
        let statements = self.statements.read()
            .map_lock_err("Failed to acquire read lock on statements")?;
        Ok(statements.len())
    }

    /// Get portal count
    pub fn portal_count(&self) -> Result<usize> {
        use crate::error::LockResultExt;
        let portals = self.portals.read()
            .map_lock_err("Failed to acquire read lock on portals")?;
        Ok(portals.len())
    }
}

impl Default for PreparedStatementManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Convert PostgreSQL wire format parameter to HeliosDB Value
pub fn decode_parameter(
    data: &[u8],
    format: i16,
    type_oid: i32,
) -> Result<Value> {
    // format: 0 = text, 1 = binary
    if format == 0 {
        // Text format
        decode_text_parameter(data, type_oid)
    } else {
        // Binary format
        decode_binary_parameter(data, type_oid)
    }
}

/// Decode text format parameter
fn decode_text_parameter(data: &[u8], type_oid: i32) -> Result<Value> {
    let text = std::str::from_utf8(data)
        .map_err(|e| Error::protocol(format!("Invalid UTF-8 in parameter: {}", e)))?;

    match type_oid {
        16 => {
            // Boolean
            let val = text == "t" || text == "true" || text == "1";
            Ok(Value::Boolean(val))
        }
        21 => {
            // Int2
            let val = text.parse::<i16>()
                .map_err(|e| Error::protocol(format!("Invalid Int2 parameter: {}", e)))?;
            Ok(Value::Int2(val))
        }
        23 => {
            // Int4
            let val = text.parse::<i32>()
                .map_err(|e| Error::protocol(format!("Invalid Int4 parameter: {}", e)))?;
            Ok(Value::Int4(val))
        }
        20 => {
            // Int8
            let val = text.parse::<i64>()
                .map_err(|e| Error::protocol(format!("Invalid Int8 parameter: {}", e)))?;
            Ok(Value::Int8(val))
        }
        700 => {
            // Float4
            let val = text.parse::<f32>()
                .map_err(|e| Error::protocol(format!("Invalid Float4 parameter: {}", e)))?;
            Ok(Value::Float4(val))
        }
        701 => {
            // Float8
            let val = text.parse::<f64>()
                .map_err(|e| Error::protocol(format!("Invalid Float8 parameter: {}", e)))?;
            Ok(Value::Float8(val))
        }
        25 | 1043 => {
            // Text, Varchar
            Ok(Value::String(text.to_string()))
        }
        114 | 3802 => {
            // Json, Jsonb
            let _json: serde_json::Value = serde_json::from_str(text)
                .map_err(|e| Error::protocol(format!("Invalid JSON parameter: {}", e)))?;
            // Value::Json stores the JSON as a String for bincode compatibility
            Ok(Value::Json(text.to_string()))
        }
        _ => {
            // Unknown type - treat as text
            Ok(Value::String(text.to_string()))
        }
    }
}

/// Decode binary format parameter
// SAFETY: All array accesses below are guarded by length checks immediately before use.
#[allow(clippy::indexing_slicing)]
fn decode_binary_parameter(data: &[u8], type_oid: i32) -> Result<Value> {
    match type_oid {
        16 => {
            // Boolean (1 byte)
            if data.is_empty() {
                return Err(Error::protocol("Empty boolean parameter"));
            }
            Ok(Value::Boolean(data[0] != 0))
        }
        21 => {
            // Int2 (2 bytes, big-endian)
            if data.len() < 2 {
                return Err(Error::protocol("Invalid Int2 parameter length"));
            }
            let val = i16::from_be_bytes([data[0], data[1]]);
            Ok(Value::Int2(val))
        }
        23 => {
            // Int4 (4 bytes, big-endian)
            if data.len() < 4 {
                return Err(Error::protocol("Invalid Int4 parameter length"));
            }
            let val = i32::from_be_bytes([data[0], data[1], data[2], data[3]]);
            Ok(Value::Int4(val))
        }
        20 => {
            // Int8 (8 bytes, big-endian)
            if data.len() < 8 {
                return Err(Error::protocol("Invalid Int8 parameter length"));
            }
            let bytes: [u8; 8] = data[0..8].try_into()
                .map_err(|_| Error::protocol("Invalid Int8 parameter"))?;
            let val = i64::from_be_bytes(bytes);
            Ok(Value::Int8(val))
        }
        700 => {
            // Float4 (4 bytes, big-endian)
            if data.len() < 4 {
                return Err(Error::protocol("Invalid Float4 parameter length"));
            }
            let bytes: [u8; 4] = data[0..4].try_into()
                .map_err(|_| Error::protocol("Invalid Float4 parameter"))?;
            let val = f32::from_be_bytes(bytes);
            Ok(Value::Float4(val))
        }
        701 => {
            // Float8 (8 bytes, big-endian)
            if data.len() < 8 {
                return Err(Error::protocol("Invalid Float8 parameter length"));
            }
            let bytes: [u8; 8] = data[0..8].try_into()
                .map_err(|_| Error::protocol("Invalid Float8 parameter"))?;
            let val = f64::from_be_bytes(bytes);
            Ok(Value::Float8(val))
        }
        25 | 1043 => {
            // Text, Varchar (variable length, UTF-8)
            let text = std::str::from_utf8(data)
                .map_err(|e| Error::protocol(format!("Invalid UTF-8 in text parameter: {}", e)))?;
            Ok(Value::String(text.to_string()))
        }
        _ => {
            // Unknown type - store as bytes
            Ok(Value::Bytes(data.to_vec()))
        }
    }
}

/// Substitute parameters in SQL query
pub fn substitute_parameters(sql: &str, params: &[Value]) -> Result<String> {
    let mut result = sql.to_string();

    // Replace $1, $2, $3, etc. with actual values
    // Note: This is a simple implementation. Production should use proper SQL parsing.
    for (i, param) in params.iter().enumerate() {
        let placeholder = format!("${}", i + 1);
        let value_str = value_to_sql_literal(param);
        result = result.replace(&placeholder, &value_str);
    }

    Ok(result)
}

/// Convert Value to SQL literal string
fn value_to_sql_literal(value: &Value) -> String {
    match value {
        Value::Null => "NULL".to_string(),
        Value::Boolean(b) => if *b { "TRUE" } else { "FALSE" }.to_string(),
        Value::Int2(i) => i.to_string(),
        Value::Int4(i) => i.to_string(),
        Value::Int8(i) => i.to_string(),
        Value::Float4(f) => f.to_string(),
        Value::Float8(f) => f.to_string(),
        Value::String(s) => format!("'{}'", s.replace('\'', "''")),
        Value::Json(j) => format!("'{}'::jsonb", j.to_string().replace('\'', "''")),
        Value::Timestamp(ts) => format!("'{}'::timestamp", ts.to_rfc3339()),
        Value::Vector(v) => {
            let arr_str = v.iter()
                .map(|x| x.to_string())
                .collect::<Vec<_>>()
                .join(",");
            format!("ARRAY[{}]", arr_str)
        }
        _ => format!("'{}'", value.to_string().replace('\'', "''")),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn test_statement_manager() {
        let manager = PreparedStatementManager::new();

        let stmt = PreparedStatement {
            name: "test_stmt".to_string(),
            query: "SELECT * FROM users WHERE id = $1".to_string(),
            param_types: vec![23], // INT4
            result_schema: None,
            cached_plan: None,
        };

        manager.store_statement(stmt.clone()).unwrap();
        assert_eq!(manager.statement_count().unwrap(), 1);

        let retrieved = manager.get_statement("test_stmt").unwrap();
        assert!(retrieved.is_some());
        assert_eq!(retrieved.as_ref().unwrap().query, stmt.query);

        manager.remove_statement("test_stmt").unwrap();
        assert_eq!(manager.statement_count().unwrap(), 0);
    }

    #[test]
    fn test_portal_manager() {
        let manager = PreparedStatementManager::new();

        let portal = Portal {
            name: "test_portal".to_string(),
            statement_name: "test_stmt".to_string(),
            params: vec![Some(b"123".to_vec())],
            param_formats: vec![0],
            result_formats: vec![0],
            state: PortalState::Ready,
        };

        manager.store_portal(portal.clone()).unwrap();
        assert_eq!(manager.portal_count().unwrap(), 1);

        let retrieved = manager.get_portal("test_portal").unwrap();
        assert!(retrieved.is_some());
        assert_eq!(retrieved.as_ref().unwrap().statement_name, portal.statement_name);
    }

    #[test]
    fn test_decode_text_parameter() {
        // Int4
        let val = decode_text_parameter(b"123", 23).unwrap();
        assert_eq!(val, Value::Int4(123));

        // Text
        let val = decode_text_parameter(b"hello", 25).unwrap();
        assert_eq!(val, Value::String("hello".to_string()));

        // Boolean
        let val = decode_text_parameter(b"t", 16).unwrap();
        assert_eq!(val, Value::Boolean(true));
    }

    #[test]
    fn test_decode_binary_parameter() {
        // Int4
        let data = 123i32.to_be_bytes();
        let val = decode_binary_parameter(&data, 23).unwrap();
        assert_eq!(val, Value::Int4(123));

        // Boolean
        let val = decode_binary_parameter(&[1], 16).unwrap();
        assert_eq!(val, Value::Boolean(true));
    }

    #[test]
    fn test_substitute_parameters() {
        let sql = "SELECT * FROM users WHERE id = $1 AND name = $2";
        let params = vec![
            Value::Int4(123),
            Value::String("Alice".to_string()),
        ];

        let result = substitute_parameters(sql, &params).unwrap();
        assert_eq!(result, "SELECT * FROM users WHERE id = 123 AND name = 'Alice'");
    }

    #[test]
    fn test_capacity_limits() {
        let manager = PreparedStatementManager::with_capacity(2, 2);

        // Add statements up to limit
        for i in 0..2 {
            let stmt = PreparedStatement {
                name: format!("stmt{}", i),
                query: "SELECT 1".to_string(),
                param_types: vec![],
                result_schema: None,
                cached_plan: None,
            };
            manager.store_statement(stmt).unwrap();
        }

        // Adding one more should succeed via LRU eviction
        let stmt = PreparedStatement {
            name: "stmt3".to_string(),
            query: "SELECT 1".to_string(),
            param_types: vec![],
            result_schema: None,
            cached_plan: None,
        };
        let result = manager.store_statement(stmt);
        assert!(result.is_ok(), "LRU eviction should allow new statement");

        // Verify stmt0 was evicted (oldest) and stmt1/stmt3 remain
        assert!(manager.get_statement("stmt0").unwrap().is_none(), "stmt0 should have been evicted");
        assert!(manager.get_statement("stmt1").unwrap().is_some(), "stmt1 should still exist");
        assert!(manager.get_statement("stmt3").unwrap().is_some(), "stmt3 should exist");
    }
}
