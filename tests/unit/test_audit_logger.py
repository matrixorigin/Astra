"""Tests for audit logger."""

import pytest
from core.auth.audit_logger import AuditLogger
from api.database import get_db_session


@pytest.fixture
def db():
    """Get test database session."""
    session = next(get_db_session())
    yield session
    session.close()


def test_log(db):
    """Test basic logging."""
    logger = AuditLogger(db)

    logger.log(
        user_id="admin",
        action="test_action",
        resource_type="test_resource",
        resource_id="test_id",
        details={"key": "value"},
        status="success",
    )

    logs = logger.get_logs(user_id="admin")
    assert len(logs) >= 1
    assert logs[0]["user_id"] == "admin"
    assert logs[0]["action"] == "test_action"


def test_log_model_add(db):
    """Test model addition logging."""
    logger = AuditLogger(db)

    logger.log_model_add("admin", "gpt-4", "global")

    logs = logger.get_logs(user_id="admin")
    assert any(log["action"] == "add_model" for log in logs)


def test_log_model_remove(db):
    """Test model removal logging."""
    logger = AuditLogger(db)

    logger.log_model_remove("admin", "gpt-4", "global")

    logs = logger.get_logs(user_id="admin")
    assert any(log["action"] == "remove_model" for log in logs)


def test_log_model_update(db):
    """Test model update logging."""
    logger = AuditLogger(db)

    logger.log_model_update("admin", "gpt-4", {"price": 0.01})

    logs = logger.get_logs(user_id="admin")
    assert any(log["action"] == "update_model" for log in logs)


def test_log_skill_register(db):
    """Test skill registration logging."""
    logger = AuditLogger(db)

    logger.log_skill_register("alice", "my_skill", "user")

    logs = logger.get_logs(user_id="alice")
    assert any(log["action"] == "register_skill" for log in logs)


def test_log_token_create(db):
    """Test token creation logging."""
    logger = AuditLogger(db)

    logger.log_token_create("admin", "llm", "openai", "global")

    logs = logger.get_logs(user_id="admin")
    assert any(log["action"] == "create_token" for log in logs)


def test_get_logs(db):
    """Test log retrieval."""
    logger = AuditLogger(db)

    # Add some logs
    logger.log_model_add("admin", "gpt-4", "global")
    logger.log_model_add("alice", "gpt-3.5", "user", "alice")

    # Get all logs
    logs = logger.get_logs()
    assert len(logs) >= 2

    # Filter by user
    logs = logger.get_logs(user_id="admin")
    assert any(log["user_id"] == "admin" for log in logs)
