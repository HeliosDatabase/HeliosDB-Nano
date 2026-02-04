"""Memory management for AutoGen agents using HeliosDB.

This module provides persistent memory capabilities that integrate
with AutoGen's conversation flows.
"""

from typing import Any, Optional
from dataclasses import dataclass, field
from datetime import datetime
import json
import uuid

from .client import HeliosDBClient, VectorSearchResult


@dataclass
class MemoryEntry:
    """A single memory entry."""
    id: str
    role: str
    content: str
    timestamp: str
    metadata: dict[str, Any] = field(default_factory=dict)
    score: Optional[float] = None  # For search results


class AgentMemoryManager:
    """Manages persistent memory for AutoGen agents.

    This class provides a high-level interface for storing and retrieving
    agent memories across sessions.

    Args:
        heliosdb_url: HeliosDB server URL
        api_key: Optional API key
        default_session: Default session ID

    Example:
        ```python
        from heliosdb_autogen import AgentMemoryManager

        memory = AgentMemoryManager("http://localhost:8080")

        # Store memories
        await memory.store("session_123", "user", "What is the capital of France?")
        await memory.store("session_123", "assistant", "The capital of France is Paris.")

        # Retrieve recent messages
        messages = await memory.get_recent("session_123", limit=10)

        # Search memories semantically
        relevant = await memory.search("session_123", "European capitals")
        ```
    """

    def __init__(
        self,
        heliosdb_url: str = "http://localhost:8080",
        api_key: Optional[str] = None,
        default_session: Optional[str] = None,
    ):
        self.client = HeliosDBClient(heliosdb_url, api_key)
        self.default_session = default_session or str(uuid.uuid4())
        self._session_metadata: dict[str, dict[str, Any]] = {}

    async def store(
        self,
        session_id: Optional[str],
        role: str,
        content: str,
        metadata: Optional[dict[str, Any]] = None,
    ) -> None:
        """Store a message in memory.

        Args:
            session_id: Session identifier (uses default if None)
            role: Message role (user, assistant, system)
            content: Message content
            metadata: Optional additional metadata
        """
        session = session_id or self.default_session
        await self.client.memory_add(session, role, content)

        # Track session metadata
        if session not in self._session_metadata:
            self._session_metadata[session] = {
                "created_at": datetime.utcnow().isoformat(),
                "message_count": 0,
            }
        self._session_metadata[session]["message_count"] += 1
        self._session_metadata[session]["last_updated"] = datetime.utcnow().isoformat()

    async def get_recent(
        self,
        session_id: Optional[str] = None,
        limit: int = 50,
    ) -> list[MemoryEntry]:
        """Get recent messages from memory.

        Args:
            session_id: Session identifier
            limit: Maximum messages to return

        Returns:
            List of memory entries in chronological order
        """
        session = session_id or self.default_session
        messages = await self.client.memory_get(session, limit)
        return [
            MemoryEntry(
                id=msg.get("id", ""),
                role=msg["role"],
                content=msg["content"],
                timestamp=msg.get("timestamp", ""),
                metadata=msg.get("metadata", {}),
            )
            for msg in messages
        ]

    async def search(
        self,
        session_id: Optional[str],
        query: str,
        top_k: int = 5,
    ) -> list[MemoryEntry]:
        """Search memory semantically.

        Args:
            session_id: Session identifier
            query: Search query
            top_k: Maximum results

        Returns:
            List of relevant memory entries with scores
        """
        session = session_id or self.default_session
        results = await self.client.memory_search(session, query, top_k)
        return [
            MemoryEntry(
                id=r.id,
                role="",  # Not returned from search
                content=r.content or "",
                timestamp="",
                metadata=r.metadata or {},
                score=r.score,
            )
            for r in results
        ]

    async def clear(self, session_id: Optional[str] = None) -> None:
        """Clear all memories for a session.

        Args:
            session_id: Session identifier
        """
        session = session_id or self.default_session
        await self.client.memory_clear(session)
        if session in self._session_metadata:
            del self._session_metadata[session]

    async def get_context(
        self,
        session_id: Optional[str] = None,
        query: Optional[str] = None,
        max_tokens: int = 2000,
        recent_count: int = 5,
        search_count: int = 3,
    ) -> str:
        """Get relevant context for a query.

        Combines recent messages with semantically relevant memories.

        Args:
            session_id: Session identifier
            query: Optional query to search for
            max_tokens: Approximate max tokens for context
            recent_count: Number of recent messages to include
            search_count: Number of search results to include

        Returns:
            Formatted context string
        """
        session = session_id or self.default_session
        context_parts = []

        # Get recent messages
        recent = await self.get_recent(session, recent_count)
        if recent:
            context_parts.append("Recent conversation:")
            for entry in recent:
                context_parts.append(f"  [{entry.role}]: {entry.content}")

        # Search for relevant memories if query provided
        if query:
            relevant = await self.search(session, query, search_count)
            if relevant:
                context_parts.append("\nRelevant memories:")
                for entry in relevant:
                    context_parts.append(f"  (score: {entry.score:.2f}) {entry.content}")

        return "\n".join(context_parts)

    def new_session(self) -> str:
        """Create a new session ID.

        Returns:
            New session identifier
        """
        session_id = str(uuid.uuid4())
        self._session_metadata[session_id] = {
            "created_at": datetime.utcnow().isoformat(),
            "message_count": 0,
        }
        return session_id


class ConversationMemory:
    """Tracks conversation history with HeliosDB persistence.

    This class implements a conversation buffer that automatically
    persists to HeliosDB.

    Example:
        ```python
        memory = ConversationMemory(
            heliosdb_url="http://localhost:8080",
            session_id="conversation_123"
        )

        # Add messages
        await memory.add_user_message("Hello!")
        await memory.add_assistant_message("Hi there! How can I help?")

        # Get conversation as messages list
        messages = await memory.get_messages()
        ```
    """

    def __init__(
        self,
        heliosdb_url: str = "http://localhost:8080",
        api_key: Optional[str] = None,
        session_id: Optional[str] = None,
        max_messages: int = 100,
    ):
        self.manager = AgentMemoryManager(heliosdb_url, api_key, session_id)
        self.session_id = session_id or self.manager.default_session
        self.max_messages = max_messages

    async def add_user_message(self, content: str) -> None:
        """Add a user message."""
        await self.manager.store(self.session_id, "user", content)

    async def add_assistant_message(self, content: str) -> None:
        """Add an assistant message."""
        await self.manager.store(self.session_id, "assistant", content)

    async def add_system_message(self, content: str) -> None:
        """Add a system message."""
        await self.manager.store(self.session_id, "system", content)

    async def get_messages(
        self,
        limit: Optional[int] = None,
    ) -> list[dict[str, str]]:
        """Get conversation messages in OpenAI format.

        Args:
            limit: Maximum messages to return

        Returns:
            List of message dicts with 'role' and 'content'
        """
        entries = await self.manager.get_recent(
            self.session_id, limit or self.max_messages
        )
        return [{"role": e.role, "content": e.content} for e in entries]

    async def get_context_string(self) -> str:
        """Get conversation as a formatted string.

        Returns:
            Formatted conversation string
        """
        entries = await self.manager.get_recent(self.session_id, self.max_messages)
        lines = []
        for entry in entries:
            lines.append(f"{entry.role.upper()}: {entry.content}")
        return "\n".join(lines)

    async def clear(self) -> None:
        """Clear conversation history."""
        await self.manager.clear(self.session_id)


class SemanticMemory:
    """Long-term semantic memory using vector search.

    This class provides a knowledge base that can be queried
    semantically to retrieve relevant information.

    Example:
        ```python
        memory = SemanticMemory(
            heliosdb_url="http://localhost:8080",
            store_name="agent_knowledge"
        )

        # Store knowledge
        await memory.remember("Python is a programming language.")
        await memory.remember("Paris is the capital of France.")

        # Query knowledge
        facts = await memory.recall("What languages are used in programming?")
        ```
    """

    def __init__(
        self,
        heliosdb_url: str = "http://localhost:8080",
        api_key: Optional[str] = None,
        store_name: str = "semantic_memory",
    ):
        self.client = HeliosDBClient(heliosdb_url, api_key)
        self.store_name = store_name

    async def remember(
        self,
        fact: str,
        metadata: Optional[dict[str, Any]] = None,
    ) -> str:
        """Store a fact in semantic memory.

        Args:
            fact: The fact or information to remember
            metadata: Optional metadata (source, timestamp, etc.)

        Returns:
            Document ID
        """
        meta = metadata or {}
        meta["stored_at"] = datetime.utcnow().isoformat()
        return await self.client.store_text(self.store_name, fact, meta)

    async def remember_many(
        self,
        facts: list[str],
        metadatas: Optional[list[dict[str, Any]]] = None,
    ) -> list[str]:
        """Store multiple facts.

        Args:
            facts: List of facts to store
            metadatas: Optional metadata for each fact

        Returns:
            List of document IDs
        """
        metas = metadatas or [{} for _ in facts]
        timestamp = datetime.utcnow().isoformat()
        for meta in metas:
            meta["stored_at"] = timestamp
        return await self.client.store_texts(self.store_name, facts, metas)

    async def recall(
        self,
        query: str,
        top_k: int = 5,
        min_score: float = 0.5,
    ) -> list[dict[str, Any]]:
        """Recall relevant facts.

        Args:
            query: Query to search for
            top_k: Maximum results
            min_score: Minimum similarity score

        Returns:
            List of relevant facts with scores
        """
        results = await self.client.vector_search(
            self.store_name, query, top_k, min_score
        )
        return [
            {
                "id": r.id,
                "fact": r.content,
                "score": r.score,
                "metadata": r.metadata,
            }
            for r in results
        ]

    async def forget(self, fact_id: str) -> None:
        """Remove a fact from memory.

        Args:
            fact_id: ID of the fact to remove
        """
        # Would need a delete endpoint - placeholder
        pass

    async def get_relevant_context(
        self,
        query: str,
        max_facts: int = 5,
        min_score: float = 0.6,
    ) -> str:
        """Get relevant context as a string.

        Args:
            query: Query to search for
            max_facts: Maximum facts to include
            min_score: Minimum similarity score

        Returns:
            Formatted context string
        """
        facts = await self.recall(query, max_facts, min_score)
        if not facts:
            return ""

        lines = ["Relevant knowledge:"]
        for fact in facts:
            lines.append(f"- {fact['fact']}")
        return "\n".join(lines)
