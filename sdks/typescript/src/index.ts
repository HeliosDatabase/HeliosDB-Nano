/**
 * HeliosDB TypeScript SDK
 *
 * Official TypeScript/JavaScript client for HeliosDB Nano - an AI-native
 * embedded database with PostgreSQL compatibility, vector search,
 * time-travel, and branching.
 *
 * @example
 * ```typescript
 * import { HeliosDB } from '@heliosdb/client';
 *
 * const db = new HeliosDB({
 *   url: 'http://localhost:8080',
 *   apiKey: process.env.HELIOSDB_API_KEY
 * });
 *
 * // Execute queries
 * const users = await db.query<User>('SELECT * FROM users WHERE active = $1', [true]);
 *
 * // Vector operations
 * const store = db.vectorStore('documents', { dimension: 1536 });
 * await store.upsert([{ id: 'doc1', vector: embedding, metadata: { title: 'Hello' } }]);
 * const results = await store.search(queryVector, { topK: 10 });
 * ```
 *
 * @packageDocumentation
 */

// Main client
export { HeliosDB, HeliosDBConfig } from './client/heliosdb';

// Vector operations
export { VectorStore, VectorStoreConfig } from './vector/store';

// Agent memory
export { AgentMemory } from './memory/agent-memory';

// Types
export type {
  QueryResult,
  Column,
  TableInfo,
  TableSchema,
  ColumnDefinition,
  IndexInfo,
  Branch,
  MergeResult,
  MergeConflict,
  VectorStoreInfo,
  VectorEntry,
  VectorSearchResult,
  VectorSearchOptions,
  MemoryMessage,
  MemorySearchResult,
  Document,
  DocumentChunk,
  DocumentSearchResult,
  ChatSession,
  ChatMessage,
  ChatSummary,
  HealthStatus,
  InferredSchema,
  ChangeRecord,
} from './types';

// Errors
export {
  HeliosDBError,
  ConnectionError,
  QueryError,
  AuthenticationError,
  NotFoundError,
  ConflictError,
  ValidationError,
  TimeoutError,
  RateLimitError,
} from './errors';

// Utilities
export { BranchContext, TableQuery } from './client/branch';
