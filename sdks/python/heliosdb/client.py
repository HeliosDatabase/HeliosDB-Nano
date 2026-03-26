"""
HeliosDB main client.

Provides both synchronous and asynchronous APIs for interacting with HeliosDB.
"""

from __future__ import annotations

import os
from contextlib import contextmanager
from dataclasses import dataclass, field
from typing import Any, Generator, Optional, Union

import httpx

from heliosdb.branch import Branch, BranchContext
from heliosdb.exceptions import (
    AuthenticationError,
    ConnectionError,
    ConflictError,
    HeliosDBError,
    NotFoundError,
    QueryError,
    RateLimitError,
    ValidationError,
)
from heliosdb.memory import AgentMemory
from heliosdb.models import (
    Branch as BranchModel,
    HealthStatus,
    InferredSchema,
    QueryResult,
    TableInfo,
    TableSchema,
)
from heliosdb.vector import VectorStore


@dataclass
class HeliosDBConfig:
    """Configuration for HeliosDB client."""

    # Connection
    url: str = "http://localhost:8080"
    api_key: Optional[str] = None
    jwt_token: Optional[str] = None

    # Timeouts (in seconds)
    connect_timeout: float = 10.0
    read_timeout: float = 30.0
    write_timeout: float = 30.0

    # Retry configuration
    max_retries: int = 3
    retry_delay: float = 0.5

    # Default branch
    default_branch: str = "main"

    # Headers
    extra_headers: dict[str, str] = field(default_factory=dict)

    @classmethod
    def from_env(cls) -> "HeliosDBConfig":
        """Create config from environment variables."""
        return cls(
            url=os.environ.get("HELIOSDB_URL", "http://localhost:8080"),
            api_key=os.environ.get("HELIOSDB_API_KEY"),
            jwt_token=os.environ.get("HELIOSDB_JWT_TOKEN"),
            default_branch=os.environ.get("HELIOSDB_DEFAULT_BRANCH", "main"),
        )


class HeliosDB:
    """
    Main HeliosDB client for interacting with HeliosDB Nano.

    Supports both embedded mode (local file) and server mode (REST API).

    Example:
        # Embedded mode (future)
        db = HeliosDB("./myapp.db")

        # Server mode
        db = HeliosDB.connect("http://localhost:8080", api_key="your-key")

        # Execute queries
        result = db.query("SELECT * FROM users")
        for row in result.to_dicts():
            print(row)

        # Vector operations
        store = db.vector_store("embeddings", dimension=1536)
        store.add_texts(["Hello world"])
        results = store.similarity_search("greeting", k=5)
    """

    def __init__(
        self,
        url_or_path: str = "http://localhost:8080",
        *,
        api_key: Optional[str] = None,
        jwt_token: Optional[str] = None,
        config: Optional[HeliosDBConfig] = None,
    ) -> None:
        """
        Initialize HeliosDB client.

        Args:
            url_or_path: Either a URL for server mode or a file path for embedded mode
            api_key: API key for authentication
            jwt_token: JWT token for authentication
            config: Optional full configuration object
        """
        if config:
            self._config = config
        else:
            self._config = HeliosDBConfig(
                url=url_or_path,
                api_key=api_key,
                jwt_token=jwt_token,
            )

        self._client: Optional[httpx.Client] = None
        self._async_client: Optional[httpx.AsyncClient] = None

    @classmethod
    def connect(
        cls,
        url: str,
        *,
        api_key: Optional[str] = None,
        jwt_token: Optional[str] = None,
    ) -> "HeliosDB":
        """
        Connect to a HeliosDB server.

        Args:
            url: Server URL (e.g., "http://localhost:8080")
            api_key: API key for authentication
            jwt_token: JWT token for authentication

        Returns:
            HeliosDB client instance
        """
        return cls(url, api_key=api_key, jwt_token=jwt_token)

    @classmethod
    def from_env(cls) -> "HeliosDB":
        """Create client from environment variables."""
        config = HeliosDBConfig.from_env()
        return cls(config=config)

    def _get_client(self) -> httpx.Client:
        """Get or create HTTP client."""
        if self._client is None:
            headers = self._build_headers()
            timeout = httpx.Timeout(
                connect=self._config.connect_timeout,
                read=self._config.read_timeout,
                write=self._config.write_timeout,
            )
            self._client = httpx.Client(
                base_url=self._config.url,
                headers=headers,
                timeout=timeout,
            )
        return self._client

    async def _get_async_client(self) -> httpx.AsyncClient:
        """Get or create async HTTP client."""
        if self._async_client is None:
            headers = self._build_headers()
            timeout = httpx.Timeout(
                connect=self._config.connect_timeout,
                read=self._config.read_timeout,
                write=self._config.write_timeout,
            )
            self._async_client = httpx.AsyncClient(
                base_url=self._config.url,
                headers=headers,
                timeout=timeout,
            )
        return self._async_client

    def _build_headers(self) -> dict[str, str]:
        """Build request headers."""
        headers = {
            "Content-Type": "application/json",
            "Accept": "application/json",
            "User-Agent": "heliosdb-python/2.6.0",
        }

        if self._config.api_key:
            headers["X-API-Key"] = self._config.api_key
        if self._config.jwt_token:
            headers["Authorization"] = f"Bearer {self._config.jwt_token}"

        headers.update(self._config.extra_headers)
        return headers

    def _handle_response(self, response: httpx.Response) -> dict[str, Any]:
        """Handle HTTP response and raise appropriate exceptions."""
        try:
            data = response.json()
        except Exception:
            data = {"message": response.text}

        if response.status_code == 200 or response.status_code == 201:
            return data
        elif response.status_code == 204:
            return {}
        elif response.status_code == 400:
            raise ValidationError(
                data.get("message", "Bad request"),
                details=data.get("details"),
            )
        elif response.status_code == 401:
            raise AuthenticationError(
                data.get("message", "Unauthorized"),
                details=data.get("details"),
            )
        elif response.status_code == 404:
            raise NotFoundError(
                data.get("message", "Not found"),
                details=data.get("details"),
            )
        elif response.status_code == 409:
            raise ConflictError(
                data.get("message", "Conflict"),
                details=data.get("details"),
            )
        elif response.status_code == 429:
            raise RateLimitError(
                data.get("message", "Rate limit exceeded"),
                retry_after=response.headers.get("Retry-After"),
                details=data.get("details"),
            )
        else:
            raise HeliosDBError(
                data.get("message", f"HTTP {response.status_code}"),
                code=data.get("code"),
                details=data.get("details"),
            )

    def close(self) -> None:
        """Close the client and release resources."""
        if self._client:
            self._client.close()
            self._client = None

    async def aclose(self) -> None:
        """Close the async client and release resources."""
        if self._async_client:
            await self._async_client.aclose()
            self._async_client = None

    def __enter__(self) -> "HeliosDB":
        return self

    def __exit__(self, *args: Any) -> None:
        self.close()

    async def __aenter__(self) -> "HeliosDB":
        return self

    async def __aexit__(self, *args: Any) -> None:
        await self.aclose()

    # ==========================================================================
    # Health & Info
    # ==========================================================================

    def health(self) -> HealthStatus:
        """Check server health status."""
        client = self._get_client()
        response = client.get("/health")
        data = self._handle_response(response)
        return HealthStatus(**data)

    async def ahealth(self) -> HealthStatus:
        """Check server health status (async)."""
        client = await self._get_async_client()
        response = await client.get("/health")
        data = self._handle_response(response)
        return HealthStatus(**data)

    # ==========================================================================
    # Query Execution
    # ==========================================================================

    def query(
        self,
        sql: str,
        params: Optional[list[Any]] = None,
        *,
        branch: Optional[str] = None,
        timeout_ms: int = 30000,
        mode: str = "normal",
    ) -> QueryResult:
        """
        Execute a SQL query.

        Args:
            sql: SQL query string
            params: Query parameters for parameterized queries
            branch: Branch to query (default: main)
            timeout_ms: Query timeout in milliseconds
            mode: Query mode (normal, safe, explain)

        Returns:
            QueryResult with columns and rows

        Example:
            result = db.query("SELECT * FROM users WHERE id = $1", [123])
            for row in result.to_dicts():
                print(row["name"])
        """
        branch = branch or self._config.default_branch
        client = self._get_client()

        payload = {
            "sql": sql,
            "params": params or [],
            "timeout_ms": timeout_ms,
            "mode": mode,
        }

        response = client.post(f"/v1/branches/{branch}/query", json=payload)
        data = self._handle_response(response)
        return QueryResult(**data)

    async def aquery(
        self,
        sql: str,
        params: Optional[list[Any]] = None,
        *,
        branch: Optional[str] = None,
        timeout_ms: int = 30000,
        mode: str = "normal",
    ) -> QueryResult:
        """Execute a SQL query (async)."""
        branch = branch or self._config.default_branch
        client = await self._get_async_client()

        payload = {
            "sql": sql,
            "params": params or [],
            "timeout_ms": timeout_ms,
            "mode": mode,
        }

        response = await client.post(f"/v1/branches/{branch}/query", json=payload)
        data = self._handle_response(response)
        return QueryResult(**data)

    def execute(
        self,
        sql: str,
        params: Optional[list[Any]] = None,
        *,
        branch: Optional[str] = None,
    ) -> int:
        """
        Execute a SQL statement (INSERT, UPDATE, DELETE).

        Args:
            sql: SQL statement
            params: Query parameters
            branch: Branch to execute on

        Returns:
            Number of affected rows
        """
        result = self.query(sql, params, branch=branch)
        return result.row_count

    async def aexecute(
        self,
        sql: str,
        params: Optional[list[Any]] = None,
        *,
        branch: Optional[str] = None,
    ) -> int:
        """Execute a SQL statement (async)."""
        result = await self.aquery(sql, params, branch=branch)
        return result.row_count

    # ==========================================================================
    # Branch Management
    # ==========================================================================

    def list_branches(self) -> list[BranchModel]:
        """List all branches."""
        client = self._get_client()
        response = client.get("/v1/branches")
        data = self._handle_response(response)
        return [BranchModel(**b) for b in data.get("branches", [])]

    def get_branch(self, name: str) -> BranchModel:
        """Get branch details."""
        client = self._get_client()
        response = client.get(f"/v1/branches/{name}")
        data = self._handle_response(response)
        return BranchModel(**data)

    def create_branch(
        self,
        name: str,
        *,
        from_branch: str = "main",
        at_timestamp: Optional[str] = None,
    ) -> BranchModel:
        """
        Create a new branch.

        Args:
            name: Branch name
            from_branch: Parent branch to fork from
            at_timestamp: Optional timestamp to fork from (for time-travel)

        Returns:
            Created branch info
        """
        client = self._get_client()
        payload: dict[str, Any] = {"name": name, "from_branch": from_branch}
        if at_timestamp:
            payload["at_timestamp"] = at_timestamp

        response = client.post("/v1/branches", json=payload)
        data = self._handle_response(response)
        return BranchModel(**data)

    def delete_branch(self, name: str) -> None:
        """Delete a branch."""
        if name == "main":
            raise ValidationError("Cannot delete main branch")

        client = self._get_client()
        response = client.delete(f"/v1/branches/{name}")
        self._handle_response(response)

    @contextmanager
    def branch(
        self,
        name: str,
        *,
        from_branch: str = "main",
        auto_cleanup: bool = True,
    ) -> Generator[BranchContext, None, None]:
        """
        Create and use a branch in a context manager.

        The branch is automatically deleted when the context exits,
        unless you call merge() or set auto_cleanup=False.

        Args:
            name: Branch name
            from_branch: Parent branch
            auto_cleanup: Whether to delete branch on exit

        Example:
            with db.branch("experiment-1") as branch:
                branch.execute("UPDATE config SET value = 'test'")
                # Auto-deleted on exit

            # Or keep changes:
            with db.branch("feature-1") as branch:
                branch.execute("INSERT INTO features ...")
                branch.merge()  # Merge to parent
        """
        branch_info = self.create_branch(name, from_branch=from_branch)
        ctx = BranchContext(self, name, from_branch, auto_cleanup)

        try:
            yield ctx
        finally:
            if auto_cleanup and not ctx._merged:
                try:
                    self.delete_branch(name)
                except Exception:
                    pass  # Best effort cleanup

    def branches(self, name: str) -> Branch:
        """
        Get a branch accessor for fluent API.

        Example:
            users = db.branches("main").table("users").select().limit(10).execute()
        """
        return Branch(self, name)

    # ==========================================================================
    # Table Operations
    # ==========================================================================

    def list_tables(self, branch: Optional[str] = None) -> list[TableInfo]:
        """List all tables in a branch."""
        branch = branch or self._config.default_branch
        client = self._get_client()
        response = client.get(f"/v1/branches/{branch}/tables")
        data = self._handle_response(response)
        return [TableInfo(**t) for t in data.get("tables", [])]

    def get_table_schema(self, table: str, branch: Optional[str] = None) -> TableSchema:
        """Get table schema."""
        branch = branch or self._config.default_branch
        client = self._get_client()
        response = client.get(f"/v1/branches/{branch}/tables/{table}")
        data = self._handle_response(response)
        return TableSchema(**data)

    def create_table(
        self,
        name: str,
        columns: list[dict[str, Any]],
        *,
        primary_key: Optional[list[str]] = None,
        branch: Optional[str] = None,
    ) -> TableInfo:
        """
        Create a new table.

        Args:
            name: Table name
            columns: List of column definitions
            primary_key: Primary key columns
            branch: Branch to create table on

        Example:
            db.create_table("users", [
                {"name": "id", "type": "INTEGER", "nullable": False},
                {"name": "name", "type": "TEXT"},
                {"name": "email", "type": "TEXT"},
            ], primary_key=["id"])
        """
        branch = branch or self._config.default_branch
        client = self._get_client()

        payload: dict[str, Any] = {"name": name, "columns": columns}
        if primary_key:
            payload["primary_key"] = primary_key

        response = client.post(f"/v1/branches/{branch}/tables", json=payload)
        data = self._handle_response(response)
        return TableInfo(**data)

    def drop_table(self, name: str, branch: Optional[str] = None) -> None:
        """Drop a table."""
        branch = branch or self._config.default_branch
        client = self._get_client()
        response = client.delete(f"/v1/branches/{branch}/tables/{name}")
        self._handle_response(response)

    # ==========================================================================
    # Data Operations
    # ==========================================================================

    def insert(
        self,
        table: str,
        rows: list[dict[str, Any]],
        *,
        branch: Optional[str] = None,
        auto_create: bool = False,
        on_conflict: str = "error",
    ) -> int:
        """
        Insert rows into a table.

        Args:
            table: Table name
            rows: List of row dictionaries
            branch: Branch to insert into
            auto_create: Auto-create table if not exists
            on_conflict: Conflict handling (error, ignore, update)

        Returns:
            Number of inserted rows
        """
        branch = branch or self._config.default_branch
        client = self._get_client()

        payload = {
            "rows": rows,
            "on_conflict": on_conflict,
        }

        params = {"auto_create": str(auto_create).lower()}
        response = client.post(
            f"/v1/branches/{branch}/tables/{table}/data",
            json=payload,
            params=params,
        )
        data = self._handle_response(response)
        return data.get("inserted", 0)

    def infer_schema(self, data: list[dict[str, Any]], table_name: str = "inferred") -> InferredSchema:
        """
        Infer schema from data.

        Args:
            data: Sample data rows
            table_name: Optional table name for generated SQL

        Returns:
            Inferred schema with SQL CREATE statement
        """
        client = self._get_client()
        payload = {"data": data, "table_name": table_name}
        response = client.post("/v1/schema/infer", json=payload)
        data = self._handle_response(response)
        return InferredSchema(**data)

    # ==========================================================================
    # Vector Store
    # ==========================================================================

    def vector_store(
        self,
        name: str,
        *,
        dimension: Optional[int] = None,
        metric: str = "cosine",
        create_if_missing: bool = True,
    ) -> VectorStore:
        """
        Get or create a vector store.

        Args:
            name: Vector store name
            dimension: Vector dimension (required for creation)
            metric: Distance metric (cosine, euclidean, dot_product)
            create_if_missing: Create store if it doesn't exist

        Returns:
            VectorStore instance for vector operations

        Example:
            store = db.vector_store("documents", dimension=1536)
            store.add_texts(["Hello world", "AI is amazing"])
            results = store.similarity_search("greeting", k=5)
        """
        return VectorStore(
            self,
            name,
            dimension=dimension,
            metric=metric,
            create_if_missing=create_if_missing,
        )

    # ==========================================================================
    # Agent Memory
    # ==========================================================================

    def agent_memory(self, session_id: str) -> AgentMemory:
        """
        Get an agent memory interface for a session.

        Args:
            session_id: Unique session identifier

        Returns:
            AgentMemory instance for managing conversation history

        Example:
            memory = db.agent_memory("agent-123")
            memory.add_message("user", "Hello!")
            memory.add_message("assistant", "Hi there!")

            # Semantic search in memory
            results = memory.search("greeting", k=5)
        """
        return AgentMemory(self, session_id)

    # ==========================================================================
    # Time Travel
    # ==========================================================================

    def time_travel_query(
        self,
        sql: str,
        timestamp: str,
        params: Optional[list[Any]] = None,
        *,
        branch: Optional[str] = None,
    ) -> QueryResult:
        """
        Execute a query at a historical timestamp.

        Args:
            sql: SQL query
            timestamp: ISO 8601 timestamp
            params: Query parameters
            branch: Branch to query

        Returns:
            Query results at the specified time

        Example:
            # Get data as it was yesterday
            result = db.time_travel_query(
                "SELECT * FROM users",
                "2024-01-01T00:00:00Z"
            )
        """
        branch = branch or self._config.default_branch
        client = self._get_client()

        payload = {
            "sql": sql,
            "timestamp": timestamp,
            "params": params or [],
        }

        response = client.post(f"/v1/branches/{branch}/time-travel", json=payload)
        data = self._handle_response(response)
        return QueryResult(**data)
