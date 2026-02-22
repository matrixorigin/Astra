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
    config_data = json.dumps({
        "provider": "openai",
        "model": "gpt-4",
        "temperature": 0.7,
        "max_tokens": 2000,
    })
    db_session.execute(
        text("""
        INSERT INTO configs (config_id, key_name, scope_type, scope_user_id, value) 
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
    db_session.commit()
    yield db_session
    # Cleanup
    db_session.execute(text("DELETE FROM llm_call_logs WHERE event_id LIKE 'test_%'"))
    db_session.execute(text("DELETE FROM configs WHERE config_id = 'llm_config'"))
    db_session.commit()


@pytest.fixture
def client(db):
    """LLM client fixture."""
    return LLMClient(db=db)


def test_load_config(client):
    """Test loading config from MatrixOne."""
    assert client.config["provider"] == "openai"
    assert client.config["model"] == "gpt-4"  # From database config
    assert client.config["temperature"] == 0.7



def test_chat_error_logging(client, db):
    """Test error logging when LLM call fails."""
    # Test that invalid provider raises error
    client.config["provider"] = "unsupported"

    messages = [LLMMessage(role="user", content="Hello")]

    # Should raise ValueError for invalid provider
    with pytest.raises(ValueError):
        client.chat(
            messages=messages,
            event_id="test_event_error",
            user_id="test_user",
        )


def test_calculate_cost(client):
    """Test cost calculation."""
    cost = client.router.calculate_cost(
        model_name="gpt-4",
        tokens_prompt=1000,
        tokens_completion=500,
    )
    # gpt-4: $0.03/1K prompt + $0.06/1K completion
    expected = (1000 * 0.03 / 1000) + (500 * 0.06 / 1000)
    assert abs(cost - expected) < 0.0001


def test_get_call_logs_by_user(client, db):
    """Test getting call logs by user."""
    # Insert test logs
    db.execute(
        text("""
        INSERT INTO llm_call_logs (
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
        }
    )
    db.commit()

    logs = client.get_call_logs(user_id="user_alice")
    assert len(logs) >= 1
    assert logs[0].user_id == "user_alice"
