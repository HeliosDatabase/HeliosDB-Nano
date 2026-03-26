# HeliosDB Python SDK

Official Python SDK for HeliosDB Nano - an AI-native embedded database with PostgreSQL compatibility, vector search, time-travel, and branching.

## Installation

```bash
pip install heliosdb
```

### Optional Dependencies

```bash
# For pandas DataFrame support
pip install heliosdb[pandas]

# For LangChain integration
pip install heliosdb[langchain]

# For LlamaIndex integration
pip install heliosdb[llamaindex]

# For local embeddings (sentence-transformers)
pip install heliosdb[embeddings]

# Install all optional dependencies
pip install heliosdb[all]
```

## Quick Start

```python
from heliosdb import HeliosDB

# Connect to server
db = HeliosDB.connect("http://localhost:8080", api_key="your-api-key")

# Or use environment variables
db = HeliosDB.from_env()  # Uses HELIOSDB_URL, HELIOSDB_API_KEY

# Execute SQL queries
result = db.query("SELECT * FROM users WHERE active = $1", [True])
for row in result.to_dicts():
    print(row["name"])

# Convert to pandas DataFrame
df = result.to_dataframe()
```

## Features

### SQL Queries

```python
# Parameterized queries
users = db.query(
    "SELECT * FROM users WHERE created_at > $1 AND status = $2",
    ["2024-01-01", "active"]
)

# Execute statements
affected = db.execute(
    "UPDATE users SET status = $1 WHERE id = $2",
    ["inactive", 123]
)
print(f"Updated {affected} rows")
```

### Vector Store

```python
# Create vector store
store = db.vector_store("documents", dimension=1536)

# Add vectors directly
store.upsert([
    {"id": "doc1", "vector": [0.1, 0.2, ...], "metadata": {"title": "Hello"}}
])

# Or add texts with auto-embedding (requires embedding provider)
store.add_texts(
    ["Hello world", "AI is amazing"],
    metadatas=[{"source": "intro"}, {"source": "main"}]
)

# Search by vector
results = store.search(query_vector, top_k=10)
for r in results:
    print(f"{r.id}: {r.score}")
```

### Agent Memory

```python
# Create agent memory
memory = db.agent_memory("session-123")

# Add messages
memory.add_message("user", "What's the weather?")
memory.add_message("assistant", "Let me check...")

# LangChain-compatible save_context
memory.save_context(
    {"input": "Tell me about AI"},
    {"output": "AI is fascinating..."}
)

# Semantic search in memory
relevant = memory.search("weather forecast", k=5)
for r in relevant:
    print(f"{r.message.content} (score: {r.score})")

# Get conversation history
messages = memory.get_messages(limit=10)
```

### Branching (Git-like)

```python
# Create and use branch with auto-cleanup
with db.branch("experiment-1") as branch:
    branch.execute("UPDATE config SET value = 'test'")
    result = branch.query("SELECT * FROM config")
    # Branch is auto-deleted on exit

# Or keep changes by merging
with db.branch("feature-1") as branch:
    branch.execute("INSERT INTO features ...")
    branch.merge()  # Merge to parent (main)
```

### Fluent Query Builder

```python
# Build queries fluently
users = db.branches("main") \
    .table("users") \
    .select(["id", "name", "email"]) \
    .where({"active": True}) \
    .order_by("created_at", descending=True) \
    .limit(10) \
    .execute()

# Convert to DataFrame
df = db.branches("main").table("users").to_dataframe()
```

### Time Travel

```python
# Query historical data
result = db.time_travel_query(
    "SELECT * FROM users",
    timestamp="2024-01-01T00:00:00Z"
)
```

### Schema Inference

```python
# Infer schema from data
schema = db.infer_schema([
    {"id": 1, "name": "Alice", "embedding": [0.1, 0.2, 0.3]},
    {"id": 2, "name": "Bob", "embedding": [0.4, 0.5, 0.6]},
], table_name="users")

print(schema.inferred_schema)
# {"id": "INTEGER", "name": "TEXT", "embedding": "VECTOR(3)"}

print(schema.create_sql)
# CREATE TABLE users (id INTEGER, name TEXT, embedding VECTOR(3))
```

## LangChain Integration

```python
from langchain_openai import OpenAIEmbeddings
from heliosdb.integrations.langchain import HeliosDBVectorStore, HeliosDBChatMemory

# Vector Store
embeddings = OpenAIEmbeddings()
vectorstore = HeliosDBVectorStore(
    connection_string="http://localhost:8080",
    collection_name="documents",
    embedding=embeddings,
)

# Add documents
vectorstore.add_documents(docs)

# Similarity search
results = vectorstore.similarity_search("AI tutorials", k=5)

# Chat Memory
memory = HeliosDBChatMemory(
    connection_string="http://localhost:8080",
    session_id="user-123",
)

# Use with LangChain agents
from langchain.agents import initialize_agent
agent = initialize_agent(tools, llm, memory=memory)
```

## LlamaIndex Integration

```python
from llama_index.core import VectorStoreIndex
from heliosdb.integrations.llamaindex import HeliosDBLlamaVectorStore

# Create vector store
vector_store = HeliosDBLlamaVectorStore(
    connection_string="http://localhost:8080",
    collection_name="documents",
)

# Build index
index = VectorStoreIndex.from_vector_store(vector_store)

# Query
query_engine = index.as_query_engine()
response = query_engine.query("What is HeliosDB?")
```

## Embedding Providers

```python
from heliosdb.embeddings import LocalEmbeddings, OpenAIEmbeddings, CohereEmbeddings

# Local (sentence-transformers) - no API needed
local = LocalEmbeddings(model="all-MiniLM-L6-v2")
vectors = local.embed_documents(["Hello world"])

# OpenAI
openai = OpenAIEmbeddings(
    api_key="sk-...",
    model="text-embedding-3-small"
)

# Cohere
cohere = CohereEmbeddings(
    api_key="...",
    model="embed-english-v3.0"
)
```

## Async Support

All operations have async variants:

```python
import asyncio
from heliosdb import HeliosDB

async def main():
    db = HeliosDB.connect("http://localhost:8080")

    # Async queries
    result = await db.aquery("SELECT * FROM users")

    # Async vector operations
    store = db.vector_store("docs", dimension=1536)
    await store.aupsert(vectors)
    results = await store.asearch(query_vector)

    # Async memory
    memory = db.agent_memory("session-1")
    await memory.aadd_message("user", "Hello!")

    await db.aclose()

asyncio.run(main())
```

## Configuration

### Environment Variables

```bash
export HELIOSDB_URL="http://localhost:8080"
export HELIOSDB_API_KEY="your-api-key"
export HELIOSDB_DEFAULT_BRANCH="main"
```

### Config Object

```python
from heliosdb import HeliosDB, HeliosDBConfig

config = HeliosDBConfig(
    url="http://localhost:8080",
    api_key="your-key",
    connect_timeout=10.0,
    read_timeout=30.0,
    max_retries=3,
    default_branch="main",
)

db = HeliosDB(config=config)
```

## Error Handling

```python
from heliosdb import HeliosDB
from heliosdb.exceptions import (
    HeliosDBError,
    QueryError,
    AuthenticationError,
    NotFoundError,
    ValidationError,
)

try:
    result = db.query("INVALID SQL")
except QueryError as e:
    print(f"Query failed: {e.message}")
    print(f"SQL: {e.sql}")
except AuthenticationError:
    print("Invalid credentials")
except NotFoundError as e:
    print(f"Resource not found: {e.resource_type}/{e.resource_id}")
except HeliosDBError as e:
    print(f"General error: {e.code} - {e.message}")
```

## License

Apache 2.0
