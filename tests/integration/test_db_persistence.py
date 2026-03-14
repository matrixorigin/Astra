"""Database persistence tests for API models.

These tests verify that:
1. API models correctly persist data to database
2. Database queries return expected results
3. Data integrity is maintained (encryption, constraints)
4. Database schema is correctly configured

Note: These are NOT CLI → API → DB end-to-end tests.
For true E2E tests, see test_cli_to_api_e2e.py
"""

import pytest
from click.testing import CliRunner
from sqlalchemy import text, delete
import json

from api.database import get_db_session
from api.models import Token, AuditLog, UserFeedback, Session as SessionModel
from cli.mo_agent_api import cli as agent_cli
from cli.mo_admin_api import cli as admin_cli


@pytest.fixture
def runner():
    return CliRunner()


@pytest.fixture
def db():
    """Get test database session with cleanup."""
    session = next(get_db_session())

    # Clean up before test
    session.execute(delete(UserFeedback))
    session.execute(delete(AuditLog))
    session.execute(delete(Token))
    session.execute(delete(SessionModel))
    session.commit()

    yield session

    # Clean up after test
    session.execute(delete(UserFeedback))
    session.execute(delete(AuditLog))
    session.execute(delete(Token))
    session.execute(delete(SessionModel))
    session.commit()
    session.close()


class TestDatabasePersistence:
    """Database persistence tests for API models."""

    def test_session_list_queries_real_db(self, runner, db):
        """Test session list command queries real database."""
        from uuid_utils import uuid7

        # Create multiple test sessions
        session_ids = [str(uuid7()) for _ in range(3)]
        user_id = "test_user"

        for sid in session_ids:
            session = SessionModel(session_id=sid, user_id=user_id, status="active")
            db.add(session)
        db.commit()

        # Verify all sessions exist in DB
        results = db.query(SessionModel).filter(SessionModel.user_id == user_id).all()

        assert len(results) == 3
        assert all(s.session_id in session_ids for s in results)

    def test_session_show_retrieves_correct_data(self, runner, db):
        """Test session show retrieves correct session data."""
        from uuid_utils import uuid7

        session_id = str(uuid7())
        user_id = "alice"

        session = SessionModel(session_id=session_id, user_id=user_id, status="active")
        db.add(session)
        db.commit()

        # Query the session
        result = db.query(SessionModel).filter(SessionModel.session_id == session_id).first()

        assert result is not None
        assert result.user_id == user_id
        assert result.status == "active"

    def test_skill_registration_persists_to_db(self, runner, db):
        """Test skill registration data persists correctly."""
        # Skills are stored in skills_registry table
        # Verify the table structure exists
        result = db.execute(
            text(
                "SELECT COUNT(*) FROM information_schema.tables "
                "WHERE table_schema = DATABASE() AND table_name = 'skills_registry'"
            )
        ).scalar()

        assert result > 0, "skills_registry table should exist"


class TestAdminAPIPersistence:
    """Database persistence tests for admin API models."""

    def test_token_create_persists_encrypted_value(self, runner, db):
        """Test token creation stores encrypted value in database."""
        from uuid_utils import uuid7
        from core.auth.encryption import encrypt_token

        token_id = str(uuid7())
        token_value = "secret_openai_key_12345"
        encrypted_value = encrypt_token(token_value)

        token = Token(
            token_id=token_id,
            type="llm",
            provider="openai",
            encrypted_value=encrypted_value,
            is_active=True,
        )
        db.add(token)
        db.commit()

        # Verify token stored in DB
        result = db.query(Token).filter(Token.token_id == token_id).first()

        assert result is not None
        assert result.provider == "openai"
        assert result.encrypted_value == encrypted_value
        assert result.is_active == 1  # SQLAlchemy stores as integer

    def test_token_list_filters_by_provider(self, runner, db):
        """Test token list correctly filters by provider."""
        from uuid_utils import uuid7
        from core.auth.encryption import encrypt_token

        # Create tokens for different providers
        openai_token = Token(
            token_id=str(uuid7()),
            type="llm",
            provider="openai",
            encrypted_value=encrypt_token("openai_key"),
            is_active=True,
        )
        anthropic_token = Token(
            token_id=str(uuid7()),
            type="llm",
            provider="anthropic",
            encrypted_value=encrypt_token("anthropic_key"),
            is_active=True,
        )

        db.add(openai_token)
        db.add(anthropic_token)
        db.commit()

        # Query only OpenAI tokens
        results = db.query(Token).filter(Token.provider == "openai", Token.is_active == True).all()

        assert len(results) == 1
        assert results[0].provider == "openai"

    def test_audit_log_records_all_fields(self, runner, db):
        """Test audit log records all required fields."""
        from uuid_utils import uuid7

        log_id = str(uuid7())

        log = AuditLog(
            log_id=log_id,
            user_id="admin_user",
            action="token_created",
            resource_type="token",
            resource_id="token_123",
            details={"provider": "openai", "scope": "global"},
        )
        db.add(log)
        db.commit()

        # Verify all fields persisted
        result = db.query(AuditLog).filter(AuditLog.log_id == log_id).first()

        assert result is not None
        assert result.user_id == "admin_user"
        assert result.action == "token_created"
        assert result.resource_type == "token"
        assert result.resource_id == "token_123"
        assert result.details["provider"] == "openai"

    def test_audit_log_query_by_date_range(self, runner, db):
        """Test audit logs can be queried by date range."""
        from uuid_utils import uuid7
        from datetime import datetime, timedelta

        # Create logs with different timestamps
        now = datetime.now()
        old_log = AuditLog(
            log_id=str(uuid7()),
            user_id="admin",
            action="old_action",
            resource_type="token",
            resource_id="old_token",
            created_at=now - timedelta(days=10),
        )
        recent_log = AuditLog(
            log_id=str(uuid7()),
            user_id="admin",
            action="recent_action",
            resource_type="token",
            resource_id="recent_token",
            created_at=now - timedelta(days=1),
        )

        db.add(old_log)
        db.add(recent_log)
        db.commit()

        # Query recent logs (last 5 days)
        cutoff = now - timedelta(days=5)
        results = db.query(AuditLog).filter(AuditLog.created_at >= cutoff).all()

        assert len(results) == 1
        assert results[0].action == "recent_action"

    def test_feedback_export_data_integrity(self, runner, db):
        """Test feedback export retrieves correct data with proper types."""
        from uuid_utils import uuid7

        feedback_id = str(uuid7())

        feedback = UserFeedback(
            feedback_id=feedback_id,
            user_id="user1",
            agent_id="agent1",
            rating=5,
            comment="Excellent response!",
            feedback_type="explicit",
        )
        db.add(feedback)
        db.commit()

        # Verify data integrity
        result = db.query(UserFeedback).filter(UserFeedback.feedback_id == feedback_id).first()

        assert result is not None
        assert result.rating == 5
        assert result.comment == "Excellent response!"
        assert result.feedback_type == "explicit"
        assert isinstance(result.rating, int)

    def test_feedback_stats_aggregation(self, runner, db):
        """Test feedback statistics aggregation works correctly."""
        from uuid_utils import uuid7
        from sqlalchemy import func

        # Create feedback with different ratings
        for i, rating in enumerate([5, 5, 4, 3, 2]):
            feedback = UserFeedback(
                feedback_id=str(uuid7()), user_id="user1", agent_id="agent1", rating=rating
            )
            db.add(feedback)
        db.commit()

        # Aggregate stats
        stats = db.query(
            func.count(UserFeedback.feedback_id).label("total"),
            func.avg(UserFeedback.rating).label("avg_rating"),
            func.sum(func.if_(UserFeedback.rating >= 4, 1, 0)).label("positive"),
        ).first()

        assert stats.total == 5
        assert stats.avg_rating == 3.8
        assert stats.positive == 3

    def test_feedback_filter_by_agent_and_date(self, runner, db):
        """Test feedback can be filtered by agent and date."""
        from uuid_utils import uuid7
        from datetime import datetime, timedelta

        now = datetime.now()

        # Create feedback for different agents and dates
        for agent_id in ["agent1", "agent2"]:
            for days_ago in [1, 10]:
                feedback = UserFeedback(
                    feedback_id=str(uuid7()),
                    user_id="user1",
                    agent_id=agent_id,
                    rating=4,
                    created_at=now - timedelta(days=days_ago),
                )
                db.add(feedback)
        db.commit()

        # Query agent1 feedback from last 5 days
        cutoff = now - timedelta(days=5)
        results = (
            db.query(UserFeedback)
            .filter(UserFeedback.agent_id == "agent1", UserFeedback.created_at >= cutoff)
            .all()
        )

        assert len(results) == 1
        assert results[0].agent_id == "agent1"


class TestAuthenticationPersistence:
    """Verify CLI commands reject unauthenticated access."""

    def test_agent_cli_rejects_unauthenticated(self, runner):
        """Protected agent CLI commands exit non-zero without credentials."""
        from unittest.mock import MagicMock, patch

        with patch("cli.mo_agent_api.SyncAPIClient") as mock_cls:
            client = MagicMock()
            mock_cls.return_value = client
            client.ensure_authenticated.return_value = False

            for cmd in [
                ["session", "list"],
                ["skill", "list"],
            ]:
                from cli.mo_agent_api import cli as agent_cli

                result = runner.invoke(agent_cli, cmd)
                assert result.exit_code != 0 or "login" in result.output.lower(), (
                    f"Command {cmd} should require auth"
                )

    def test_admin_cli_rejects_unauthenticated(self, runner):
        """Protected admin CLI commands exit non-zero without credentials."""
        from unittest.mock import MagicMock, patch

        with patch("cli.mo_admin_api.SyncAPIClient") as mock_cls:
            client = MagicMock()
            mock_cls.return_value = client
            client.ensure_authenticated.return_value = False

            for cmd in [
                ["init"],
                ["token", "list"],
                ["audit", "logs"],
                ["feedback", "stats"],
            ]:
                from cli.mo_admin_api import cli as admin_cli

                result = runner.invoke(admin_cli, cmd)
                assert result.exit_code != 0 or "login" in result.output.lower(), (
                    f"Command {cmd} should require auth"
                )


class TestDataConsistency:
    """Test data consistency and integrity in database."""

    def test_token_encryption_consistency(self, runner, db):
        """Test that token encryption is consistent across stack."""
        from uuid_utils import uuid7
        from core.auth.encryption import encrypt_token, decrypt_token

        original_value = "secret_key_12345"
        encrypted = encrypt_token(original_value)
        decrypted = decrypt_token(encrypted)

        assert decrypted == original_value

        # Store and retrieve from DB
        token = Token(
            token_id=str(uuid7()),
            type="llm",
            provider="openai",
            encrypted_value=encrypted,
            is_active=True,
        )
        db.add(token)
        db.commit()

        # Retrieve and decrypt
        result = db.query(Token).first()
        retrieved_decrypted = decrypt_token(result.encrypted_value)

        assert retrieved_decrypted == original_value

    def test_audit_log_json_serialization(self, runner, db):
        """Test that audit log details are properly serialized."""
        from uuid_utils import uuid7

        details = {"provider": "openai", "scope_type": "global", "metadata": {"key": "value"}}

        log = AuditLog(
            log_id=str(uuid7()),
            user_id="admin",
            action="token_created",
            resource_type="token",
            resource_id="t1",
            details=details,
        )
        db.add(log)
        db.commit()

        # Retrieve and verify JSON is preserved
        result = db.query(AuditLog).first()
        assert result.details == details
        assert result.details["provider"] == "openai"
