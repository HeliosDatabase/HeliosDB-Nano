/**
 * Agent memory operations
 */

import type { HeliosDB } from '../client/heliosdb';
import type { MemoryMessage, MemorySearchResult } from '../types';

/**
 * Agent memory for conversation history
 *
 * @example
 * ```typescript
 * const memory = db.agentMemory('session-123');
 *
 * await memory.addMessage('user', 'Hello!');
 * await memory.addMessage('assistant', 'Hi there!');
 *
 * const relevant = await memory.search('greeting', { topK: 5 });
 * ```
 */
export class AgentMemory {
  constructor(
    private readonly client: HeliosDB,
    readonly sessionId: string
  ) {}

  /**
   * Add a message to memory
   *
   * @param role - Message role
   * @param content - Message content
   * @param options - Additional options
   * @returns Message ID
   */
  async addMessage(
    role: 'user' | 'assistant' | 'system' | 'tool',
    content: string,
    options?: {
      metadata?: Record<string, unknown>;
      generateEmbedding?: boolean;
    }
  ): Promise<string> {
    const data = await this.client._request<{ id: string }>(
      'POST',
      `/v1/agents/memory/${this.sessionId}/add`,
      {
        role,
        content,
        metadata: options?.metadata,
        generate_embedding: options?.generateEmbedding ?? true,
      }
    );

    return data.id;
  }

  /**
   * Get recent messages from memory
   */
  async getMessages(limit = 50): Promise<MemoryMessage[]> {
    const data = await this.client._request<{
      session_id: string;
      messages: Array<{
        id: string;
        role: 'user' | 'assistant' | 'system' | 'tool';
        content: string;
        metadata: Record<string, unknown>;
        timestamp: string;
      }>;
    }>('GET', `/v1/agents/memory/${this.sessionId}`, undefined, {
      limit: String(limit),
    });

    return data.messages.map((m) => ({
      id: m.id,
      role: m.role,
      content: m.content,
      metadata: m.metadata,
      timestamp: new Date(m.timestamp),
    }));
  }

  /**
   * Semantic search in memory
   */
  async search(
    query: string,
    options?: {
      topK?: number;
      filter?: Record<string, unknown>;
    }
  ): Promise<MemorySearchResult[]> {
    const data = await this.client._request<{
      results: Array<{
        message: {
          id: string;
          role: 'user' | 'assistant' | 'system' | 'tool';
          content: string;
          metadata: Record<string, unknown>;
          timestamp: string;
        };
        score: number;
      }>;
    }>('POST', `/v1/agents/memory/${this.sessionId}/search`, {
      query,
      top_k: options?.topK ?? 5,
      filter: options?.filter,
    });

    return data.results.map((r) => ({
      message: {
        id: r.message.id,
        role: r.message.role,
        content: r.message.content,
        metadata: r.message.metadata,
        timestamp: new Date(r.message.timestamp),
      },
      score: r.score,
    }));
  }

  /**
   * Clear all messages from memory
   */
  async clear(): Promise<void> {
    await this.client._request('DELETE', `/v1/agents/memory/${this.sessionId}`);
  }

  /**
   * Save context (LangChain-compatible)
   */
  async saveContext(
    inputs: Record<string, unknown>,
    outputs: Record<string, unknown>
  ): Promise<void> {
    const userInput = (inputs.input as string) ?? JSON.stringify(inputs);
    const assistantOutput = (outputs.output as string) ?? JSON.stringify(outputs);

    await this.addMessage('user', userInput);
    await this.addMessage('assistant', assistantOutput);
  }

  /**
   * Load memory variables (LangChain-compatible)
   */
  async loadMemoryVariables(): Promise<{ history: string }> {
    const messages = await this.getMessages();

    const history = messages
      .map((m) => {
        switch (m.role) {
          case 'user':
            return `Human: ${m.content}`;
          case 'assistant':
            return `AI: ${m.content}`;
          default:
            return `${m.role.charAt(0).toUpperCase() + m.role.slice(1)}: ${m.content}`;
        }
      })
      .join('\n');

    return { history };
  }
}
