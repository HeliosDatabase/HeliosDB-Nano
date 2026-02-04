/**
 * Branch operations and fluent query builder
 */

import type { HeliosDB } from './heliosdb';
import type { QueryResult, MergeResult } from '../types';

/**
 * Branch context for use with withBranch
 */
export class BranchContext {
  /** @internal */
  _merged = false;

  constructor(
    private readonly client: HeliosDB,
    readonly name: string,
    readonly parent: string
  ) {}

  /**
   * Whether this branch has been merged
   */
  get merged(): boolean {
    return this._merged;
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

  /**
   * Merge this branch into parent
   */
  async merge(strategy: 'fast_forward' | 'three_way' | 'rebase' = 'three_way'): Promise<MergeResult> {
    const data = await this.client._request<{
      success: boolean;
      conflicts: Array<{
        table: string;
        row_id: string;
        source_value: Record<string, unknown>;
        target_value: Record<string, unknown>;
      }>;
      changes_applied: number;
    }>('POST', `/v1/branches/${this.name}/merge`, {
      target: this.parent,
      strategy,
    });

    this._merged = true;

    return {
      success: data.success,
      conflicts: data.conflicts.map((c) => ({
        table: c.table,
        rowId: c.row_id,
        sourceValue: c.source_value,
        targetValue: c.target_value,
      })),
      changesApplied: data.changes_applied,
    };
  }
}

/**
 * Fluent query builder for table operations
 *
 * @example
 * ```typescript
 * const users = await db.branches('main')
 *   .table('users')
 *   .select(['id', 'name', 'email'])
 *   .where({ active: true })
 *   .orderBy('created_at', { descending: true })
 *   .limit(10)
 *   .execute();
 * ```
 */
export class TableQuery {
  private selectColumns?: string[];
  private whereClause?: Record<string, unknown>;
  private orderByColumn?: string;
  private orderDescending = false;
  private limitValue?: number;
  private offsetValue?: number;

  constructor(
    private readonly client: HeliosDB,
    private readonly branch: string,
    private readonly table: string
  ) {}

  /**
   * Select specific columns
   */
  select(columns?: string[]): this {
    this.selectColumns = columns;
    return this;
  }

  /**
   * Add WHERE conditions
   */
  where(conditions: Record<string, unknown>): this {
    this.whereClause = conditions;
    return this;
  }

  /**
   * Add ORDER BY
   */
  orderBy(column: string, options?: { descending?: boolean }): this {
    this.orderByColumn = column;
    this.orderDescending = options?.descending ?? false;
    return this;
  }

  /**
   * Limit results
   */
  limit(n: number): this {
    this.limitValue = n;
    return this;
  }

  /**
   * Offset results
   */
  offset(n: number): this {
    this.offsetValue = n;
    return this;
  }

  /**
   * Build SQL query
   */
  private buildSql(): { sql: string; params: unknown[] } {
    // SELECT clause
    const cols = this.selectColumns
      ? this.selectColumns.map((c) => `"${c}"`).join(', ')
      : '*';

    let sql = `SELECT ${cols} FROM "${this.table}"`;
    const params: unknown[] = [];

    // WHERE clause
    if (this.whereClause) {
      const conditions: string[] = [];
      let paramIndex = 1;
      for (const [key, value] of Object.entries(this.whereClause)) {
        conditions.push(`"${key}" = $${paramIndex}`);
        params.push(value);
        paramIndex++;
      }
      sql += ` WHERE ${conditions.join(' AND ')}`;
    }

    // ORDER BY
    if (this.orderByColumn) {
      const direction = this.orderDescending ? 'DESC' : 'ASC';
      sql += ` ORDER BY "${this.orderByColumn}" ${direction}`;
    }

    // LIMIT
    if (this.limitValue !== undefined) {
      sql += ` LIMIT ${this.limitValue}`;
    }

    // OFFSET
    if (this.offsetValue !== undefined) {
      sql += ` OFFSET ${this.offsetValue}`;
    }

    return { sql, params };
  }

  /**
   * Execute the query
   */
  async execute<T = Record<string, unknown>>(): Promise<QueryResult<T>> {
    const { sql, params } = this.buildSql();
    return this.client.query<T>(sql, params, { branch: this.branch });
  }

  /**
   * Execute and return as array of objects
   */
  async toObjects<T = Record<string, unknown>>(): Promise<T[]> {
    const result = await this.execute<T>();
    return result.toObjects();
  }
}
