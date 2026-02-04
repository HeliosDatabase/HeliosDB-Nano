"""
HeliosDB Vector Store operations.

Provides vector storage, similarity search, and text embedding functionality.
"""

from __future__ import annotations

from typing import TYPE_CHECKING, Any, Optional

from heliosdb.models import VectorEntry, VectorSearchResult, VectorStore as VectorStoreModel

if TYPE_CHECKING:
    from heliosdb.client import HeliosDB


class VectorStore:
    """
    Vector store for semantic search and embeddings.

    Supports storing vectors with metadata, similarity search,
    and automatic text embedding.

    Example:
        store = db.vector_store("documents", dimension=1536)

        # Add vectors directly
        store.upsert([
            {"id": "doc1", "vector": [0.1, 0.2, ...], "metadata": {"title": "Hello"}}
        ])

        # Or add texts (auto-embedded)
        store.add_texts(["Hello world", "AI is amazing"], metadatas=[{"source": "intro"}])

        # Search
        results = store.similarity_search("greeting", k=5)
        for r in results:
            print(f"{r.id}: {r.score}")
    """

    def __init__(
        self,
        client: "HeliosDB",
        name: str,
        *,
        dimension: Optional[int] = None,
        metric: str = "cosine",
        create_if_missing: bool = True,
    ) -> None:
        self._client = client
        self.name = name
        self.dimension = dimension
        self.metric = metric
        self._info: Optional[VectorStoreModel] = None

        if create_if_missing and dimension is not None:
            self._ensure_exists()

    def _ensure_exists(self) -> None:
        """Ensure the vector store exists, create if missing."""
        try:
            self._info = self._get_info()
        except Exception:
            if self.dimension is None:
                raise ValueError("dimension is required to create vector store")
            self._create()

    def _get_info(self) -> VectorStoreModel:
        """Get vector store info from server."""
        http_client = self._client._get_client()
        response = http_client.get(f"/v1/vectors/stores/{self.name}")
        data = self._client._handle_response(response)
        return VectorStoreModel(**data)

    def _create(self) -> VectorStoreModel:
        """Create the vector store."""
        http_client = self._client._get_client()
        payload = {
            "name": self.name,
            "dimension": self.dimension,
            "metric": self.metric,
        }
        response = http_client.post("/v1/vectors/stores", json=payload)
        data = self._client._handle_response(response)
        self._info = VectorStoreModel(**data)
        return self._info

    @property
    def info(self) -> VectorStoreModel:
        """Get vector store information."""
        if self._info is None:
            self._info = self._get_info()
        return self._info

    def upsert(
        self,
        vectors: list[dict[str, Any]],
    ) -> int:
        """
        Upsert vectors into the store.

        Args:
            vectors: List of vector entries with id, vector, and optional metadata

        Returns:
            Number of upserted vectors

        Example:
            store.upsert([
                {"id": "doc1", "vector": [0.1, 0.2, ...], "metadata": {"title": "Doc 1"}},
                {"id": "doc2", "vector": [0.3, 0.4, ...], "metadata": {"title": "Doc 2"}},
            ])
        """
        http_client = self._client._get_client()
        payload = {"vectors": vectors}
        response = http_client.post(f"/v1/vectors/stores/{self.name}/vectors", json=payload)
        data = self._client._handle_response(response)
        return data.get("upserted", 0)

    async def aupsert(self, vectors: list[dict[str, Any]]) -> int:
        """Upsert vectors (async)."""
        http_client = await self._client._get_async_client()
        payload = {"vectors": vectors}
        response = await http_client.post(f"/v1/vectors/stores/{self.name}/vectors", json=payload)
        data = self._client._handle_response(response)
        return data.get("upserted", 0)

    def search(
        self,
        vector: list[float],
        *,
        top_k: int = 10,
        filter: Optional[dict[str, Any]] = None,
        include_metadata: bool = True,
        include_vectors: bool = False,
    ) -> list[VectorSearchResult]:
        """
        Search for similar vectors.

        Args:
            vector: Query vector
            top_k: Number of results to return
            filter: Metadata filter
            include_metadata: Include metadata in results
            include_vectors: Include vectors in results

        Returns:
            List of search results sorted by similarity
        """
        http_client = self._client._get_client()
        payload: dict[str, Any] = {
            "vector": vector,
            "top_k": top_k,
            "include_metadata": include_metadata,
            "include_vectors": include_vectors,
        }
        if filter:
            payload["filter"] = filter

        response = http_client.post(f"/v1/vectors/stores/{self.name}/search", json=payload)
        data = self._client._handle_response(response)
        return [VectorSearchResult(**r) for r in data.get("results", [])]

    async def asearch(
        self,
        vector: list[float],
        *,
        top_k: int = 10,
        filter: Optional[dict[str, Any]] = None,
        include_metadata: bool = True,
        include_vectors: bool = False,
    ) -> list[VectorSearchResult]:
        """Search for similar vectors (async)."""
        http_client = await self._client._get_async_client()
        payload: dict[str, Any] = {
            "vector": vector,
            "top_k": top_k,
            "include_metadata": include_metadata,
            "include_vectors": include_vectors,
        }
        if filter:
            payload["filter"] = filter

        response = await http_client.post(f"/v1/vectors/stores/{self.name}/search", json=payload)
        data = self._client._handle_response(response)
        return [VectorSearchResult(**r) for r in data.get("results", [])]

    def add_texts(
        self,
        texts: list[str],
        *,
        metadatas: Optional[list[dict[str, Any]]] = None,
        ids: Optional[list[str]] = None,
        embedding_model: Optional[str] = None,
    ) -> list[str]:
        """
        Add texts with automatic embedding.

        Args:
            texts: List of text strings to embed and store
            metadatas: Optional metadata for each text
            ids: Optional IDs for each text (auto-generated if not provided)
            embedding_model: Embedding model to use

        Returns:
            List of IDs for the added texts

        Example:
            ids = store.add_texts(
                ["Hello world", "AI is great"],
                metadatas=[{"source": "intro"}, {"source": "main"}]
            )
        """
        http_client = self._client._get_client()
        payload: dict[str, Any] = {"texts": texts}
        if metadatas:
            payload["metadatas"] = metadatas
        if ids:
            payload["ids"] = ids
        if embedding_model:
            payload["embedding_model"] = embedding_model

        response = http_client.post(f"/v1/vectors/stores/{self.name}/texts", json=payload)
        data = self._client._handle_response(response)
        return data.get("ids", [])

    async def aadd_texts(
        self,
        texts: list[str],
        *,
        metadatas: Optional[list[dict[str, Any]]] = None,
        ids: Optional[list[str]] = None,
        embedding_model: Optional[str] = None,
    ) -> list[str]:
        """Add texts with automatic embedding (async)."""
        http_client = await self._client._get_async_client()
        payload: dict[str, Any] = {"texts": texts}
        if metadatas:
            payload["metadatas"] = metadatas
        if ids:
            payload["ids"] = ids
        if embedding_model:
            payload["embedding_model"] = embedding_model

        response = await http_client.post(f"/v1/vectors/stores/{self.name}/texts", json=payload)
        data = self._client._handle_response(response)
        return data.get("ids", [])

    def similarity_search(
        self,
        query: str,
        *,
        k: int = 5,
        filter: Optional[dict[str, Any]] = None,
        embedding_model: Optional[str] = None,
    ) -> list[VectorSearchResult]:
        """
        Search by text query (auto-embedded).

        This is a convenience method that embeds the query text
        and performs similarity search.

        Args:
            query: Text query to search for
            k: Number of results
            filter: Metadata filter
            embedding_model: Embedding model to use

        Returns:
            List of similar results

        Example:
            results = store.similarity_search("machine learning tutorials", k=5)
        """
        # For now, use the texts endpoint which handles embedding
        # In a full implementation, this would embed locally or call an API
        http_client = self._client._get_client()
        payload: dict[str, Any] = {
            "texts": [query],
            "top_k": k,
        }
        if filter:
            payload["filter"] = filter
        if embedding_model:
            payload["embedding_model"] = embedding_model

        # Use semantic search endpoint
        response = http_client.post(
            f"/v1/branches/main/semantic-search",
            json={
                "query": query,
                "table": self.name,
                "limit": k,
                "filters": filter,
            },
        )

        # Fallback to vector search if semantic search not available
        try:
            data = self._client._handle_response(response)
            return [VectorSearchResult(**r) for r in data.get("results", [])]
        except Exception:
            # Fallback: would need to embed query first
            raise NotImplementedError(
                "Text similarity search requires embedding. "
                "Either provide a vector or use the REST API's semantic search endpoint."
            )

    def delete(self, ids: list[str]) -> int:
        """
        Delete vectors by ID.

        Args:
            ids: List of vector IDs to delete

        Returns:
            Number of deleted vectors
        """
        http_client = self._client._get_client()
        payload = {"ids": ids}
        response = http_client.request(
            "DELETE",
            f"/v1/vectors/stores/{self.name}/vectors",
            json=payload,
        )
        data = self._client._handle_response(response)
        return data.get("deleted", 0)

    def drop(self) -> None:
        """Delete this vector store."""
        http_client = self._client._get_client()
        response = http_client.delete(f"/v1/vectors/stores/{self.name}")
        self._client._handle_response(response)
        self._info = None

    def count(self) -> int:
        """Get the number of vectors in the store."""
        return self.info.vector_count

    def __repr__(self) -> str:
        return f"VectorStore(name={self.name!r}, dimension={self.dimension})"
