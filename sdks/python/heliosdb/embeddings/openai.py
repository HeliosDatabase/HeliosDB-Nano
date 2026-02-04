"""
OpenAI embedding provider.
"""

import os
from typing import List, Optional

import httpx

from heliosdb.embeddings.base import EmbeddingProvider


class OpenAIEmbeddings(EmbeddingProvider):
    """
    OpenAI embedding provider.

    Uses OpenAI's embedding API for high-quality embeddings.

    Example:
        from heliosdb.embeddings import OpenAIEmbeddings

        embeddings = OpenAIEmbeddings(
            api_key="sk-...",
            model="text-embedding-3-small"
        )

        vectors = embeddings.embed_documents(["Hello world"])
    """

    # Model dimensions
    MODEL_DIMENSIONS = {
        "text-embedding-3-small": 1536,
        "text-embedding-3-large": 3072,
        "text-embedding-ada-002": 1536,
    }

    def __init__(
        self,
        api_key: Optional[str] = None,
        model: str = "text-embedding-3-small",
        base_url: str = "https://api.openai.com/v1",
        dimensions: Optional[int] = None,
    ) -> None:
        """
        Initialize OpenAI embeddings.

        Args:
            api_key: OpenAI API key (or set OPENAI_API_KEY env var)
            model: Embedding model name
            base_url: API base URL (for custom endpoints)
            dimensions: Override embedding dimensions (for text-embedding-3-* models)
        """
        self._api_key = api_key or os.environ.get("OPENAI_API_KEY")
        if not self._api_key:
            raise ValueError(
                "OpenAI API key is required. "
                "Set OPENAI_API_KEY environment variable or pass api_key parameter."
            )

        self._model = model
        self._base_url = base_url.rstrip("/")
        self._dimensions = dimensions

        # Determine actual dimension
        if dimensions:
            self._actual_dimension = dimensions
        elif model in self.MODEL_DIMENSIONS:
            self._actual_dimension = self.MODEL_DIMENSIONS[model]
        else:
            self._actual_dimension = 1536  # Default fallback

        self._client = httpx.Client(
            base_url=self._base_url,
            headers={
                "Authorization": f"Bearer {self._api_key}",
                "Content-Type": "application/json",
            },
            timeout=60.0,
        )

    @property
    def dimension(self) -> int:
        """Return the embedding dimension."""
        return self._actual_dimension

    def _embed(self, texts: List[str]) -> List[List[float]]:
        """Call OpenAI embedding API."""
        payload = {
            "input": texts,
            "model": self._model,
        }

        if self._dimensions and self._model.startswith("text-embedding-3"):
            payload["dimensions"] = self._dimensions

        response = self._client.post("/embeddings", json=payload)
        response.raise_for_status()
        data = response.json()

        # Sort by index to ensure correct order
        embeddings = sorted(data["data"], key=lambda x: x["index"])
        return [e["embedding"] for e in embeddings]

    def embed_documents(self, texts: List[str]) -> List[List[float]]:
        """
        Embed a list of documents.

        Args:
            texts: List of texts to embed

        Returns:
            List of embedding vectors
        """
        # OpenAI has batch limits, process in chunks
        batch_size = 2000
        all_embeddings = []

        for i in range(0, len(texts), batch_size):
            batch = texts[i : i + batch_size]
            embeddings = self._embed(batch)
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
        embeddings = self._embed([text])
        return embeddings[0]

    def __del__(self) -> None:
        """Clean up client."""
        if hasattr(self, "_client"):
            self._client.close()

    def __repr__(self) -> str:
        return f"OpenAIEmbeddings(model={self._model!r}, dimension={self._actual_dimension})"
