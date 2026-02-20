"""Model router with pluggable strategies, registry, and fallback."""

import json
import logging
from abc import ABC, abstractmethod
from typing import Any

from pydantic import BaseModel
from sqlalchemy import text

from core.llm.models import LLMProvider

logger = logging.getLogger(__name__)


class ModelConfig(BaseModel):
    """Model configuration — one entry per deployable model."""

    model_name: str
    provider: LLMProvider
    context_window: int = 128000
    price_per_1k_prompt: float = 0.0
    price_per_1k_completion: float = 0.0
    # Prompt caching pricing (per 1k tokens)
    # Anthropic: cache write = 1.25x base, cache read = 0.1x base
    # OpenAI: cached = 0.5x base (automatic prefix caching)
    price_per_1k_cache_read: float | None = None  # None = auto-derive from provider defaults
    price_per_1k_cache_write: float | None = None
    enable_cache: bool = True  # Set False to disable prompt caching for this model
    rpm_limit: int = 500
    tpm_limit: int = 150000
    fallback_to: str | None = None
    is_active: bool = True
    tags: list[str] = []  # e.g. ["code", "fast", "cheap", "reasoning"]


# ── Routing Strategies (#1 MoE, #2 Pluggable) ─────────────────


class RoutingStrategy(ABC):
    """Pluggable routing strategy interface."""

    @abstractmethod
    def select(
        self, model: str, registry: "ModelRegistry", task_hint: str | None = None
    ) -> list[ModelConfig]:
        """Return ordered list of models to try (primary + fallbacks)."""
        ...


class FallbackChainStrategy(RoutingStrategy):
    """Default: follow the static fallback_to chain."""

    def select(self, model, registry, task_hint=None) -> list[ModelConfig]:
        chain = []
        seen = set()
        current = model
        while current and current not in seen:
            seen.add(current)
            cfg = registry.get(current)
            if cfg and cfg.is_active:
                chain.append(cfg)
            current = cfg.fallback_to if cfg else None
        return chain


class TaskBasedStrategy(RoutingStrategy):
    """Model-level MoE: route by task_hint tag matching."""

    # task_hint → preferred tags, in priority order
    TASK_TAG_MAP: dict[str, list[str]] = {
        "code": ["code", "reasoning"],
        "chat": ["fast", "cheap"],
        "analysis": ["reasoning"],
        "simple": ["cheap", "fast"],
    }

    def select(self, model, registry, task_hint=None) -> list[ModelConfig]:
        if not task_hint or task_hint not in self.TASK_TAG_MAP:
            return FallbackChainStrategy().select(model, registry)

        preferred_tags = self.TASK_TAG_MAP[task_hint]
        all_models = registry.list_active()

        # Score models by tag overlap
        scored = []
        for m in all_models:
            score = sum(1 for t in preferred_tags if t in m.tags)
            if score > 0:
                scored.append((score, m))
        scored.sort(key=lambda x: -x[0])

        if scored:
            return [m for _, m in scored]

        # No tag match — fall back to default chain
        return FallbackChainStrategy().select(model, registry)


class CostOptimizedStrategy(RoutingStrategy):
    """Route to cheapest model first, with fallback to more expensive ones."""

    def select(self, model: str, registry: Any, task_hint: str | None = None) -> list[ModelConfig]:
        models: list[ModelConfig] = registry.list_active()
        models.sort(key=lambda m: m.price_per_1k_prompt + m.price_per_1k_completion)
        return models


# ── Model Registry ─────────────────────────────────────────────

DEFAULT_MODELS: list[ModelConfig] = [
    ModelConfig(
        model_name="gpt-4o",
        provider=LLMProvider.OPENAI,
        context_window=128000,
        price_per_1k_prompt=0.0025,
        price_per_1k_completion=0.01,
        rpm_limit=500,
        tpm_limit=150000,
        fallback_to="gpt-4o-mini",
        tags=["code", "reasoning"],
    ),
    ModelConfig(
        model_name="gpt-4o-mini",
        provider=LLMProvider.OPENAI,
        context_window=128000,
        price_per_1k_prompt=0.00015,
        price_per_1k_completion=0.0006,
        rpm_limit=1000,
        tpm_limit=200000,
        tags=["fast", "cheap"],
    ),
    ModelConfig(
        model_name="gpt-4",
        provider=LLMProvider.OPENAI,
        context_window=8192,
        price_per_1k_prompt=0.03,
        price_per_1k_completion=0.06,
        rpm_limit=200,
        tpm_limit=40000,
        fallback_to="gpt-4o",
        tags=["reasoning"],
    ),
    ModelConfig(
        model_name="gpt-4-turbo",
        provider=LLMProvider.OPENAI,
        context_window=128000,
        price_per_1k_prompt=0.01,
        price_per_1k_completion=0.03,
        rpm_limit=500,
        tpm_limit=150000,
        fallback_to="gpt-4o",
        tags=["reasoning"],
    ),
    ModelConfig(
        model_name="gpt-3.5-turbo",
        provider=LLMProvider.OPENAI,
        context_window=16385,
        price_per_1k_prompt=0.0005,
        price_per_1k_completion=0.0015,
        rpm_limit=1000,
        tpm_limit=200000,
        tags=["fast", "cheap"],
    ),
    ModelConfig(
        model_name="llama3-70b",
        provider=LLMProvider.GROQ,
        context_window=8192,
        price_per_1k_prompt=0.0007,
        price_per_1k_completion=0.0008,
        rpm_limit=30,
        tpm_limit=6000,
        fallback_to="gpt-4o-mini",
        tags=["fast"],
    ),
    ModelConfig(
        model_name="mixtral-8x7b",
        provider=LLMProvider.GROQ,
        context_window=32768,
        price_per_1k_prompt=0.0003,
        price_per_1k_completion=0.0003,
        rpm_limit=30,
        tpm_limit=5000,
        tags=["fast", "cheap"],
    ),
    ModelConfig(
        model_name="claude-3-5-sonnet-20241022",
        provider=LLMProvider.ANTHROPIC,
        context_window=200000,
        price_per_1k_prompt=0.003,
        price_per_1k_completion=0.015,
        rpm_limit=50,
        tpm_limit=40000,
        tags=["code", "reasoning"],
    ),
    ModelConfig(
        model_name="claude-3-haiku-20240307",
        provider=LLMProvider.ANTHROPIC,
        context_window=200000,
        price_per_1k_prompt=0.00025,
        price_per_1k_completion=0.00125,
        rpm_limit=100,
        tpm_limit=50000,
        tags=["fast", "cheap"],
    ),
]


class ModelRegistry:
    """In-memory model registry with DB/env override."""

    def __init__(self, use_defaults: bool = True):
        """Initialize model registry.

        Args:
            use_defaults: If True, load DEFAULT_MODELS. Set to False in production
                         to enforce strict database-based access control.
        """
        self._models: dict[str, ModelConfig] = {}
        if use_defaults:
            for m in DEFAULT_MODELS:
                self._models[m.model_name] = m

    def load_from_db(self, db, user_id: str | None = None):
        """Load model registry with scope-based access control.

        Priority: user > global
        """
        try:
            # Try user scope first
            if user_id:
                result = db.execute(
                    text("SELECT value FROM configs WHERE key_name = 'model_registry' "
                         "AND scope_type = 'user' AND scope_id = :user_id LIMIT 1"),
                    {"user_id": user_id}
                )
                row = result.first()
                if row:
                    for c in json.loads(row.value):
                        mc = ModelConfig(**c)
                        self._models[mc.model_name] = mc
                    return

            # Fallback to global scope
            result = db.execute(
                text("SELECT value FROM configs WHERE key_name = 'model_registry' "
                     "AND scope_type = 'global' LIMIT 1")
            )
            row = result.first()
            if row:
                for c in json.loads(row.value):
                    mc = ModelConfig(**c)
                    self._models[mc.model_name] = mc
        except Exception as e:
            logger.debug(f"No model registry in DB: {e}")

    def get(self, model_name: str) -> ModelConfig | None:
        return self._models.get(model_name)

    def list_models(self) -> list[ModelConfig]:
        """List all models (active and inactive)."""
        return list(self._models.values())

    def list_active(self) -> list[ModelConfig]:
        return [m for m in self._models.values() if m.is_active]

    def register(self, config: ModelConfig):
        self._models[config.model_name] = config

    def unregister(self, model_name: str):
        self._models.pop(model_name, None)

    def reload(self, db):
        """Hot reload from DB (#4 动态配置)."""
        self.load_from_db(db, self.user_id)


# ── ModelRouter (composes registry + strategy) ─────────────────


class ModelRouter:
    """Routes model requests using pluggable strategy."""

    def __init__(
        self,
        db=None,
        strategy: RoutingStrategy | None = None,
        user_id: str | None = None,
        use_defaults: bool = True,
    ):
        """Initialize model router.

        Args:
            use_defaults: If True, load DEFAULT_MODELS as fallback. Set to False
                         in production to enforce strict scope-based access control.
        """
        self.registry = ModelRegistry(use_defaults=use_defaults)
        self.strategy = strategy or FallbackChainStrategy()
        self.user_id = user_id
        if db:
            self.registry.load_from_db(db, user_id)

    def route(self, model: str, task_hint: str | None = None) -> list[ModelConfig]:
        """Select models using current strategy."""
        return self.strategy.select(model, self.registry, task_hint)

    def set_strategy(self, strategy: RoutingStrategy):
        self.strategy = strategy

    def calculate_cost(
        self,
        model_name: str,
        tokens_prompt: int,
        tokens_completion: int,
        cache_read_tokens: int = 0,
        cache_creation_tokens: int = 0,
    ) -> float:
        cfg = self.registry.get(model_name)
        if not cfg:
            return 0.0

        # Non-cached prompt tokens = total prompt - cache_read - cache_creation
        regular_prompt = max(tokens_prompt - cache_read_tokens - cache_creation_tokens, 0)

        # Cache pricing defaults by provider
        cache_read_price = cfg.price_per_1k_cache_read
        cache_write_price = cfg.price_per_1k_cache_write
        if cache_read_price is None:
            if cfg.provider == LLMProvider.ANTHROPIC:
                cache_read_price = cfg.price_per_1k_prompt * 0.1  # 90% discount
            else:
                cache_read_price = cfg.price_per_1k_prompt * 0.5  # OpenAI 50% discount
        if cache_write_price is None:
            if cfg.provider == LLMProvider.ANTHROPIC:
                cache_write_price = cfg.price_per_1k_prompt * 1.25  # 25% surcharge
            else:
                cache_write_price = cfg.price_per_1k_prompt  # OpenAI: no write surcharge

        cost = (
            regular_prompt * cfg.price_per_1k_prompt / 1000
            + cache_read_tokens * cache_read_price / 1000
            + cache_creation_tokens * cache_write_price / 1000
            + tokens_completion * cfg.price_per_1k_completion / 1000
        )
        return round(cost, 6)

    def estimate_cost(self, model_name: str, estimated_tokens: int) -> float:
        """Pre-call cost estimate for budget checking (#7)."""
        cfg = self.registry.get(model_name)
        if not cfg:
            return 0.0
        # Assume 30% prompt, 70% completion as rough estimate
        prompt_tokens = int(estimated_tokens * 0.3)
        completion_tokens = int(estimated_tokens * 0.7)
        return self.calculate_cost(model_name, prompt_tokens, completion_tokens)

    # Backward compat
    def get(self, model_name: str) -> ModelConfig | None:
        return self.registry.get(model_name)

    def get_with_fallback(self, model_name: str) -> list[ModelConfig]:
        return self.route(model_name)

    def list_models(self) -> list[ModelConfig]:
        return self.registry.list_active()

    def register(self, config: ModelConfig):
        self.registry.register(config)

    def reload(self, db):
        """Hot reload (#4)."""
        self.registry.reload(db)
