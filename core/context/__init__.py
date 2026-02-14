"""Context management package."""

from core.context.embeddings import EmbeddingService
from core.context.knowledge import KnowledgeExtractor
from core.context.manager import Context, ContextFragment, ContextManager, TaskType
from core.context.prompts import PromptManager
from core.context.scorer import RelevanceScorer

__all__ = [
    "Context",
    "ContextFragment",
    "ContextManager",
    "EmbeddingService",
    "KnowledgeExtractor",
    "PromptManager",
    "RelevanceScorer",
    "TaskType",
]
