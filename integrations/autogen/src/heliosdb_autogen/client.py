"""HeliosDB client for AutoGen integration."""

from typing import Any, Optional
import httpx
from pydantic import BaseModel


class QueryResult(BaseModel):
    """Result from a SQL query."""
    rows: list[dict[str, Any]]
    columns: list[str]
    rows_affected: int = 0


class VectorSearchResult(BaseModel):
    """Result from a vector search."""
    id: str
    score: float
    content: Optional[str] = None
    metadata: Optional[dict[str, Any]] = None


class HeliosDBClient:
    """Async HTTP client for HeliosDB.

    This client provides low-level access to HeliosDB APIs for use
    in AutoGen agents and tools.

    Args:
        base_url: HeliosDB server URL
        api_key: Optional API key for authentication
        branch: Default branch name (defaults to "main")
        timeout: Request timeout in seconds

    Example:
        ```python
        client = HeliosDBClient("http://localhost:8080", api_key="key")

        # Execute SQL
        result = await client.query("SELECT * FROM users WHERE id = $1", [1])

        # Vector search
        results = await client.vector_search("documents", "hello world", top_k=5)
        ```
    """

    def __init__(
        self,
        base_url: str = "http://localhost:8080",
        api_key: Optional[str] = None,
        branch: str = "main",
        timeout: float = 30.0,
    ):
        self.base_url = base_url.rstrip("/")
        self.api_key = api_key
        self.branch = branch
        self._client = httpx.AsyncClient(
            base_url=self.base_url,
            timeout=timeout,
            headers=self._build_headers(),
        )

    def _build_headers(self) -> dict[str, str]:
        """Build request headers."""
        headers = {"Content-Type": "application/json"}
        if self.api_key:
            headers["X-API-Key"] = self.api_key
        return headers

    async def close(self) -> None:
        """Close the HTTP client."""
        await self._client.aclose()

    async def __aenter__(self) -> "HeliosDBClient":
        return self

    async def __aexit__(self, *args: Any) -> None:
        await self.close()

    # SQL Operations

    async def query(
        self,
        sql: str,
        params: Optional[list[Any]] = None,
        branch: Optional[str] = None,
    ) -> QueryResult:
        """Execute a SQL query.

        Args:
            sql: SQL query string with $1, $2, etc. placeholders
            params: Query parameters
            branch: Branch to query (defaults to client branch)

        Returns:
            QueryResult with rows and column information
        """
        branch = branch or self.branch
        response = await self._client.post(
            f"/v1/branches/{branch}/query",
            json={"sql": sql, "params": params or []},
        )
        response.raise_for_status()
        return QueryResult(**response.json())

    async def execute(
        self,
        sql: str,
        params: Optional[list[Any]] = None,
        branch: Optional[str] = None,
    ) -> int:
        """Execute a SQL statement and return rows affected.

        Args:
            sql: SQL statement
            params: Query parameters
            branch: Branch to execute on

        Returns:
            Number of rows affected
        """
        result = await self.query(sql, params, branch)
        return result.rows_affected

    async def query_at(
        self,
        sql: str,
        timestamp: str,
        params: Optional[list[Any]] = None,
    ) -> QueryResult:
        """Query at a specific point in time (time-travel).

        Args:
            sql: SQL query string
            timestamp: ISO 8601 timestamp
            params: Query parameters

        Returns:
            QueryResult from that point in time
        """
        response = await self._client.post(
            f"/v1/branches/{self.branch}/query",
            json={
                "sql": sql,
                "params": params or [],
                "as_of_timestamp": timestamp,
            },
        )
        response.raise_for_status()
        return QueryResult(**response.json())

    # Vector Operations

    async def vector_search(
        self,
        store: str,
        query: str,
        top_k: int = 10,
        min_score: Optional[float] = None,
        filter: Optional[dict[str, Any]] = None,
    ) -> list[VectorSearchResult]:
        """Perform semantic vector search with text query.

        Args:
            store: Vector store name
            query: Text query (will be embedded)
            top_k: Maximum number of results
            min_score: Minimum similarity score
            filter: Metadata filter

        Returns:
            List of search results with scores
        """
        body: dict[str, Any] = {"text": query, "top_k": top_k}
        if min_score is not None:
            body["min_score"] = min_score
        if filter:
            body["filter"] = filter

        response = await self._client.post(
            f"/v1/vectors/stores/{store}/search/text",
            json=body,
        )
        response.raise_for_status()
        return [VectorSearchResult(**r) for r in response.json()["results"]]

    async def vector_search_by_vector(
        self,
        store: str,
        vector: list[float],
        top_k: int = 10,
        min_score: Optional[float] = None,
        filter: Optional[dict[str, Any]] = None,
    ) -> list[VectorSearchResult]:
        """Perform vector search with raw vector.

        Args:
            store: Vector store name
            vector: Query vector
            top_k: Maximum number of results
            min_score: Minimum similarity score
            filter: Metadata filter

        Returns:
            List of search results
        """
        body: dict[str, Any] = {"vector": vector, "top_k": top_k}
        if min_score is not None:
            body["min_score"] = min_score
        if filter:
            body["filter"] = filter

        response = await self._client.post(
            f"/v1/vectors/stores/{store}/search",
            json=body,
        )
        response.raise_for_status()
        return [VectorSearchResult(**r) for r in response.json()["results"]]

    async def store_text(
        self,
        store: str,
        text: str,
        metadata: Optional[dict[str, Any]] = None,
    ) -> str:
        """Store text with automatic embedding.

        Args:
            store: Vector store name
            text: Text to store
            metadata: Optional metadata

        Returns:
            Document ID
        """
        response = await self._client.post(
            f"/v1/vectors/stores/{store}/texts",
            json={
                "texts": [text],
                "metadatas": [metadata or {}],
            },
        )
        response.raise_for_status()
        return response.json()["ids"][0]

    async def store_texts(
        self,
        store: str,
        texts: list[str],
        metadatas: Optional[list[dict[str, Any]]] = None,
    ) -> list[str]:
        """Store multiple texts with automatic embedding.

        Args:
            store: Vector store name
            texts: Texts to store
            metadatas: Optional metadata for each text

        Returns:
            List of document IDs
        """
        response = await self._client.post(
            f"/v1/vectors/stores/{store}/texts",
            json={
                "texts": texts,
                "metadatas": metadatas or [{} for _ in texts],
            },
        )
        response.raise_for_status()
        return response.json()["ids"]

    async def create_vector_store(
        self,
        name: str,
        dimensions: int,
        metric: str = "cosine",
    ) -> dict[str, Any]:
        """Create a new vector store.

        Args:
            name: Store name
            dimensions: Vector dimensions
            metric: Distance metric (cosine, euclidean, dot)

        Returns:
            Store information
        """
        response = await self._client.post(
            "/v1/vectors/stores",
            json={"name": name, "dimensions": dimensions, "metric": metric},
        )
        response.raise_for_status()
        return response.json()

    async def list_vector_stores(self) -> list[dict[str, Any]]:
        """List all vector stores."""
        response = await self._client.get("/v1/vectors/stores")
        response.raise_for_status()
        return response.json()["stores"]

    # Branch Operations

    async def list_branches(self) -> list[dict[str, Any]]:
        """List all database branches."""
        response = await self._client.get("/v1/branches")
        response.raise_for_status()
        return response.json()["branches"]

    async def create_branch(self, name: str, from_branch: Optional[str] = None) -> dict[str, Any]:
        """Create a new branch.

        Args:
            name: New branch name
            from_branch: Parent branch (defaults to current branch)

        Returns:
            Branch information
        """
        response = await self._client.post(
            "/v1/branches",
            json={"name": name, "from_branch": from_branch or self.branch},
        )
        response.raise_for_status()
        return response.json()

    async def merge_branch(self, source: str, target: str) -> None:
        """Merge source branch into target.

        Args:
            source: Source branch name
            target: Target branch name
        """
        response = await self._client.post(
            f"/v1/branches/{source}/merge",
            json={"target": target},
        )
        response.raise_for_status()

    # Agent Memory Operations

    async def memory_add(
        self,
        session_id: str,
        role: str,
        content: str,
    ) -> None:
        """Add a message to agent memory.

        Args:
            session_id: Memory session ID
            role: Message role (user, assistant, system)
            content: Message content
        """
        response = await self._client.post(
            f"/v1/agents/memory/{session_id}/add",
            json={"role": role, "content": content},
        )
        response.raise_for_status()

    async def memory_get(
        self,
        session_id: str,
        limit: int = 50,
    ) -> list[dict[str, Any]]:
        """Get messages from agent memory.

        Args:
            session_id: Memory session ID
            limit: Maximum messages to return

        Returns:
            List of messages
        """
        response = await self._client.get(
            f"/v1/agents/memory/{session_id}/messages",
            params={"limit": limit},
        )
        response.raise_for_status()
        return response.json()["messages"]

    async def memory_search(
        self,
        session_id: str,
        query: str,
        top_k: int = 5,
    ) -> list[VectorSearchResult]:
        """Search agent memory semantically.

        Args:
            session_id: Memory session ID
            query: Search query
            top_k: Maximum results

        Returns:
            Relevant memory entries
        """
        response = await self._client.post(
            f"/v1/agents/memory/{session_id}/search",
            json={"query": query, "top_k": top_k},
        )
        response.raise_for_status()
        return [VectorSearchResult(**r) for r in response.json()["results"]]

    async def memory_clear(self, session_id: str) -> None:
        """Clear agent memory for a session.

        Args:
            session_id: Memory session ID
        """
        response = await self._client.delete(f"/v1/agents/memory/{session_id}")
        response.raise_for_status()

    # Natural Language Query

    async def nl_query(self, question: str) -> tuple[QueryResult, str]:
        """Execute a natural language query.

        Args:
            question: Natural language question

        Returns:
            Tuple of (query result, generated SQL)
        """
        response = await self._client.post(
            "/v1/nl/query",
            json={"question": question, "branch": self.branch},
        )
        response.raise_for_status()
        data = response.json()
        result = QueryResult(
            rows=data["rows"],
            columns=data["columns"],
            rows_affected=0,
        )
        return result, data["sql"]

    # Health Check

    async def health(self) -> dict[str, Any]:
        """Check server health."""
        response = await self._client.get("/health")
        response.raise_for_status()
        return response.json()
