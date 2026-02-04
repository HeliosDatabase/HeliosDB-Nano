"""
HeliosDB data models.

Pydantic models for API responses and data structures.
"""

from datetime import datetime
from typing import Any, Optional
from pydantic import BaseModel, Field


class Column(BaseModel):
    """Column information from query results."""

    name: str
    type: str


class QueryResult(BaseModel):
    """Result of a SQL query execution."""

    columns: list[Column]
    rows: list[list[Any]]
    row_count: int
    execution_time_ms: float

    def to_dicts(self) -> list[dict[str, Any]]:
        """Convert rows to list of dictionaries."""
        col_names = [c.name for c in self.columns]
        return [dict(zip(col_names, row)) for row in self.rows]

    def to_dataframe(self) -> Any:
        """Convert to pandas DataFrame (requires pandas)."""
        try:
            import pandas as pd

            col_names = [c.name for c in self.columns]
            return pd.DataFrame(self.rows, columns=col_names)
        except ImportError:
            raise ImportError("pandas is required for to_dataframe(). Install with: pip install heliosdb[pandas]")


class ColumnDefinition(BaseModel):
    """Column definition for table schema."""

    name: str
    type: str
    nullable: bool = True
    default: Optional[str] = None


class IndexInfo(BaseModel):
    """Index information."""

    name: str
    columns: list[str]
    type: str = "btree"
    unique: bool = False


class TableSchema(BaseModel):
    """Table schema information."""

    name: str
    columns: list[ColumnDefinition]
    primary_key: list[str] = Field(default_factory=list)
    indexes: list[IndexInfo] = Field(default_factory=list)


class TableInfo(BaseModel):
    """Basic table information."""

    name: str
    row_count: int
    size_bytes: int
    created_at: datetime


class Branch(BaseModel):
    """Branch information."""

    name: str
    parent: Optional[str] = None
    created_at: datetime
    commit_count: int


class MergeConflict(BaseModel):
    """Merge conflict information."""

    table: str
    row_id: str
    source_value: dict[str, Any]
    target_value: dict[str, Any]


class MergeResult(BaseModel):
    """Result of a branch merge operation."""

    success: bool
    conflicts: list[MergeConflict] = Field(default_factory=list)
    changes_applied: int


class VectorStore(BaseModel):
    """Vector store information."""

    name: str
    dimension: int
    metric: str = "cosine"
    vector_count: int
    created_at: datetime


class VectorEntry(BaseModel):
    """A vector entry with metadata."""

    id: str
    vector: list[float]
    metadata: dict[str, Any] = Field(default_factory=dict)


class VectorSearchResult(BaseModel):
    """Result from a vector similarity search."""

    id: str
    score: float
    vector: Optional[list[float]] = None
    metadata: dict[str, Any] = Field(default_factory=dict)


class MemoryMessage(BaseModel):
    """A message in agent memory."""

    id: str
    role: str  # user, assistant, system, tool
    content: str
    metadata: dict[str, Any] = Field(default_factory=dict)
    timestamp: datetime


class MemorySearchResult(BaseModel):
    """Result from semantic memory search."""

    message: MemoryMessage
    score: float


class DocumentChunk(BaseModel):
    """A chunk of a document."""

    id: str
    content: str
    index: int


class Document(BaseModel):
    """A document with chunks."""

    id: str
    content: str
    metadata: dict[str, Any] = Field(default_factory=dict)
    chunks: list[DocumentChunk] = Field(default_factory=list)
    created_at: datetime


class DocumentSearchResult(BaseModel):
    """Result from document search."""

    document_id: str
    chunk_id: str
    content: str
    score: float
    metadata: dict[str, Any] = Field(default_factory=dict)


class ChatMessage(BaseModel):
    """A chat message."""

    id: str
    role: str  # user, assistant, system
    content: str
    timestamp: datetime


class ChatSession(BaseModel):
    """A chat session."""

    id: str
    name: Optional[str] = None
    created_at: datetime
    message_count: int
    metadata: dict[str, Any] = Field(default_factory=dict)


class ChatSummary(BaseModel):
    """Summary of a chat conversation."""

    summary: str
    key_topics: list[str]
    message_count: int


class ChangeRecord(BaseModel):
    """A change record from time-travel history."""

    timestamp: datetime
    operation: str  # insert, update, delete
    row_id: str
    old_values: Optional[dict[str, Any]] = None
    new_values: Optional[dict[str, Any]] = None


class InferredSchema(BaseModel):
    """Inferred schema from data."""

    inferred_schema: dict[str, str]
    create_sql: str
    confidence: float


class HealthStatus(BaseModel):
    """Server health status."""

    status: str  # healthy, degraded, unhealthy
    version: str
    uptime_seconds: int


class UsageStats(BaseModel):
    """Usage statistics."""

    period: str
    queries_executed: int
    rows_read: int
    storage_bytes: int
    vector_searches: int
    embeddings_generated: int
    limits: dict[str, int] = Field(default_factory=dict)
