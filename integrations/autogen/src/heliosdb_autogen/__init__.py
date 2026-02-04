"""HeliosDB integration for Microsoft AutoGen multi-agent framework.

This package provides tools, agents, and utilities for using HeliosDB
as a persistent memory and knowledge store in AutoGen multi-agent workflows.

Example:
    ```python
    from autogen import AssistantAgent, UserProxyAgent
    from heliosdb_autogen import HeliosDBAgent, HeliosDBTools, AgentMemoryManager

    # Create HeliosDB-powered agent
    db_agent = HeliosDBAgent(
        name="db_assistant",
        heliosdb_url="http://localhost:8080",
        api_key="your-api-key"
    )

    # Or add HeliosDB tools to existing agents
    tools = HeliosDBTools(heliosdb_url="http://localhost:8080")
    assistant = AssistantAgent(
        name="assistant",
        llm_config={"tools": tools.get_tool_definitions()}
    )
    ```
"""

from .client import HeliosDBClient
from .agents import HeliosDBAgent, DataAnalystAgent, RAGAgent
from .tools import HeliosDBTools, HeliosDBToolkit
from .memory import AgentMemoryManager, ConversationMemory, SemanticMemory
from .retrievers import HeliosDBRetriever, HybridRetriever
from .group_chat import HeliosDBGroupChatManager

__version__ = "3.0.0"
__all__ = [
    # Client
    "HeliosDBClient",
    # Agents
    "HeliosDBAgent",
    "DataAnalystAgent",
    "RAGAgent",
    # Tools
    "HeliosDBTools",
    "HeliosDBToolkit",
    # Memory
    "AgentMemoryManager",
    "ConversationMemory",
    "SemanticMemory",
    # Retrievers
    "HeliosDBRetriever",
    "HybridRetriever",
    # Group Chat
    "HeliosDBGroupChatManager",
]
