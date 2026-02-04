"""
HeliosDB Agent Memory.

Provides conversation history management with semantic search for AI agents.
"""

from __future__ import annotations

from typing import TYPE_CHECKING, Any, Optional

from heliosdb.models import MemoryMessage, MemorySearchResult

if TYPE_CHECKING:
    from heliosdb.client import HeliosDB


class AgentMemory:
    """
    Agent memory management for conversation history.

    Supports storing messages with embeddings for semantic retrieval,
    making it ideal for AI agents that need to recall relevant context.

    Example:
        memory = db.agent_memory("agent-123")

        # Add messages
        memory.add_message("user", "What's the weather like?")
        memory.add_message("assistant", "I'll check that for you...")

        # Save context (LangChain-compatible)
        memory.save_context(
            {"input": "Tell me about AI"},
            {"output": "AI is fascinating..."}
        )

        # Get recent messages
        messages = memory.get_messages(limit=10)

        # Semantic search in memory
        relevant = memory.search("weather forecast", k=5)
    """

    def __init__(self, client: "HeliosDB", session_id: str) -> None:
        self._client = client
        self.session_id = session_id

    def add_message(
        self,
        role: str,
        content: str,
        *,
        metadata: Optional[dict[str, Any]] = None,
        generate_embedding: bool = True,
    ) -> str:
        """
        Add a message to memory.

        Args:
            role: Message role (user, assistant, system, tool)
            content: Message content
            metadata: Optional metadata
            generate_embedding: Whether to generate embedding for semantic search

        Returns:
            Message ID

        Example:
            msg_id = memory.add_message("user", "Hello!")
        """
        http_client = self._client._get_client()
        payload: dict[str, Any] = {
            "role": role,
            "content": content,
            "generate_embedding": generate_embedding,
        }
        if metadata:
            payload["metadata"] = metadata

        response = http_client.post(
            f"/v1/agents/memory/{self.session_id}/add",
            json=payload,
        )
        data = self._client._handle_response(response)
        return data.get("id", "")

    async def aadd_message(
        self,
        role: str,
        content: str,
        *,
        metadata: Optional[dict[str, Any]] = None,
        generate_embedding: bool = True,
    ) -> str:
        """Add a message to memory (async)."""
        http_client = await self._client._get_async_client()
        payload: dict[str, Any] = {
            "role": role,
            "content": content,
            "generate_embedding": generate_embedding,
        }
        if metadata:
            payload["metadata"] = metadata

        response = await http_client.post(
            f"/v1/agents/memory/{self.session_id}/add",
            json=payload,
        )
        data = self._client._handle_response(response)
        return data.get("id", "")

    def get_messages(self, limit: int = 50) -> list[MemoryMessage]:
        """
        Get recent messages from memory.

        Args:
            limit: Maximum number of messages to return

        Returns:
            List of messages in chronological order
        """
        http_client = self._client._get_client()
        response = http_client.get(
            f"/v1/agents/memory/{self.session_id}",
            params={"limit": limit},
        )
        data = self._client._handle_response(response)
        return [MemoryMessage(**m) for m in data.get("messages", [])]

    async def aget_messages(self, limit: int = 50) -> list[MemoryMessage]:
        """Get recent messages from memory (async)."""
        http_client = await self._client._get_async_client()
        response = await http_client.get(
            f"/v1/agents/memory/{self.session_id}",
            params={"limit": limit},
        )
        data = self._client._handle_response(response)
        return [MemoryMessage(**m) for m in data.get("messages", [])]

    def search(
        self,
        query: str,
        *,
        k: int = 5,
        filter: Optional[dict[str, Any]] = None,
    ) -> list[MemorySearchResult]:
        """
        Semantic search in memory.

        Args:
            query: Search query
            k: Number of results
            filter: Optional metadata filter

        Returns:
            List of relevant messages with scores

        Example:
            results = memory.search("weather forecast", k=5)
            for r in results:
                print(f"{r.message.role}: {r.message.content} (score: {r.score})")
        """
        http_client = self._client._get_client()
        payload: dict[str, Any] = {
            "query": query,
            "top_k": k,
        }
        if filter:
            payload["filter"] = filter

        response = http_client.post(
            f"/v1/agents/memory/{self.session_id}/search",
            json=payload,
        )
        data = self._client._handle_response(response)
        return [MemorySearchResult(**r) for r in data.get("results", [])]

    async def asearch(
        self,
        query: str,
        *,
        k: int = 5,
        filter: Optional[dict[str, Any]] = None,
    ) -> list[MemorySearchResult]:
        """Semantic search in memory (async)."""
        http_client = await self._client._get_async_client()
        payload: dict[str, Any] = {
            "query": query,
            "top_k": k,
        }
        if filter:
            payload["filter"] = filter

        response = await http_client.post(
            f"/v1/agents/memory/{self.session_id}/search",
            json=payload,
        )
        data = self._client._handle_response(response)
        return [MemorySearchResult(**r) for r in data.get("results", [])]

    def clear(self) -> None:
        """Clear all messages from memory."""
        http_client = self._client._get_client()
        response = http_client.delete(f"/v1/agents/memory/{self.session_id}")
        self._client._handle_response(response)

    async def aclear(self) -> None:
        """Clear all messages from memory (async)."""
        http_client = await self._client._get_async_client()
        response = await http_client.delete(f"/v1/agents/memory/{self.session_id}")
        self._client._handle_response(response)

    # LangChain-compatible methods

    def save_context(
        self,
        inputs: dict[str, Any],
        outputs: dict[str, Any],
    ) -> None:
        """
        Save conversation context (LangChain-compatible).

        Args:
            inputs: Input dictionary with 'input' key
            outputs: Output dictionary with 'output' key

        Example:
            memory.save_context(
                {"input": "What is AI?"},
                {"output": "AI stands for Artificial Intelligence..."}
            )
        """
        # Extract input/output from dicts
        user_input = inputs.get("input", str(inputs))
        assistant_output = outputs.get("output", str(outputs))

        self.add_message("user", user_input)
        self.add_message("assistant", assistant_output)

    async def asave_context(
        self,
        inputs: dict[str, Any],
        outputs: dict[str, Any],
    ) -> None:
        """Save conversation context (async, LangChain-compatible)."""
        user_input = inputs.get("input", str(inputs))
        assistant_output = outputs.get("output", str(outputs))

        await self.aadd_message("user", user_input)
        await self.aadd_message("assistant", assistant_output)

    def load_memory_variables(self, inputs: Optional[dict[str, Any]] = None) -> dict[str, Any]:
        """
        Load memory variables (LangChain-compatible).

        Returns conversation history formatted for LangChain.
        """
        messages = self.get_messages()
        history = []
        for msg in messages:
            if msg.role == "user":
                history.append(f"Human: {msg.content}")
            elif msg.role == "assistant":
                history.append(f"AI: {msg.content}")
            else:
                history.append(f"{msg.role.title()}: {msg.content}")

        return {"history": "\n".join(history)}

    async def aload_memory_variables(
        self, inputs: Optional[dict[str, Any]] = None
    ) -> dict[str, Any]:
        """Load memory variables (async, LangChain-compatible)."""
        messages = await self.aget_messages()
        history = []
        for msg in messages:
            if msg.role == "user":
                history.append(f"Human: {msg.content}")
            elif msg.role == "assistant":
                history.append(f"AI: {msg.content}")
            else:
                history.append(f"{msg.role.title()}: {msg.content}")

        return {"history": "\n".join(history)}

    def __repr__(self) -> str:
        return f"AgentMemory(session_id={self.session_id!r})"
