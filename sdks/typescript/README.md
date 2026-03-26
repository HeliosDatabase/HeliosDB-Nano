# HeliosDB TypeScript SDK

Official TypeScript/JavaScript SDK for HeliosDB Nano - an AI-native embedded database with PostgreSQL compatibility, vector search, time-travel, and branching.

## Installation

```bash
npm install @heliosdb/client
# or
yarn add @heliosdb/client
# or
pnpm add @heliosdb/client
```

## Quick Start

```typescript
import { HeliosDB } from '@heliosdb/client';

// Connect to server
const db = new HeliosDB({
  url: 'http://localhost:8080',
  apiKey: process.env.HELIOSDB_API_KEY,
});

// Or use environment variables
const db = HeliosDB.fromEnv();

// Execute SQL queries
interface User {
  id: number;
  name: string;
  email: string;
}

const result = await db.query<User>('SELECT * FROM users WHERE active = $1', [true]);
for (const user of result.toObjects()) {
  console.log(user.name);
}
```

## Features

### Type-Safe Queries

```typescript
interface User {
  id: number;
  name: string;
  email: string;
  createdAt: Date;
}

// Generic type parameter for type-safe results
const users = await db.query<User>('SELECT * FROM users WHERE id = $1', [123]);

// Access typed objects
for (const user of users.toObjects()) {
  console.log(user.name); // TypeScript knows this is string
}
```

### Fluent Query Builder

```typescript
const activeUsers = await db
  .branches('main')
  .table('users')
  .select(['id', 'name', 'email'])
  .where({ active: true, role: 'admin' })
  .orderBy('createdAt', { descending: true })
  .limit(10)
  .execute();

// Convert to objects
const users = await db.branches('main').table('users').toObjects<User>();
```

### Vector Store

```typescript
// Create vector store
const store = db.vectorStore('documents', { dimension: 1536 });

// Upsert vectors
await store.upsert([
  {
    id: 'doc1',
    vector: embedding1,
    metadata: { title: 'Introduction', source: 'docs' },
  },
  {
    id: 'doc2',
    vector: embedding2,
    metadata: { title: 'Getting Started', source: 'docs' },
  },
]);

// Search similar vectors
const results = await store.search(queryVector, {
  topK: 10,
  filter: { source: 'docs' },
  includeMetadata: true,
});

for (const result of results) {
  console.log(`${result.id}: ${result.score}`);
  console.log(result.metadata);
}

// Add texts with auto-embedding
const ids = await store.addTexts(['Hello world', 'AI is amazing'], {
  metadatas: [{ source: 'intro' }, { source: 'main' }],
});
```

### Agent Memory

```typescript
// Create agent memory
const memory = db.agentMemory('session-123');

// Add messages
await memory.addMessage('user', "What's the weather like?");
await memory.addMessage('assistant', "Let me check the weather for you...");

// LangChain-compatible save context
await memory.saveContext({ input: 'Tell me about AI' }, { output: 'AI is fascinating...' });

// Semantic search in memory
const relevant = await memory.search('weather forecast', { topK: 5 });
for (const r of relevant) {
  console.log(`${r.message.content} (score: ${r.score})`);
}

// Get conversation history
const messages = await memory.getMessages(50);

// Load memory variables (LangChain-compatible)
const { history } = await memory.loadMemoryVariables();
```

### Branching (Git-like)

```typescript
// Create and use branch with automatic cleanup
await db.withBranch('experiment-1', async (branch) => {
  await branch.execute("UPDATE config SET value = 'test'");
  const result = await branch.query('SELECT * FROM config');
  // Branch is auto-deleted on exit
});

// Or keep changes by merging
await db.withBranch(
  'feature-1',
  async (branch) => {
    await branch.execute('INSERT INTO features (name) VALUES ($1)', ['new-feature']);
    await branch.merge(); // Merge to parent (main)
  },
  { autoCleanup: false }
);

// Manual branch management
const branchInfo = await db.createBranch('my-branch', { fromBranch: 'main' });
const branches = await db.listBranches();
await db.deleteBranch('my-branch');
```

### Time Travel

```typescript
// Query historical data
const historicalData = await db.timeTravelQuery<User>(
  'SELECT * FROM users',
  '2024-01-01T00:00:00Z'
);

// Compare current vs historical
const currentUsers = await db.query<User>('SELECT COUNT(*) as count FROM users');
const pastUsers = await db.timeTravelQuery<{ count: number }>(
  'SELECT COUNT(*) as count FROM users',
  '2024-06-01T00:00:00Z'
);
```

### Schema Inference

```typescript
// Infer schema from data
const schema = await db.inferSchema(
  [
    { id: 1, name: 'Alice', embedding: [0.1, 0.2, 0.3] },
    { id: 2, name: 'Bob', embedding: [0.4, 0.5, 0.6] },
  ],
  'users'
);

console.log(schema.inferredSchema);
// { id: 'INTEGER', name: 'TEXT', embedding: 'VECTOR(3)' }

console.log(schema.createSql);
// CREATE TABLE users (id INTEGER, name TEXT, embedding VECTOR(3))
```

## Configuration

### Environment Variables

```bash
export HELIOSDB_URL="http://localhost:8080"
export HELIOSDB_API_KEY="your-api-key"
export HELIOSDB_JWT_TOKEN="optional-jwt-token"
export HELIOSDB_DEFAULT_BRANCH="main"
```

### Config Object

```typescript
import { HeliosDB, HeliosDBConfig } from '@heliosdb/client';

const config: HeliosDBConfig = {
  url: 'http://localhost:8080',
  apiKey: 'your-key',
  jwtToken: 'optional-jwt',
  connectTimeout: 10000,
  readTimeout: 30000,
  defaultBranch: 'main',
  headers: {
    'X-Custom-Header': 'value',
  },
};

const db = new HeliosDB(config);
```

## Error Handling

```typescript
import {
  HeliosDB,
  HeliosDBError,
  QueryError,
  AuthenticationError,
  NotFoundError,
  ValidationError,
  RateLimitError,
} from '@heliosdb/client';

try {
  const result = await db.query('INVALID SQL');
} catch (error) {
  if (error instanceof QueryError) {
    console.error(`Query failed: ${error.message}`);
    console.error(`SQL: ${error.sql}`);
  } else if (error instanceof AuthenticationError) {
    console.error('Invalid credentials');
  } else if (error instanceof NotFoundError) {
    console.error(`Resource not found: ${error.resourceType}/${error.resourceId}`);
  } else if (error instanceof RateLimitError) {
    console.error(`Rate limited. Retry after: ${error.retryAfter}s`);
  } else if (error instanceof HeliosDBError) {
    console.error(`Error [${error.code}]: ${error.message}`);
  }
}
```

## Real-time Subscriptions (Future)

```typescript
// Coming in v2.7.0
db.subscribe('users', (change) => {
  console.log('Change type:', change.type); // insert, update, delete
  console.log('Old value:', change.oldValue);
  console.log('New value:', change.newValue);
});
```

## TypeScript Support

This SDK is written in TypeScript and provides full type definitions:

- Generic query results with type inference
- Strict null checking
- Comprehensive interface definitions
- JSDoc comments for IDE support

## Requirements

- Node.js >= 18.0.0
- TypeScript >= 5.0 (for TypeScript users)

## License

Apache 2.0
