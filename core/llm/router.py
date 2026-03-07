"""Model router with pluggable strategies, registry, and fallback."""

import json
import logging
from abc import ABC, abstractmethod
from typing import Any

from pydantic import BaseModel, model_validator
from sqlalchemy import text

from core.llm.models import LLMProvider

logger = logging.getLogger(__name__)


class ModelPricing(BaseModel):
    """Per-token pricing in USD per 1k tokens."""

    prompt: float = 0.0
    completion: float = 0.0
    cache_read: float | None = None   # None = auto-derive from provider defaults
    cache_write: float | None = None
    image: float | None = None        # per image (vision models)
    request: float | None = None      # flat per-request fee


class ModelQuirks(BaseModel):
    """Model-specific behavioral quirks that require special handling.

    Add a new field here when a model deviates from the OpenAI standard.
    Each field is False/None by default so existing models are unaffected.
    """
    # Temperature
    fixed_temperature: float | None = None      # Model only accepts one temperature value (e.g. kimi-k2.5 → 1.0)

    # Reasoning / thinking models
    # When True, reasoning_content from the LLM response is preserved in
    # assistant messages sent back to the model on subsequent turns.  Models
    # like kimi-k2.5 REQUIRE this — they reject tool-call continuations that
    # omit the reasoning_content they emitted.  For other models it is harmless
    # (they ignore the extra field), so the code always preserves it when
    # present rather than gating on this flag.  The flag exists to document
    # which models depend on this behavior and to enable future optimizations
    # (e.g. stripping reasoning_content to save tokens on models that ignore it).
    preserve_reasoning_content: bool = False

    # Tool calling
    no_parallel_tool_calls: bool = False        # Model doesn't support parallel tool calls
    tool_choice_required: bool = False          # Must always pass tool_choice (some models reject omitting it)
    strict_tool_call_ids: bool = False          # Model rejects non-standard tool_call_ids (e.g. "read_file:1"); rewrite to "call_xxx"

    # Context
    no_system_message: bool = False             # Model rejects system role (use first user message instead)
    system_as_user_prefix: bool = False         # Prepend system prompt to first user message


class ModelConfig(BaseModel):
    """Model configuration — one entry per deployable model.

    Schema inspired by OpenRouter's model metadata standard:
    capabilities, pricing, architecture, and operational limits.
    """

    model_name: str
    provider: LLMProvider | str
    description: str | None = None

    # ── Capabilities ──
    context_window: int = 128000
    max_completion_tokens: int | None = None
    input_modalities: list[str] = ["text"]       # text, image, file, audio
    output_modalities: list[str] = ["text"]       # text, image, audio
    supported_parameters: list[str] = []          # tools, structured_outputs, reasoning, vision, ...
    is_moderated: bool = False

    # ── Pricing ──
    pricing: ModelPricing = ModelPricing()
    enable_cache: bool = True

    # ── Architecture ──
    architecture: str | None = None               # e.g. "transformer", "moe"
    parameter_count: str | None = None            # e.g. "70B", "8x7B"

    # ── Operational ──
    rpm_limit: int = 500
    tpm_limit: int = 150000
    fallback_to: str | None = None
    is_active: bool = True
    tags: list[str] = []  # e.g. ["code", "fast", "cheap", "reasoning"]

    # ── Quirks (model-specific deviations from OpenAI standard) ──
    quirks: ModelQuirks = ModelQuirks()

    # ── Backward compat: fixed_temperature as top-level alias ──
    @property
    def fixed_temperature(self) -> float | None:
        return self.quirks.fixed_temperature

    @model_validator(mode="before")
    @classmethod
    def _migrate_quirks(cls, data: dict) -> dict:
        """Migrate flat quirk fields into nested ModelQuirks."""
        if not isinstance(data, dict):
            return data
        quirks = dict(data.get("quirks") or {})
        # Migrate fixed_temperature from top-level
        if "fixed_temperature" in data and "fixed_temperature" not in quirks:
            quirks["fixed_temperature"] = data.pop("fixed_temperature")
        if quirks:
            data["quirks"] = quirks
        return data

    # ── Backward compat properties ──
    @property
    def price_per_1k_prompt(self) -> float:
        return self.pricing.prompt

    @property
    def price_per_1k_completion(self) -> float:
        return self.pricing.completion

    @property
    def price_per_1k_cache_read(self) -> float | None:
        return self.pricing.cache_read

    @property
    def price_per_1k_cache_write(self) -> float | None:
        return self.pricing.cache_write

    @model_validator(mode="before")
    @classmethod
    def _migrate_flat_pricing(cls, data):
        """Accept old flat price_per_1k_* fields and convert to nested pricing."""
        if isinstance(data, dict) and "pricing" not in data:
            pricing = {}
            for old, new in [
                ("price_per_1k_prompt", "prompt"),
                ("price_per_1k_completion", "completion"),
                ("price_per_1k_cache_read", "cache_read"),
                ("price_per_1k_cache_write", "cache_write"),
            ]:
                if old in data:
                    pricing[new] = data.pop(old)
            if pricing:
                data["pricing"] = pricing
        return data


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

        # Score models by tag overlap; tie-break by cost (cheaper first)
        scored = []
        for m in all_models:
            score = sum(1 for t in preferred_tags if t in m.tags)
            if score > 0:
                cost = m.pricing.prompt + m.pricing.completion
                scored.append((score, cost, m))
        scored.sort(key=lambda x: (-x[0], x[1]))

        if scored:
            return [m for _, _, m in scored]

        # No tag match — fall back to default chain
        return FallbackChainStrategy().select(model, registry)


class CostOptimizedStrategy(RoutingStrategy):
    """Route to cheapest model first, with fallback to more expensive ones."""

    def select(self, model: str, registry: Any, task_hint: str | None = None) -> list[ModelConfig]:
        models: list[ModelConfig] = registry.list_active()
        models.sort(key=lambda m: m.pricing.prompt + m.pricing.completion)
        return models


# ── Model Registry ─────────────────────────────────────────────


class ModelRegistry:
    """In-memory model registry loaded from infra_llm_models table."""

    def __init__(self):
        self._models: dict[str, ModelConfig] = {}

    def load_from_db(self, db, user_id: str | None = None):
        """Load active models from infra_llm_models table."""
        try:
            rows = db.execute(
                text(
                    "SELECT model_name, provider, context_window, max_completion_tokens, "
                    "input_modalities, output_modalities, supported_parameters, "
                    "pricing, architecture, tags, is_active, base_url, description, quirks "
                    "FROM infra_llm_models WHERE is_active = 1"
                )
            ).fetchall()
            for row in rows:
                quirks_raw = row.quirks
                if isinstance(quirks_raw, str):
                    try:
                        quirks_raw = json.loads(quirks_raw)
                    except (json.JSONDecodeError, TypeError):
                        quirks_raw = {}
                mc = ModelConfig(
                    model_name=row.model_name,
                    provider=row.provider,
                    description=row.description,
                    context_window=row.context_window or 128000,
                    max_completion_tokens=row.max_completion_tokens,
                    input_modalities=json.loads(row.input_modalities) if isinstance(row.input_modalities, str) else (row.input_modalities or ["text"]),
                    output_modalities=json.loads(row.output_modalities) if isinstance(row.output_modalities, str) else (row.output_modalities or ["text"]),
                    supported_parameters=json.loads(row.supported_parameters) if isinstance(row.supported_parameters, str) else (row.supported_parameters or []),
                    pricing=json.loads(row.pricing) if isinstance(row.pricing, str) else (row.pricing or {}),
                    architecture=row.architecture,
                    tags=json.loads(row.tags) if isinstance(row.tags, str) else (row.tags or []),
                    is_active=bool(row.is_active),
                    quirks=ModelQuirks(**(quirks_raw or {})),
                )
                self._models[mc.model_name] = mc
        except Exception as e:
            logger.debug(f"Failed to load model registry: {e}")

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

    def reload(self, db, user_id: str | None = None):
        """Hot reload from DB — clears stale models first."""
        self._models.clear()
        self.load_from_db(db, user_id)


# ── ModelRouter (composes registry + strategy) ─────────────────


class ModelRouter:
    """Routes model requests using pluggable strategy."""

    def __init__(
        self,
        db=None,
        strategy: RoutingStrategy | None = None,
        user_id: str | None = None,
    ):
        """Initialize model router. Models are loaded from database only."""
        self.registry = ModelRegistry()
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

        regular_prompt = max(tokens_prompt - cache_read_tokens - cache_creation_tokens, 0)

        cache_read_price = cfg.pricing.cache_read
        cache_write_price = cfg.pricing.cache_write
        if cache_read_price is None:
            if cfg.provider == LLMProvider.ANTHROPIC:
                cache_read_price = cfg.pricing.prompt * 0.1
            else:
                cache_read_price = cfg.pricing.prompt * 0.5
        if cache_write_price is None:
            if cfg.provider == LLMProvider.ANTHROPIC:
                cache_write_price = cfg.pricing.prompt * 1.25
            else:
                cache_write_price = cfg.pricing.prompt

        cost = (
            regular_prompt * cfg.pricing.prompt / 1000
            + cache_read_tokens * cache_read_price / 1000
            + cache_creation_tokens * cache_write_price / 1000
            + tokens_completion * cfg.pricing.completion / 1000
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

    def escalate(self, model: str) -> str | None:
        """Return a higher-tier model that falls back to `model`, or None."""
        candidates = [c for c in self.registry.list_active() if c.fallback_to == model]
        if not candidates:
            return None
        return max(candidates, key=lambda c: c.pricing.prompt).model_name

    def reload(self, db):
        """Hot reload (#4)."""
        self.registry.reload(db, self.user_id)
