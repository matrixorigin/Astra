"""Tests for LLM client."""

import json
from unittest.mock import MagicMock, patch

import pytest

from core.llm import LLMClient, LLMMessage, LLMProvider
from sqlalchemy import text
from sqlalchemy.orm import Session
from api.database import get_db_session


@pytest.fixture
def db(db_session):
    """Database fixture."""
    # Insert test config
    config_data = json.dumps(
        {
            "provider": "openai",
            "model": "gpt-4",
            "temperature": 0.7,
            "max_tokens": 2000,
        }
    )
    db_session.execute(
        text("""
        INSERT INTO infra_configs (config_id, key_name, scope_type, scope_user_id, value) 
        VALUES (:config_id, :key_name, :scope_type, :scope_user_id, :value) 
        ON DUPLICATE KEY UPDATE value = :value2
        """),
        {
            "config_id": "test_llm_config_001",
            "key_name": "llm_config",
            "scope_type": "global",
            "scope_user_id": None,
            "value": config_data,
            "value2": config_data,
        },
    )
    # Register gpt-4 model in registry (no longer hardcoded)
    models_json = json.dumps(
        [
            {
                "model_name": "gpt-4",
                "provider": "openai",
                "context_window": 8192,
                "pricing": {"prompt": 0.03, "completion": 0.06},
                "is_active": True,
            }
        ]
    )
    db_session.execute(
        text(
            "DELETE FROM infra_configs WHERE key_name = 'model_registry' AND scope_type = 'global'"
        )
    )
    db_session.execute(
        text("""
        INSERT INTO infra_configs (config_id, key_name, scope_type, scope_user_id, value)
        VALUES ('test_model_reg', 'model_registry', 'global', NULL, :value)
        """),
        {"value": models_json},
    )
    db_session.commit()
    yield db_session
    # Cleanup
    db_session.execute(text("DELETE FROM eval_llm_call_logs WHERE event_id LIKE 'test_%'"))
    db_session.execute(
        text(
            "DELETE FROM infra_configs WHERE config_id IN ('test_llm_config_001', 'test_model_reg')"
        )
    )
    db_session.commit()


@pytest.fixture
def client(db):
    """LLM client fixture."""
    return LLMClient(lambda: db)


def test_load_config(client):
    """Test loading config from MatrixOne."""
    assert client.config["provider"] == "openai"
    assert client.config["model"] == "gpt-4"  # From database config
    assert client.config["temperature"] == 0.7


def test_chat_error_logging(client, db):
    """Test error logging when LLM call fails."""
    messages = [LLMMessage(role="user", content="Hello")]

    # Model 'nonexistent' is not registered — should raise PermissionError
    with pytest.raises(PermissionError):
        client.chat(
            messages=messages,
            event_id="test_event_error",
            user_id="test_user",
            model="nonexistent",
        )


def test_calculate_cost(client, db):
    """Test cost calculation."""
    # Register model with known pricing directly in registry
    from core.llm.router import ModelConfig, ModelPricing

    client.router.registry._models["gpt-4"] = ModelConfig(
        model_name="gpt-4",
        provider="openai",
        pricing=ModelPricing(prompt=0.03, completion=0.06),
        is_active=True,
    )

    cost = client.router.calculate_cost(
        model_name="gpt-4",
        tokens_prompt=1000,
        tokens_completion=500,
    )
    expected = (1000 * 0.03 / 1000) + (500 * 0.06 / 1000)
    assert abs(cost - expected) < 0.0001


def test_get_call_logs_by_user(client, db):
    """Test getting call logs by user."""
    # Insert test logs
    db.execute(
        text("""
        INSERT INTO eval_llm_call_logs (
            log_id, event_id, user_id, provider, model,
            tokens_prompt, tokens_completion, tokens_total,
            cost_usd, latency_ms, status, created_at
        ) VALUES (:log_id, :event_id, :user_id, :provider, :model, 
                  :tokens_prompt, :tokens_completion, :tokens_total,
                  :cost_usd, :latency_ms, :status, NOW())
        """),
        {
            "log_id": "log_1",
            "event_id": "test_event_2",
            "user_id": "user_alice",
            "provider": "openai",
            "model": "gpt-4",
            "tokens_prompt": 100,
            "tokens_completion": 200,
            "tokens_total": 300,
            "cost_usd": 0.015,
            "latency_ms": 1500,
            "status": "success",
        },
    )
    db.commit()

    logs = client.get_call_logs(user_id="user_alice")
    assert len(logs) >= 1
    assert logs[0].user_id == "user_alice"
