"""Tests for audit logger."""

from core.auth.audit_logger import AuditLogger


class MockDB:
    """Mock database for testing."""

    def __init__(self):
        self.logs = []

    def execute(self, query, params=None):
        if "INSERT INTO agent_config.audit_logs" in query:
            # New schema: log_id, user_id, action, resource_type, resource_id, new_value, status, created_at
            self.logs.append(
                {
                    "log_id": params[0],
                    "user_id": params[1],
                    "action": params[2],
                    "resource_type": params[3],
                    "resource_id": params[4],
                    "details": params[5],  # stored in new_value column
                    "status": params[6],
                    "created_at": params[7],
                }
            )

    def fetchall(self, query, params=None):
        return self.logs


def test_log():
    """Test basic logging."""
    db = MockDB()
    logger = AuditLogger(db)

    logger.log(
        user_id="admin",
        action="test_action",
        resource_type="test_resource",
        resource_id="test_id",
        details={"key": "value"},
        status="success",
    )

    assert len(db.logs) == 1
    assert db.logs[0]["user_id"] == "admin"
    assert db.logs[0]["action"] == "test_action"
    assert db.logs[0]["resource_type"] == "test_resource"


def test_log_model_add():
    """Test model addition logging."""
    db = MockDB()
    logger = AuditLogger(db)

    logger.log_model_add("admin", "gpt-4", "global")

    assert len(db.logs) == 1
    assert db.logs[0]["action"] == "add_model"
    assert db.logs[0]["resource_id"] == "gpt-4"


def test_log_model_remove():
    """Test model removal logging."""
    db = MockDB()
    logger = AuditLogger(db)

    logger.log_model_remove("admin", "gpt-4", "global")

    assert len(db.logs) == 1
    assert db.logs[0]["action"] == "remove_model"


def test_log_model_update():
    """Test model update logging."""
    db = MockDB()
    logger = AuditLogger(db)

    logger.log_model_update("admin", "gpt-4", {"price": 0.01})

    assert len(db.logs) == 1
    assert db.logs[0]["action"] == "update_model"


def test_log_skill_register():
    """Test skill registration logging."""
    db = MockDB()
    logger = AuditLogger(db)

    logger.log_skill_register("alice", "my_skill", "user")

    assert len(db.logs) == 1
    assert db.logs[0]["action"] == "register_skill"
    assert db.logs[0]["resource_id"] == "my_skill"


def test_log_token_create():
    """Test token creation logging."""
    db = MockDB()
    logger = AuditLogger(db)

    logger.log_token_create("admin", "llm", "openai", "global")

    assert len(db.logs) == 1
    assert db.logs[0]["action"] == "create_token"


def test_get_logs():
    """Test log retrieval."""
    db = MockDB()
    logger = AuditLogger(db)

    # Add some logs
    logger.log_model_add("admin", "gpt-4", "global")
    logger.log_model_add("alice", "gpt-3.5", "user", "alice")

    # Get all logs
    logs = logger.get_logs()
    assert len(logs) == 2

    # Filter by user
    logs = logger.get_logs(user_id="admin")
    assert len(logs) == 2  # MockDB doesn't filter, just returns all
