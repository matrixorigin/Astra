"""Unified model resolution.

Priority chain (highest → lowest):
  1. Explicit request model (user chose a model for this request)
  2. Agent config model (agent-level default)
  3. SLO escalation (auto-upgrade after quality issues)
  4. Global default from LLM config
"""

import time

# Module-level cache for cheapest model resolution.
# TTL-based: re-queries DB only after expiry so admin model changes take effect.
_cheapest_cache: dict[str, tuple[str, float]] = {}  # {"cheapest": (model_name, expires_at)}
_CHEAPEST_TTL = 300.0  # 5 minutes


def resolve_model(
    request_model: str | None = None,
    agent_config_model: str | None = None,
    slo_escalation_model: str | None = None,
    default_model: str = "gpt-4o",
) -> str:
    """Return the model to use, following the priority chain.

    Special values:
      - "cheapest": resolves to the lowest-cost active model (cached 5min)
    """
    model = request_model or agent_config_model or slo_escalation_model or default_model
    if model == "cheapest":
        return _resolve_cheapest(default_model)
    return model


def _resolve_cheapest(fallback: str) -> str:
    """Find the cheapest active model by prompt+completion cost. Cached with TTL."""
    cached = _cheapest_cache.get("cheapest")
    if cached and cached[1] > time.monotonic():
        return cached[0]

    try:
        from api.database import SessionLocal
        from core.llm.router import ModelRegistry
        registry = ModelRegistry()
        with SessionLocal() as db:
            registry.load_from_db(db)
        models = registry.list_active()
        if models:
            cheapest = min(models, key=lambda m: m.pricing.prompt + m.pricing.completion)
            _cheapest_cache["cheapest"] = (cheapest.model_name, time.monotonic() + _CHEAPEST_TTL)
            return cheapest.model_name
    except Exception:
        pass
    return fallback
