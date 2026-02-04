/**
 * HeliosDB type definitions
 */

/**
 * Column information from query results
 */
export interface Column {
  name: string;
  type: string;
}

/**
 * Result of a SQL query execution
 */
export interface QueryResult<T = Record<string, unknown>> {
  columns: Column[];
  rows: unknown[][];
  rowCount: number;
  executionTimeMs: number;

  /**
   * Convert rows to array of objects
   */
  toObjects(): T[];
}

/**
 * Table information
 */
export interface TableInfo {
  name: string;
  rowCount: number;
  sizeBytes: number;
  createdAt: Date;
}

/**
 * Column definition for table schema
 */
export interface ColumnDefinition {
  name: string;
  type: string;
  nullable?: boolean;
  default?: string;
}

/**
 * Index information
 */
export interface IndexInfo {
  name: string;
  columns: string[];
  type: 'btree' | 'hash' | 'hnsw' | 'ivfflat';
  unique: boolean;
}

/**
 * Table schema
 */
export interface TableSchema {
  name: string;
  columns: ColumnDefinition[];
  primaryKey: string[];
  indexes: IndexInfo[];
}

/**
 * Branch information
 */
export interface Branch {
  name: string;
  parent: string | null;
  createdAt: Date;
  commitCount: number;
}

/**
 * Merge conflict information
 */
export interface MergeConflict {
  table: string;
  rowId: string;
  sourceValue: Record<string, unknown>;
  targetValue: Record<string, unknown>;
}

/**
 * Result of a branch merge
 */
export interface MergeResult {
  success: boolean;
  conflicts: MergeConflict[];
  changesApplied: number;
}

/**
 * Vector store information
 */
export interface VectorStoreInfo {
  name: string;
  dimension: number;
  metric: 'cosine' | 'euclidean' | 'dot_product';
  vectorCount: number;
  createdAt: Date;
}

/**
 * Vector entry for upsert
 */
export interface VectorEntry {
  id: string;
  vector: number[];
  metadata?: Record<string, unknown>;
}

/**
 * Vector search options
 */
export interface VectorSearchOptions {
  topK?: number;
  filter?: Record<string, unknown>;
  includeMetadata?: boolean;
  includeVectors?: boolean;
}

/**
 * Vector search result
 */
export interface VectorSearchResult {
  id: string;
  score: number;
  vector?: number[];
  metadata: Record<string, unknown>;
}

/**
 * Memory message
 */
export interface MemoryMessage {
  id: string;
  role: 'user' | 'assistant' | 'system' | 'tool';
  content: string;
  metadata: Record<string, unknown>;
  timestamp: Date;
}

/**
 * Memory search result
 */
export interface MemorySearchResult {
  message: MemoryMessage;
  score: number;
}

/**
 * Document chunk
 */
export interface DocumentChunk {
  id: string;
  content: string;
  index: number;
}

/**
 * Document
 */
export interface Document {
  id: string;
  content: string;
  metadata: Record<string, unknown>;
  chunks: DocumentChunk[];
  createdAt: Date;
}

/**
 * Document search result
 */
export interface DocumentSearchResult {
  documentId: string;
  chunkId: string;
  content: string;
  score: number;
  metadata: Record<string, unknown>;
}

/**
 * Chat session
 */
export interface ChatSession {
  id: string;
  name?: string;
  createdAt: Date;
  messageCount: number;
  metadata: Record<string, unknown>;
}

/**
 * Chat message
 */
export interface ChatMessage {
  id: string;
  role: 'user' | 'assistant' | 'system';
  content: string;
  timestamp: Date;
}

/**
 * Chat summary
 */
export interface ChatSummary {
  summary: string;
  keyTopics: string[];
  messageCount: number;
}

/**
 * Health status
 */
export interface HealthStatus {
  status: 'healthy' | 'degraded' | 'unhealthy';
  version: string;
  uptimeSeconds: number;
}

/**
 * Inferred schema
 */
export interface InferredSchema {
  inferredSchema: Record<string, string>;
  createSql: string;
  confidence: number;
}

/**
 * Change record for time-travel
 */
export interface ChangeRecord {
  timestamp: Date;
  operation: 'insert' | 'update' | 'delete';
  rowId: string;
  oldValues?: Record<string, unknown>;
  newValues?: Record<string, unknown>;
}

/**
 * Query options
 */
export interface QueryOptions {
  branch?: string;
  timeoutMs?: number;
  mode?: 'normal' | 'safe' | 'explain';
}

/**
 * Insert options
 */
export interface InsertOptions {
  branch?: string;
  autoCreate?: boolean;
  onConflict?: 'error' | 'ignore' | 'update';
}

/**
 * Create branch options
 */
export interface CreateBranchOptions {
  fromBranch?: string;
  atTimestamp?: string;
}

/**
 * Time travel query options
 */
export interface TimeTravelOptions {
  branch?: string;
}
