"""
Cohere embedding provider.
"""

import os
from typing import List, Optional

import httpx

from heliosdb.embeddings.base import EmbeddingProvider


class CohereEmbeddings(EmbeddingProvider):
    """
    Cohere embedding provider.

    Uses Cohere's embedding API for multilingual embeddings.

    Example:
        from heliosdb.embeddings import CohereEmbeddings

        embeddings = CohereEmbeddings(
            api_key="...",
            model="embed-english-v3.0"
        )

        vectors = embeddings.embed_documents(["Hello world"])
    """

    # Model dimensions
    MODEL_DIMENSIONS = {
        "embed-english-v3.0": 1024,
        "embed-multilingual-v3.0": 1024,
        "embed-english-light-v3.0": 384,
        "embed-multilingual-light-v3.0": 384,
        "embed-english-v2.0": 4096,
        "embed-multilingual-v2.0": 768,
    }

    def __init__(
        self,
        api_key: Optional[str] = None,
        model: str = "embed-english-v3.0",
        input_type: str = "search_document",
    ) -> None:
        """
        Initialize Cohere embeddings.

        Args:
            api_key: Cohere API key (or set COHERE_API_KEY env var)
            model: Embedding model name
            input_type: Input type for v3 models (search_document, search_query, classification, clustering)
        """
        self._api_key = api_key or os.environ.get("COHERE_API_KEY")
        if not self._api_key:
            raise ValueError(
                "Cohere API key is required. "
                "Set COHERE_API_KEY environment variable or pass api_key parameter."
            )

        self._model = model
        self._input_type = input_type

        # Determine dimension
        if model in self.MODEL_DIMENSIONS:
            self._dimension = self.MODEL_DIMENSIONS[model]
        else:
            self._dimension = 1024  # Default fallback

        self._client = httpx.Client(
            base_url="https://api.cohere.ai/v1",
            headers={
                "Authorization": f"Bearer {self._api_key}",
                "Content-Type": "application/json",
            },
            timeout=60.0,
        )

    @property
    def dimension(self) -> int:
        """Return the embedding dimension."""
        return self._dimension

    def _embed(self, texts: List[str], input_type: str) -> List[List[float]]:
        """Call Cohere embedding API."""
        payload = {
            "texts": texts,
            "model": self._model,
            "truncate": "END",
        }

        # Add input_type for v3 models
        if "v3" in self._model:
            payload["input_type"] = input_type

        response = self._client.post("/embed", json=payload)
        response.raise_for_status()
        data = response.json()

        return data["embeddings"]

    def embed_documents(self, texts: List[str]) -> List[List[float]]:
        """
        Embed a list of documents.

        Args:
            texts: List of texts to embed

        Returns:
            List of embedding vectors
        """
        # Cohere has batch limits, process in chunks
        batch_size = 96
        all_embeddings = []

        for i in range(0, len(texts), batch_size):
            batch = texts[i : i + batch_size]
            embeddings = self._embed(batch, "search_document")
            all_embeddings.extend(embeddings)

        return all_embeddings

    def embed_query(self, text: str) -> List[float]:
        """
        Embed a single query text.

        Args:
            text: Query text to embed

        Returns:
            Embedding vector
        """
        embeddings = self._embed([text], "search_query")
        return embeddings[0]

    def __del__(self) -> None:
        """Clean up client."""
        if hasattr(self, "_client"):
            self._client.close()

    def __repr__(self) -> str:
        return f"CohereEmbeddings(model={self._model!r}, dimension={self._dimension})"
