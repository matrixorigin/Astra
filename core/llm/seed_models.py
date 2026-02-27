"""Seed default models into the model registry."""

import json

from sqlalchemy import text
from sqlalchemy.orm import Session
from uuid_utils import uuid7

SEED_MODELS = [
    {
        "model_name": "gpt-4o",
        "provider": "openai",
        "context_window": 128000,
        "max_completion_tokens": 16384,
        "input_modalities": ["text", "image"],
        "output_modalities": ["text"],
        "supported_parameters": ["tools", "structured_outputs", "vision", "temperature", "top_p"],
        "pricing": {"prompt": 0.0025, "completion": 0.01},
        "architecture": "transformer",
        "tags": ["code", "reasoning"],
        "fallback_to": "gpt-4o-mini",
        "is_active": True,
    },
    {
        "model_name": "gpt-4o-mini",
        "provider": "openai",
        "context_window": 128000,
        "max_completion_tokens": 16384,
        "input_modalities": ["text", "image"],
        "output_modalities": ["text"],
        "supported_parameters": ["tools", "structured_outputs", "vision", "temperature", "top_p"],
        "pricing": {"prompt": 0.00015, "completion": 0.0006},
        "architecture": "transformer",
        "tags": ["fast", "cheap"],
        "is_active": True,
    },
    {
        "model_name": "claude-3-5-sonnet-20241022",
        "provider": "anthropic",
        "context_window": 200000,
        "max_completion_tokens": 8192,
        "input_modalities": ["text", "image"],
        "output_modalities": ["text"],
        "supported_parameters": ["tools", "vision", "temperature", "top_p"],
        "pricing": {"prompt": 0.003, "completion": 0.015, "cache_read": 0.0003, "cache_write": 0.00375},
        "architecture": "transformer",
        "tags": ["code", "reasoning"],
        "is_active": True,
    },
    {
        "model_name": "deepseek-chat",
        "provider": "deepseek",
        "context_window": 64000,
        "max_completion_tokens": 8192,
        "input_modalities": ["text"],
        "output_modalities": ["text"],
        "supported_parameters": ["tools", "temperature", "top_p"],
        "pricing": {"prompt": 0.00014, "completion": 0.00028},
        "architecture": "moe",
        "tags": ["code", "reasoning", "cheap"],
        "is_active": True,
    },
]


def seed_models(db: Session) -> int:
    """Insert default models if registry is empty. Returns count of seeded models."""
    existing = db.execute(
        text("SELECT 1 FROM infra_configs WHERE key_name = 'model_registry' AND scope_type = 'global' LIMIT 1")
    ).fetchone()
    if existing:
        return 0

    db.execute(
        text(
            "INSERT INTO infra_configs (config_id, key_name, value, scope_type, scope_user_id) "
            "VALUES (:id, 'model_registry', :value, 'global', NULL)"
        ),
        {"id": str(uuid7()), "value": json.dumps(SEED_MODELS)},
    )
    db.commit()
    return len(SEED_MODELS)
