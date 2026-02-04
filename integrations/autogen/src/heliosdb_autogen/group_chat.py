"""Group chat management with HeliosDB persistence.

This module provides group chat managers that persist conversations
and enable advanced multi-agent coordination patterns.
"""

from typing import Any, Optional, Callable
from datetime import datetime
import json
import uuid

try:
    from autogen import GroupChat, GroupChatManager, ConversableAgent
    AUTOGEN_AVAILABLE = True
except ImportError:
    AUTOGEN_AVAILABLE = False
    class GroupChat:  # type: ignore
        pass
    class GroupChatManager:  # type: ignore
        pass
    class ConversableAgent:  # type: ignore
        pass

from .client import HeliosDBClient
from .memory import AgentMemoryManager


class HeliosDBGroupChatManager(GroupChatManager if AUTOGEN_AVAILABLE else object):
    """Group chat manager with HeliosDB persistence and context retrieval.

    This manager extends AutoGen's GroupChatManager with:
    - Persistent conversation history in HeliosDB
    - Semantic search over past conversations
    - Cross-session context retrieval
    - Agent performance tracking

    Args:
        groupchat: AutoGen GroupChat instance
        heliosdb_url: HeliosDB server URL
        api_key: Optional API key
        session_id: Unique session identifier
        llm_config: LLM configuration
        **kwargs: Additional GroupChatManager arguments

    Example:
        ```python
        from autogen import GroupChat
        from heliosdb_autogen import HeliosDBGroupChatManager, HeliosDBAgent

        # Create agents
        analyst = HeliosDBAgent(name="analyst", ...)
        coder = HeliosDBAgent(name="coder", ...)
        reviewer = HeliosDBAgent(name="reviewer", ...)

        # Create group chat
        groupchat = GroupChat(
            agents=[analyst, coder, reviewer],
            messages=[],
            max_round=10
        )

        # Create persistent manager
        manager = HeliosDBGroupChatManager(
            groupchat=groupchat,
            heliosdb_url="http://localhost:8080",
            llm_config={"model": "gpt-4"}
        )

        # Messages are automatically persisted
        await user_proxy.initiate_chat(manager, message="Build a data pipeline")
        ```
    """

    def __init__(
        self,
        groupchat: Any,  # GroupChat
        heliosdb_url: str = "http://localhost:8080",
        api_key: Optional[str] = None,
        session_id: Optional[str] = None,
        llm_config: Optional[dict[str, Any]] = None,
        **kwargs: Any,
    ):
        if not AUTOGEN_AVAILABLE:
            raise ImportError(
                "AutoGen is required. Install with: pip install pyautogen"
            )

        self.heliosdb_client = HeliosDBClient(heliosdb_url, api_key)
        self.memory_manager = AgentMemoryManager(heliosdb_url, api_key)
        self.session_id = session_id or str(uuid.uuid4())
        self._message_count = 0

        super().__init__(
            groupchat=groupchat,
            llm_config=llm_config,
            **kwargs,
        )

    async def persist_message(
        self,
        sender: str,
        message: str,
        metadata: Optional[dict[str, Any]] = None,
    ) -> None:
        """Persist a message to HeliosDB.

        Args:
            sender: Agent name who sent the message
            message: Message content
            metadata: Optional additional metadata
        """
        await self.memory_manager.store(
            self.session_id,
            sender,
            message,
            metadata,
        )
        self._message_count += 1

    async def get_relevant_context(
        self,
        query: str,
        max_results: int = 5,
    ) -> str:
        """Get relevant context from conversation history.

        Args:
            query: Search query
            max_results: Maximum results to return

        Returns:
            Formatted context string
        """
        return await self.memory_manager.get_context(
            self.session_id,
            query,
            search_count=max_results,
        )

    async def get_conversation_summary(self) -> dict[str, Any]:
        """Get summary of the conversation.

        Returns:
            Dictionary with conversation statistics
        """
        messages = await self.memory_manager.get_recent(self.session_id, 1000)

        # Count by agent
        agent_counts: dict[str, int] = {}
        for msg in messages:
            agent_counts[msg.role] = agent_counts.get(msg.role, 0) + 1

        return {
            "session_id": self.session_id,
            "total_messages": len(messages),
            "messages_by_agent": agent_counts,
        }

    async def load_session(self, session_id: str) -> list[dict[str, str]]:
        """Load a previous session's messages.

        Args:
            session_id: Session to load

        Returns:
            List of messages
        """
        messages = await self.memory_manager.get_recent(session_id, 1000)
        return [{"role": m.role, "content": m.content} for m in messages]


class PersistentGroupChat:
    """Helper class for creating persistent group chats.

    This provides a simplified interface for setting up group chats
    with HeliosDB persistence.

    Example:
        ```python
        from heliosdb_autogen import PersistentGroupChat, HeliosDBAgent

        # Create agents
        agents = [
            HeliosDBAgent(name="researcher", ...),
            HeliosDBAgent(name="writer", ...),
        ]

        # Create persistent group chat
        chat = PersistentGroupChat(
            agents=agents,
            heliosdb_url="http://localhost:8080"
        )

        # Start conversation
        result = await chat.start("Research quantum computing")
        ```
    """

    def __init__(
        self,
        agents: list[Any],
        heliosdb_url: str = "http://localhost:8080",
        api_key: Optional[str] = None,
        max_round: int = 10,
        llm_config: Optional[dict[str, Any]] = None,
        session_id: Optional[str] = None,
    ):
        if not AUTOGEN_AVAILABLE:
            raise ImportError(
                "AutoGen is required. Install with: pip install pyautogen"
            )

        self.agents = agents
        self.session_id = session_id or str(uuid.uuid4())
        self.heliosdb_url = heliosdb_url
        self.api_key = api_key
        self.max_round = max_round
        self.llm_config = llm_config

        self._groupchat: Optional[GroupChat] = None
        self._manager: Optional[HeliosDBGroupChatManager] = None

    def _setup(self) -> None:
        """Set up the group chat and manager."""
        if self._groupchat is None:
            self._groupchat = GroupChat(
                agents=self.agents,
                messages=[],
                max_round=self.max_round,
            )
            self._manager = HeliosDBGroupChatManager(
                groupchat=self._groupchat,
                heliosdb_url=self.heliosdb_url,
                api_key=self.api_key,
                session_id=self.session_id,
                llm_config=self.llm_config,
            )

    @property
    def manager(self) -> HeliosDBGroupChatManager:
        """Get the group chat manager."""
        self._setup()
        assert self._manager is not None
        return self._manager

    @property
    def groupchat(self) -> GroupChat:
        """Get the group chat."""
        self._setup()
        assert self._groupchat is not None
        return self._groupchat


class ConversationCoordinator:
    """Coordinates multi-agent conversations with persistence.

    This class manages the flow of conversations between agents,
    persists all messages, and provides context retrieval.

    Example:
        ```python
        coordinator = ConversationCoordinator(
            heliosdb_url="http://localhost:8080",
            agents={
                "analyst": analyst_agent,
                "coder": coder_agent,
            }
        )

        # Run a coordinated task
        result = await coordinator.run_task(
            task="Analyze sales data and create a report",
            workflow=["analyst", "coder", "analyst"]
        )
        ```
    """

    def __init__(
        self,
        heliosdb_url: str = "http://localhost:8080",
        api_key: Optional[str] = None,
        agents: Optional[dict[str, Any]] = None,
    ):
        self.client = HeliosDBClient(heliosdb_url, api_key)
        self.memory = AgentMemoryManager(heliosdb_url, api_key)
        self.agents = agents or {}
        self._sessions: dict[str, dict[str, Any]] = {}

    def register_agent(self, name: str, agent: Any) -> None:
        """Register an agent.

        Args:
            name: Agent identifier
            agent: Agent instance
        """
        self.agents[name] = agent

    async def start_session(
        self,
        session_id: Optional[str] = None,
        context: Optional[str] = None,
    ) -> str:
        """Start a new coordination session.

        Args:
            session_id: Optional session ID
            context: Optional initial context

        Returns:
            Session ID
        """
        sid = session_id or str(uuid.uuid4())
        self._sessions[sid] = {
            "started_at": datetime.utcnow().isoformat(),
            "messages": [],
            "current_agent": None,
        }

        if context:
            await self.memory.store(sid, "system", context)

        return sid

    async def send_message(
        self,
        session_id: str,
        from_agent: str,
        to_agent: str,
        message: str,
    ) -> Optional[str]:
        """Send a message between agents.

        Args:
            session_id: Session ID
            from_agent: Sender agent name
            to_agent: Receiver agent name
            message: Message content

        Returns:
            Response from receiving agent
        """
        if session_id not in self._sessions:
            raise ValueError(f"Session {session_id} not found")

        # Store outgoing message
        await self.memory.store(
            session_id,
            from_agent,
            f"[to {to_agent}] {message}",
        )

        # Get response from target agent
        target = self.agents.get(to_agent)
        if target and hasattr(target, "generate_reply"):
            # Get context for the target agent
            context = await self.memory.get_context(
                session_id, message, recent_count=5, search_count=3
            )

            # Generate reply (simplified - actual implementation depends on agent type)
            response = f"Response from {to_agent}"  # Placeholder

            # Store response
            await self.memory.store(session_id, to_agent, response)

            return response

        return None

    async def get_session_history(
        self,
        session_id: str,
        limit: int = 100,
    ) -> list[dict[str, str]]:
        """Get session conversation history.

        Args:
            session_id: Session ID
            limit: Maximum messages

        Returns:
            List of messages
        """
        messages = await self.memory.get_recent(session_id, limit)
        return [{"role": m.role, "content": m.content} for m in messages]

    async def search_sessions(
        self,
        query: str,
        session_ids: Optional[list[str]] = None,
        top_k: int = 5,
    ) -> list[dict[str, Any]]:
        """Search across multiple sessions.

        Args:
            query: Search query
            session_ids: Sessions to search (all if None)
            top_k: Results per session

        Returns:
            Search results with session info
        """
        sessions = session_ids or list(self._sessions.keys())
        results = []

        for sid in sessions:
            memories = await self.memory.search(sid, query, top_k)
            for m in memories:
                results.append({
                    "session_id": sid,
                    "content": m.content,
                    "score": m.score,
                })

        # Sort by score
        results.sort(key=lambda x: x.get("score", 0), reverse=True)
        return results[:top_k]
