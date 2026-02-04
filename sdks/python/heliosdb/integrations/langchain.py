"""
LangChain integration for HeliosDB.

Provides LangChain-compatible VectorStore and Memory implementations.
"""

from __future__ import annotations

from typing import Any, Iterable, List, Optional, Tuple, Type

try:
    from langchain_core.documents import Document
    from langchain_core.embeddings import Embeddings
    from langchain_core.vectorstores import VectorStore
    from langchain_core.memory import BaseMemory
    from langchain_core.document_loaders import BaseLoader

    LANGCHAIN_AVAILABLE = True
except ImportError:
    LANGCHAIN_AVAILABLE = False
    # Create placeholder classes for type hints
    Document = Any  # type: ignore
    Embeddings = Any  # type: ignore
    VectorStore = object  # type: ignore
    BaseMemory = object  # type: ignore
    BaseLoader = object  # type: ignore

from heliosdb.client import HeliosDB


def _check_langchain() -> None:
    """Check if LangChain is available."""
    if not LANGCHAIN_AVAILABLE:
        raise ImportError(
            "langchain-core is required for LangChain integration. "
            "Install with: pip install heliosdb[langchain]"
        )


class HeliosDBVectorStore(VectorStore if LANGCHAIN_AVAILABLE else object):  # type: ignore
    """
    LangChain VectorStore implementation for HeliosDB.

    Example:
        from langchain_openai import OpenAIEmbeddings
        from heliosdb.integrations.langchain import HeliosDBVectorStore

        embeddings = OpenAIEmbeddings()
        vectorstore = HeliosDBVectorStore(
            connection_string="http://localhost:8080",
            collection_name="documents",
            embedding=embeddings,
        )

        # Add documents
        vectorstore.add_documents(docs)

        # Search
        results = vectorstore.similarity_search("query", k=5)
    """

    def __init__(
        self,
        connection_string: str = "http://localhost:8080",
        collection_name: str = "documents",
        embedding: Optional[Embeddings] = None,
        api_key: Optional[str] = None,
        dimension: int = 1536,
        metric: str = "cosine",
    ) -> None:
        """
        Initialize HeliosDB VectorStore.

        Args:
            connection_string: HeliosDB server URL or file path
            collection_name: Name of the vector collection
            embedding: LangChain Embeddings instance
            api_key: API key for authentication
            dimension: Vector dimension
            metric: Distance metric (cosine, euclidean, dot_product)
        """
        _check_langchain()

        self._client = HeliosDB.connect(connection_string, api_key=api_key)
        self._collection_name = collection_name
        self._embedding = embedding
        self._dimension = dimension
        self._metric = metric
        self._store = self._client.vector_store(
            collection_name,
            dimension=dimension,
            metric=metric,
        )

    @property
    def embeddings(self) -> Optional[Embeddings]:
        """Return the embeddings instance."""
        return self._embedding

    def add_texts(
        self,
        texts: Iterable[str],
        metadatas: Optional[List[dict]] = None,
        **kwargs: Any,
    ) -> List[str]:
        """
        Add texts to the vectorstore.

        Args:
            texts: Texts to add
            metadatas: Optional metadata for each text

        Returns:
            List of IDs for added texts
        """
        texts_list = list(texts)

        if self._embedding is None:
            # Use server-side embedding
            return self._store.add_texts(
                texts_list,
                metadatas=metadatas,
            )

        # Embed locally and upsert
        embeddings = self._embedding.embed_documents(texts_list)
        vectors = []
        for i, (text, embedding) in enumerate(zip(texts_list, embeddings)):
            vector_id = kwargs.get("ids", [None] * len(texts_list))[i]
            if vector_id is None:
                import uuid
                vector_id = str(uuid.uuid4())

            metadata = metadatas[i] if metadatas and i < len(metadatas) else {}
            metadata["text"] = text

            vectors.append({
                "id": vector_id,
                "vector": embedding,
                "metadata": metadata,
            })

        self._store.upsert(vectors)
        return [v["id"] for v in vectors]

    def add_documents(
        self,
        documents: List[Document],
        **kwargs: Any,
    ) -> List[str]:
        """
        Add documents to the vectorstore.

        Args:
            documents: LangChain Documents to add

        Returns:
            List of IDs for added documents
        """
        texts = [doc.page_content for doc in documents]
        metadatas = [doc.metadata for doc in documents]
        return self.add_texts(texts, metadatas, **kwargs)

    def similarity_search(
        self,
        query: str,
        k: int = 4,
        **kwargs: Any,
    ) -> List[Document]:
        """
        Search for similar documents.

        Args:
            query: Query text
            k: Number of results

        Returns:
            List of similar Documents
        """
        if self._embedding is None:
            raise ValueError("Embeddings are required for similarity_search")

        query_embedding = self._embedding.embed_query(query)
        return self.similarity_search_by_vector(query_embedding, k, **kwargs)

    def similarity_search_by_vector(
        self,
        embedding: List[float],
        k: int = 4,
        **kwargs: Any,
    ) -> List[Document]:
        """
        Search for similar documents by vector.

        Args:
            embedding: Query vector
            k: Number of results

        Returns:
            List of similar Documents
        """
        filter_dict = kwargs.get("filter")
        results = self._store.search(
            embedding,
            top_k=k,
            filter=filter_dict,
            include_metadata=True,
        )

        documents = []
        for result in results:
            text = result.metadata.pop("text", "")
            documents.append(Document(
                page_content=text,
                metadata={
                    **result.metadata,
                    "score": result.score,
                    "id": result.id,
                },
            ))
        return documents

    def similarity_search_with_score(
        self,
        query: str,
        k: int = 4,
        **kwargs: Any,
    ) -> List[Tuple[Document, float]]:
        """
        Search for similar documents with scores.

        Args:
            query: Query text
            k: Number of results

        Returns:
            List of (Document, score) tuples
        """
        docs = self.similarity_search(query, k, **kwargs)
        return [(doc, doc.metadata.get("score", 0.0)) for doc in docs]

    @classmethod
    def from_texts(
        cls: Type["HeliosDBVectorStore"],
        texts: List[str],
        embedding: Embeddings,
        metadatas: Optional[List[dict]] = None,
        **kwargs: Any,
    ) -> "HeliosDBVectorStore":
        """
        Create a vectorstore from texts.

        Args:
            texts: Texts to add
            embedding: Embeddings instance
            metadatas: Optional metadata

        Returns:
            HeliosDBVectorStore instance
        """
        connection_string = kwargs.pop("connection_string", "http://localhost:8080")
        collection_name = kwargs.pop("collection_name", "documents")

        store = cls(
            connection_string=connection_string,
            collection_name=collection_name,
            embedding=embedding,
            **kwargs,
        )
        store.add_texts(texts, metadatas)
        return store

    @classmethod
    def from_documents(
        cls: Type["HeliosDBVectorStore"],
        documents: List[Document],
        embedding: Embeddings,
        **kwargs: Any,
    ) -> "HeliosDBVectorStore":
        """
        Create a vectorstore from documents.

        Args:
            documents: Documents to add
            embedding: Embeddings instance

        Returns:
            HeliosDBVectorStore instance
        """
        texts = [doc.page_content for doc in documents]
        metadatas = [doc.metadata for doc in documents]
        return cls.from_texts(texts, embedding, metadatas, **kwargs)

    def delete(self, ids: Optional[List[str]] = None, **kwargs: Any) -> Optional[bool]:
        """Delete vectors by ID."""
        if ids:
            self._store.delete(ids)
            return True
        return False


class HeliosDBChatMemory(BaseMemory if LANGCHAIN_AVAILABLE else object):  # type: ignore
    """
    LangChain Memory implementation backed by HeliosDB.

    Example:
        from heliosdb.integrations.langchain import HeliosDBChatMemory

        memory = HeliosDBChatMemory(
            connection_string="http://localhost:8080",
            session_id="user-123",
        )

        # Use with LangChain agent
        from langchain.agents import initialize_agent
        agent = initialize_agent(tools, llm, memory=memory)
    """

    def __init__(
        self,
        connection_string: str = "http://localhost:8080",
        session_id: str = "default",
        api_key: Optional[str] = None,
        memory_key: str = "history",
        human_prefix: str = "Human",
        ai_prefix: str = "AI",
    ) -> None:
        """
        Initialize HeliosDB Chat Memory.

        Args:
            connection_string: HeliosDB server URL
            session_id: Unique session identifier
            api_key: API key for authentication
            memory_key: Key for memory in chain
            human_prefix: Prefix for human messages
            ai_prefix: Prefix for AI messages
        """
        _check_langchain()

        self._client = HeliosDB.connect(connection_string, api_key=api_key)
        self._memory = self._client.agent_memory(session_id)
        self.memory_key = memory_key
        self.human_prefix = human_prefix
        self.ai_prefix = ai_prefix

    @property
    def memory_variables(self) -> List[str]:
        """Return memory variables."""
        return [self.memory_key]

    def load_memory_variables(self, inputs: dict[str, Any]) -> dict[str, Any]:
        """Load memory variables for chain."""
        messages = self._memory.get_messages()
        history_lines = []
        for msg in messages:
            if msg.role == "user":
                history_lines.append(f"{self.human_prefix}: {msg.content}")
            elif msg.role == "assistant":
                history_lines.append(f"{self.ai_prefix}: {msg.content}")
            else:
                history_lines.append(f"{msg.role.title()}: {msg.content}")

        return {self.memory_key: "\n".join(history_lines)}

    def save_context(self, inputs: dict[str, Any], outputs: dict[str, str]) -> None:
        """Save context from chain run."""
        self._memory.save_context(inputs, outputs)

    def clear(self) -> None:
        """Clear memory."""
        self._memory.clear()


class HeliosDBDocumentLoader(BaseLoader if LANGCHAIN_AVAILABLE else object):  # type: ignore
    """
    LangChain Document Loader for HeliosDB.

    Loads documents from HeliosDB tables as LangChain Documents.

    Example:
        from heliosdb.integrations.langchain import HeliosDBDocumentLoader

        loader = HeliosDBDocumentLoader(
            connection_string="http://localhost:8080",
            query="SELECT id, content, metadata FROM documents",
            content_column="content",
            metadata_columns=["id", "metadata"],
        )

        docs = loader.load()
    """

    def __init__(
        self,
        connection_string: str = "http://localhost:8080",
        query: str = "",
        content_column: str = "content",
        metadata_columns: Optional[List[str]] = None,
        api_key: Optional[str] = None,
        branch: str = "main",
    ) -> None:
        """
        Initialize HeliosDB Document Loader.

        Args:
            connection_string: HeliosDB server URL
            query: SQL query to load documents
            content_column: Column containing document content
            metadata_columns: Columns to include in metadata
            api_key: API key for authentication
            branch: Branch to query
        """
        _check_langchain()

        self._client = HeliosDB.connect(connection_string, api_key=api_key)
        self._query = query
        self._content_column = content_column
        self._metadata_columns = metadata_columns or []
        self._branch = branch

    def load(self) -> List[Document]:
        """Load documents from HeliosDB."""
        result = self._client.query(self._query, branch=self._branch)
        documents = []

        for row_dict in result.to_dicts():
            content = row_dict.get(self._content_column, "")
            metadata = {
                col: row_dict.get(col)
                for col in self._metadata_columns
                if col in row_dict
            }
            documents.append(Document(page_content=str(content), metadata=metadata))

        return documents
