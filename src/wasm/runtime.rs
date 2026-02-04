//! WASM Runtime Core
//!
//! Core runtime for WebAssembly environments with memory management,
//! async support, and platform detection.

use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::Arc;

/// WASM Runtime configuration
#[derive(Debug, Clone)]
pub struct WasmConfig {
    /// Maximum memory (in MB)
    pub max_memory_mb: usize,
    /// Enable SharedArrayBuffer (requires cross-origin isolation)
    pub shared_memory: bool,
    /// Enable SIMD instructions
    pub simd_enabled: bool,
    /// Enable threads (requires SharedArrayBuffer)
    pub threads_enabled: bool,
    /// Storage backend
    pub storage_backend: StorageBackend,
    /// Enable debug logging
    pub debug: bool,
}

impl Default for WasmConfig {
    fn default() -> Self {
        Self {
            max_memory_mb: 256,
            shared_memory: false,
            simd_enabled: true,
            threads_enabled: false,
            storage_backend: StorageBackend::IndexedDb,
            debug: false,
        }
    }
}

/// Storage backend options for WASM
#[derive(Debug, Clone, PartialEq)]
pub enum StorageBackend {
    /// Browser IndexedDB
    IndexedDb,
    /// Browser localStorage (limited to 5MB)
    LocalStorage,
    /// In-memory only (ephemeral)
    Memory,
    /// Origin Private File System (modern browsers)
    Opfs,
    /// Custom backend via JavaScript callback
    Custom(String),
}

/// Platform detection for WASM environments
#[derive(Debug, Clone, PartialEq)]
pub enum WasmPlatform {
    /// Web browser
    Browser,
    /// Cloudflare Workers
    CloudflareWorkers,
    /// Deno Deploy
    DenoRuntime,
    /// Node.js with WASM
    NodeJs,
    /// Fastly Compute@Edge
    FastlyCompute,
    /// Vercel Edge Functions
    VercelEdge,
    /// Unknown platform
    Unknown,
}

/// WASM Runtime instance
pub struct WasmRuntime {
    config: WasmConfig,
    platform: WasmPlatform,
    memory_usage: Arc<RwLock<MemoryStats>>,
    databases: Arc<RwLock<HashMap<String, WasmDatabase>>>,
}

/// Memory statistics
#[derive(Debug, Clone, Default)]
pub struct MemoryStats {
    pub heap_used: usize,
    pub heap_total: usize,
    pub external: usize,
    pub array_buffers: usize,
}

/// WASM Database instance
pub struct WasmDatabase {
    pub name: String,
    pub tables: HashMap<String, WasmTable>,
    pub vector_stores: HashMap<String, WasmVectorStore>,
    pub branches: HashMap<String, WasmBranch>,
}

/// WASM Table
pub struct WasmTable {
    pub name: String,
    pub columns: Vec<WasmColumn>,
    pub row_count: usize,
}

/// WASM Column definition
#[derive(Debug, Clone)]
pub struct WasmColumn {
    pub name: String,
    pub data_type: String,
    pub nullable: bool,
    pub primary_key: bool,
}

/// WASM Vector Store
pub struct WasmVectorStore {
    pub name: String,
    pub dimensions: usize,
    pub metric: String,
    pub count: usize,
}

/// WASM Branch
pub struct WasmBranch {
    pub name: String,
    pub parent: Option<String>,
    pub created_at: u64,
}

impl WasmRuntime {
    /// Create new WASM runtime with configuration
    pub fn new(config: WasmConfig) -> Self {
        let platform = Self::detect_platform();

        Self {
            config,
            platform,
            memory_usage: Arc::new(RwLock::new(MemoryStats::default())),
            databases: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Detect the current WASM platform
    fn detect_platform() -> WasmPlatform {
        // Platform detection would use JavaScript interop in actual WASM
        // For now, return Browser as default
        WasmPlatform::Browser
    }

    /// Get current platform
    pub fn platform(&self) -> &WasmPlatform {
        &self.platform
    }

    /// Check if SIMD is available
    pub fn has_simd(&self) -> bool {
        self.config.simd_enabled && self.check_simd_support()
    }

    /// Check SIMD support (would be JS interop in WASM)
    fn check_simd_support(&self) -> bool {
        // In actual WASM, this would check WebAssembly.validate with SIMD opcodes
        true
    }

    /// Check if threads are available
    pub fn has_threads(&self) -> bool {
        self.config.threads_enabled && self.config.shared_memory
    }

    /// Get memory statistics
    pub fn memory_stats(&self) -> MemoryStats {
        self.memory_usage.read().clone()
    }

    /// Update memory statistics
    pub fn update_memory_stats(&self, stats: MemoryStats) {
        *self.memory_usage.write() = stats;
    }

    /// Create a new database
    pub fn create_database(&self, name: &str) -> Result<(), WasmError> {
        let mut dbs = self.databases.write();

        if dbs.contains_key(name) {
            return Err(WasmError::DatabaseExists(name.to_string()));
        }

        let db = WasmDatabase {
            name: name.to_string(),
            tables: HashMap::new(),
            vector_stores: HashMap::new(),
            branches: {
                let mut branches = HashMap::new();
                branches.insert("main".to_string(), WasmBranch {
                    name: "main".to_string(),
                    parent: None,
                    created_at: Self::current_timestamp(),
                });
                branches
            },
        };

        dbs.insert(name.to_string(), db);
        Ok(())
    }

    /// Get database by name
    pub fn database(&self, name: &str) -> Option<String> {
        let dbs = self.databases.read();
        dbs.get(name).map(|db| db.name.clone())
    }

    /// List all databases
    pub fn list_databases(&self) -> Vec<String> {
        let dbs = self.databases.read();
        dbs.keys().cloned().collect()
    }

    /// Current timestamp (milliseconds since epoch)
    fn current_timestamp() -> u64 {
        // In WASM, would use Date.now() via JS interop
        0
    }

    /// Execute SQL query
    pub fn execute_sql(&self, db_name: &str, sql: &str, _branch: &str) -> Result<QueryResult, WasmError> {
        let dbs = self.databases.read();

        if !dbs.contains_key(db_name) {
            return Err(WasmError::DatabaseNotFound(db_name.to_string()));
        }

        // Parse and execute SQL (simplified)
        let sql_lower = sql.to_lowercase();

        if sql_lower.starts_with("select") {
            Ok(QueryResult {
                rows: Vec::new(),
                columns: Vec::new(),
                rows_affected: 0,
                execution_time_ms: 0,
            })
        } else if sql_lower.starts_with("insert") {
            Ok(QueryResult {
                rows: Vec::new(),
                columns: Vec::new(),
                rows_affected: 1,
                execution_time_ms: 0,
            })
        } else if sql_lower.starts_with("create table") {
            Ok(QueryResult {
                rows: Vec::new(),
                columns: Vec::new(),
                rows_affected: 0,
                execution_time_ms: 0,
            })
        } else {
            Err(WasmError::UnsupportedOperation(sql.to_string()))
        }
    }

    /// Vector search
    pub fn vector_search(
        &self,
        _db_name: &str,
        _store_name: &str,
        _vector: &[f32],
        _top_k: usize,
    ) -> Result<Vec<VectorSearchResult>, WasmError> {
        // Simplified vector search implementation
        Ok(Vec::new())
    }

    /// Store vector with metadata
    pub fn store_vector(
        &self,
        _db_name: &str,
        _store_name: &str,
        _id: &str,
        _vector: &[f32],
        _metadata: Option<&str>,
    ) -> Result<(), WasmError> {
        Ok(())
    }

    /// Get storage backend info
    pub fn storage_info(&self) -> StorageInfo {
        StorageInfo {
            backend: format!("{:?}", self.config.storage_backend),
            available: true,
            quota_bytes: match self.config.storage_backend {
                StorageBackend::LocalStorage => Some(5 * 1024 * 1024), // 5MB
                StorageBackend::IndexedDb => None, // Varies by browser
                StorageBackend::Opfs => None,
                StorageBackend::Memory => Some(self.config.max_memory_mb * 1024 * 1024),
                StorageBackend::Custom(_) => None,
            },
            used_bytes: 0,
        }
    }

    /// Persist data to storage
    pub fn persist(&self) -> Result<(), WasmError> {
        match &self.config.storage_backend {
            StorageBackend::Memory => Ok(()), // No persistence needed
            StorageBackend::LocalStorage => {
                // Would serialize to localStorage via JS
                Ok(())
            }
            StorageBackend::IndexedDb => {
                // Would persist to IndexedDB via JS
                Ok(())
            }
            StorageBackend::Opfs => {
                // Would persist to OPFS via JS
                Ok(())
            }
            StorageBackend::Custom(handler) => {
                // Would call custom JS handler
                log::debug!("Custom persist handler: {}", handler);
                Ok(())
            }
        }
    }

    /// Load data from storage
    pub fn hydrate(&self) -> Result<(), WasmError> {
        match &self.config.storage_backend {
            StorageBackend::Memory => Ok(()), // Nothing to load
            _ => {
                // Would load from respective storage
                Ok(())
            }
        }
    }
}

/// Query result
#[derive(Debug, Clone)]
pub struct QueryResult {
    pub rows: Vec<Vec<serde_json::Value>>,
    pub columns: Vec<String>,
    pub rows_affected: usize,
    pub execution_time_ms: u64,
}

/// Vector search result
#[derive(Debug, Clone)]
pub struct VectorSearchResult {
    pub id: String,
    pub score: f32,
    pub metadata: Option<serde_json::Value>,
}

/// Storage info
#[derive(Debug, Clone)]
pub struct StorageInfo {
    pub backend: String,
    pub available: bool,
    pub quota_bytes: Option<usize>,
    pub used_bytes: usize,
}

/// WASM-specific errors
#[derive(Debug, thiserror::Error)]
pub enum WasmError {
    #[error("Database not found: {0}")]
    DatabaseNotFound(String),

    #[error("Database already exists: {0}")]
    DatabaseExists(String),

    #[error("Table not found: {0}")]
    TableNotFound(String),

    #[error("Vector store not found: {0}")]
    VectorStoreNotFound(String),

    #[error("Branch not found: {0}")]
    BranchNotFound(String),

    #[error("Storage error: {0}")]
    Storage(String),

    #[error("Memory limit exceeded")]
    MemoryExceeded,

    #[error("Unsupported operation: {0}")]
    UnsupportedOperation(String),

    #[error("JavaScript interop error: {0}")]
    JsError(String),

    #[error("Serialization error: {0}")]
    Serialization(String),
}

/// Feature flags for WASM build
#[derive(Debug, Clone)]
pub struct WasmFeatures {
    /// Full SQL support
    pub full_sql: bool,
    /// Vector search with HNSW
    pub vector_search: bool,
    /// Branching and time-travel
    pub branching: bool,
    /// Agent memory
    pub agent_memory: bool,
    /// NL to SQL
    pub nl_query: bool,
    /// RAG pipeline
    pub rag: bool,
}

impl Default for WasmFeatures {
    fn default() -> Self {
        Self {
            full_sql: true,
            vector_search: true,
            branching: true,
            agent_memory: true,
            nl_query: false, // Requires external LLM
            rag: false,      // Requires external LLM
        }
    }
}

/// Check available features at runtime
pub fn available_features() -> WasmFeatures {
    WasmFeatures::default()
}
