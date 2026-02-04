"""
Base embedding provider interface.
"""

from abc import ABC, abstractmethod
from typing import List


class EmbeddingProvider(ABC):
    """
    Abstract base class for embedding providers.

    All embedding providers must implement:
    - embed_documents: Embed multiple texts
    - embed_query: Embed a single query
    - dimension: Return the embedding dimension
    """

    @abstractmethod
    def embed_documents(self, texts: List[str]) -> List[List[float]]:
        """
        Embed a list of documents.

        Args:
            texts: List of texts to embed

        Returns:
            List of embedding vectors
        """
        pass

    @abstractmethod
    def embed_query(self, text: str) -> List[float]:
        """
        Embed a single query text.

        Args:
            text: Query text to embed

        Returns:
            Embedding vector
        """
        pass

    @property
    @abstractmethod
    def dimension(self) -> int:
        """Return the embedding dimension."""
        pass

    def __repr__(self) -> str:
        return f"{self.__class__.__name__}(dimension={self.dimension})"
