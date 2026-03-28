#![allow(dead_code)]

/// MySQL-Specific SQL Features
///
/// Implements MySQL-specific syntax and features:
/// - AUTO_INCREMENT support
/// - ON DUPLICATE KEY UPDATE
/// - SHOW commands (SHOW TABLES, SHOW CREATE TABLE, etc.)
/// - MySQL system tables (information_schema)
/// - MySQL-specific functions and operators
use bytes::Bytes;
use std::collections::HashMap;

/// AUTO_INCREMENT sequence manager
#[derive(Debug, Clone)]
pub struct AutoIncrementManager {
    sequences: HashMap<String, u64>, // table_name -> next_value
}

impl AutoIncrementManager {
    pub fn new() -> Self {
        Self {
            sequences: HashMap::new(),
        }
    }

    /// Get next AUTO_INCREMENT value for table
    pub fn next_value(&mut self, table: &str) -> u64 {
        let value = self.sequences.entry(table.to_string()).or_insert(1);
        let result = *value;
        *value += 1;
        result
    }

    /// Set AUTO_INCREMENT value for table
    pub fn set_value(&mut self, table: &str, value: u64) {
        self.sequences.insert(table.to_string(), value);
    }

    /// Get current AUTO_INCREMENT value without incrementing
    pub fn current_value(&self, table: &str) -> u64 {
        self.sequences.get(table).copied().unwrap_or(1)
    }

    /// Reset AUTO_INCREMENT for table
    pub fn reset(&mut self, table: &str) {
        self.sequences.insert(table.to_string(), 1);
    }

    /// Parse AUTO_INCREMENT from CREATE TABLE statement
    pub fn parse_auto_increment(sql: &str) -> Option<String> {
        let sql_upper = sql.to_uppercase();
        if sql_upper.contains("AUTO_INCREMENT") {
            // Find column with AUTO_INCREMENT
            for line in sql.lines() {
                let line_upper = line.to_uppercase();
                if line_upper.contains("AUTO_INCREMENT") && !line_upper.trim().starts_with("--") {
                    // Extract column name
                    let parts: Vec<&str> = line.trim().split_whitespace().collect();
                    if !parts.is_empty() {
                        return Some(parts[0].trim_matches('`').to_string());
                    }
                }
            }
        }
        None
    }
}

/// ON DUPLICATE KEY UPDATE handler
#[derive(Debug, Clone)]
pub struct OnDuplicateKeyUpdate {
    pub updates: Vec<(String, String)>, // (column, value/expression)
}

impl OnDuplicateKeyUpdate {
    /// Parse ON DUPLICATE KEY UPDATE clause
    pub fn parse(sql: &str) -> Option<Self> {
        let sql_upper = sql.to_uppercase();
        if let Some(pos) = sql_upper.find("ON DUPLICATE KEY UPDATE") {
            let update_part = &sql[pos + 23..].trim();

            // Parse update assignments
            let mut updates = Vec::new();
            for assignment in update_part.split(',') {
                let parts: Vec<&str> = assignment.trim().splitn(2, '=').collect();
                if parts.len() == 2 {
                    let column = parts[0].trim().to_string();
                    let value = parts[1].trim().to_string();
                    updates.push((column, value));
                }
            }

            if !updates.is_empty() {
                return Some(OnDuplicateKeyUpdate { updates });
            }
        }
        None
    }

    /// Check if expression uses VALUES() function
    pub fn uses_values_function(&self) -> bool {
        self.updates
            .iter()
            .any(|(_, expr)| expr.to_uppercase().contains("VALUES("))
    }

    /// Apply update to existing row
    pub fn apply_update(&self, existing_row: &mut HashMap<String, Bytes>, new_values: &HashMap<String, Bytes>) {
        for (column, expr) in &self.updates {
            if expr.to_uppercase().starts_with("VALUES(") {
                // VALUES(column_name) - use value from INSERT
                let col_name = expr[7..expr.len() - 1].trim();
                if let Some(new_value) = new_values.get(col_name) {
                    existing_row.insert(column.clone(), new_value.clone());
                }
            } else {
                // Direct value or expression
                existing_row.insert(column.clone(), Bytes::from(expr.clone()));
            }
        }
    }
}

/// SHOW commands handler
#[derive(Debug)]
pub struct ShowCommandHandler {
    databases: Vec<String>,
    tables: HashMap<String, Vec<String>>, // database -> tables
    table_schemas: HashMap<String, TableSchema>,
}

#[derive(Debug, Clone)]
pub struct TableSchema {
    pub name: String,
    pub columns: Vec<ColumnSchema>,
    pub indexes: Vec<IndexSchema>,
    pub engine: String,
    pub charset: String,
    pub collation: String,
    pub auto_increment: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct ColumnSchema {
    pub name: String,
    pub data_type: String,
    pub nullable: bool,
    pub key: String, // PRI, UNI, MUL, or empty
    pub default: Option<String>,
    pub extra: String, // auto_increment, on update current_timestamp, etc.
}

#[derive(Debug, Clone)]
pub struct IndexSchema {
    pub name: String,
    pub columns: Vec<String>,
    pub unique: bool,
    pub index_type: String,
}

impl ShowCommandHandler {
    pub fn new() -> Self {
        let mut handler = Self {
            databases: Vec::new(),
            tables: HashMap::new(),
            table_schemas: HashMap::new(),
        };

        // Add default databases
        handler.databases.push("information_schema".to_string());
        handler.databases.push("mysql".to_string());
        handler.databases.push("performance_schema".to_string());
        handler.databases.push("sys".to_string());
        handler.databases.push("heliosdb".to_string());

        handler
    }

    /// Handle SHOW DATABASES
    pub fn show_databases(&self) -> Vec<HashMap<String, Bytes>> {
        self.databases
            .iter()
            .map(|db| {
                let mut row = HashMap::new();
                row.insert("Database".to_string(), Bytes::from(db.clone()));
                row
            })
            .collect()
    }

    /// Handle SHOW TABLES
    pub fn show_tables(&self, database: &str) -> Vec<HashMap<String, Bytes>> {
        if let Some(tables) = self.tables.get(database) {
            tables
                .iter()
                .map(|table| {
                    let mut row = HashMap::new();
                    row.insert(format!("Tables_in_{database}"), Bytes::from(table.clone()));
                    row
                })
                .collect()
        } else {
            Vec::new()
        }
    }

    /// Handle SHOW CREATE TABLE
    pub fn show_create_table(&self, table_name: &str) -> Option<HashMap<String, Bytes>> {
        if let Some(schema) = self.table_schemas.get(table_name) {
            let create_sql = self.generate_create_table_sql(schema);

            let mut row = HashMap::new();
            row.insert("Table".to_string(), Bytes::from(schema.name.clone()));
            row.insert("Create Table".to_string(), Bytes::from(create_sql));

            Some(row)
        } else {
            None
        }
    }

    fn generate_create_table_sql(&self, schema: &TableSchema) -> String {
        let mut sql = format!("CREATE TABLE `{}` (\n", schema.name);

        // Add columns
        for (i, col) in schema.columns.iter().enumerate() {
            sql.push_str(&format!("  `{}` {}", col.name, col.data_type));

            if !col.nullable {
                sql.push_str(" NOT NULL");
            }

            if let Some(ref default) = col.default {
                sql.push_str(&format!(" DEFAULT {default}"));
            }

            if !col.extra.is_empty() {
                sql.push_str(&format!(" {}", col.extra));
            }

            if i < schema.columns.len() - 1 || !schema.indexes.is_empty() {
                sql.push(',');
            }
            sql.push('\n');
        }

        // Add indexes
        for (i, idx) in schema.indexes.iter().enumerate() {
            let key_type = if idx.unique { "UNIQUE KEY" } else { "KEY" };
            sql.push_str(&format!(
                "  {} `{}` ({})",
                key_type,
                idx.name,
                idx.columns
                    .iter()
                    .map(|c| format!("`{c}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));

            if i < schema.indexes.len() - 1 {
                sql.push(',');
            }
            sql.push('\n');
        }

        sql.push_str(&format!(
            ") ENGINE={} DEFAULT CHARSET={} COLLATE={}",
            schema.engine, schema.charset, schema.collation
        ));

        if let Some(auto_inc) = schema.auto_increment {
            sql.push_str(&format!(" AUTO_INCREMENT={auto_inc}"));
        }

        sql
    }

    /// Handle SHOW COLUMNS
    pub fn show_columns(&self, table_name: &str) -> Vec<HashMap<String, Bytes>> {
        if let Some(schema) = self.table_schemas.get(table_name) {
            schema
                .columns
                .iter()
                .map(|col| {
                    let mut row = HashMap::new();
                    row.insert("Field".to_string(), Bytes::from(col.name.clone()));
                    row.insert("Type".to_string(), Bytes::from(col.data_type.clone()));
                    row.insert("Null".to_string(), Bytes::from(if col.nullable { "YES" } else { "NO" }));
                    row.insert("Key".to_string(), Bytes::from(col.key.clone()));
                    row.insert(
                        "Default".to_string(),
                        Bytes::from(col.default.clone().unwrap_or_else(|| "NULL".to_string())),
                    );
                    row.insert("Extra".to_string(), Bytes::from(col.extra.clone()));
                    row
                })
                .collect()
        } else {
            Vec::new()
        }
    }

    /// Handle SHOW INDEX
    pub fn show_index(&self, table_name: &str) -> Vec<HashMap<String, Bytes>> {
        if let Some(schema) = self.table_schemas.get(table_name) {
            let mut rows = Vec::new();
            for idx in &schema.indexes {
                for (seq, col) in idx.columns.iter().enumerate() {
                    let mut row = HashMap::new();
                    row.insert("Table".to_string(), Bytes::from(schema.name.clone()));
                    row.insert(
                        "Non_unique".to_string(),
                        Bytes::from(if idx.unique { "0" } else { "1" }),
                    );
                    row.insert("Key_name".to_string(), Bytes::from(idx.name.clone()));
                    row.insert("Seq_in_index".to_string(), Bytes::from((seq + 1).to_string()));
                    row.insert("Column_name".to_string(), Bytes::from(col.clone()));
                    row.insert("Collation".to_string(), Bytes::from("A"));
                    row.insert("Cardinality".to_string(), Bytes::from("0"));
                    row.insert("Index_type".to_string(), Bytes::from(idx.index_type.clone()));
                    rows.push(row);
                }
            }
            rows
        } else {
            Vec::new()
        }
    }

    /// Handle SHOW TABLE STATUS
    pub fn show_table_status(&self, database: &str) -> Vec<HashMap<String, Bytes>> {
        if let Some(tables) = self.tables.get(database) {
            tables
                .iter()
                .filter_map(|table| {
                    self.table_schemas.get(table).map(|schema| {
                        let mut row = HashMap::new();
                        row.insert("Name".to_string(), Bytes::from(schema.name.clone()));
                        row.insert("Engine".to_string(), Bytes::from(schema.engine.clone()));
                        row.insert("Version".to_string(), Bytes::from("10"));
                        row.insert("Row_format".to_string(), Bytes::from("Dynamic"));
                        row.insert("Rows".to_string(), Bytes::from("0"));
                        row.insert("Avg_row_length".to_string(), Bytes::from("0"));
                        row.insert("Data_length".to_string(), Bytes::from("0"));
                        row.insert("Max_data_length".to_string(), Bytes::from("0"));
                        row.insert("Index_length".to_string(), Bytes::from("0"));
                        row.insert("Data_free".to_string(), Bytes::from("0"));
                        row.insert(
                            "Auto_increment".to_string(),
                            Bytes::from(schema.auto_increment.map(|v| v.to_string()).unwrap_or_default()),
                        );
                        row.insert("Create_time".to_string(), Bytes::from("2025-10-12 00:00:00"));
                        row.insert("Collation".to_string(), Bytes::from(schema.collation.clone()));
                        row
                    })
                })
                .collect()
        } else {
            Vec::new()
        }
    }

    /// Handle SHOW VARIABLES
    pub fn show_variables(&self, pattern: Option<&str>) -> Vec<HashMap<String, Bytes>> {
        let mut variables = vec![
            ("version", "8.0.35-HeliosDB-Nano"),
            ("version_comment", "HeliosDB Nano compatible with MySQL"),
            ("character_set_client", "utf8mb4"),
            ("character_set_connection", "utf8mb4"),
            ("character_set_database", "utf8mb4"),
            ("character_set_results", "utf8mb4"),
            ("character_set_server", "utf8mb4"),
            ("collation_connection", "utf8mb4_general_ci"),
            ("collation_database", "utf8mb4_general_ci"),
            ("collation_server", "utf8mb4_general_ci"),
            ("max_allowed_packet", "67108864"),
            ("sql_mode", "STRICT_TRANS_TABLES,NO_ENGINE_SUBSTITUTION"),
            ("time_zone", "SYSTEM"),
            ("transaction_isolation", "REPEATABLE-READ"),
            ("autocommit", "ON"),
        ];

        if let Some(pattern) = pattern {
            let pattern_lower = pattern.to_lowercase();
            variables.retain(|(name, _)| name.to_lowercase().contains(&pattern_lower));
        }

        variables
            .iter()
            .map(|(name, value)| {
                let mut row = HashMap::new();
                row.insert("Variable_name".to_string(), Bytes::from(*name));
                row.insert("Value".to_string(), Bytes::from(*value));
                row
            })
            .collect()
    }

    /// Handle SHOW STATUS
    pub fn show_status(&self, pattern: Option<&str>) -> Vec<HashMap<String, Bytes>> {
        let mut status_vars = vec![
            ("Threads_connected", "1"),
            ("Threads_running", "1"),
            ("Questions", "0"),
            ("Uptime", "3600"),
            ("Bytes_received", "0"),
            ("Bytes_sent", "0"),
            ("Com_select", "0"),
            ("Com_insert", "0"),
            ("Com_update", "0"),
            ("Com_delete", "0"),
        ];

        if let Some(pattern) = pattern {
            let pattern_lower = pattern.to_lowercase();
            status_vars.retain(|(name, _)| name.to_lowercase().contains(&pattern_lower));
        }

        status_vars
            .iter()
            .map(|(name, value)| {
                let mut row = HashMap::new();
                row.insert("Variable_name".to_string(), Bytes::from(*name));
                row.insert("Value".to_string(), Bytes::from(*value));
                row
            })
            .collect()
    }

    /// Add table schema
    pub fn add_table(&mut self, database: &str, schema: TableSchema) {
        self.tables
            .entry(database.to_string())
            .or_default()
            .push(schema.name.clone());

        self.table_schemas.insert(schema.name.clone(), schema);
    }

    /// Check if query is a SHOW command
    pub fn is_show_command(sql: &str) -> bool {
        sql.trim().to_uppercase().starts_with("SHOW")
    }

    /// Route SHOW command to appropriate handler
    pub fn handle_show_command(&self, sql: &str, current_database: &str) -> Option<Vec<HashMap<String, Bytes>>> {
        let sql_upper = sql.to_uppercase();

        if sql_upper.contains("SHOW DATABASES") {
            Some(self.show_databases())
        } else if sql_upper.contains("SHOW TABLES") {
            Some(self.show_tables(current_database))
        } else if sql_upper.contains("SHOW CREATE TABLE") {
            // Extract table name
            let parts: Vec<&str> = sql.split_whitespace().collect();
            if let Some(table) = parts.last() {
                let table_name = table.trim_matches(';').trim_matches('`');
                self.show_create_table(table_name).map(|row| vec![row])
            } else {
                None
            }
        } else if sql_upper.contains("SHOW COLUMNS") || sql_upper.contains("SHOW FIELDS") {
            // Extract table name (SHOW COLUMNS FROM table_name)
            let parts: Vec<&str> = sql.split_whitespace().collect();
            if let Some(from_idx) = parts.iter().position(|&p| p.to_uppercase() == "FROM") {
                if from_idx + 1 < parts.len() {
                    let table_name = parts[from_idx + 1].trim_matches(';').trim_matches('`');
                    Some(self.show_columns(table_name))
                } else {
                    None
                }
            } else {
                None
            }
        } else if sql_upper.contains("SHOW INDEX")
            || sql_upper.contains("SHOW INDEXES")
            || sql_upper.contains("SHOW KEYS")
        {
            // Extract table name
            let parts: Vec<&str> = sql.split_whitespace().collect();
            if let Some(from_idx) = parts.iter().position(|&p| p.to_uppercase() == "FROM") {
                if from_idx + 1 < parts.len() {
                    let table_name = parts[from_idx + 1].trim_matches(';').trim_matches('`');
                    Some(self.show_index(table_name))
                } else {
                    None
                }
            } else {
                None
            }
        } else if sql_upper.contains("SHOW TABLE STATUS") {
            Some(self.show_table_status(current_database))
        } else if sql_upper.contains("SHOW VARIABLES") {
            // Check for LIKE pattern
            let pattern = if let Some(like_pos) = sql_upper.find("LIKE") {
                let s = &sql[like_pos + 4..];
                Some(s.trim().trim_matches('\'').trim_matches('"'))
            } else {
                None
            };
            Some(self.show_variables(pattern))
        } else if sql_upper.contains("SHOW STATUS") {
            let pattern = if let Some(like_pos) = sql_upper.find("LIKE") {
                let s = &sql[like_pos + 4..];
                Some(s.trim().trim_matches('\'').trim_matches('"'))
            } else {
                None
            };
            Some(self.show_status(pattern))
        } else {
            None
        }
    }
}

impl Default for ShowCommandHandler {
    fn default() -> Self {
        Self::new()
    }
}

impl Default for AutoIncrementManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_auto_increment_manager() {
        let mut mgr = AutoIncrementManager::new();

        assert_eq!(mgr.next_value("users"), 1);
        assert_eq!(mgr.next_value("users"), 2);
        assert_eq!(mgr.next_value("users"), 3);

        mgr.set_value("users", 100);
        assert_eq!(mgr.next_value("users"), 100);
    }

    #[test]
    fn test_on_duplicate_key_update_parse() {
        let sql = "INSERT INTO users (id, name) VALUES (1, 'John') ON DUPLICATE KEY UPDATE name = VALUES(name), updated_at = NOW()";
        let handler = OnDuplicateKeyUpdate::parse(sql).expect("should parse");

        assert_eq!(handler.updates.len(), 2);
        assert_eq!(handler.updates[0].0, "name");
        assert!(handler.uses_values_function());
    }

    #[test]
    fn test_show_command_handler() {
        let handler = ShowCommandHandler::new();

        let databases = handler.show_databases();
        assert!(databases.len() >= 5);

        let variables = handler.show_variables(Some("character_set"));
        assert!(!variables.is_empty());
    }

    #[test]
    fn test_is_show_command() {
        assert!(ShowCommandHandler::is_show_command("SHOW DATABASES"));
        assert!(ShowCommandHandler::is_show_command("show tables"));
        assert!(!ShowCommandHandler::is_show_command("SELECT * FROM users"));
    }
}
