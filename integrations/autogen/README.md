# HeliosDB AutoGen Integration

Integration package for using HeliosDB with Microsoft AutoGen multi-agent framework.

## Installation

```bash
pip install heliosdb-autogen
```

Or install from source:

```bash
cd integrations/autogen
pip install -e .
```

## Quick Start

### Basic Agent with HeliosDB Tools

```python
from autogen import UserProxyAgent
from heliosdb_autogen import HeliosDBAgent

# Create HeliosDB-powered agent
agent = HeliosDBAgent(
    name="data_assistant",
    heliosdb_url="http://localhost:8080",
    api_key="your-api-key",
    llm_config={"model": "gpt-4"}
)

# Create user proxy
user = UserProxyAgent(
    name="user",
    human_input_mode="ALWAYS"
)

# Start conversation
user.initiate_chat(agent, message="List all tables in the database")
```

### Adding HeliosDB Tools to Existing Agents

```python
from autogen import AssistantAgent
from heliosdb_autogen import HeliosDBTools

# Create tools
tools = HeliosDBTools(
    heliosdb_url="http://localhost:8080",
    api_key="your-api-key"
)

# Add to agent
assistant = AssistantAgent(
    name="assistant",
    llm_config={
        "model": "gpt-4",
        "tools": tools.get_tool_definitions()
    }
)

# Register tool executors
tools.register_tools(assistant)
```

## Features

### Pre-built Agents

#### HeliosDBAgent
General-purpose agent with full HeliosDB capabilities:
- SQL queries and execution
- Vector search
- Agent memory
- Database branching
- Time-travel queries

#### DataAnalystAgent
Specialized for data analysis:
```python
from heliosdb_autogen import DataAnalystAgent

analyst = DataAnalystAgent(
    heliosdb_url="http://localhost:8080",
    llm_config={"model": "gpt-4"}
)
```

#### RAGAgent
Optimized for retrieval-augmented generation:
```python
from heliosdb_autogen import RAGAgent

rag = RAGAgent(
    heliosdb_url="http://localhost:8080",
    default_vector_store="documents",
    llm_config={"model": "gpt-4"}
)

# Search and index
results = await rag.search("machine learning concepts")
doc_id = await rag.index("New information to store")
```

### Tools

Available tools for function calling:

| Tool | Description |
|------|-------------|
| `heliosdb_query` | Execute SQL queries |
| `heliosdb_execute` | Execute SQL statements |
| `heliosdb_vector_search` | Semantic vector search |
| `heliosdb_store_text` | Store text with embedding |
| `heliosdb_nl_query` | Natural language to SQL |
| `heliosdb_list_tables` | List database tables |
| `heliosdb_describe_table` | Get table schema |
| `heliosdb_memory_add` | Store in agent memory |
| `heliosdb_memory_search` | Search agent memory |
| `heliosdb_create_branch` | Create database branch |
| `heliosdb_query_at` | Time-travel queries |

### Memory Management

#### AgentMemoryManager
Persistent memory across sessions:
```python
from heliosdb_autogen import AgentMemoryManager

memory = AgentMemoryManager("http://localhost:8080")

# Store memories
await memory.store("session_123", "user", "What is Python?")
await memory.store("session_123", "assistant", "Python is a programming language.")

# Retrieve
messages = await memory.get_recent("session_123", limit=10)

# Search semantically
relevant = await memory.search("session_123", "programming languages")
```

#### ConversationMemory
Track conversation history:
```python
from heliosdb_autogen import ConversationMemory

conv = ConversationMemory(
    heliosdb_url="http://localhost:8080",
    session_id="chat_456"
)

await conv.add_user_message("Hello!")
await conv.add_assistant_message("Hi there!")

messages = await conv.get_messages()
```

#### SemanticMemory
Long-term knowledge storage:
```python
from heliosdb_autogen import SemanticMemory

knowledge = SemanticMemory(
    heliosdb_url="http://localhost:8080",
    store_name="facts"
)

# Store facts
await knowledge.remember("The capital of France is Paris.")
await knowledge.remember("Python was created by Guido van Rossum.")

# Recall relevant facts
facts = await knowledge.recall("European capitals")
```

### Retrievers

#### HeliosDBRetriever
Vector-based document retrieval:
```python
from heliosdb_autogen import HeliosDBRetriever

retriever = HeliosDBRetriever(
    heliosdb_url="http://localhost:8080",
    store_name="documents",
    top_k=5
)

docs = await retriever.retrieve("How does authentication work?")
context = await retriever.retrieve_as_context("user permissions")
```

#### HybridRetriever
Combine vector search with SQL:
```python
from heliosdb_autogen import HybridRetriever

retriever = HybridRetriever(
    heliosdb_url="http://localhost:8080",
    vector_store="articles",
    table_name="article_metadata"
)

results = await retriever.search(
    query="machine learning",
    sql_filter="category = 'tech'"
)
```

#### MultiStoreRetriever
Search across multiple vector stores:
```python
from heliosdb_autogen import MultiStoreRetriever

retriever = MultiStoreRetriever(
    heliosdb_url="http://localhost:8080",
    stores=["docs", "code", "faq"]
)

# Merged results sorted by relevance
docs = await retriever.search_merged("authentication flow", top_k=10)
```

### Group Chat with Persistence

```python
from autogen import GroupChat, UserProxyAgent
from heliosdb_autogen import HeliosDBAgent, HeliosDBGroupChatManager

# Create specialized agents
analyst = HeliosDBAgent(name="analyst", ...)
coder = HeliosDBAgent(name="coder", ...)
reviewer = HeliosDBAgent(name="reviewer", ...)

# Create group chat
groupchat = GroupChat(
    agents=[analyst, coder, reviewer],
    messages=[],
    max_round=10
)

# Persistent manager
manager = HeliosDBGroupChatManager(
    groupchat=groupchat,
    heliosdb_url="http://localhost:8080",
    session_id="project_alpha",
    llm_config={"model": "gpt-4"}
)

# All messages are automatically persisted
user = UserProxyAgent(name="user")
user.initiate_chat(manager, message="Build a data pipeline")

# Later: retrieve relevant context
context = await manager.get_relevant_context("data pipeline architecture")
```

## Advanced Usage

### Selective Tool Categories

```python
from heliosdb_autogen import HeliosDBToolkit

toolkit = HeliosDBToolkit("http://localhost:8080")

# Get only SQL tools
sql_tools = toolkit.get_sql_tools()

# Get only vector tools
vector_tools = toolkit.get_vector_tools()

# Get only memory tools
memory_tools = toolkit.get_memory_tools()
```

### Agent with Persistent Memory

```python
from heliosdb_autogen import HeliosDBAgent

agent = HeliosDBAgent(
    name="assistant",
    heliosdb_url="http://localhost:8080",
    llm_config={"model": "gpt-4"}
).with_memory("session_789")

# Agent automatically persists and recalls memories
await agent.remember("User prefers concise answers")
relevant = await agent.recall("user preferences")
```

### Create Agent Team

```python
from heliosdb_autogen import create_agent_team

team = create_agent_team(
    heliosdb_url="http://localhost:8080",
    llm_config={"model": "gpt-4"}
)

analyst = team["data_analyst"]
rag = team["rag_agent"]
general = team["general"]
```

## API Reference

### Client

```python
from heliosdb_autogen import HeliosDBClient

client = HeliosDBClient(
    base_url="http://localhost:8080",
    api_key="your-key",
    branch="main"
)

# SQL operations
result = await client.query("SELECT * FROM users WHERE id = $1", [1])
affected = await client.execute("UPDATE users SET name = $1 WHERE id = $2", ["Alice", 1])

# Vector operations
results = await client.vector_search("docs", "hello world", top_k=5)
doc_id = await client.store_text("docs", "Some text", {"source": "user"})

# Memory operations
await client.memory_add("session", "user", "Hello")
messages = await client.memory_get("session", limit=10)
relevant = await client.memory_search("session", "greetings", top_k=5)

# Branching
branches = await client.list_branches()
await client.create_branch("feature", from_branch="main")
await client.merge_branch("feature", "main")

# Time-travel
result = await client.query_at("SELECT * FROM users", "2024-01-01T00:00:00Z")
```

## Requirements

- Python 3.9+
- pyautogen >= 0.2.0
- httpx >= 0.25.0
- pydantic >= 2.0.0

## License

Apache-2.0
