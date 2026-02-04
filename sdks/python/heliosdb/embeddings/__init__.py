"""
HeliosDB Embedding Providers.

Provides pluggable embedding providers for automatic text embedding:
- Local embeddings (sentence-transformers)
- OpenAI embeddings
- Cohere embeddings
- Custom embedding providers
"""

from heliosdb.embeddings.base import EmbeddingProvider
from heliosdb.embeddings.local import LocalEmbeddings
from heliosdb.embeddings.openai import OpenAIEmbeddings
from heliosdb.embeddings.cohere import CohereEmbeddings

__all__ = [
    "EmbeddingProvider",
    "LocalEmbeddings",
    "OpenAIEmbeddings",
    "CohereEmbeddings",
]
