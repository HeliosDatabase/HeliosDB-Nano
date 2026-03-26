/**
 * HeliosDB main client
 */

import { request } from 'undici';
import {
  QueryResult,
  Column,
  TableInfo,
  TableSchema,
  Branch,
  HealthStatus,
  InferredSchema,
  QueryOptions,
  InsertOptions,
  CreateBranchOptions,
  TimeTravelOptions,
} from '../types';
import { createErrorFromResponse, ValidationError } from '../errors';
import { VectorStore, VectorStoreConfig } from '../vector/store';
import { AgentMemory } from '../memory/agent-memory';
import { BranchContext, TableQuery } from './branch';

/**
 * Configuration for HeliosDB client
 */
export interface HeliosDBConfig {
  /** Server URL */
  url: string;
  /** API key for authentication */
  apiKey?: string;
  /** JWT token for authentication */
  jwtToken?: string;
  /** Connection timeout in ms */
  connectTimeout?: number;
  /** Read timeout in ms */
  readTimeout?: number;
  /** Default branch */
  defaultBranch?: string;
  /** Extra headers */
  headers?: Record<string, string>;
}

/**
 * Internal query result implementation
 */
class QueryResultImpl<T = Record<string, unknown>> implements QueryResult<T> {
  columns: Column[];
  rows: unknown[][];
  rowCount: number;
  executionTimeMs: number;

  constructor(data: {
    columns: Column[];
    rows: unknown[][];
    row_count: number;
    execution_time_ms: number;
  }) {
    this.columns = data.columns;
    this.rows = data.rows;
    this.rowCount = data.row_count;
    this.executionTimeMs = data.execution_time_ms;
  }

  toObjects(): T[] {
    const colNames = this.columns.map((c) => c.name);
    return this.rows.map((row) => {
      const obj: Record<string, unknown> = {};
      for (let i = 0; i < colNames.length; i++) {
        obj[colNames[i]!] = row[i];
      }
      return obj as T;
    });
  }
}

/**
 * HeliosDB client for interacting with HeliosDB Nano
 *
 * @example
 * ```typescript
 * const db = new HeliosDB({
 *   url: 'http://localhost:8080',
 *   apiKey: 'your-api-key'
 * });
 *
 * // Execute queries
 * const result = await db.query('SELECT * FROM users');
 * console.log(result.toObjects());
 *
 * // Vector operations
 * const store = db.vectorStore('embeddings', { dimension: 1536 });
 * await store.upsert([{ id: 'doc1', vector: [...], metadata: {} }]);
 * ```
 */
export class HeliosDB {
  private readonly config: Required<
    Pick<HeliosDBConfig, 'url' | 'connectTimeout' | 'readTimeout' | 'defaultBranch'>
  > &
    HeliosDBConfig;

  constructor(config: HeliosDBConfig) {
    this.config = {
      ...config,
      url: config.url.replace(/\/$/, ''),
      connectTimeout: config.connectTimeout ?? 10000,
      readTimeout: config.readTimeout ?? 30000,
      defaultBranch: config.defaultBranch ?? 'main',
    };
  }

  /**
   * Create client from environment variables
   */
  static fromEnv(): HeliosDB {
    const url = process.env.HELIOSDB_URL ?? 'http://localhost:8080';
    const apiKey = process.env.HELIOSDB_API_KEY;
    const jwtToken = process.env.HELIOSDB_JWT_TOKEN;
    const defaultBranch = process.env.HELIOSDB_DEFAULT_BRANCH ?? 'main';

    return new HeliosDB({ url, apiKey, jwtToken, defaultBranch });
  }

  /**
   * Build request headers
   */
  private buildHeaders(): Record<string, string> {
    const headers: Record<string, string> = {
      'Content-Type': 'application/json',
      Accept: 'application/json',
      'User-Agent': 'heliosdb-typescript/2.6.0',
    };

    if (this.config.apiKey) {
      headers['X-API-Key'] = this.config.apiKey;
    }
    if (this.config.jwtToken) {
      headers['Authorization'] = `Bearer ${this.config.jwtToken}`;
    }

    return { ...headers, ...this.config.headers };
  }

  /**
   * Make HTTP request
   */
  private async request<T>(
    method: string,
    path: string,
    body?: unknown,
    params?: Record<string, string>
  ): Promise<T> {
    let url = `${this.config.url}${path}`;

    if (params) {
      const searchParams = new URLSearchParams(params);
      url += `?${searchParams.toString()}`;
    }

    const response = await request(url, {
      method,
      headers: this.buildHeaders(),
      body: body ? JSON.stringify(body) : undefined,
      bodyTimeout: this.config.readTimeout,
      headersTimeout: this.config.connectTimeout,
    });

    const data = (await response.body.json()) as Record<string, unknown>;

    if (response.statusCode >= 400) {
      throw createErrorFromResponse(response.statusCode, data);
    }

    return data as T;
  }

  // ==========================================================================
  // Health & Info
  // ==========================================================================

  /**
   * Check server health
   */
  async health(): Promise<HealthStatus> {
    const data = await this.request<{
      status: 'healthy' | 'degraded' | 'unhealthy';
      version: string;
      uptime_seconds: number;
    }>('GET', '/health');

    return {
      status: data.status,
      version: data.version,
      uptimeSeconds: data.uptime_seconds,
    };
  }

  // ==========================================================================
  // Query Execution
  // ==========================================================================

  /**
   * Execute a SQL query
   *
   * @param sql - SQL query string
   * @param params - Query parameters
   * @param options - Query options
   * @returns Query results
   *
   * @example
   * ```typescript
   * const result = await db.query<User>(
   *   'SELECT * FROM users WHERE active = $1',
   *   [true]
   * );
   * for (const user of result.toObjects()) {
   *   console.log(user.name);
   * }
   * ```
   */
  async query<T = Record<string, unknown>>(
    sql: string,
    params?: unknown[],
    options?: QueryOptions
  ): Promise<QueryResult<T>> {
    const branch = options?.branch ?? this.config.defaultBranch;

    const data = await this.request<{
      columns: Column[];
      rows: unknown[][];
      row_count: number;
      execution_time_ms: number;
    }>('POST', `/v1/branches/${branch}/query`, {
      sql,
      params: params ?? [],
      timeout_ms: options?.timeoutMs ?? 30000,
      mode: options?.mode ?? 'normal',
    });

    return new QueryResultImpl<T>(data);
  }

  /**
   * Execute a SQL statement (INSERT, UPDATE, DELETE)
   *
   * @param sql - SQL statement
   * @param params - Query parameters
   * @param options - Query options
   * @returns Number of affected rows
   */
  async execute(sql: string, params?: unknown[], options?: QueryOptions): Promise<number> {
    const result = await this.query(sql, params, options);
    return result.rowCount;
  }

  // ==========================================================================
  // Branch Management
  // ==========================================================================

  /**
   * List all branches
   */
  async listBranches(): Promise<Branch[]> {
    const data = await this.request<{
      branches: Array<{
        name: string;
        parent: string | null;
        created_at: string;
        commit_count: number;
      }>;
    }>('GET', '/v1/branches');

    return data.branches.map((b) => ({
      name: b.name,
      parent: b.parent,
      createdAt: new Date(b.created_at),
      commitCount: b.commit_count,
    }));
  }

  /**
   * Get branch details
   */
  async getBranch(name: string): Promise<Branch> {
    const data = await this.request<{
      name: string;
      parent: string | null;
      created_at: string;
      commit_count: number;
    }>('GET', `/v1/branches/${name}`);

    return {
      name: data.name,
      parent: data.parent,
      createdAt: new Date(data.created_at),
      commitCount: data.commit_count,
    };
  }

  /**
   * Create a new branch
   */
  async createBranch(name: string, options?: CreateBranchOptions): Promise<Branch> {
    const data = await this.request<{
      name: string;
      parent: string | null;
      created_at: string;
      commit_count: number;
    }>('POST', '/v1/branches', {
      name,
      from_branch: options?.fromBranch ?? 'main',
      at_timestamp: options?.atTimestamp,
    });

    return {
      name: data.name,
      parent: data.parent,
      createdAt: new Date(data.created_at),
      commitCount: data.commit_count,
    };
  }

  /**
   * Delete a branch
   */
  async deleteBranch(name: string): Promise<void> {
    if (name === 'main') {
      throw new ValidationError('Cannot delete main branch');
    }
    await this.request('DELETE', `/v1/branches/${name}`);
  }

  /**
   * Use a branch with automatic cleanup
   *
   * @example
   * ```typescript
   * await db.withBranch('experiment-1', async (branch) => {
   *   await branch.execute("UPDATE config SET value = 'test'");
   *   // Branch is auto-deleted on exit
   * });
   * ```
   */
  async withBranch<T>(
    name: string,
    fn: (branch: BranchContext) => Promise<T>,
    options?: { fromBranch?: string; autoCleanup?: boolean }
  ): Promise<T> {
    const fromBranch = options?.fromBranch ?? 'main';
    const autoCleanup = options?.autoCleanup ?? true;

    await this.createBranch(name, { fromBranch });
    const ctx = new BranchContext(this, name, fromBranch);

    try {
      return await fn(ctx);
    } finally {
      if (autoCleanup && !ctx.merged) {
        try {
          await this.deleteBranch(name);
        } catch {
          // Best effort cleanup
        }
      }
    }
  }

  /**
   * Get a branch accessor for fluent API
   *
   * @example
   * ```typescript
   * const users = await db.branches('main')
   *   .table('users')
   *   .select(['id', 'name'])
   *   .where({ active: true })
   *   .limit(10)
   *   .execute();
   * ```
   */
  branches(name: string): BranchAccessor {
    return new BranchAccessor(this, name);
  }

  // ==========================================================================
  // Table Operations
  // ==========================================================================

  /**
   * List tables in a branch
   */
  async listTables(branch?: string): Promise<TableInfo[]> {
    const b = branch ?? this.config.defaultBranch;
    const data = await this.request<{
      tables: Array<{
        name: string;
        row_count: number;
        size_bytes: number;
        created_at: string;
      }>;
    }>('GET', `/v1/branches/${b}/tables`);

    return data.tables.map((t) => ({
      name: t.name,
      rowCount: t.row_count,
      sizeBytes: t.size_bytes,
      createdAt: new Date(t.created_at),
    }));
  }

  /**
   * Get table schema
   */
  async getTableSchema(table: string, branch?: string): Promise<TableSchema> {
    const b = branch ?? this.config.defaultBranch;
    const data = await this.request<{
      name: string;
      columns: Array<{
        name: string;
        type: string;
        nullable?: boolean;
        default?: string;
      }>;
      primary_key: string[];
      indexes: Array<{
        name: string;
        columns: string[];
        type: string;
        unique: boolean;
      }>;
    }>('GET', `/v1/branches/${b}/tables/${table}`);

    return {
      name: data.name,
      columns: data.columns,
      primaryKey: data.primary_key,
      indexes: data.indexes.map((idx) => ({
        name: idx.name,
        columns: idx.columns,
        type: idx.type as 'btree' | 'hash' | 'hnsw' | 'ivfflat',
        unique: idx.unique,
      })),
    };
  }

  // ==========================================================================
  // Data Operations
  // ==========================================================================

  /**
   * Insert rows into a table
   */
  async insert(
    table: string,
    rows: Record<string, unknown>[],
    options?: InsertOptions
  ): Promise<number> {
    const branch = options?.branch ?? this.config.defaultBranch;

    const data = await this.request<{ inserted: number }>(
      'POST',
      `/v1/branches/${branch}/tables/${table}/data`,
      {
        rows,
        on_conflict: options?.onConflict ?? 'error',
      },
      { auto_create: String(options?.autoCreate ?? false) }
    );

    return data.inserted;
  }

  /**
   * Infer schema from data
   */
  async inferSchema(
    data: Record<string, unknown>[],
    tableName?: string
  ): Promise<InferredSchema> {
    const result = await this.request<{
      inferred_schema: Record<string, string>;
      create_sql: string;
      confidence: number;
    }>('POST', '/v1/schema/infer', {
      data,
      table_name: tableName ?? 'inferred',
    });

    return {
      inferredSchema: result.inferred_schema,
      createSql: result.create_sql,
      confidence: result.confidence,
    };
  }

  // ==========================================================================
  // Vector Store
  // ==========================================================================

  /**
   * Get or create a vector store
   *
   * @example
   * ```typescript
   * const store = db.vectorStore('documents', { dimension: 1536 });
   *
   * await store.upsert([
   *   { id: 'doc1', vector: [...], metadata: { title: 'Hello' } }
   * ]);
   *
   * const results = await store.search(queryVector, { topK: 10 });
   * ```
   */
  vectorStore(name: string, config?: VectorStoreConfig): VectorStore {
    return new VectorStore(this, name, config);
  }

  // ==========================================================================
  // Agent Memory
  // ==========================================================================

  /**
   * Get an agent memory interface
   *
   * @example
   * ```typescript
   * const memory = db.agentMemory('session-123');
   *
   * await memory.addMessage('user', 'Hello!');
   * await memory.addMessage('assistant', 'Hi there!');
   *
   * const results = await memory.search('greeting', { topK: 5 });
   * ```
   */
  agentMemory(sessionId: string): AgentMemory {
    return new AgentMemory(this, sessionId);
  }

  // ==========================================================================
  // Time Travel
  // ==========================================================================

  /**
   * Execute a query at a historical timestamp
   */
  async timeTravelQuery<T = Record<string, unknown>>(
    sql: string,
    timestamp: string,
    params?: unknown[],
    options?: TimeTravelOptions
  ): Promise<QueryResult<T>> {
    const branch = options?.branch ?? this.config.defaultBranch;

    const data = await this.request<{
      columns: Column[];
      rows: unknown[][];
      row_count: number;
      execution_time_ms: number;
    }>('POST', `/v1/branches/${branch}/time-travel`, {
      sql,
      timestamp,
      params: params ?? [],
    });

    return new QueryResultImpl<T>(data);
  }

  // Internal request method for use by other classes
  /** @internal */
  _request<T>(
    method: string,
    path: string,
    body?: unknown,
    params?: Record<string, string>
  ): Promise<T> {
    return this.request(method, path, body, params);
  }

  /** @internal */
  get _defaultBranch(): string {
    return this.config.defaultBranch;
  }
}

/**
 * Branch accessor for fluent API
 */
class BranchAccessor {
  constructor(
    private readonly client: HeliosDB,
    readonly name: string
  ) {}

  /**
   * Start a fluent query on a table
   */
  table(name: string): TableQuery {
    return new TableQuery(this.client, this.name, name);
  }

  /**
   * Execute a query on this branch
   */
  query<T = Record<string, unknown>>(
    sql: string,
    params?: unknown[]
  ): Promise<QueryResult<T>> {
    return this.client.query<T>(sql, params, { branch: this.name });
  }

  /**
   * Execute a statement on this branch
   */
  execute(sql: string, params?: unknown[]): Promise<number> {
    return this.client.execute(sql, params, { branch: this.name });
  }
}
