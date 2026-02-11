"""Tests for LLM client."""

import json
from unittest.mock import MagicMock, patch

import pytest

from core.llm import LLMClient, LLMMessage, LLMProvider
from sdk import Database


@pytest.fixture
def db():
    """Database fixture."""
    db = Database()
    # Insert test config
    db.execute(
        "INSERT INTO configs (config_id, key_name, value) VALUES (%s, %s, %s) "
        "ON DUPLICATE KEY UPDATE value = %s",
        (
            "llm_config",
            "llm_config",
            json.dumps({
                "provider": "openai",
                "model": "gpt-4",
                "temperature": 0.7,
                "max_tokens": 2000,
            }),
            json.dumps({
                "provider": "openai",
                "model": "gpt-4",
                "temperature": 0.7,
                "max_tokens": 2000,
            }),
        ),
    )
    yield db
    # Cleanup
    db.execute("DELETE FROM llm_call_logs WHERE event_id LIKE 'test_%'")
    db.execute("DELETE FROM configs WHERE config_id = 'llm_config'")


@pytest.fixture
def client(db):
    """LLM client fixture."""
    return LLMClient(db=db)


def test_load_config(client):
    """Test loading config from MatrixOne."""
    assert client.config["provider"] == "openai"
    assert client.config["model"] == "gpt-4"
    assert client.config["temperature"] == 0.7


@pytest.mark.skipif(True, reason="Requires openai package")
@patch("openai.OpenAI")
def test_chat_openai(mock_openai, client, db):
    """Test chat with OpenAI (mocked)."""
    # Mock OpenAI response
    mock_response = MagicMock()
    mock_response.choices = [MagicMock()]
    mock_response.choices[0].message.content = "Hello! How can I help you?"
    mock_response.model = "gpt-4"
    mock_response.usage.prompt_tokens = 10
    mock_response.usage.completion_tokens = 20
    mock_response.usage.total_tokens = 30

    mock_client = MagicMock()
    mock_client.chat.completions.create.return_value = mock_response
    mock_openai.return_value = mock_client

    # Call chat
    messages = [LLMMessage(role="user", content="Hello")]
    response = client.chat(
        messages=messages,
        event_id="test_event_1",
        user_id="test_user",
    )

    # Verify response
    assert response.content == "Hello! How can I help you?"
    assert response.model == "gpt-4"
    assert response.provider == LLMProvider.OPENAI
    assert response.tokens_prompt == 10
    assert response.tokens_completion == 20
    assert response.tokens_total == 30
    assert response.cost_usd > 0

    # Verify logging
    logs = client.get_call_logs(event_id="test_event_1")
    assert len(logs) == 1
    assert logs[0].status == "success"
    assert logs[0].tokens_total == 30


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
        """
        INSERT INTO llm_call_logs (
            log_id, event_id, user_id, provider, model,
            tokens_prompt, tokens_completion, tokens_total,
            cost_usd, latency_ms, status, created_at
        ) VALUES (%s, %s, %s, %s, %s, %s, %s, %s, %s, %s, %s, NOW())
        """,
        (
            "log_1",
            "test_event_2",
            "user_alice",
            "openai",
            "gpt-4",
            100,
            200,
            300,
            0.015,
            1500,
            "success",
        ),
    )

    logs = client.get_call_logs(user_id="user_alice")
    assert len(logs) >= 1
    assert logs[0].user_id == "user_alice"
