"""
Local embedding provider using sentence-transformers.
"""

from typing import List, Optional

from heliosdb.embeddings.base import EmbeddingProvider


class LocalEmbeddings(EmbeddingProvider):
    """
    Local embedding provider using sentence-transformers.

    No API calls needed - runs entirely locally on CPU or GPU.

    Example:
        from heliosdb.embeddings import LocalEmbeddings

        embeddings = LocalEmbeddings(model="all-MiniLM-L6-v2")

        # Embed documents
        vectors = embeddings.embed_documents(["Hello world", "AI is great"])

        # Embed query
        query_vector = embeddings.embed_query("greeting")
    """

    def __init__(
        self,
        model: str = "all-MiniLM-L6-v2",
        device: Optional[str] = None,
        normalize: bool = True,
    ) -> None:
        """
        Initialize local embeddings.

        Args:
            model: Model name from sentence-transformers
            device: Device to use ('cpu', 'cuda', 'mps', or None for auto)
            normalize: Whether to normalize embeddings
        """
        try:
            from sentence_transformers import SentenceTransformer
        except ImportError:
            raise ImportError(
                "sentence-transformers is required for local embeddings. "
                "Install with: pip install heliosdb[embeddings]"
            )

        self._model_name = model
        self._normalize = normalize
        self._model = SentenceTransformer(model, device=device)
        self._dimension = self._model.get_sentence_embedding_dimension()

    @property
    def dimension(self) -> int:
        """Return the embedding dimension."""
        return self._dimension

    def embed_documents(self, texts: List[str]) -> List[List[float]]:
        """
        Embed a list of documents.

        Args:
            texts: List of texts to embed

        Returns:
            List of embedding vectors
        """
        embeddings = self._model.encode(
            texts,
            normalize_embeddings=self._normalize,
            show_progress_bar=len(texts) > 10,
        )
        return embeddings.tolist()

    def embed_query(self, text: str) -> List[float]:
        """
        Embed a single query text.

        Args:
            text: Query text to embed

        Returns:
            Embedding vector
        """
        embedding = self._model.encode(
            text,
            normalize_embeddings=self._normalize,
        )
        return embedding.tolist()

    def __repr__(self) -> str:
        return f"LocalEmbeddings(model={self._model_name!r}, dimension={self._dimension})"
