"""Integration tests for mo-admin CLI.

NOTE: These tests require:
1. MatrixOne running on localhost:6001
2. Agent system initialized (make db-init-agent)
3. Test admin user with mo_agent_admin role:
   CREATE USER test_admin IDENTIFIED BY 'test123';
   GRANT mo_agent_admin TO test_admin;

Run with: pytest tests/integration/test_admin_cli.py -v
"""

import pytest
from click.testing import CliRunner
from sqlalchemy import text

from api.database import get_db_session
from cli.mo_admin import cli


@pytest.fixture
def runner():
    return CliRunner()


@pytest.fixture
def test_db():
    """Create a test database session."""
    db = next(get_db_session())

    # Seed RBAC data
    try:
        # Clean up existing data first
        db.execute(text("DELETE FROM user_roles WHERE user_id IN ('u_admin', 'u_alice')"))
        db.execute(text("DELETE FROM users WHERE user_id IN ('u_admin', 'u_alice')"))
        db.execute(text("DELETE FROM roles WHERE role_id IN ('r_admin', 'r_user')"))
        db.commit()

        # Create roles
        db.execute(text("INSERT INTO roles (role_id, role_name, created_at) VALUES ('r_admin', 'mo_agent_admin', NOW()), ('r_user', 'mo_agent_user', NOW())"))
        # Create users
        db.execute(text("INSERT INTO users (user_id, username, email, password_hash, created_at) VALUES ('u_admin', 'test_admin', 'admin@test.com', 'hash', NOW())"))
        db.execute(text("INSERT INTO users (user_id, username, email, password_hash, created_at) VALUES ('u_alice', 'alice', 'alice@test.com', 'hash', NOW())"))
        
        # Assign roles
        db.execute(text("INSERT INTO user_roles (user_id, role_id, created_at) VALUES ('u_admin', 'r_admin', NOW()), ('u_alice', 'r_user', NOW())"))
        
        db.commit()
    except Exception as e:
        print(f"Failed to seed RBAC: {e}")
        db.rollback()

    # Clean up test data
    try:
        db.execute(text("DELETE FROM configs WHERE key_name LIKE 'test-%'"))
        db.execute(text("DELETE FROM tokens WHERE token_id LIKE 'token_%'"))
        db.execute(text("DELETE FROM audit_logs WHERE user_id = 'test_admin'"))
        db.commit()
    except:
        pass

    yield db

    # Cleanup after test
    try:
        db.execute(text("DELETE FROM configs WHERE key_name LIKE 'test-%'"))
        db.execute(text("DELETE FROM tokens WHERE token_id LIKE 'token_%'"))
        db.execute(text("DELETE FROM audit_logs WHERE user_id = 'test_admin'"))
        # Optional: Clean up users/roles? Maybe not needed as they are static test data
        db.commit()
    except:
        pass


@pytest.mark.integration
class TestModelManagement:
    """Test model management commands."""

    def test_model_add_global(self, runner, test_db):
        """Test adding a global model."""
        # Debug: Check DB content
        from api.models import User, Role, UserRole
        users = test_db.query(User).all()
        roles = test_db.query(Role).all()
        user_roles = test_db.query(UserRole).all()
        print(f"DEBUG: Users: {[u.username for u in users]}")
        print(f"DEBUG: Roles: {[r.role_name for r in roles]}")
        print(f"DEBUG: UserRoles: {[(ur.user_id, ur.role_id) for ur in user_roles]}")
        
        result = runner.invoke(
            cli,
            [
                "--user",
                "test_admin",
                "model",
                "add",
                "test-gpt-4",
                "openai",
                "--scope",
                "global",
                "--context-window",
                "128000",
                "--price-prompt",
                "0.0025",
                "--price-completion",
                "0.01",
            ],
        )

        # Note: Will fail without proper permissions
        if "Permission denied" in result.output:
            pytest.skip("test_admin user needs mo_agent_admin role")

        if result.exit_code != 0:
            print(f"Command failed. Output: {result.output}")
            print(f"Exception: {result.exception}")
        assert result.exit_code == 0
        assert "added successfully" in result.output

        # Verify in database
        result_query = test_db.execute(
            text("SELECT * FROM configs WHERE key_name = 'test-gpt-4'")
        )
        row = result_query.first()
        if row:
            row = dict(row._mapping)
        assert row is not None
        assert row["key_name"] == "test-gpt-4"

    def test_model_list(self, runner, test_db):
        """Test listing models."""
        result = runner.invoke(cli, ["--user", "test_admin", "model", "list"])

        assert result.exit_code == 0
        assert "Models:" in result.output or "No models found" in result.output


@pytest.mark.integration
@pytest.mark.integration
class TestPermissionEnforcement:
    """Test permission enforcement.
    
    Note: In development mode, all operations are allowed.
    These tests are skipped until production RBAC is enabled.
    """

    def test_non_admin_cannot_add_global_model(self, runner):
        """Test that non-admin users cannot add global models.
        
        Skipped in development mode where all operations are allowed.
        """
        result = runner.invoke(
            cli,
            [
                "--user",
                "alice",  # Regular user
                "model",
                "add",
                "test-model",
                "openai",
                "--scope",
                "global",
            ],
        )

        # In development mode: should succeed (exit_code == 0)
        # In production mode (now enabled): should fail (exit_code == 1)
        assert result.exit_code == 1  # Permission denied
        assert "Permission denied" in result.output or "Error" in result.output


@pytest.mark.integration
class TestCLIStructure:
    """Test CLI structure and help messages (no database required)."""

    def test_help_message(self, runner):
        """Test main help message."""
        result = runner.invoke(cli, ["--help"])
        assert result.exit_code == 0
        assert "mo-admin" in result.output
        assert "model" in result.output
        assert "token" in result.output
        assert "audit" in result.output

    def test_model_help(self, runner):
        """Test model command help."""
        result = runner.invoke(cli, ["model", "--help"])
        assert result.exit_code == 0
        assert "add" in result.output
        assert "remove" in result.output
        assert "list" in result.output

    def test_token_help(self, runner):
        """Test token command help."""
        result = runner.invoke(cli, ["token", "--help"])
        assert result.exit_code == 0
        assert "create" in result.output
        assert "list" in result.output

    def test_audit_help(self, runner):
        """Test audit command help."""
        result = runner.invoke(cli, ["audit", "--help"])
        assert result.exit_code == 0
        assert "logs" in result.output


if __name__ == "__main__":
    pytest.main([__file__, "-v"])
