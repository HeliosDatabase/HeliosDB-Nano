/**
 * Vector store operations
 */

import type { HeliosDB } from '../client/heliosdb';
import type {
  VectorStoreInfo,
  VectorEntry,
  VectorSearchResult,
  VectorSearchOptions,
} from '../types';

/**
 * Vector store configuration
 */
export interface VectorStoreConfig {
  /** Vector dimension (required for creation) */
  dimension?: number;
  /** Distance metric */
  metric?: 'cosine' | 'euclidean' | 'dot_product';
  /** Create store if it doesn't exist */
  createIfMissing?: boolean;
}

/**
 * Vector store for semantic search and embeddings
 *
 * @example
 * ```typescript
 * const store = db.vectorStore('documents', { dimension: 1536 });
 *
 * // Add vectors
 * await store.upsert([
 *   { id: 'doc1', vector: [...], metadata: { title: 'Hello' } }
 * ]);
 *
 * // Search
 * const results = await store.search(queryVector, { topK: 10 });
 * ```
 */
export class VectorStore {
  private info?: VectorStoreInfo;

  constructor(
    private readonly client: HeliosDB,
    readonly name: string,
    private readonly config?: VectorStoreConfig
  ) {
    if (config?.createIfMissing !== false && config?.dimension) {
      // Will lazily create on first use
    }
  }

  /**
   * Ensure store exists
   */
  private async ensureExists(): Promise<void> {
    if (this.info) return;

    try {
      this.info = await this.getInfo();
    } catch {
      if (this.config?.dimension) {
        await this.create();
      } else {
        throw new Error(`Vector store '${this.name}' does not exist and dimension not specified`);
      }
    }
  }

  /**
   * Get store info
   */
  async getInfo(): Promise<VectorStoreInfo> {
    const data = await this.client._request<{
      name: string;
      dimension: number;
      metric: string;
      vector_count: number;
      created_at: string;
    }>('GET', `/v1/vectors/stores/${this.name}`);

    return {
      name: data.name,
      dimension: data.dimension,
      metric: data.metric as 'cosine' | 'euclidean' | 'dot_product',
      vectorCount: data.vector_count,
      createdAt: new Date(data.created_at),
    };
  }

  /**
   * Create the vector store
   */
  private async create(): Promise<void> {
    if (!this.config?.dimension) {
      throw new Error('Dimension is required to create vector store');
    }

    const data = await this.client._request<{
      name: string;
      dimension: number;
      metric: string;
      vector_count: number;
      created_at: string;
    }>('POST', '/v1/vectors/stores', {
      name: this.name,
      dimension: this.config.dimension,
      metric: this.config.metric ?? 'cosine',
    });

    this.info = {
      name: data.name,
      dimension: data.dimension,
      metric: data.metric as 'cosine' | 'euclidean' | 'dot_product',
      vectorCount: data.vector_count,
      createdAt: new Date(data.created_at),
    };
  }

  /**
   * Upsert vectors
   *
   * @param vectors - Vectors to upsert
   * @returns Number of upserted vectors
   */
  async upsert(vectors: VectorEntry[]): Promise<number> {
    await this.ensureExists();

    const data = await this.client._request<{ upserted: number }>(
      'POST',
      `/v1/vectors/stores/${this.name}/vectors`,
      { vectors }
    );

    return data.upserted;
  }

  /**
   * Search for similar vectors
   *
   * @param vector - Query vector
   * @param options - Search options
   * @returns Search results
   */
  async search(vector: number[], options?: VectorSearchOptions): Promise<VectorSearchResult[]> {
    await this.ensureExists();

    const data = await this.client._request<{
      results: Array<{
        id: string;
        score: number;
        vector?: number[];
        metadata: Record<string, unknown>;
      }>;
    }>('POST', `/v1/vectors/stores/${this.name}/search`, {
      vector,
      top_k: options?.topK ?? 10,
      filter: options?.filter,
      include_metadata: options?.includeMetadata ?? true,
      include_vectors: options?.includeVectors ?? false,
    });

    return data.results;
  }

  /**
   * Add texts with automatic embedding
   *
   * @param texts - Texts to add
   * @param options - Options including metadata and embedding model
   * @returns IDs of added texts
   */
  async addTexts(
    texts: string[],
    options?: {
      metadatas?: Record<string, unknown>[];
      ids?: string[];
      embeddingModel?: string;
    }
  ): Promise<string[]> {
    await this.ensureExists();

    const data = await this.client._request<{ ids: string[] }>(
      'POST',
      `/v1/vectors/stores/${this.name}/texts`,
      {
        texts,
        metadatas: options?.metadatas,
        ids: options?.ids,
        embedding_model: options?.embeddingModel,
      }
    );

    return data.ids;
  }

  /**
   * Delete vectors by ID
   */
  async delete(ids: string[]): Promise<number> {
    const data = await this.client._request<{ deleted: number }>(
      'DELETE',
      `/v1/vectors/stores/${this.name}/vectors`,
      { ids }
    );

    return data.deleted;
  }

  /**
   * Drop this vector store
   */
  async drop(): Promise<void> {
    await this.client._request('DELETE', `/v1/vectors/stores/${this.name}`);
    this.info = undefined;
  }

  /**
   * Get vector count
   */
  async count(): Promise<number> {
    const info = await this.getInfo();
    return info.vectorCount;
  }
}
