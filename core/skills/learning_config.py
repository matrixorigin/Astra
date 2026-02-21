"""Runtime configuration loading and weight resolution for self-improving selector."""

import json
from datetime import datetime, timezone
from typing import Any

from sqlalchemy import func
from sqlalchemy.orm import Session

from api.models import Config
from core.logging_config import get_logger
from core.skills.learning_signals import SignalType, SignalWeights
from core.skills.learning_similarity import normalize_confidence

logger = get_logger(__name__)

CONFIG_KEY_LEARNING_WEIGHTS = "selector_learning_weights"
CONFIG_KEY_LEARNING_DECAY = "selector_learning_decay"
CONFIG_KEY_SEMANTIC_SIMILARITY = "selector_semantic_similarity_threshold"
CONFIG_KEY_SEMANTIC_MATCH_LIMIT = "selector_semantic_match_limit"
RUNTIME_CONFIG_TTL_SECONDS = 30
SEMANTIC_SIMILARITY_THRESHOLD = 0.78
SEMANTIC_MATCH_LIMIT = 50

_ALL_CONFIG_KEYS = [
    CONFIG_KEY_LEARNING_WEIGHTS,
    CONFIG_KEY_LEARNING_DECAY,
    CONFIG_KEY_SEMANTIC_SIMILARITY,
    CONFIG_KEY_SEMANTIC_MATCH_LIMIT,
]


def parse_json_config(raw: str | None) -> Any:
    if not raw:
        return None
    try:
        return json.loads(raw)
    except Exception:
        logger.warning("Failed to parse selector runtime config, using defaults")
        return None


def merge_weights(base: SignalWeights, override: Any) -> SignalWeights:
    if not isinstance(override, dict):
        return base
    merged = base.to_dict()
    for key in ["accuracy", "speed", "cost", "satisfaction"]:
        if key in override:
            merged[key] = float(override[key])
    try:
        return SignalWeights(**merged)
    except (TypeError, ValueError):
        logger.warning("Invalid selector_learning_weights, using defaults")
        return base


def sanitize_per_signal_weights(per_signal: Any) -> dict[str, dict[str, float]]:
    if not isinstance(per_signal, dict):
        logger.warning("Invalid selector_learning_weights per_signal, using defaults")
        return {}
    valid_signals = {st.value for st in SignalType}
    allowed_keys = {"accuracy", "speed", "cost", "satisfaction"}
    sanitized: dict[str, dict[str, float]] = {}
    for signal_type, override in per_signal.items():
        if signal_type not in valid_signals:
            continue
        if not isinstance(override, dict):
            continue
        cleaned: dict[str, float] = {}
        for key, value in override.items():
            if key not in allowed_keys:
                continue
            try:
                cleaned[key] = float(value)
            except (TypeError, ValueError):
                pass
        if cleaned:
            sanitized[signal_type] = cleaned
    return sanitized


def resolve_weights_for_signal(
    signal_type: str | None, base: SignalWeights, per_signal: dict[str, Any],
) -> SignalWeights:
    if not signal_type:
        return base
    override = per_signal.get(signal_type)
    if not isinstance(override, dict):
        return base
    merged = base.to_dict()
    for key in ["accuracy", "speed", "cost", "satisfaction"]:
        if key in override:
            merged[key] = float(override[key])
    total = sum(merged.values())
    if total <= 0:
        return base
    if abs(total - 1.0) > 0.01:
        merged = {key: value / total for key, value in merged.items()}
    try:
        return SignalWeights(**merged)
    except (TypeError, ValueError):
        return base


def resolve_decay_config(decay: dict[str, Any], signal_type: str | None) -> dict[str, Any]:
    base = {
        "enabled": bool(decay.get("enabled", False)),
        "half_life_days": float(decay.get("half_life_days", 0.0) or 0.0),
        "min_confidence": float(decay.get("min_confidence", 0.0) or 0.0),
    }
    if not signal_type:
        return base
    per_signal = decay.get("per_signal") or {}
    override = per_signal.get(signal_type)
    if not isinstance(override, dict):
        return base
    merged = base.copy()
    if "enabled" in override:
        merged["enabled"] = bool(override["enabled"])
    if "half_life_days" in override:
        merged["half_life_days"] = float(override["half_life_days"] or 0.0)
    if "min_confidence" in override:
        merged["min_confidence"] = float(override["min_confidence"] or 0.0)
    return merged


def effective_confidence(
    learning, decay: dict[str, Any], signal_type: str | None,
) -> float:
    """Compute confidence with time-based decay."""
    normalized = normalize_confidence(learning.confidence)
    decay_config = resolve_decay_config(decay, signal_type)
    if not decay_config.get("enabled") or not decay_config.get("half_life_days"):
        return normalized
    reference = learning.updated_at or learning.created_at
    if not reference:
        return normalized
    if reference.tzinfo is None:
        reference = reference.replace(tzinfo=timezone.utc)
    age_days = (datetime.now(timezone.utc) - reference).total_seconds() / 86400.0
    factor = 0.5 ** (age_days / float(decay_config["half_life_days"]))
    decayed = normalized * factor
    min_conf = float(decay_config.get("min_confidence", 0.0) or 0.0)
    return min(normalized, max(min_conf, decayed))


def load_runtime_config(
    session: Session,
    base_weights: SignalWeights,
    *,
    cache: dict[str, Any] | None = None,
    cache_loaded_at: datetime | None = None,
    cache_last_updated_at: datetime | None = None,
    ttl_seconds: int = RUNTIME_CONFIG_TTL_SECONDS,
) -> tuple[dict[str, Any], datetime, datetime | None]:
    """Load runtime config from DB with TTL cache.

    Returns (config_dict, loaded_at, last_updated_at).
    """
    now = datetime.now(timezone.utc)

    # Check cache validity
    if cache and cache_loaded_at:
        cache_age = (now - cache_loaded_at).total_seconds()
        if cache_age < ttl_seconds:
            latest = session.query(func.max(Config.updated_at)).filter(
                Config.key_name.in_(_ALL_CONFIG_KEYS)
            ).scalar()
            if (
                (latest is None and cache_last_updated_at is None)
                or (latest and cache_last_updated_at and latest <= cache_last_updated_at)
            ):
                return cache, cache_loaded_at, cache_last_updated_at

    # Full reload
    configs = session.query(Config).filter(Config.key_name.in_(_ALL_CONFIG_KEYS)).all()

    weights = base_weights
    per_signal_weights: dict[str, Any] = {}
    decay: dict[str, Any] = {
        "enabled": False, "half_life_days": 0.0, "min_confidence": 0.0, "per_signal": {},
    }
    sem_threshold = SEMANTIC_SIMILARITY_THRESHOLD
    sem_limit = SEMANTIC_MATCH_LIMIT
    latest_updated_at = None

    for cfg in configs:
        if cfg.updated_at and (latest_updated_at is None or cfg.updated_at > latest_updated_at):
            latest_updated_at = cfg.updated_at
        parsed = parse_json_config(cfg.value)
        if cfg.key_name == CONFIG_KEY_LEARNING_WEIGHTS:
            if isinstance(parsed, dict):
                per_signal_weights = sanitize_per_signal_weights(parsed.get("per_signal", {}) or {})
            weights = merge_weights(weights, parsed)
        elif cfg.key_name == CONFIG_KEY_LEARNING_DECAY:
            if isinstance(parsed, dict):
                decay = {
                    "enabled": bool(parsed.get("enabled", False)),
                    "half_life_days": float(parsed.get("half_life_days", 0.0) or 0.0),
                    "min_confidence": float(parsed.get("min_confidence", 0.0) or 0.0),
                    "per_signal": parsed.get("per_signal", {}) or {},
                }
        elif cfg.key_name == CONFIG_KEY_SEMANTIC_SIMILARITY:
            if isinstance(parsed, dict) and "threshold" in parsed:
                sem_threshold = float(parsed["threshold"])
            elif isinstance(parsed, (int, float, str)):
                sem_threshold = float(parsed)
        elif cfg.key_name == CONFIG_KEY_SEMANTIC_MATCH_LIMIT:
            if isinstance(parsed, dict) and "limit" in parsed:
                sem_limit = int(parsed["limit"])
            elif isinstance(parsed, (int, float, str)):
                sem_limit = int(float(parsed))

    result = {
        "weights": weights,
        "weights_per_signal": per_signal_weights,
        "decay": decay,
        "semantic_similarity_threshold": sem_threshold,
        "semantic_match_limit": sem_limit,
    }
    return result, now, latest_updated_at
