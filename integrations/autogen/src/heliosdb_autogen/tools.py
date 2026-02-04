"""HeliosDB tools for AutoGen agents.

This module provides function tools that can be registered with AutoGen
agents for database operations.
"""

from typing import Any, Callable, Optional
import json
from functools import wraps
import asyncio

from .client import HeliosDBClient


def _make_sync(async_func: Callable[..., Any]) -> Callable[..., Any]:
    """Convert async function to sync for AutoGen tool compatibility."""
    @wraps(async_func)
    def wrapper(*args: Any, **kwargs: Any) -> Any:
        loop = asyncio.get_event_loop()
        if loop.is_running():
            # If we're in an async context, create a new loop in a thread
            import concurrent.futures
            with concurrent.futures.ThreadPoolExecutor() as pool:
                future = pool.submit(asyncio.run, async_func(*args, **kwargs))
                return future.result()
        return loop.run_until_complete(async_func(*args, **kwargs))
    return wrapper


class HeliosDBTools:
    """Collection of HeliosDB tools for AutoGen agents.

    This class provides tool definitions and implementations that can be
    used with AutoGen's function calling capabilities.

    Example:
        ```python
        from autogen import AssistantAgent
        from heliosdb_autogen import HeliosDBTools

        tools = HeliosDBTools("http://localhost:8080", api_key="key")

        assistant = AssistantAgent(
            name="assistant",
            llm_config={
                "tools": tools.get_tool_definitions(),
                "tool_choice": "auto",
            }
        )

        # Register tool executors
        tools.register_tools(assistant)
        ```
    """

    def __init__(
        self,
        heliosdb_url: str = "http://localhost:8080",
        api_key: Optional[str] = None,
        branch: str = "main",
    ):
        self.client = HeliosDBClient(heliosdb_url, api_key, branch)
        self._tools: dict[str, Callable[..., Any]] = {}
        self._setup_tools()

    def _setup_tools(self) -> None:
        """Set up all tool implementations."""
        self._tools = {
            "heliosdb_query": self._query,
            "heliosdb_execute": self._execute,
            "heliosdb_vector_search": self._vector_search,
            "heliosdb_store_text": self._store_text,
            "heliosdb_nl_query": self._nl_query,
            "heliosdb_list_tables": self._list_tables,
            "heliosdb_describe_table": self._describe_table,
            "heliosdb_memory_add": self._memory_add,
            "heliosdb_memory_search": self._memory_search,
            "heliosdb_create_branch": self._create_branch,
            "heliosdb_query_at": self._query_at,
        }

    @_make_sync
    async def _query(self, sql: str, params: Optional[str] = None) -> str:
        """Execute SQL query."""
        param_list = json.loads(params) if params else []
        result = await self.client.query(sql, param_list)
        return json.dumps({
            "rows": result.rows,
            "columns": result.columns,
            "row_count": len(result.rows),
        }, indent=2)

    @_make_sync
    async def _execute(self, sql: str, params: Optional[str] = None) -> str:
        """Execute SQL statement."""
        param_list = json.loads(params) if params else []
        affected = await self.client.execute(sql, param_list)
        return json.dumps({"rows_affected": affected})

    @_make_sync
    async def _vector_search(
        self,
        store: str,
        query: str,
        top_k: int = 5,
        min_score: Optional[float] = None,
    ) -> str:
        """Perform semantic vector search."""
        results = await self.client.vector_search(
            store, query, top_k, min_score
        )
        return json.dumps([
            {
                "id": r.id,
                "score": r.score,
                "content": r.content,
                "metadata": r.metadata,
            }
            for r in results
        ], indent=2)

    @_make_sync
    async def _store_text(
        self,
        store: str,
        text: str,
        metadata: Optional[str] = None,
    ) -> str:
        """Store text with embedding."""
        meta = json.loads(metadata) if metadata else None
        doc_id = await self.client.store_text(store, text, meta)
        return json.dumps({"id": doc_id})

    @_make_sync
    async def _nl_query(self, question: str) -> str:
        """Execute natural language query."""
        result, sql = await self.client.nl_query(question)
        return json.dumps({
            "sql": sql,
            "rows": result.rows,
            "columns": result.columns,
        }, indent=2)

    @_make_sync
    async def _list_tables(self) -> str:
        """List all tables in database."""
        result = await self.client.query(
            "SELECT table_name FROM information_schema.tables WHERE table_schema = 'public'"
        )
        tables = [row.get("table_name") for row in result.rows]
        return json.dumps({"tables": tables})

    @_make_sync
    async def _describe_table(self, table_name: str) -> str:
        """Get table schema information."""
        result = await self.client.query(
            """
            SELECT column_name, data_type, is_nullable, column_default
            FROM information_schema.columns
            WHERE table_name = $1
            ORDER BY ordinal_position
            """,
            [table_name]
        )
        return json.dumps({"columns": result.rows}, indent=2)

    @_make_sync
    async def _memory_add(
        self,
        session_id: str,
        role: str,
        content: str,
    ) -> str:
        """Add message to agent memory."""
        await self.client.memory_add(session_id, role, content)
        return json.dumps({"status": "stored"})

    @_make_sync
    async def _memory_search(
        self,
        session_id: str,
        query: str,
        top_k: int = 5,
    ) -> str:
        """Search agent memory."""
        results = await self.client.memory_search(session_id, query, top_k)
        return json.dumps([
            {"content": r.content, "score": r.score}
            for r in results
        ], indent=2)

    @_make_sync
    async def _create_branch(self, name: str, from_branch: Optional[str] = None) -> str:
        """Create database branch."""
        result = await self.client.create_branch(name, from_branch)
        return json.dumps(result)

    @_make_sync
    async def _query_at(
        self,
        sql: str,
        timestamp: str,
        params: Optional[str] = None,
    ) -> str:
        """Query at a specific point in time."""
        param_list = json.loads(params) if params else []
        result = await self.client.query_at(sql, timestamp, param_list)
        return json.dumps({
            "rows": result.rows,
            "columns": result.columns,
        }, indent=2)

    def get_tool_definitions(self) -> list[dict[str, Any]]:
        """Get AutoGen-compatible tool definitions.

        Returns:
            List of tool definitions for llm_config
        """
        return [
            {
                "type": "function",
                "function": {
                    "name": "heliosdb_query",
                    "description": "Execute a SQL query on HeliosDB and return results. Use parameterized queries with $1, $2, etc. for safety.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "sql": {
                                "type": "string",
                                "description": "SQL query with optional $1, $2 parameter placeholders"
                            },
                            "params": {
                                "type": "string",
                                "description": "JSON array of parameter values, e.g., '[1, \"hello\"]'"
                            }
                        },
                        "required": ["sql"]
                    }
                }
            },
            {
                "type": "function",
                "function": {
                    "name": "heliosdb_execute",
                    "description": "Execute a SQL statement (INSERT, UPDATE, DELETE) and return rows affected.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "sql": {
                                "type": "string",
                                "description": "SQL statement"
                            },
                            "params": {
                                "type": "string",
                                "description": "JSON array of parameter values"
                            }
                        },
                        "required": ["sql"]
                    }
                }
            },
            {
                "type": "function",
                "function": {
                    "name": "heliosdb_vector_search",
                    "description": "Perform semantic vector search to find similar documents.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "store": {
                                "type": "string",
                                "description": "Name of the vector store"
                            },
                            "query": {
                                "type": "string",
                                "description": "Search query text"
                            },
                            "top_k": {
                                "type": "integer",
                                "description": "Maximum number of results (default: 5)",
                                "default": 5
                            },
                            "min_score": {
                                "type": "number",
                                "description": "Minimum similarity score (0-1)"
                            }
                        },
                        "required": ["store", "query"]
                    }
                }
            },
            {
                "type": "function",
                "function": {
                    "name": "heliosdb_store_text",
                    "description": "Store text in a vector store with automatic embedding.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "store": {
                                "type": "string",
                                "description": "Name of the vector store"
                            },
                            "text": {
                                "type": "string",
                                "description": "Text content to store"
                            },
                            "metadata": {
                                "type": "string",
                                "description": "JSON object with metadata"
                            }
                        },
                        "required": ["store", "text"]
                    }
                }
            },
            {
                "type": "function",
                "function": {
                    "name": "heliosdb_nl_query",
                    "description": "Execute a natural language query that gets converted to SQL.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "question": {
                                "type": "string",
                                "description": "Natural language question about the data"
                            }
                        },
                        "required": ["question"]
                    }
                }
            },
            {
                "type": "function",
                "function": {
                    "name": "heliosdb_list_tables",
                    "description": "List all tables in the database.",
                    "parameters": {
                        "type": "object",
                        "properties": {}
                    }
                }
            },
            {
                "type": "function",
                "function": {
                    "name": "heliosdb_describe_table",
                    "description": "Get schema information for a table.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "table_name": {
                                "type": "string",
                                "description": "Name of the table"
                            }
                        },
                        "required": ["table_name"]
                    }
                }
            },
            {
                "type": "function",
                "function": {
                    "name": "heliosdb_memory_add",
                    "description": "Store a message in agent memory for later retrieval.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "session_id": {
                                "type": "string",
                                "description": "Memory session identifier"
                            },
                            "role": {
                                "type": "string",
                                "description": "Message role (user, assistant, system)"
                            },
                            "content": {
                                "type": "string",
                                "description": "Message content"
                            }
                        },
                        "required": ["session_id", "role", "content"]
                    }
                }
            },
            {
                "type": "function",
                "function": {
                    "name": "heliosdb_memory_search",
                    "description": "Search agent memory semantically.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "session_id": {
                                "type": "string",
                                "description": "Memory session identifier"
                            },
                            "query": {
                                "type": "string",
                                "description": "Search query"
                            },
                            "top_k": {
                                "type": "integer",
                                "description": "Maximum results (default: 5)",
                                "default": 5
                            }
                        },
                        "required": ["session_id", "query"]
                    }
                }
            },
            {
                "type": "function",
                "function": {
                    "name": "heliosdb_create_branch",
                    "description": "Create a new database branch for isolated changes.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "name": {
                                "type": "string",
                                "description": "New branch name"
                            },
                            "from_branch": {
                                "type": "string",
                                "description": "Parent branch (defaults to current)"
                            }
                        },
                        "required": ["name"]
                    }
                }
            },
            {
                "type": "function",
                "function": {
                    "name": "heliosdb_query_at",
                    "description": "Query the database at a specific point in time (time-travel).",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "sql": {
                                "type": "string",
                                "description": "SQL query"
                            },
                            "timestamp": {
                                "type": "string",
                                "description": "ISO 8601 timestamp"
                            },
                            "params": {
                                "type": "string",
                                "description": "JSON array of parameters"
                            }
                        },
                        "required": ["sql", "timestamp"]
                    }
                }
            },
        ]

    def get_tool_executor(self, tool_name: str) -> Optional[Callable[..., str]]:
        """Get executor function for a tool.

        Args:
            tool_name: Name of the tool

        Returns:
            Tool executor function or None
        """
        return self._tools.get(tool_name)

    def register_tools(self, agent: Any) -> None:
        """Register tool executors with an AutoGen agent.

        Args:
            agent: AutoGen agent to register tools with
        """
        for name, func in self._tools.items():
            if hasattr(agent, "register_function"):
                agent.register_function(
                    function_map={name: func}
                )


class HeliosDBToolkit:
    """Higher-level toolkit that provides categorized tools.

    This toolkit organizes tools into categories for easier management
    in complex multi-agent scenarios.
    """

    def __init__(
        self,
        heliosdb_url: str = "http://localhost:8080",
        api_key: Optional[str] = None,
    ):
        self._tools = HeliosDBTools(heliosdb_url, api_key)

    def get_sql_tools(self) -> list[dict[str, Any]]:
        """Get SQL-related tools only."""
        all_tools = self._tools.get_tool_definitions()
        sql_names = {"heliosdb_query", "heliosdb_execute", "heliosdb_list_tables", "heliosdb_describe_table"}
        return [t for t in all_tools if t["function"]["name"] in sql_names]

    def get_vector_tools(self) -> list[dict[str, Any]]:
        """Get vector search tools only."""
        all_tools = self._tools.get_tool_definitions()
        vector_names = {"heliosdb_vector_search", "heliosdb_store_text"}
        return [t for t in all_tools if t["function"]["name"] in vector_names]

    def get_memory_tools(self) -> list[dict[str, Any]]:
        """Get agent memory tools only."""
        all_tools = self._tools.get_tool_definitions()
        memory_names = {"heliosdb_memory_add", "heliosdb_memory_search"}
        return [t for t in all_tools if t["function"]["name"] in memory_names]

    def get_branching_tools(self) -> list[dict[str, Any]]:
        """Get branching and time-travel tools."""
        all_tools = self._tools.get_tool_definitions()
        branch_names = {"heliosdb_create_branch", "heliosdb_query_at"}
        return [t for t in all_tools if t["function"]["name"] in branch_names]

    def get_all_tools(self) -> list[dict[str, Any]]:
        """Get all tools."""
        return self._tools.get_tool_definitions()

    def get_tool_executor(self, tool_name: str) -> Optional[Callable[..., str]]:
        """Get tool executor."""
        return self._tools.get_tool_executor(tool_name)
