"""HeliosDB-powered agents for AutoGen.

This module provides pre-configured AutoGen agents with HeliosDB capabilities.
"""

from typing import Any, Optional, Union
import json

try:
    from autogen import AssistantAgent, ConversableAgent
    AUTOGEN_AVAILABLE = True
except ImportError:
    AUTOGEN_AVAILABLE = False
    # Provide stub classes for type hints
    class AssistantAgent:  # type: ignore
        pass
    class ConversableAgent:  # type: ignore
        pass

from .tools import HeliosDBTools
from .client import HeliosDBClient


class HeliosDBAgent(AssistantAgent if AUTOGEN_AVAILABLE else object):
    """AutoGen agent with built-in HeliosDB capabilities.

    This agent comes pre-configured with HeliosDB tools for SQL queries,
    vector search, agent memory, and database branching.

    Args:
        name: Agent name
        heliosdb_url: HeliosDB server URL
        api_key: Optional API key
        branch: Default database branch
        system_message: Optional custom system message
        llm_config: LLM configuration dict
        **kwargs: Additional AssistantAgent arguments

    Example:
        ```python
        from heliosdb_autogen import HeliosDBAgent

        agent = HeliosDBAgent(
            name="data_analyst",
            heliosdb_url="http://localhost:8080",
            api_key="your-api-key",
            llm_config={"model": "gpt-4"}
        )

        # Agent can now query databases, search vectors, etc.
        ```
    """

    DEFAULT_SYSTEM_MESSAGE = """You are a data analyst assistant with access to HeliosDB.
You can:
- Execute SQL queries to analyze data
- Perform semantic vector searches
- Store and retrieve information in agent memory
- Create database branches for isolated experiments
- Query historical data using time-travel

Always use parameterized queries ($1, $2, etc.) to prevent SQL injection.
When searching for information, consider using both SQL and vector search for best results.
Use database branches when making experimental changes.
"""

    def __init__(
        self,
        name: str,
        heliosdb_url: str = "http://localhost:8080",
        api_key: Optional[str] = None,
        branch: str = "main",
        system_message: Optional[str] = None,
        llm_config: Optional[dict[str, Any]] = None,
        **kwargs: Any,
    ):
        if not AUTOGEN_AVAILABLE:
            raise ImportError(
                "AutoGen is required. Install with: pip install pyautogen"
            )

        self.heliosdb_tools = HeliosDBTools(heliosdb_url, api_key, branch)
        self.heliosdb_client = HeliosDBClient(heliosdb_url, api_key, branch)

        # Build LLM config with tools
        config = llm_config or {}
        config["tools"] = self.heliosdb_tools.get_tool_definitions()

        super().__init__(
            name=name,
            system_message=system_message or self.DEFAULT_SYSTEM_MESSAGE,
            llm_config=config,
            **kwargs,
        )

        # Register tool executors
        self.heliosdb_tools.register_tools(self)

    def with_memory(self, session_id: str) -> "HeliosDBAgent":
        """Configure agent to use persistent memory.

        Args:
            session_id: Memory session identifier

        Returns:
            Self for chaining
        """
        self._memory_session_id = session_id
        return self

    async def remember(self, content: str, role: str = "assistant") -> None:
        """Store something in agent memory.

        Args:
            content: Content to remember
            role: Message role
        """
        if hasattr(self, "_memory_session_id"):
            await self.heliosdb_client.memory_add(
                self._memory_session_id, role, content
            )

    async def recall(self, query: str, top_k: int = 5) -> list[dict[str, Any]]:
        """Recall relevant memories.

        Args:
            query: Search query
            top_k: Maximum results

        Returns:
            List of relevant memories
        """
        if hasattr(self, "_memory_session_id"):
            results = await self.heliosdb_client.memory_search(
                self._memory_session_id, query, top_k
            )
            return [{"content": r.content, "score": r.score} for r in results]
        return []


class DataAnalystAgent(HeliosDBAgent):
    """Specialized agent for data analysis tasks.

    This agent is optimized for SQL querying, data exploration,
    and analytical insights.
    """

    DEFAULT_SYSTEM_MESSAGE = """You are an expert data analyst with access to HeliosDB.

Your capabilities:
1. SQL Analysis: Write and execute complex SQL queries for data analysis
2. Schema Exploration: Explore tables, columns, and relationships
3. Statistical Analysis: Calculate aggregations, trends, and patterns
4. Data Quality: Identify issues like nulls, duplicates, and outliers

Best Practices:
- Always start by listing tables and understanding the schema
- Use CTEs for complex queries
- Include appropriate filters to avoid scanning too much data
- Format results clearly for the user
- Explain your analysis approach
- Use parameterized queries for any user-provided values

When asked to analyze data:
1. First explore the relevant tables
2. Understand the data types and relationships
3. Write efficient queries
4. Present findings clearly with insights
"""

    def __init__(
        self,
        name: str = "data_analyst",
        heliosdb_url: str = "http://localhost:8080",
        api_key: Optional[str] = None,
        llm_config: Optional[dict[str, Any]] = None,
        **kwargs: Any,
    ):
        super().__init__(
            name=name,
            heliosdb_url=heliosdb_url,
            api_key=api_key,
            system_message=self.DEFAULT_SYSTEM_MESSAGE,
            llm_config=llm_config,
            **kwargs,
        )


class RAGAgent(HeliosDBAgent):
    """Agent specialized for Retrieval-Augmented Generation.

    This agent excels at combining vector search with structured
    data queries for comprehensive information retrieval.
    """

    DEFAULT_SYSTEM_MESSAGE = """You are a RAG (Retrieval-Augmented Generation) specialist with access to HeliosDB.

Your capabilities:
1. Vector Search: Find semantically similar documents and text
2. SQL Queries: Access structured data in tables
3. Hybrid Search: Combine vector and SQL results for comprehensive answers
4. Knowledge Storage: Store new information for future retrieval

Workflow:
1. When asked a question, first search relevant vector stores
2. If needed, complement with SQL queries for structured data
3. Synthesize information from multiple sources
4. Cite your sources when providing answers

Best Practices:
- Use vector search for semantic/conceptual queries
- Use SQL for factual/structured data
- Combine both for comprehensive answers
- Store important findings for future reference
- Always indicate confidence level in your answers
"""

    def __init__(
        self,
        name: str = "rag_agent",
        heliosdb_url: str = "http://localhost:8080",
        api_key: Optional[str] = None,
        default_vector_store: Optional[str] = None,
        llm_config: Optional[dict[str, Any]] = None,
        **kwargs: Any,
    ):
        super().__init__(
            name=name,
            heliosdb_url=heliosdb_url,
            api_key=api_key,
            system_message=self.DEFAULT_SYSTEM_MESSAGE,
            llm_config=llm_config,
            **kwargs,
        )
        self.default_vector_store = default_vector_store

    async def search(
        self,
        query: str,
        vector_store: Optional[str] = None,
        top_k: int = 5,
        min_score: float = 0.7,
    ) -> list[dict[str, Any]]:
        """Search for relevant documents.

        Args:
            query: Search query
            vector_store: Vector store name (uses default if not specified)
            top_k: Maximum results
            min_score: Minimum similarity score

        Returns:
            List of relevant documents
        """
        store = vector_store or self.default_vector_store
        if not store:
            raise ValueError("No vector store specified")

        results = await self.heliosdb_client.vector_search(
            store, query, top_k, min_score
        )
        return [
            {
                "id": r.id,
                "content": r.content,
                "score": r.score,
                "metadata": r.metadata,
            }
            for r in results
        ]

    async def index(
        self,
        text: str,
        vector_store: Optional[str] = None,
        metadata: Optional[dict[str, Any]] = None,
    ) -> str:
        """Index new text for retrieval.

        Args:
            text: Text to index
            vector_store: Vector store name
            metadata: Optional metadata

        Returns:
            Document ID
        """
        store = vector_store or self.default_vector_store
        if not store:
            raise ValueError("No vector store specified")

        return await self.heliosdb_client.store_text(store, text, metadata)


def create_agent_team(
    heliosdb_url: str = "http://localhost:8080",
    api_key: Optional[str] = None,
    llm_config: Optional[dict[str, Any]] = None,
) -> dict[str, HeliosDBAgent]:
    """Create a team of specialized agents.

    Args:
        heliosdb_url: HeliosDB server URL
        api_key: API key
        llm_config: LLM configuration

    Returns:
        Dictionary of agent name to agent instance

    Example:
        ```python
        team = create_agent_team(
            heliosdb_url="http://localhost:8080",
            llm_config={"model": "gpt-4"}
        )

        analyst = team["data_analyst"]
        rag = team["rag_agent"]
        ```
    """
    return {
        "data_analyst": DataAnalystAgent(
            heliosdb_url=heliosdb_url,
            api_key=api_key,
            llm_config=llm_config,
        ),
        "rag_agent": RAGAgent(
            heliosdb_url=heliosdb_url,
            api_key=api_key,
            llm_config=llm_config,
        ),
        "general": HeliosDBAgent(
            name="general_assistant",
            heliosdb_url=heliosdb_url,
            api_key=api_key,
            llm_config=llm_config,
        ),
    }
