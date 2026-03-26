# HeliosDB Retool Integration

Connect HeliosDB Nano to Retool for building internal tools with AI-native database capabilities.

## Setup

### 1. Create REST API Resource

1. In Retool, go to **Resources** → **Create New** → **REST API**
2. Configure the connection:
   - **Name**: `HeliosDB`
   - **Base URL**: Your HeliosDB instance URL (e.g., `https://your-instance.example.com`)
   - **Headers**: Add `X-API-Key` with your API key (if authentication is enabled)

### 2. Import Query Templates

The `heliosdb-datasource.json` file contains pre-built query templates. You can manually create these queries in Retool or use them as reference.

## Available Queries

### SQL Operations

| Query | Description |
|-------|-------------|
| `executeQuery` | Execute any SQL query |
| `listTables` | Get all tables in the database |
| `getTableSchema` | Get column information for a table |
| `insertRow` | Insert a new row into a table |
| `updateRows` | Update rows matching a condition |
| `deleteRows` | Delete rows matching a condition |

### Vector Search

| Query | Description |
|-------|-------------|
| `vectorSearch` | Semantic similarity search |
| `storeDocument` | Store text with auto-embedding |
| `listVectorStores` | List all vector stores |
| `createVectorStore` | Create a new vector store |

### Agent Memory

| Query | Description |
|-------|-------------|
| `agentMemoryLoad` | Load conversation history |
| `agentMemorySave` | Save a message to memory |
| `agentMemorySearch` | Semantic search over memory |

### Branching & Time Travel

| Query | Description |
|-------|-------------|
| `listBranches` | List all database branches |
| `createBranch` | Create a new branch |
| `mergeBranch` | Merge branches |
| `timeTravelQuery` | Query data at a point in time |

### AI Features

| Query | Description |
|-------|-------------|
| `chatCompletion` | RAG-enabled chat completion |
| `naturalLanguageQuery` | Query using natural language |

## Example Usage

### Building a Table Browser

```javascript
// Query 1: List all tables
const tables = await listTables.trigger();

// Query 2: Get schema for selected table
const schema = await getTableSchema.trigger({
  additionalScope: {
    table: tableSelect.value
  }
});

// Query 3: Execute custom query
const data = await executeQuery.trigger({
  additionalScope: {
    sql: sqlEditor.value,
    branch: branchSelect.value || 'main'
  }
});
```

### Building a Semantic Search Interface

```javascript
// Search for similar documents
const results = await vectorSearch.trigger({
  additionalScope: {
    storeName: 'knowledge_base',
    query: searchInput.value,
    topK: 10,
    minScore: 0.7
  }
});

// Display in table
table1.setData(results);
```

### Building a Chat Interface with RAG

```javascript
// Send message with RAG context
const response = await chatCompletion.trigger({
  additionalScope: {
    message: userInput.value,
    vectorStore: 'docs',
    systemPrompt: 'You are a helpful assistant for our documentation.'
  }
});

// Append to chat history
const messages = [...chatHistory.value, {
  role: 'user',
  content: userInput.value
}, {
  role: 'assistant',
  content: response.content
}];
chatHistory.setValue(messages);
```

### Natural Language to SQL

```javascript
// Convert question to SQL and execute
const result = await naturalLanguageQuery.trigger({
  additionalScope: {
    question: nlInput.value,
    branch: 'main'
  }
});

// Show generated SQL
sqlDisplay.setValue(result.sql);

// Show results
resultsTable.setData(result.results);
```

## Retool Components

### Recommended Component Structure

```
App
├── Header
│   ├── BranchSelector (Select)
│   └── ConnectionStatus (Text)
├── Sidebar
│   ├── TableList (Listbox)
│   └── VectorStoreList (Listbox)
├── Main Content
│   ├── Tabs
│   │   ├── SQL Editor (Code)
│   │   ├── Vector Search (Container)
│   │   ├── Chat Interface (Container)
│   │   └── Natural Language (Container)
│   └── Results Table
└── Footer
    └── Query Status (Text)
```

## Transformer Examples

### Parse Vector Search Results

```javascript
// Transform vector search results for display
const results = {{ vectorSearch.data }};
return results.map(r => ({
  id: r.id,
  score: (r.score * 100).toFixed(1) + '%',
  content: r.content.substring(0, 200) + '...',
  metadata: JSON.stringify(r.metadata)
}));
```

### Format Time Travel Results

```javascript
// Show diff between current and historical data
const current = {{ currentQuery.data }};
const historical = {{ timeTravelQuery.data }};

return current.map(row => {
  const hist = historical.find(h => h.id === row.id);
  return {
    ...row,
    _changed: hist ? JSON.stringify(row) !== JSON.stringify(hist) : true,
    _previous: hist || null
  };
});
```

## Best Practices

1. **Use Branches for Development**: Create branches for testing changes before merging to main
2. **Parameterize Queries**: Use Retool's `{{ }}` syntax to make queries dynamic
3. **Handle Errors**: Add error handling for failed queries
4. **Cache Results**: Use Retool's caching for frequently-accessed data
5. **Rate Limiting**: Consider adding debounce to search inputs

## Troubleshooting

### Connection Issues
- Verify the HeliosDB instance is running and accessible
- Check CORS settings if accessing from browser
- Verify API key is correct

### Query Errors
- Check SQL syntax
- Verify table and column names
- Check branch exists

### Vector Search Issues
- Ensure vector store exists
- Check embedding dimensions match
- Verify minimum score threshold
