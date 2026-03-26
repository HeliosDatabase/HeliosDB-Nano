/**
 * HeliosDB Zapier Integration
 *
 * Provides Zapier triggers and actions for HeliosDB Nano:
 * - Triggers: New row, Query results, Vector search match
 * - Actions: Execute query, Insert row, Vector search, Save to memory
 */

const { version } = require('./package.json');

// Authentication
const authentication = {
  type: 'custom',
  test: {
    url: '{{bundle.authData.url}}/health',
    method: 'GET',
  },
  fields: [
    {
      key: 'url',
      label: 'HeliosDB URL',
      type: 'string',
      required: true,
      helpText: 'Your HeliosDB Nano instance URL (e.g., https://your-instance.example.com)',
    },
    {
      key: 'api_key',
      label: 'API Key',
      type: 'string',
      required: false,
      helpText: 'Your HeliosDB API key (if authentication is enabled)',
    },
  ],
  connectionLabel: (z, bundle) => {
    return bundle.authData.url;
  },
};

// Helper: Make authenticated request
const makeRequest = async (z, bundle, options) => {
  const headers = {
    'Content-Type': 'application/json',
  };

  if (bundle.authData.api_key) {
    headers['X-API-Key'] = bundle.authData.api_key;
  }

  const response = await z.request({
    ...options,
    url: `${bundle.authData.url}${options.url}`,
    headers: { ...headers, ...options.headers },
  });

  return response.data;
};

// ============================================================================
// Triggers
// ============================================================================

// Trigger: New Row
const newRowTrigger = {
  key: 'new_row',
  noun: 'Row',
  display: {
    label: 'New Row',
    description: 'Triggers when a new row is inserted into a table.',
  },
  operation: {
    inputFields: [
      {
        key: 'table',
        label: 'Table',
        type: 'string',
        required: true,
        helpText: 'Table name to monitor',
      },
      {
        key: 'branch',
        label: 'Branch',
        type: 'string',
        default: 'main',
        helpText: 'Database branch',
      },
    ],
    perform: async (z, bundle) => {
      const result = await makeRequest(z, bundle, {
        url: `/v1/branches/${bundle.inputData.branch}/query`,
        method: 'POST',
        body: {
          sql: `SELECT * FROM ${bundle.inputData.table} ORDER BY created_at DESC LIMIT 10`,
          params: [],
        },
      });

      return result.rows || [];
    },
    sample: {
      id: 1,
      name: 'Sample Row',
      created_at: new Date().toISOString(),
    },
  },
};

// Trigger: Query Results
const queryResultsTrigger = {
  key: 'query_results',
  noun: 'Query Result',
  display: {
    label: 'Query Results',
    description: 'Triggers with results from a custom SQL query.',
  },
  operation: {
    inputFields: [
      {
        key: 'sql',
        label: 'SQL Query',
        type: 'text',
        required: true,
        helpText: 'SQL query to execute (must include ORDER BY for proper polling)',
      },
      {
        key: 'branch',
        label: 'Branch',
        type: 'string',
        default: 'main',
      },
    ],
    perform: async (z, bundle) => {
      const result = await makeRequest(z, bundle, {
        url: `/v1/branches/${bundle.inputData.branch}/query`,
        method: 'POST',
        body: {
          sql: bundle.inputData.sql,
          params: [],
        },
      });

      return result.rows || [];
    },
    sample: {
      id: 1,
      result: 'Sample Result',
    },
  },
};

// Trigger: Vector Search Match
const vectorSearchTrigger = {
  key: 'vector_match',
  noun: 'Vector Match',
  display: {
    label: 'Vector Search Match',
    description: 'Triggers when a vector search finds matching documents.',
  },
  operation: {
    inputFields: [
      {
        key: 'store_name',
        label: 'Vector Store',
        type: 'string',
        required: true,
      },
      {
        key: 'query_text',
        label: 'Search Query',
        type: 'string',
        required: true,
        helpText: 'Text to search for similar documents',
      },
      {
        key: 'min_score',
        label: 'Minimum Score',
        type: 'number',
        default: '0.8',
        helpText: 'Minimum similarity score (0-1)',
      },
      {
        key: 'top_k',
        label: 'Max Results',
        type: 'integer',
        default: '5',
      },
    ],
    perform: async (z, bundle) => {
      const result = await makeRequest(z, bundle, {
        url: `/v1/vectors/stores/${bundle.inputData.store_name}/search/text`,
        method: 'POST',
        body: {
          text: bundle.inputData.query_text,
          top_k: bundle.inputData.top_k,
          min_score: bundle.inputData.min_score,
        },
      });

      return result.results || [];
    },
    sample: {
      id: 'doc_123',
      score: 0.95,
      content: 'Sample matching document',
      metadata: {},
    },
  },
};

// ============================================================================
// Actions
// ============================================================================

// Action: Execute Query
const executeQueryAction = {
  key: 'execute_query',
  noun: 'Query',
  display: {
    label: 'Execute SQL Query',
    description: 'Execute a SQL query against HeliosDB.',
  },
  operation: {
    inputFields: [
      {
        key: 'sql',
        label: 'SQL Query',
        type: 'text',
        required: true,
        helpText: 'SQL query to execute',
      },
      {
        key: 'branch',
        label: 'Branch',
        type: 'string',
        default: 'main',
      },
    ],
    perform: async (z, bundle) => {
      const result = await makeRequest(z, bundle, {
        url: `/v1/branches/${bundle.inputData.branch}/query`,
        method: 'POST',
        body: {
          sql: bundle.inputData.sql,
          params: [],
        },
      });

      return {
        success: true,
        rows: result.rows,
        row_count: result.rows ? result.rows.length : 0,
      };
    },
    sample: {
      success: true,
      rows: [{ id: 1, name: 'Sample' }],
      row_count: 1,
    },
  },
};

// Action: Insert Row
const insertRowAction = {
  key: 'insert_row',
  noun: 'Row',
  display: {
    label: 'Insert Row',
    description: 'Insert a new row into a table.',
  },
  operation: {
    inputFields: [
      {
        key: 'table',
        label: 'Table',
        type: 'string',
        required: true,
      },
      {
        key: 'data',
        label: 'Row Data',
        type: 'string',
        required: true,
        helpText: 'JSON object with column values (e.g., {"name": "John", "email": "john@example.com"})',
      },
      {
        key: 'branch',
        label: 'Branch',
        type: 'string',
        default: 'main',
      },
    ],
    perform: async (z, bundle) => {
      const data = JSON.parse(bundle.inputData.data);

      const result = await makeRequest(z, bundle, {
        url: `/v1/branches/${bundle.inputData.branch}/tables/${bundle.inputData.table}/data`,
        method: 'POST',
        body: {
          rows: [data],
        },
      });

      return {
        success: true,
        inserted: result.inserted || 1,
      };
    },
    sample: {
      success: true,
      inserted: 1,
    },
  },
};

// Action: Vector Search
const vectorSearchAction = {
  key: 'vector_search',
  noun: 'Search Result',
  display: {
    label: 'Vector Search',
    description: 'Search for similar documents using semantic similarity.',
  },
  operation: {
    inputFields: [
      {
        key: 'store_name',
        label: 'Vector Store',
        type: 'string',
        required: true,
      },
      {
        key: 'query',
        label: 'Search Query',
        type: 'string',
        required: true,
        helpText: 'Text to search for',
      },
      {
        key: 'top_k',
        label: 'Max Results',
        type: 'integer',
        default: '10',
      },
    ],
    perform: async (z, bundle) => {
      const result = await makeRequest(z, bundle, {
        url: `/v1/vectors/stores/${bundle.inputData.store_name}/search/text`,
        method: 'POST',
        body: {
          text: bundle.inputData.query,
          top_k: bundle.inputData.top_k,
        },
      });

      return {
        results: result.results || [],
        count: result.results ? result.results.length : 0,
      };
    },
    sample: {
      results: [{ id: 'doc_1', score: 0.95, content: 'Similar document' }],
      count: 1,
    },
  },
};

// Action: Store Text with Embedding
const storeTextAction = {
  key: 'store_text',
  noun: 'Document',
  display: {
    label: 'Store Text',
    description: 'Store text with automatic embedding generation.',
  },
  operation: {
    inputFields: [
      {
        key: 'store_name',
        label: 'Vector Store',
        type: 'string',
        required: true,
      },
      {
        key: 'text',
        label: 'Text Content',
        type: 'text',
        required: true,
      },
      {
        key: 'metadata',
        label: 'Metadata',
        type: 'string',
        helpText: 'JSON metadata (optional)',
      },
    ],
    perform: async (z, bundle) => {
      const metadata = bundle.inputData.metadata
        ? JSON.parse(bundle.inputData.metadata)
        : {};

      const result = await makeRequest(z, bundle, {
        url: `/v1/vectors/stores/${bundle.inputData.store_name}/texts`,
        method: 'POST',
        body: {
          texts: [bundle.inputData.text],
          metadatas: [metadata],
        },
      });

      return {
        success: true,
        stored_count: result.stored_count || 1,
        ids: result.ids || [],
      };
    },
    sample: {
      success: true,
      stored_count: 1,
      ids: ['doc_123'],
    },
  },
};

// Action: Save Agent Memory
const saveMemoryAction = {
  key: 'save_memory',
  noun: 'Memory',
  display: {
    label: 'Save to Agent Memory',
    description: 'Save a message to agent conversation memory.',
  },
  operation: {
    inputFields: [
      {
        key: 'session_id',
        label: 'Session ID',
        type: 'string',
        required: true,
      },
      {
        key: 'role',
        label: 'Role',
        type: 'string',
        choices: ['user', 'assistant', 'system'],
        required: true,
      },
      {
        key: 'content',
        label: 'Content',
        type: 'text',
        required: true,
      },
    ],
    perform: async (z, bundle) => {
      const result = await makeRequest(z, bundle, {
        url: `/v1/agents/memory/${bundle.inputData.session_id}/add`,
        method: 'POST',
        body: {
          role: bundle.inputData.role,
          content: bundle.inputData.content,
        },
      });

      return {
        success: true,
        message: result,
      };
    },
    sample: {
      success: true,
      message: { role: 'user', content: 'Hello', timestamp: new Date().toISOString() },
    },
  },
};

// Action: Create Branch
const createBranchAction = {
  key: 'create_branch',
  noun: 'Branch',
  display: {
    label: 'Create Branch',
    description: 'Create a new database branch.',
  },
  operation: {
    inputFields: [
      {
        key: 'name',
        label: 'Branch Name',
        type: 'string',
        required: true,
      },
      {
        key: 'from_branch',
        label: 'Source Branch',
        type: 'string',
        default: 'main',
      },
    ],
    perform: async (z, bundle) => {
      const result = await makeRequest(z, bundle, {
        url: '/v1/branches',
        method: 'POST',
        body: {
          name: bundle.inputData.name,
          from_branch: bundle.inputData.from_branch,
        },
      });

      return result;
    },
    sample: {
      name: 'feature-branch',
      created_at: new Date().toISOString(),
    },
  },
};

// ============================================================================
// Searches (for dynamic dropdowns)
// ============================================================================

const listTablesSearch = {
  key: 'list_tables',
  noun: 'Table',
  display: {
    label: 'List Tables',
    description: 'Lists available tables.',
    hidden: true,
  },
  operation: {
    perform: async (z, bundle) => {
      const result = await makeRequest(z, bundle, {
        url: '/v1/branches/main/query',
        method: 'POST',
        body: {
          sql: "SELECT table_name FROM information_schema.tables WHERE table_schema = 'public'",
          params: [],
        },
      });

      return (result.rows || []).map((row) => ({
        id: row.table_name,
        name: row.table_name,
      }));
    },
  },
};

const listVectorStoresSearch = {
  key: 'list_vector_stores',
  noun: 'Vector Store',
  display: {
    label: 'List Vector Stores',
    description: 'Lists available vector stores.',
    hidden: true,
  },
  operation: {
    perform: async (z, bundle) => {
      const result = await makeRequest(z, bundle, {
        url: '/v1/vectors/stores',
        method: 'GET',
      });

      return (result.data || []).map((store) => ({
        id: store.name,
        name: store.name,
      }));
    },
  },
};

// ============================================================================
// App Definition
// ============================================================================

module.exports = {
  version,
  platformVersion: require('zapier-platform-core').version,

  authentication,

  triggers: {
    [newRowTrigger.key]: newRowTrigger,
    [queryResultsTrigger.key]: queryResultsTrigger,
    [vectorSearchTrigger.key]: vectorSearchTrigger,
  },

  actions: {
    [executeQueryAction.key]: executeQueryAction,
    [insertRowAction.key]: insertRowAction,
    [vectorSearchAction.key]: vectorSearchAction,
    [storeTextAction.key]: storeTextAction,
    [saveMemoryAction.key]: saveMemoryAction,
    [createBranchAction.key]: createBranchAction,
  },

  searches: {
    [listTablesSearch.key]: listTablesSearch,
    [listVectorStoresSearch.key]: listVectorStoresSearch,
  },
};
