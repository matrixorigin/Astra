"""Context management package."""

from core.context.embeddings import EmbeddingService, get_embedding_client
from core.context.hybrid_retrieval import HybridRetriever
from skills.knowledge.api import KnowledgeExtractor
from core.context.lifecycle import MemoryGovernanceEngine
from core.context.manager import Context, ContextFragment, ContextManager, TaskType
from core.context.prompts import PromptManager
from core.context.scheduler import (
    AsyncIOBackend,
    GovernanceTaskRunner,
    MemoryGovernanceScheduler,
    SchedulerBackend,
)
from core.context.scorer import RelevanceScorer

__all__ = [
    "AsyncIOBackend",
    "Context",
    "ContextFragment",
    "ContextManager",
    "EmbeddingService",
    "GovernanceTaskRunner",
    "HybridRetriever",
    "KnowledgeExtractor",
    "MemoryGovernanceEngine",
    "MemoryGovernanceScheduler",
    "PromptManager",
    "RelevanceScorer",
    "SchedulerBackend",
    "TaskType",
]
