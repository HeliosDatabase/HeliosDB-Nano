"""
LlamaIndex integration for HeliosDB.

Provides LlamaIndex-compatible VectorStore implementation.
"""

from __future__ import annotations

from typing import Any, List, Optional

try:
    from llama_index.core.schema import TextNode, BaseNode
    from llama_index.core.vector_stores.types import (
        VectorStore,
        VectorStoreQuery,
        VectorStoreQueryResult,
    )

    LLAMAINDEX_AVAILABLE = True
except ImportError:
    LLAMAINDEX_AVAILABLE = False
    # Create placeholder classes
    TextNode = Any  # type: ignore
    BaseNode = Any  # type: ignore
    VectorStore = object  # type: ignore
    VectorStoreQuery = Any  # type: ignore
    VectorStoreQueryResult = Any  # type: ignore

from heliosdb.client import HeliosDB


def _check_llamaindex() -> None:
    """Check if LlamaIndex is available."""
    if not LLAMAINDEX_AVAILABLE:
        raise ImportError(
            "llama-index-core is required for LlamaIndex integration. "
            "Install with: pip install heliosdb[llamaindex]"
        )


class HeliosDBLlamaVectorStore(VectorStore if LLAMAINDEX_AVAILABLE else object):  # type: ignore
    """
    LlamaIndex VectorStore implementation for HeliosDB.

    Example:
        from llama_index.core import VectorStoreIndex
        from heliosdb.integrations.llamaindex import HeliosDBLlamaVectorStore

        vector_store = HeliosDBLlamaVectorStore(
            connection_string="http://localhost:8080",
            collection_name="documents",
        )

        index = VectorStoreIndex.from_vector_store(vector_store)
        query_engine = index.as_query_engine()
        response = query_engine.query("What is HeliosDB?")
    """

    stores_text: bool = True
    flat_metadata: bool = True

    def __init__(
        self,
        connection_string: str = "http://localhost:8080",
        collection_name: str = "documents",
        api_key: Optional[str] = None,
        dimension: int = 1536,
        metric: str = "cosine",
    ) -> None:
        """
        Initialize HeliosDB LlamaIndex VectorStore.

        Args:
            connection_string: HeliosDB server URL or file path
            collection_name: Name of the vector collection
            api_key: API key for authentication
            dimension: Vector dimension
            metric: Distance metric (cosine, euclidean, dot_product)
        """
        _check_llamaindex()

        self._client = HeliosDB.connect(connection_string, api_key=api_key)
        self._collection_name = collection_name
        self._dimension = dimension
        self._metric = metric
        self._store = self._client.vector_store(
            collection_name,
            dimension=dimension,
            metric=metric,
        )

    @classmethod
    def class_name(cls) -> str:
        """Return class name for serialization."""
        return "HeliosDBVectorStore"

    @property
    def client(self) -> Any:
        """Return the HeliosDB client."""
        return self._client

    def add(
        self,
        nodes: List[BaseNode],
        **kwargs: Any,
    ) -> List[str]:
        """
        Add nodes to the vector store.

        Args:
            nodes: List of LlamaIndex nodes to add

        Returns:
            List of node IDs
        """
        vectors = []
        ids = []

        for node in nodes:
            node_id = node.node_id
            ids.append(node_id)

            # Get embedding
            embedding = node.get_embedding()
            if embedding is None:
                raise ValueError(f"Node {node_id} has no embedding")

            # Build metadata
            metadata = node.metadata.copy() if node.metadata else {}
            metadata["text"] = node.get_content()
            metadata["node_id"] = node_id

            # Handle relationships
            if hasattr(node, "relationships"):
                for rel_type, rel_info in node.relationships.items():
                    metadata[f"rel_{rel_type.name}"] = str(rel_info.node_id)

            vectors.append({
                "id": node_id,
                "vector": embedding,
                "metadata": metadata,
            })

        self._store.upsert(vectors)
        return ids

    def delete(self, ref_doc_id: str, **kwargs: Any) -> None:
        """
        Delete nodes by reference document ID.

        Args:
            ref_doc_id: Reference document ID to delete
        """
        # Query for nodes with this ref_doc_id
        # In a full implementation, we would query by metadata
        # For now, just delete by ID if it matches
        try:
            self._store.delete([ref_doc_id])
        except Exception:
            pass  # Best effort

    def query(
        self,
        query: VectorStoreQuery,
        **kwargs: Any,
    ) -> VectorStoreQueryResult:
        """
        Query the vector store.

        Args:
            query: LlamaIndex VectorStoreQuery

        Returns:
            VectorStoreQueryResult with matching nodes
        """
        _check_llamaindex()

        if query.query_embedding is None:
            raise ValueError("Query must have an embedding")

        # Build filter from query
        filter_dict = None
        if query.filters:
            filter_dict = {}
            for f in query.filters.filters:
                filter_dict[f.key] = f.value

        # Search
        results = self._store.search(
            query.query_embedding,
            top_k=query.similarity_top_k,
            filter=filter_dict,
            include_metadata=True,
        )

        # Convert to LlamaIndex format
        nodes = []
        similarities = []
        ids = []

        for result in results:
            text = result.metadata.pop("text", "")
            node_id = result.metadata.pop("node_id", result.id)

            # Remove relationship metadata
            metadata = {
                k: v for k, v in result.metadata.items()
                if not k.startswith("rel_")
            }

            node = TextNode(
                text=text,
                id_=node_id,
                metadata=metadata,
            )
            nodes.append(node)
            similarities.append(result.score)
            ids.append(node_id)

        return VectorStoreQueryResult(
            nodes=nodes,
            similarities=similarities,
            ids=ids,
        )
