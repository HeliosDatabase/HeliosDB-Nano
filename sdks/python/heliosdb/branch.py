"""
HeliosDB Branch operations.

Provides Git-like branching for database isolation and experiments.
"""

from __future__ import annotations

from typing import TYPE_CHECKING, Any, Optional

from heliosdb.models import QueryResult, MergeResult

if TYPE_CHECKING:
    from heliosdb.client import HeliosDB


class Branch:
    """
    Fluent API for branch operations.

    Provides a builder pattern for querying and manipulating data
    within a specific branch.

    Example:
        users = db.branches("main").table("users").select().where({"active": True}).limit(10).execute()
    """

    def __init__(self, client: "HeliosDB", name: str) -> None:
        self._client = client
        self.name = name

    def query(
        self,
        sql: str,
        params: Optional[list[Any]] = None,
        *,
        timeout_ms: int = 30000,
    ) -> QueryResult:
        """Execute a SQL query on this branch."""
        return self._client.query(sql, params, branch=self.name, timeout_ms=timeout_ms)

    async def aquery(
        self,
        sql: str,
        params: Optional[list[Any]] = None,
        *,
        timeout_ms: int = 30000,
    ) -> QueryResult:
        """Execute a SQL query on this branch (async)."""
        return await self._client.aquery(sql, params, branch=self.name, timeout_ms=timeout_ms)

    def execute(self, sql: str, params: Optional[list[Any]] = None) -> int:
        """Execute a SQL statement on this branch."""
        return self._client.execute(sql, params, branch=self.name)

    async def aexecute(self, sql: str, params: Optional[list[Any]] = None) -> int:
        """Execute a SQL statement on this branch (async)."""
        return await self._client.aexecute(sql, params, branch=self.name)

    def table(self, name: str) -> "TableQuery":
        """Start a fluent query on a table."""
        return TableQuery(self._client, self.name, name)

    def merge_into(
        self,
        target: str = "main",
        *,
        strategy: str = "three_way",
    ) -> MergeResult:
        """
        Merge this branch into target.

        Args:
            target: Target branch to merge into
            strategy: Merge strategy (fast_forward, three_way, rebase)

        Returns:
            Merge result with conflicts if any
        """
        http_client = self._client._get_client()
        payload = {
            "target": target,
            "strategy": strategy,
        }
        response = http_client.post(f"/v1/branches/{self.name}/merge", json=payload)
        data = self._client._handle_response(response)
        return MergeResult(**data)

    def to_dataframe(self, table: str) -> Any:
        """
        Load entire table as pandas DataFrame.

        Args:
            table: Table name

        Returns:
            pandas DataFrame
        """
        result = self.query(f'SELECT * FROM "{table}"')
        return result.to_dataframe()

    def __repr__(self) -> str:
        return f"Branch({self.name!r})"


class BranchContext:
    """
    Context manager for branch operations.

    Used with db.branch() for automatic cleanup.
    """

    def __init__(
        self,
        client: "HeliosDB",
        name: str,
        parent: str,
        auto_cleanup: bool,
    ) -> None:
        self._client = client
        self.name = name
        self.parent = parent
        self._auto_cleanup = auto_cleanup
        self._merged = False

    def query(
        self,
        sql: str,
        params: Optional[list[Any]] = None,
        *,
        timeout_ms: int = 30000,
    ) -> QueryResult:
        """Execute a SQL query on this branch."""
        return self._client.query(sql, params, branch=self.name, timeout_ms=timeout_ms)

    def execute(self, sql: str, params: Optional[list[Any]] = None) -> int:
        """Execute a SQL statement on this branch."""
        return self._client.execute(sql, params, branch=self.name)

    def merge(self, strategy: str = "three_way") -> MergeResult:
        """
        Merge this branch into parent.

        Args:
            strategy: Merge strategy

        Returns:
            Merge result
        """
        http_client = self._client._get_client()
        payload = {
            "target": self.parent,
            "strategy": strategy,
        }
        response = http_client.post(f"/v1/branches/{self.name}/merge", json=payload)
        data = self._client._handle_response(response)
        self._merged = True
        return MergeResult(**data)

    def __repr__(self) -> str:
        return f"BranchContext({self.name!r}, parent={self.parent!r})"


class TableQuery:
    """
    Fluent query builder for table operations.

    Example:
        result = db.branches("main").table("users") \
            .select(["id", "name", "email"]) \
            .where({"active": True}) \
            .order_by("created_at", descending=True) \
            .limit(10) \
            .execute()
    """

    def __init__(self, client: "HeliosDB", branch: str, table: str) -> None:
        self._client = client
        self._branch = branch
        self._table = table
        self._select_columns: Optional[list[str]] = None
        self._where_clause: Optional[dict[str, Any]] = None
        self._order_by_column: Optional[str] = None
        self._order_descending: bool = False
        self._limit_value: Optional[int] = None
        self._offset_value: Optional[int] = None

    def select(self, columns: Optional[list[str]] = None) -> "TableQuery":
        """Select specific columns (or all if None)."""
        self._select_columns = columns
        return self

    def where(self, conditions: dict[str, Any]) -> "TableQuery":
        """Add WHERE conditions."""
        self._where_clause = conditions
        return self

    def order_by(self, column: str, *, descending: bool = False) -> "TableQuery":
        """Add ORDER BY clause."""
        self._order_by_column = column
        self._order_descending = descending
        return self

    def limit(self, n: int) -> "TableQuery":
        """Limit number of results."""
        self._limit_value = n
        return self

    def offset(self, n: int) -> "TableQuery":
        """Offset results."""
        self._offset_value = n
        return self

    def _build_sql(self) -> tuple[str, list[Any]]:
        """Build SQL query from builder state."""
        # SELECT clause
        if self._select_columns:
            cols = ", ".join(f'"{c}"' for c in self._select_columns)
        else:
            cols = "*"

        sql = f'SELECT {cols} FROM "{self._table}"'
        params: list[Any] = []

        # WHERE clause
        if self._where_clause:
            conditions = []
            for i, (key, value) in enumerate(self._where_clause.items(), start=1):
                conditions.append(f'"{key}" = ${i}')
                params.append(value)
            sql += " WHERE " + " AND ".join(conditions)

        # ORDER BY clause
        if self._order_by_column:
            direction = "DESC" if self._order_descending else "ASC"
            sql += f' ORDER BY "{self._order_by_column}" {direction}'

        # LIMIT clause
        if self._limit_value is not None:
            sql += f" LIMIT {self._limit_value}"

        # OFFSET clause
        if self._offset_value is not None:
            sql += f" OFFSET {self._offset_value}"

        return sql, params

    def execute(self) -> QueryResult:
        """Execute the query and return results."""
        sql, params = self._build_sql()
        return self._client.query(sql, params, branch=self._branch)

    async def aexecute(self) -> QueryResult:
        """Execute the query and return results (async)."""
        sql, params = self._build_sql()
        return await self._client.aquery(sql, params, branch=self._branch)

    def to_dataframe(self) -> Any:
        """Execute query and return as pandas DataFrame."""
        result = self.execute()
        return result.to_dataframe()

    def to_dicts(self) -> list[dict[str, Any]]:
        """Execute query and return as list of dicts."""
        result = self.execute()
        return result.to_dicts()

    def __repr__(self) -> str:
        sql, _ = self._build_sql()
        return f"TableQuery({sql!r})"
