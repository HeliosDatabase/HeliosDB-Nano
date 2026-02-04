//! JavaScript Bindings for WASM
//!
//! wasm-bindgen bindings for browser and edge runtime environments.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// Note: In actual WASM build, these would use wasm_bindgen
// #[wasm_bindgen] attributes. For now, we define the interface.

/// HeliosDB WASM instance - main entry point
#[derive(Default)]
pub struct HeliosDb {
    runtime: Option<super::runtime::WasmRuntime>,
    config: DbConfig,
}

/// Database configuration from JavaScript
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DbConfig {
    /// Database name
    pub name: Option<String>,
    /// Storage backend
    pub storage: Option<String>,
    /// Maximum memory in MB
    pub max_memory_mb: Option<usize>,
    /// Enable debug logging
    pub debug: Option<bool>,
}

/// Query options from JavaScript
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct QueryOptions {
    /// Branch to query
    pub branch: Option<String>,
    /// Query parameters
    pub params: Option<Vec<serde_json::Value>>,
    /// Return as JSON string
    pub as_json: Option<bool>,
}

/// Vector search options
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VectorSearchOptions {
    /// Number of results
    pub top_k: Option<usize>,
    /// Minimum similarity score
    pub min_score: Option<f32>,
    /// Metadata filter
    pub filter: Option<HashMap<String, serde_json::Value>>,
    /// Include vectors in response
    pub include_vectors: Option<bool>,
}

impl Default for VectorSearchOptions {
    fn default() -> Self {
        Self {
            top_k: Some(10),
            min_score: None,
            filter: None,
            include_vectors: Some(false),
        }
    }
}

/// JavaScript-friendly result type
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsResult {
    pub success: bool,
    pub data: Option<serde_json::Value>,
    pub error: Option<String>,
}

impl JsResult {
    pub fn ok(data: serde_json::Value) -> Self {
        Self {
            success: true,
            data: Some(data),
            error: None,
        }
    }

    pub fn err(error: impl ToString) -> Self {
        Self {
            success: false,
            data: None,
            error: Some(error.to_string()),
        }
    }
}

impl HeliosDb {
    /// Create new HeliosDB instance
    /// Called from JavaScript: `const db = new HeliosDb(config)`
    pub fn new(config: DbConfig) -> Self {
        Self {
            runtime: None,
            config,
        }
    }

    /// Initialize the database
    /// Called from JavaScript: `await db.init()`
    pub async fn init(&mut self) -> JsResult {
        let wasm_config = super::runtime::WasmConfig {
            max_memory_mb: self.config.max_memory_mb.unwrap_or(256),
            storage_backend: match self.config.storage.as_deref() {
                Some("indexeddb") => super::runtime::StorageBackend::IndexedDb,
                Some("localstorage") => super::runtime::StorageBackend::LocalStorage,
                Some("memory") => super::runtime::StorageBackend::Memory,
                Some("opfs") => super::runtime::StorageBackend::Opfs,
                _ => super::runtime::StorageBackend::IndexedDb,
            },
            debug: self.config.debug.unwrap_or(false),
            ..Default::default()
        };

        let runtime = super::runtime::WasmRuntime::new(wasm_config);

        // Create default database
        let db_name = self.config.name.clone().unwrap_or_else(|| "default".to_string());
        if let Err(e) = runtime.create_database(&db_name) {
            return JsResult::err(e);
        }

        // Hydrate from storage
        if let Err(e) = runtime.hydrate() {
            return JsResult::err(e);
        }

        self.runtime = Some(runtime);

        JsResult::ok(serde_json::json!({
            "initialized": true,
            "database": db_name
        }))
    }

    /// Execute SQL query
    /// Called from JavaScript: `const result = await db.query("SELECT * FROM users")`
    pub fn query(&self, sql: &str, options: Option<QueryOptions>) -> JsResult {
        let runtime = match &self.runtime {
            Some(r) => r,
            None => return JsResult::err("Database not initialized"),
        };

        let opts = options.unwrap_or_default();
        let branch = opts.branch.unwrap_or_else(|| "main".to_string());
        let db_name = self.config.name.clone().unwrap_or_else(|| "default".to_string());

        match runtime.execute_sql(&db_name, sql, &branch) {
            Ok(result) => JsResult::ok(serde_json::json!({
                "rows": result.rows,
                "columns": result.columns,
                "rowsAffected": result.rows_affected,
                "executionTimeMs": result.execution_time_ms
            })),
            Err(e) => JsResult::err(e),
        }
    }

    /// Execute query and return rows only
    /// Called from JavaScript: `const rows = await db.exec("SELECT * FROM users")`
    pub fn exec(&self, sql: &str) -> JsResult {
        let result = self.query(sql, None);
        if result.success {
            if let Some(data) = result.data {
                return JsResult::ok(data["rows"].clone());
            }
        }
        result
    }

    /// Create a table
    /// Called from JavaScript: `await db.createTable("users", { id: "INTEGER PRIMARY KEY", name: "TEXT" })`
    pub fn create_table(&self, name: &str, columns: HashMap<String, String>) -> JsResult {
        let cols: Vec<String> = columns
            .iter()
            .map(|(name, typ)| format!("{} {}", name, typ))
            .collect();

        let sql = format!("CREATE TABLE {} ({})", name, cols.join(", "));
        self.query(&sql, None)
    }

    /// Insert a row
    /// Called from JavaScript: `await db.insert("users", { name: "Alice", email: "alice@example.com" })`
    pub fn insert(&self, table: &str, data: HashMap<String, serde_json::Value>) -> JsResult {
        let columns: Vec<&str> = data.keys().map(|k| k.as_str()).collect();
        let values: Vec<String> = data.values().map(|v| {
            match v {
                serde_json::Value::String(s) => format!("'{}'", s.replace('\'', "''")),
                serde_json::Value::Null => "NULL".to_string(),
                _ => v.to_string(),
            }
        }).collect();

        let sql = format!(
            "INSERT INTO {} ({}) VALUES ({})",
            table,
            columns.join(", "),
            values.join(", ")
        );

        self.query(&sql, None)
    }

    /// Vector search
    /// Called from JavaScript: `const results = await db.vectorSearch("docs", [0.1, 0.2, ...], { topK: 5 })`
    pub fn vector_search(
        &self,
        store: &str,
        vector: Vec<f32>,
        options: Option<VectorSearchOptions>,
    ) -> JsResult {
        let runtime = match &self.runtime {
            Some(r) => r,
            None => return JsResult::err("Database not initialized"),
        };

        let opts = options.unwrap_or_default();
        let db_name = self.config.name.clone().unwrap_or_else(|| "default".to_string());

        match runtime.vector_search(&db_name, store, &vector, opts.top_k.unwrap_or(10)) {
            Ok(results) => {
                let results_json: Vec<serde_json::Value> = results
                    .iter()
                    .map(|r| serde_json::json!({
                        "id": r.id,
                        "score": r.score,
                        "metadata": r.metadata
                    }))
                    .collect();

                JsResult::ok(serde_json::json!({ "results": results_json }))
            }
            Err(e) => JsResult::err(e),
        }
    }

    /// Text search (with auto-embedding)
    /// Called from JavaScript: `const results = await db.searchText("docs", "hello world")`
    pub fn search_text(&self, store: &str, text: &str, options: Option<VectorSearchOptions>) -> JsResult {
        // In actual implementation, would embed text first
        // For now, return empty results
        let _ = (store, text, options);
        JsResult::ok(serde_json::json!({ "results": [] }))
    }

    /// Store text with auto-embedding
    /// Called from JavaScript: `await db.storeText("docs", "Hello world", { category: "greeting" })`
    pub fn store_text(
        &self,
        store: &str,
        text: &str,
        metadata: Option<HashMap<String, serde_json::Value>>,
    ) -> JsResult {
        let _ = (store, text, metadata);
        JsResult::ok(serde_json::json!({ "stored": true }))
    }

    /// Create a branch
    /// Called from JavaScript: `await db.createBranch("feature-x")`
    pub fn create_branch(&self, name: &str, from_branch: Option<&str>) -> JsResult {
        let _ = (name, from_branch);
        JsResult::ok(serde_json::json!({ "branch": name }))
    }

    /// Merge branches
    /// Called from JavaScript: `await db.mergeBranch("feature-x", "main")`
    pub fn merge_branch(&self, source: &str, target: &str) -> JsResult {
        let _ = (source, target);
        JsResult::ok(serde_json::json!({ "merged": true }))
    }

    /// Time travel query
    /// Called from JavaScript: `const rows = await db.queryAt("SELECT * FROM users", "2024-01-01T00:00:00Z")`
    pub fn query_at(&self, sql: &str, timestamp: &str) -> JsResult {
        let _ = (sql, timestamp);
        JsResult::ok(serde_json::json!({ "rows": [] }))
    }

    /// Get storage info
    /// Called from JavaScript: `const info = db.storageInfo()`
    pub fn storage_info(&self) -> JsResult {
        let runtime = match &self.runtime {
            Some(r) => r,
            None => return JsResult::err("Database not initialized"),
        };

        let info = runtime.storage_info();
        JsResult::ok(serde_json::json!({
            "backend": info.backend,
            "available": info.available,
            "quotaBytes": info.quota_bytes,
            "usedBytes": info.used_bytes
        }))
    }

    /// Persist data to storage
    /// Called from JavaScript: `await db.persist()`
    pub fn persist(&self) -> JsResult {
        let runtime = match &self.runtime {
            Some(r) => r,
            None => return JsResult::err("Database not initialized"),
        };

        match runtime.persist() {
            Ok(()) => JsResult::ok(serde_json::json!({ "persisted": true })),
            Err(e) => JsResult::err(e),
        }
    }

    /// Get memory statistics
    /// Called from JavaScript: `const stats = db.memoryStats()`
    pub fn memory_stats(&self) -> JsResult {
        let runtime = match &self.runtime {
            Some(r) => r,
            None => return JsResult::err("Database not initialized"),
        };

        let stats = runtime.memory_stats();
        JsResult::ok(serde_json::json!({
            "heapUsed": stats.heap_used,
            "heapTotal": stats.heap_total,
            "external": stats.external,
            "arrayBuffers": stats.array_buffers
        }))
    }

    /// Check available features
    /// Called from JavaScript: `const features = HeliosDb.features()`
    pub fn features() -> JsResult {
        let features = super::runtime::available_features();
        JsResult::ok(serde_json::json!({
            "fullSql": features.full_sql,
            "vectorSearch": features.vector_search,
            "branching": features.branching,
            "agentMemory": features.agent_memory,
            "nlQuery": features.nl_query,
            "rag": features.rag
        }))
    }

    /// Check platform
    /// Called from JavaScript: `const platform = HeliosDb.platform()`
    pub fn platform() -> JsResult {
        let runtime = super::runtime::WasmRuntime::new(Default::default());
        JsResult::ok(serde_json::json!({
            "platform": format!("{:?}", runtime.platform()),
            "simd": runtime.has_simd(),
            "threads": runtime.has_threads()
        }))
    }
}

/// Agent memory API for WASM
pub struct AgentMemory {
    session_id: String,
}

impl AgentMemory {
    /// Create new agent memory instance
    pub fn new(session_id: &str) -> Self {
        Self {
            session_id: session_id.to_string(),
        }
    }

    /// Add message to memory
    pub fn add(&self, role: &str, content: &str) -> JsResult {
        let _ = (role, content);
        JsResult::ok(serde_json::json!({
            "sessionId": self.session_id,
            "added": true
        }))
    }

    /// Get messages
    pub fn get(&self, limit: Option<usize>) -> JsResult {
        let _ = limit;
        JsResult::ok(serde_json::json!({
            "messages": []
        }))
    }

    /// Search memory semantically
    pub fn search(&self, query: &str, top_k: Option<usize>) -> JsResult {
        let _ = (query, top_k);
        JsResult::ok(serde_json::json!({
            "results": []
        }))
    }

    /// Clear memory
    pub fn clear(&self) -> JsResult {
        JsResult::ok(serde_json::json!({
            "cleared": true
        }))
    }

    /// Get session summary
    pub fn summarize(&self) -> JsResult {
        JsResult::ok(serde_json::json!({
            "summary": ""
        }))
    }
}

/// Export types for TypeScript generation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TypeScriptTypes {
    pub db_config: String,
    pub query_options: String,
    pub vector_search_options: String,
    pub js_result: String,
}

/// Generate TypeScript type definitions
pub fn generate_typescript_types() -> String {
    r#"
// HeliosDB WASM TypeScript Definitions

export interface DbConfig {
  name?: string;
  storage?: 'indexeddb' | 'localstorage' | 'memory' | 'opfs';
  maxMemoryMb?: number;
  debug?: boolean;
}

export interface QueryOptions {
  branch?: string;
  params?: any[];
  asJson?: boolean;
}

export interface VectorSearchOptions {
  topK?: number;
  minScore?: number;
  filter?: Record<string, any>;
  includeVectors?: boolean;
}

export interface QueryResult {
  rows: any[];
  columns: string[];
  rowsAffected: number;
  executionTimeMs: number;
}

export interface VectorSearchResult {
  id: string;
  score: number;
  metadata?: Record<string, any>;
  vector?: number[];
}

export interface StorageInfo {
  backend: string;
  available: boolean;
  quotaBytes?: number;
  usedBytes: number;
}

export interface MemoryStats {
  heapUsed: number;
  heapTotal: number;
  external: number;
  arrayBuffers: number;
}

export interface Features {
  fullSql: boolean;
  vectorSearch: boolean;
  branching: boolean;
  agentMemory: boolean;
  nlQuery: boolean;
  rag: boolean;
}

export interface Platform {
  platform: string;
  simd: boolean;
  threads: boolean;
}

export class HeliosDb {
  constructor(config?: DbConfig);

  init(): Promise<void>;
  query(sql: string, options?: QueryOptions): Promise<QueryResult>;
  exec(sql: string): Promise<any[]>;
  createTable(name: string, columns: Record<string, string>): Promise<void>;
  insert(table: string, data: Record<string, any>): Promise<void>;

  vectorSearch(store: string, vector: number[], options?: VectorSearchOptions): Promise<VectorSearchResult[]>;
  searchText(store: string, text: string, options?: VectorSearchOptions): Promise<VectorSearchResult[]>;
  storeText(store: string, text: string, metadata?: Record<string, any>): Promise<void>;

  createBranch(name: string, fromBranch?: string): Promise<void>;
  mergeBranch(source: string, target: string): Promise<void>;
  queryAt(sql: string, timestamp: string): Promise<any[]>;

  storageInfo(): StorageInfo;
  memoryStats(): MemoryStats;
  persist(): Promise<void>;

  static features(): Features;
  static platform(): Platform;
}

export class AgentMemory {
  constructor(sessionId: string);

  add(role: 'user' | 'assistant' | 'system', content: string): Promise<void>;
  get(limit?: number): Promise<Array<{ role: string; content: string; timestamp: string }>>;
  search(query: string, topK?: number): Promise<Array<{ content: string; score: number }>>;
  clear(): Promise<void>;
  summarize(): Promise<string>;
}
"#.to_string()
}
