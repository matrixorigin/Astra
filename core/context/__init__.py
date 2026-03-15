"""Context management package."""

from core.context.embeddings import EmbeddingService, get_embedding_client
from core.context.hybrid_retrieval import HybridRetriever
from skills.knowledge.api import KnowledgeExtractor
from core.context.lifecycle import MemoryGovernanceEngine
from core.context.manager import Context, ContextFragment, ContextManager, TaskType
from core.context.prompts import PromptManager
from core.context.scorer import RelevanceScorer

__all__ = [
    "Context",
    "ContextFragment",
    "ContextManager",
    "EmbeddingService",
    "HybridRetriever",
    "KnowledgeExtractor",
    "MemoryGovernanceEngine",
    "PromptManager",
    "RelevanceScorer",
    "TaskType",
]
