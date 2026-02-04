"""Retriever classes for RAG workflows with AutoGen.

These retrievers integrate with AutoGen's RetrieveChat and other
retrieval-augmented patterns.
"""

from typing import Any, Optional, Callable
from dataclasses import dataclass
import json

from .client import HeliosDBClient


@dataclass
class Document:
    """A retrieved document."""
    id: str
    content: str
    score: float
    metadata: dict[str, Any]

    def to_dict(self) -> dict[str, Any]:
        """Convert to dictionary."""
        return {
            "id": self.id,
            "content": self.content,
            "score": self.score,
            "metadata": self.metadata,
        }


class HeliosDBRetriever:
    """Vector retriever using HeliosDB.

    This retriever can be used with AutoGen's RAG patterns to
    retrieve relevant documents from vector stores.

    Args:
        heliosdb_url: HeliosDB server URL
        api_key: Optional API key
        store_name: Vector store name
        top_k: Default number of results
        min_score: Default minimum similarity score

    Example:
        ```python
        from heliosdb_autogen import HeliosDBRetriever

        retriever = HeliosDBRetriever(
            heliosdb_url="http://localhost:8080",
            store_name="documents",
            top_k=5
        )

        # Retrieve documents
        docs = await retriever.retrieve("How does authentication work?")
        for doc in docs:
            print(f"{doc.score:.2f}: {doc.content[:100]}...")
        ```
    """

    def __init__(
        self,
        heliosdb_url: str = "http://localhost:8080",
        api_key: Optional[str] = None,
        store_name: str = "documents",
        top_k: int = 5,
        min_score: float = 0.5,
    ):
        self.client = HeliosDBClient(heliosdb_url, api_key)
        self.store_name = store_name
        self.top_k = top_k
        self.min_score = min_score

    async def retrieve(
        self,
        query: str,
        top_k: Optional[int] = None,
        min_score: Optional[float] = None,
        filter: Optional[dict[str, Any]] = None,
    ) -> list[Document]:
        """Retrieve relevant documents.

        Args:
            query: Search query
            top_k: Number of results (overrides default)
            min_score: Minimum score (overrides default)
            filter: Metadata filter

        Returns:
            List of relevant documents
        """
        results = await self.client.vector_search(
            self.store_name,
            query,
            top_k or self.top_k,
            min_score or self.min_score,
            filter,
        )
        return [
            Document(
                id=r.id,
                content=r.content or "",
                score=r.score,
                metadata=r.metadata or {},
            )
            for r in results
        ]

    async def retrieve_as_context(
        self,
        query: str,
        top_k: Optional[int] = None,
        include_scores: bool = True,
    ) -> str:
        """Retrieve documents formatted as context string.

        Args:
            query: Search query
            top_k: Number of results
            include_scores: Whether to include similarity scores

        Returns:
            Formatted context string
        """
        docs = await self.retrieve(query, top_k)
        if not docs:
            return "No relevant documents found."

        lines = ["Retrieved documents:"]
        for i, doc in enumerate(docs, 1):
            if include_scores:
                lines.append(f"\n[{i}] (score: {doc.score:.2f})")
            else:
                lines.append(f"\n[{i}]")
            lines.append(doc.content)

        return "\n".join(lines)

    async def add_document(
        self,
        content: str,
        metadata: Optional[dict[str, Any]] = None,
    ) -> str:
        """Add a document to the store.

        Args:
            content: Document content
            metadata: Optional metadata

        Returns:
            Document ID
        """
        return await self.client.store_text(self.store_name, content, metadata)

    async def add_documents(
        self,
        contents: list[str],
        metadatas: Optional[list[dict[str, Any]]] = None,
    ) -> list[str]:
        """Add multiple documents.

        Args:
            contents: Document contents
            metadatas: Optional metadata for each document

        Returns:
            List of document IDs
        """
        return await self.client.store_texts(self.store_name, contents, metadatas)


class HybridRetriever:
    """Hybrid retriever combining vector search with SQL queries.

    This retriever can search both unstructured (vector) and
    structured (SQL) data sources.

    Args:
        heliosdb_url: HeliosDB server URL
        api_key: Optional API key
        vector_store: Vector store name
        table_name: SQL table name for structured data

    Example:
        ```python
        retriever = HybridRetriever(
            heliosdb_url="http://localhost:8080",
            vector_store="articles",
            table_name="article_metadata"
        )

        # Search both vector and structured data
        results = await retriever.search(
            query="machine learning",
            sql_filter="category = 'tech' AND published_date > '2024-01-01'"
        )
        ```
    """

    def __init__(
        self,
        heliosdb_url: str = "http://localhost:8080",
        api_key: Optional[str] = None,
        vector_store: str = "documents",
        table_name: Optional[str] = None,
    ):
        self.client = HeliosDBClient(heliosdb_url, api_key)
        self.vector_store = vector_store
        self.table_name = table_name

    async def search(
        self,
        query: str,
        top_k: int = 5,
        min_score: float = 0.5,
        sql_filter: Optional[str] = None,
        metadata_filter: Optional[dict[str, Any]] = None,
    ) -> dict[str, Any]:
        """Perform hybrid search.

        Args:
            query: Search query
            top_k: Maximum vector results
            min_score: Minimum similarity score
            sql_filter: SQL WHERE clause for structured data
            metadata_filter: Vector metadata filter

        Returns:
            Combined results from both sources
        """
        results: dict[str, Any] = {
            "vector_results": [],
            "sql_results": [],
        }

        # Vector search
        vector_results = await self.client.vector_search(
            self.vector_store, query, top_k, min_score, metadata_filter
        )
        results["vector_results"] = [
            {
                "id": r.id,
                "content": r.content,
                "score": r.score,
                "metadata": r.metadata,
            }
            for r in vector_results
        ]

        # SQL search if table and filter provided
        if self.table_name and sql_filter:
            sql = f"SELECT * FROM {self.table_name} WHERE {sql_filter} LIMIT {top_k}"
            sql_result = await self.client.query(sql)
            results["sql_results"] = sql_result.rows

        return results

    async def search_with_join(
        self,
        query: str,
        join_column: str = "id",
        top_k: int = 5,
    ) -> list[dict[str, Any]]:
        """Search vectors and join with SQL data.

        First performs vector search, then enriches results with
        structured data from the SQL table.

        Args:
            query: Search query
            join_column: Column to join on (from vector metadata)
            top_k: Maximum results

        Returns:
            Enriched results
        """
        if not self.table_name:
            raise ValueError("No table_name configured for SQL join")

        # Vector search
        vector_results = await self.client.vector_search(
            self.vector_store, query, top_k
        )

        # Enrich with SQL data
        enriched = []
        for r in vector_results:
            result = {
                "id": r.id,
                "content": r.content,
                "score": r.score,
                "metadata": r.metadata,
            }

            # Try to get structured data
            if r.metadata and join_column in r.metadata:
                join_value = r.metadata[join_column]
                sql_result = await self.client.query(
                    f"SELECT * FROM {self.table_name} WHERE {join_column} = $1",
                    [join_value]
                )
                if sql_result.rows:
                    result["structured_data"] = sql_result.rows[0]

            enriched.append(result)

        return enriched

    async def get_context(
        self,
        query: str,
        top_k: int = 5,
        include_structured: bool = True,
    ) -> str:
        """Get combined context from hybrid search.

        Args:
            query: Search query
            top_k: Maximum results
            include_structured: Whether to include SQL results

        Returns:
            Formatted context string
        """
        parts = []

        # Vector results
        vector_results = await self.client.vector_search(
            self.vector_store, query, top_k
        )
        if vector_results:
            parts.append("Relevant documents:")
            for r in vector_results:
                parts.append(f"- {r.content}")

        # SQL results if configured
        if include_structured and self.table_name:
            # Simple full-text search approximation
            sql = f"""
                SELECT * FROM {self.table_name}
                WHERE LOWER(content) LIKE LOWER($1)
                LIMIT {top_k}
            """
            try:
                sql_result = await self.client.query(sql, [f"%{query}%"])
                if sql_result.rows:
                    parts.append("\nStructured data:")
                    for row in sql_result.rows:
                        parts.append(f"- {json.dumps(row)}")
            except Exception:
                pass  # SQL table might not have 'content' column

        return "\n".join(parts) if parts else "No relevant information found."


class MultiStoreRetriever:
    """Retriever that searches across multiple vector stores.

    Useful for searching different knowledge domains or document types.

    Example:
        ```python
        retriever = MultiStoreRetriever(
            heliosdb_url="http://localhost:8080",
            stores=["docs", "code", "faq"]
        )

        results = await retriever.search("authentication flow")
        ```
    """

    def __init__(
        self,
        heliosdb_url: str = "http://localhost:8080",
        api_key: Optional[str] = None,
        stores: Optional[list[str]] = None,
    ):
        self.client = HeliosDBClient(heliosdb_url, api_key)
        self.stores = stores or []

    async def search(
        self,
        query: str,
        top_k_per_store: int = 3,
        min_score: float = 0.5,
        stores: Optional[list[str]] = None,
    ) -> dict[str, list[Document]]:
        """Search across multiple stores.

        Args:
            query: Search query
            top_k_per_store: Results per store
            min_score: Minimum similarity score
            stores: Override stores to search

        Returns:
            Dictionary mapping store name to results
        """
        target_stores = stores or self.stores
        results: dict[str, list[Document]] = {}

        for store in target_stores:
            try:
                store_results = await self.client.vector_search(
                    store, query, top_k_per_store, min_score
                )
                results[store] = [
                    Document(
                        id=r.id,
                        content=r.content or "",
                        score=r.score,
                        metadata={"store": store, **(r.metadata or {})},
                    )
                    for r in store_results
                ]
            except Exception:
                results[store] = []

        return results

    async def search_merged(
        self,
        query: str,
        top_k: int = 10,
        min_score: float = 0.5,
    ) -> list[Document]:
        """Search and merge results from all stores.

        Results are sorted by score across all stores.

        Args:
            query: Search query
            top_k: Total results to return
            min_score: Minimum similarity score

        Returns:
            Merged and sorted results
        """
        # Get more per store to have good selection
        per_store = max(top_k // len(self.stores), 3) if self.stores else top_k
        all_results = await self.search(query, per_store, min_score)

        # Merge and sort
        merged = []
        for store_docs in all_results.values():
            merged.extend(store_docs)

        merged.sort(key=lambda d: d.score, reverse=True)
        return merged[:top_k]

    async def get_context(
        self,
        query: str,
        top_k: int = 5,
        show_sources: bool = True,
    ) -> str:
        """Get merged context from all stores.

        Args:
            query: Search query
            top_k: Maximum results
            show_sources: Whether to show which store each result came from

        Returns:
            Formatted context string
        """
        docs = await self.search_merged(query, top_k)
        if not docs:
            return "No relevant documents found."

        lines = ["Retrieved context:"]
        for doc in docs:
            if show_sources:
                store = doc.metadata.get("store", "unknown")
                lines.append(f"\n[{store}] (score: {doc.score:.2f})")
            else:
                lines.append(f"\n(score: {doc.score:.2f})")
            lines.append(doc.content)

        return "\n".join(lines)
