import {
  IExecuteFunctions,
  INodeExecutionData,
  INodeType,
  INodeTypeDescription,
  NodeOperationError,
} from 'n8n-workflow';

export class HeliosDb implements INodeType {
  description: INodeTypeDescription = {
    displayName: 'HeliosDB',
    name: 'heliosDb',
    icon: 'file:heliosdb.svg',
    group: ['transform'],
    version: 1,
    subtitle: '={{$parameter["operation"]}}',
    description: 'Interact with HeliosDB-Lite database',
    defaults: {
      name: 'HeliosDB',
    },
    inputs: ['main'],
    outputs: ['main'],
    credentials: [
      {
        name: 'heliosDbApi',
        required: true,
      },
    ],
    properties: [
      {
        displayName: 'Operation',
        name: 'operation',
        type: 'options',
        noDataExpression: true,
        options: [
          {
            name: 'Execute Query',
            value: 'query',
            description: 'Execute a SQL query',
            action: 'Execute a SQL query',
          },
          {
            name: 'Vector Search',
            value: 'vectorSearch',
            description: 'Search by vector similarity',
            action: 'Search by vector similarity',
          },
          {
            name: 'Store Embedding',
            value: 'storeEmbedding',
            description: 'Store text with embedding',
            action: 'Store text with embedding',
          },
          {
            name: 'Agent Memory - Save',
            value: 'memorySave',
            description: 'Save to agent memory',
            action: 'Save to agent memory',
          },
          {
            name: 'Agent Memory - Load',
            value: 'memoryLoad',
            description: 'Load from agent memory',
            action: 'Load from agent memory',
          },
          {
            name: 'Create Branch',
            value: 'createBranch',
            description: 'Create a new branch',
            action: 'Create a new branch',
          },
          {
            name: 'Merge Branch',
            value: 'mergeBranch',
            description: 'Merge a branch',
            action: 'Merge a branch',
          },
          {
            name: 'Insert Data',
            value: 'insert',
            description: 'Insert rows into a table',
            action: 'Insert rows into a table',
          },
        ],
        default: 'query',
      },
      // Query operation
      {
        displayName: 'SQL',
        name: 'sql',
        type: 'string',
        typeOptions: {
          rows: 4,
        },
        displayOptions: {
          show: {
            operation: ['query'],
          },
        },
        default: 'SELECT * FROM users LIMIT 10',
        description: 'SQL query to execute',
      },
      {
        displayName: 'Branch',
        name: 'branch',
        type: 'string',
        default: 'main',
        description: 'Branch to operate on',
      },
      // Vector search
      {
        displayName: 'Store Name',
        name: 'storeName',
        type: 'string',
        displayOptions: {
          show: {
            operation: ['vectorSearch', 'storeEmbedding'],
          },
        },
        default: 'documents',
        description: 'Vector store name',
      },
      {
        displayName: 'Query Vector',
        name: 'queryVector',
        type: 'json',
        displayOptions: {
          show: {
            operation: ['vectorSearch'],
          },
        },
        default: '[]',
        description: 'Vector for similarity search',
      },
      {
        displayName: 'Top K',
        name: 'topK',
        type: 'number',
        displayOptions: {
          show: {
            operation: ['vectorSearch'],
          },
        },
        default: 10,
        description: 'Number of results',
      },
      // Store embedding
      {
        displayName: 'Text',
        name: 'text',
        type: 'string',
        displayOptions: {
          show: {
            operation: ['storeEmbedding'],
          },
        },
        default: '',
        description: 'Text to embed and store',
      },
      {
        displayName: 'Metadata',
        name: 'metadata',
        type: 'json',
        displayOptions: {
          show: {
            operation: ['storeEmbedding'],
          },
        },
        default: '{}',
        description: 'Metadata for the embedding',
      },
      // Agent memory
      {
        displayName: 'Session ID',
        name: 'sessionId',
        type: 'string',
        displayOptions: {
          show: {
            operation: ['memorySave', 'memoryLoad'],
          },
        },
        default: '',
        description: 'Agent session ID',
      },
      {
        displayName: 'Role',
        name: 'role',
        type: 'options',
        displayOptions: {
          show: {
            operation: ['memorySave'],
          },
        },
        options: [
          { name: 'User', value: 'user' },
          { name: 'Assistant', value: 'assistant' },
          { name: 'System', value: 'system' },
        ],
        default: 'user',
      },
      {
        displayName: 'Content',
        name: 'content',
        type: 'string',
        displayOptions: {
          show: {
            operation: ['memorySave'],
          },
        },
        default: '',
        description: 'Message content',
      },
      // Branch operations
      {
        displayName: 'Branch Name',
        name: 'branchName',
        type: 'string',
        displayOptions: {
          show: {
            operation: ['createBranch', 'mergeBranch'],
          },
        },
        default: '',
        description: 'Branch name',
      },
      {
        displayName: 'Source Branch',
        name: 'sourceBranch',
        type: 'string',
        displayOptions: {
          show: {
            operation: ['createBranch'],
          },
        },
        default: 'main',
        description: 'Branch to create from',
      },
      {
        displayName: 'Target Branch',
        name: 'targetBranch',
        type: 'string',
        displayOptions: {
          show: {
            operation: ['mergeBranch'],
          },
        },
        default: 'main',
        description: 'Branch to merge into',
      },
      // Insert operation
      {
        displayName: 'Table',
        name: 'table',
        type: 'string',
        displayOptions: {
          show: {
            operation: ['insert'],
          },
        },
        default: '',
        description: 'Table to insert into',
      },
      {
        displayName: 'Data',
        name: 'data',
        type: 'json',
        displayOptions: {
          show: {
            operation: ['insert'],
          },
        },
        default: '[]',
        description: 'Array of row objects to insert',
      },
    ],
  };

  async execute(this: IExecuteFunctions): Promise<INodeExecutionData[][]> {
    const items = this.getInputData();
    const returnData: INodeExecutionData[] = [];
    const credentials = await this.getCredentials('heliosDbApi');

    const baseUrl = credentials.url as string;
    const apiKey = credentials.apiKey as string;

    const operation = this.getNodeParameter('operation', 0) as string;
    const branch = this.getNodeParameter('branch', 0, 'main') as string;

    const headers: Record<string, string> = {
      'Content-Type': 'application/json',
    };
    if (apiKey) {
      headers['X-API-Key'] = apiKey;
    }

    for (let i = 0; i < items.length; i++) {
      try {
        let response: any;

        switch (operation) {
          case 'query': {
            const sql = this.getNodeParameter('sql', i) as string;
            response = await this.helpers.request({
              method: 'POST',
              url: `${baseUrl}/v1/branches/${branch}/query`,
              headers,
              body: { sql, params: [] },
              json: true,
            });
            break;
          }

          case 'vectorSearch': {
            const storeName = this.getNodeParameter('storeName', i) as string;
            const queryVector = JSON.parse(this.getNodeParameter('queryVector', i) as string);
            const topK = this.getNodeParameter('topK', i) as number;

            response = await this.helpers.request({
              method: 'POST',
              url: `${baseUrl}/v1/vectors/stores/${storeName}/search`,
              headers,
              body: { vector: queryVector, top_k: topK },
              json: true,
            });
            break;
          }

          case 'storeEmbedding': {
            const storeName = this.getNodeParameter('storeName', i) as string;
            const text = this.getNodeParameter('text', i) as string;
            const metadata = JSON.parse(this.getNodeParameter('metadata', i) as string);

            response = await this.helpers.request({
              method: 'POST',
              url: `${baseUrl}/v1/vectors/stores/${storeName}/texts`,
              headers,
              body: { texts: [text], metadatas: [metadata] },
              json: true,
            });
            break;
          }

          case 'memorySave': {
            const sessionId = this.getNodeParameter('sessionId', i) as string;
            const role = this.getNodeParameter('role', i) as string;
            const content = this.getNodeParameter('content', i) as string;

            response = await this.helpers.request({
              method: 'POST',
              url: `${baseUrl}/v1/agents/memory/${sessionId}/add`,
              headers,
              body: { role, content },
              json: true,
            });
            break;
          }

          case 'memoryLoad': {
            const sessionId = this.getNodeParameter('sessionId', i) as string;

            response = await this.helpers.request({
              method: 'GET',
              url: `${baseUrl}/v1/agents/memory/${sessionId}`,
              headers,
              json: true,
            });
            break;
          }

          case 'createBranch': {
            const branchName = this.getNodeParameter('branchName', i) as string;
            const sourceBranch = this.getNodeParameter('sourceBranch', i) as string;

            response = await this.helpers.request({
              method: 'POST',
              url: `${baseUrl}/v1/branches`,
              headers,
              body: { name: branchName, from_branch: sourceBranch },
              json: true,
            });
            break;
          }

          case 'mergeBranch': {
            const branchName = this.getNodeParameter('branchName', i) as string;
            const targetBranch = this.getNodeParameter('targetBranch', i) as string;

            response = await this.helpers.request({
              method: 'POST',
              url: `${baseUrl}/v1/branches/${branchName}/merge`,
              headers,
              body: { target: targetBranch },
              json: true,
            });
            break;
          }

          case 'insert': {
            const table = this.getNodeParameter('table', i) as string;
            const data = JSON.parse(this.getNodeParameter('data', i) as string);

            response = await this.helpers.request({
              method: 'POST',
              url: `${baseUrl}/v1/branches/${branch}/tables/${table}/data`,
              headers,
              body: { rows: Array.isArray(data) ? data : [data] },
              json: true,
            });
            break;
          }

          default:
            throw new NodeOperationError(this.getNode(), `Unknown operation: ${operation}`);
        }

        returnData.push({
          json: response,
          pairedItem: i,
        });
      } catch (error: any) {
        if (this.continueOnFail()) {
          returnData.push({
            json: { error: error.message },
            pairedItem: i,
          });
          continue;
        }
        throw new NodeOperationError(this.getNode(), error.message, { itemIndex: i });
      }
    }

    return [returnData];
  }
}
