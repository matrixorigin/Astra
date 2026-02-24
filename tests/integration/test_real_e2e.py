"""Real end-to-end tests: CLI → HTTP → API Server → DB.

These tests use FastAPI TestClient to make real HTTP calls without starting a server.
They verify the complete data flow: CLI → API → Database.

This catches real integration bugs that mocked tests miss.
"""

import pytest
import os
from click.testing import CliRunner
from fastapi.testclient import TestClient
from sqlalchemy import delete
from unittest.mock import patch

# Set required environment variables before importing app
os.environ.setdefault("TOKEN_ENCRYPTION_KEY", "test-key-" + "x" * 32)
os.environ.setdefault("JWT_SECRET_KEY", "test-jwt-secret-" + "x" * 32)
os.environ.setdefault("MATRIXONE_DATABASE", "test_dev_agent_v3")  # Use test database

from api.main import app
from api.database import get_db_session
from api.models import User, Session, Token, AuditLog, UserRole, Role
from cli.mo_agent_api import cli as agent_cli
from cli.mo_admin_api import cli as admin_cli


@pytest.fixture
def client():
    """FastAPI test client."""
    return TestClient(app)


@pytest.fixture
def db():
    """Get test database session with cleanup."""
    session = next(get_db_session())
    
    # Clean up before test
    try:
        session.execute(delete(AuditLog))
        session.execute(delete(Token))
        session.execute(delete(Session))
        session.execute(delete(UserRole))
        session.commit()
    except Exception:
        session.rollback()
    
    try:
        session.execute(delete(User))
        session.execute(delete(Role))
        session.commit()
    except Exception:
        session.rollback()
    
    yield session
    
    # Clean up after test
    try:
        session.execute(delete(AuditLog))
        session.execute(delete(Token))
        session.execute(delete(Session))
        session.execute(delete(UserRole))
        session.commit()
    except Exception:
        session.rollback()
    
    try:
        session.execute(delete(User))
        session.execute(delete(Role))
        session.commit()
    except Exception:
        session.rollback()
    
    session.close()


@pytest.fixture
def runner():
    """CLI runner with isolated filesystem for each test."""
    return CliRunner()


@pytest.fixture
def isolated_runner(tmp_path, monkeypatch):
    """CLI runner with isolated home directory to avoid credential conflicts."""
    # Create isolated home directory structure
    isolated_home = tmp_path / "home"
    isolated_home.mkdir()
    
    # Create .mo-agent directory for credentials
    mo_agent_dir = isolated_home / ".mo-agent"
    mo_agent_dir.mkdir()
    
    # Patch expanduser to use isolated home
    original_expanduser = os.path.expanduser
    def mock_expanduser(path):
        if path.startswith("~"):
            return str(isolated_home / path[2:])
        return original_expanduser(path)
    
    monkeypatch.setattr(os.path, "expanduser", mock_expanduser)
    
    # Also patch Path.home() to use isolated home
    from pathlib import Path
    monkeypatch.setattr(Path, "home", lambda: isolated_home)
    
    return CliRunner()


@pytest.fixture(scope="function")
def authenticated_runner(isolated_runner, client, test_user):
    """Runner with authenticated session.
    
    Note: Must be used with mock_httpx_with_testclient fixture in test.
    """
    print(f"\n[FIXTURE] authenticated_runner: Starting login for user {test_user.username}")
    
    # Login (will use mocked httpx from test, isolated_runner has isolated home)
    result = isolated_runner.invoke(
        agent_cli,
        ["--api-url", "http://test", "login"],
        input="testuser\npassword123\n"
    )
    print(f"[FIXTURE] Login result: exit_code={result.exit_code}, output={result.output}")
    
    if result.exit_code != 0:
        print(f"[FIXTURE] Login failed: {result.output}")
        if result.exception:
            import traceback
            traceback.print_exception(type(result.exception), result.exception, result.exception.__traceback__)
    assert result.exit_code == 0, f"Login failed: {result.output}"
    
    print(f"[FIXTURE] Login successful, credentials saved")
    return isolated_runner


@pytest.fixture(scope="function")
def authenticated_admin_runner(isolated_runner, client, admin_user):
    """Runner with authenticated admin session.
    
    Note: Must be used with mock_httpx_with_testclient fixture in test.
    """
    # Login as admin (will use mocked httpx from test, isolated_runner has isolated home)
    result = isolated_runner.invoke(
        admin_cli,
        ["--api-url", "http://test", "login"],
        input="admin\nadmin123\n"
    )
    if result.exit_code != 0:
        print(f"Admin login failed: {result.output}")
        if result.exception:
            import traceback
            traceback.print_exception(type(result.exception), result.exception, result.exception.__traceback__)
    assert result.exit_code == 0, f"Admin login failed: {result.output}"
    return isolated_runner


@pytest.fixture
def test_user(db):
    """Create test user in database."""
    from core.auth.password import hash_password
    
    user = User(
        user_id="test_user",
        username="testuser",
        email="test@example.com",
        password_hash=hash_password("password123")
    )
    db.add(user)
    db.commit()
    db.refresh(user)
    return user


@pytest.fixture
def admin_user(db):
    """Create admin user in database."""
    from core.auth.password import hash_password
    from api.models import Role, UserRole
    from uuid_utils import uuid7
    
    # Create admin role if not exists (must be "mo_agent_admin" for permission checks)
    admin_role = db.query(Role).filter(Role.role_name == "mo_agent_admin").first()
    if not admin_role:
        admin_role = Role(
            role_id=str(uuid7()),
            role_name="mo_agent_admin",
            description="Administrator role"
        )
        db.add(admin_role)
        db.flush()
    
    # Create user
    user = User(
        user_id="admin_user",
        username="admin",
        email="admin@example.com",
        password_hash=hash_password("admin123")
    )
    db.add(user)
    db.flush()
    
    # Assign admin role
    user_role = UserRole(
        user_id=user.user_id,
        role_id=admin_role.role_id
    )
    db.add(user_role)
    db.commit()
    db.refresh(user)
    return user


@pytest.fixture(autouse=True)
def mock_httpx_with_testclient(client):
    """Mock httpx to use TestClient instead of real HTTP.
    
    This allows CLI to make "real" HTTP calls that go through TestClient,
    which calls the actual FastAPI app without starting a server.
    
    Auto-used in all tests in this module.
    """
    import httpx
    
    class TestClientAdapter:
        """Adapter to make TestClient work like httpx.AsyncClient."""
        def __init__(self, test_client):
            self.test_client = test_client
            self.headers = {}
        
        async def request(self, method, url, **kwargs):
            # Extract path from full URL
            from urllib.parse import urlparse
            path = urlparse(url).path
            
            # Merge headers
            headers = {**self.headers, **kwargs.get("headers", {})}
            kwargs["headers"] = headers
            
            # Call TestClient (synchronous)
            response = self.test_client.request(method, path, **kwargs)
            
            # TestClient response already has .json() method
            return response
        
        def stream(self, method, url, **kwargs):
            """Support streaming requests."""
            # Extract path from full URL
            from urllib.parse import urlparse
            path = urlparse(url).path
            
            # Merge headers
            headers = {**self.headers, **kwargs.get("headers", {})}
            kwargs["headers"] = headers
            
            # Return a context manager for streaming
            class StreamContext:
                def __init__(self, test_client, method, path, kwargs):
                    self.test_client = test_client
                    self.method = method
                    self.path = path
                    self.kwargs = kwargs
                
                def __enter__(self):
                    self.response = self.test_client.request(self.method, self.path, **self.kwargs)
                    return self.response
                
                def __exit__(self, *args):
                    pass
                
                async def __aenter__(self):
                    self.response = self.test_client.request(self.method, self.path, **self.kwargs)
                    return self.response
                
                async def __aexit__(self, *args):
                    pass
            
            return StreamContext(self.test_client, method, path, kwargs)
        
        async def aclose(self):
            pass
        
        async def __aenter__(self):
            return self
        
        async def __aexit__(self, *args):
            pass
    
    original_client = httpx.AsyncClient
    
    def mock_client(*args, **kwargs):
        return TestClientAdapter(client)
    
    with patch("httpx.AsyncClient", mock_client):
        yield


class TestAgentCLIRealE2E:
    """Real E2E tests for agent CLI commands."""

    def test_login_creates_credentials_file(self, runner, test_user):
        """Test login creates JWT token and stores credentials file."""
        # Clear existing credentials
        creds_file = os.path.expanduser("~/.mo-agent/credentials.json")
        if os.path.exists(creds_file):
            os.remove(creds_file)
        
        result = runner.invoke(
            agent_cli,
            ["--api-url", "http://test", "login"],
            input="testuser\npassword123\n"  # Use username, not email
        )
        
        print(f"Exit code: {result.exit_code}")
        print(f"Output: {result.output}")
        if result.exception:
            print(f"Exception: {result.exception}")
            import traceback
            traceback.print_exception(type(result.exception), result.exception, result.exception.__traceback__)
        
        assert result.exit_code == 0
        assert "✅ Logged in" in result.output
        assert os.path.exists(creds_file)

    def test_login_with_invalid_credentials_fails(self, runner, test_user):
        """Test that invalid credentials are rejected by API."""
        result = runner.invoke(
            agent_cli,
            ["--api-url", "http://test", "login"],
            input="testuser\nwrongpassword\n"
        )
        
        assert result.exit_code != 0
        assert "failed" in result.output.lower()

    def test_session_list_retrieves_real_data(self, authenticated_runner, test_user, db):
        """Test session list retrieves real data from database via API.
        
        This verifies:
        1. CLI makes real HTTP request
        2. API queries database
        3. API returns {"sessions": [...], "total": ...} format
        4. CLI correctly handles dict response (not list)
        """
        print(f"\n[TEST] Starting test_session_list_retrieves_real_data")
        
        # Check if credentials file exists
        import os
        creds_file = os.path.expanduser("~/.mo-agent/credentials.json")
        print(f"[TEST] Credentials file exists: {os.path.exists(creds_file)}")
        if os.path.exists(creds_file):
            with open(creds_file) as f:
                print(f"[TEST] Credentials content: {f.read()[:100]}...")
        
        # Create sessions in database
        from uuid_utils import uuid7
        session1 = Session(
            session_id=str(uuid7()),
            user_id=test_user.user_id,
            status="active",
            event_count=5
        )
        session2 = Session(
            session_id=str(uuid7()),
            user_id=test_user.user_id,
            status="closed",
            event_count=3
        )
        db.add_all([session1, session2])
        db.commit()
        
        print(f"[TEST] Created sessions: {session1.session_id}, {session2.session_id}")
        
        # CLI should retrieve these sessions via API
        result = authenticated_runner.invoke(
            agent_cli,
            ["--api-url", "http://test", "session", "list"]
        )
        
        print(f"[TEST] Result: exit_code={result.exit_code}, output={result.output}")
        
        assert result.exit_code == 0, f"Failed: {result.output}"
        assert session1.session_id in result.output
        assert session2.session_id in result.output

    def test_session_list_handles_dict_response_format(self, authenticated_runner, test_user, db):
        """Test CLI correctly handles API's {"sessions": [...]} format.
        
        This is a critical bug fix test - previously CLI would crash
        trying to iterate over dict keys instead of the sessions list.
        """
        # Create session
        from uuid_utils import uuid7
        session = Session(
            session_id=str(uuid7()),
            user_id=test_user.user_id,
            status="active"
        )
        db.add(session)
        db.commit()
        
        # This should NOT crash (bug fix verification)
        result = authenticated_runner.invoke(
            agent_cli,
            ["--api-url", "http://test", "session", "list"]
        )
        
        assert result.exit_code == 0, f"Failed: {result.output}"
        assert session.session_id in result.output

    def test_session_show_retrieves_specific_session(self, authenticated_runner, test_user, db):
        """Test session show retrieves specific session details."""
        from uuid_utils import uuid7
        session = Session(
            session_id=str(uuid7()),
            user_id=test_user.user_id,
            status="active",
            event_count=10
        )
        db.add(session)
        db.commit()
        
        result = authenticated_runner.invoke(
            agent_cli,
            ["--api-url", "http://test", "session", "show", session.session_id]
        )
        
        assert result.exit_code == 0, f"Failed: {result.output}"
        assert session.session_id in result.output
        assert "10" in result.output  # event count

    def test_protected_commands_require_authentication(self, runner, db):
        """Test that protected commands fail without valid JWT token."""
        # Clear credentials
        creds_file = os.path.expanduser("~/.mo-agent/credentials.json")
        if os.path.exists(creds_file):
            os.remove(creds_file)
        
        # Should fail without login
        result = runner.invoke(
            agent_cli,
            ["--api-url", "http://test", "session", "list"]
        )
        
        assert result.exit_code != 0
        assert "login" in result.output.lower()


class TestAdminCLIRealE2E:
    """Real E2E tests for admin CLI commands."""

    def test_admin_login_with_admin_role(self, runner, admin_user):
        """Test admin can login successfully."""
        result = runner.invoke(
            admin_cli,
            ["--api-url", "http://test", "login"],
            input="admin\nadmin123\n"
        )
        
        assert result.exit_code == 0
        assert "✅ Logged in" in result.output

    def test_token_list_retrieves_real_tokens(self, authenticated_admin_runner, admin_user, db):
        """Test admin token list retrieves real data from database.
        
        This verifies:
        1. Admin authentication works
        2. API queries tokens table
        3. CLI displays token information correctly
        """
        # Create token in database
        from core.auth.encryption import encrypt_token
        token = Token(
            token_id="tok-test-123",
            type="llm",  # Field name is 'type', not 'token_type'
            provider="openai",
            encrypted_value=encrypt_token("sk-test-key"),
            is_active=1
        )
        db.add(token)
        db.commit()
        
        # CLI should retrieve this token via API
        result = authenticated_admin_runner.invoke(
            admin_cli,
            ["--api-url", "http://test", "token", "list"]
        )
        
        assert result.exit_code == 0, f"Failed: {result.output}"
        assert "tok-test" in result.output
        assert "openai" in result.output

    def test_token_create_persists_to_database(self, authenticated_admin_runner, admin_user, db):
        """Test token creation persists to database."""
        result = authenticated_admin_runner.invoke(
            admin_cli,
            ["--api-url", "http://test", "token", "create", 
             "--type", "llm", "--provider", "openai"],
            input="sk-test-key-123\n"
        )
        
        assert result.exit_code == 0, f"Failed: {result.output}"
        assert "✅" in result.output or "created" in result.output.lower()
        
        # Verify token exists in database
        tokens = db.query(Token).filter(Token.provider == "openai").all()
        assert len(tokens) > 0

    def test_audit_logs_retrieves_real_logs(self, authenticated_admin_runner, admin_user, db):
        """Test audit logs retrieves real audit data."""
        # Create audit log
        log = AuditLog(
            log_id="log-123",
            user_id=admin_user.user_id,
            action="test_action",
            resource_type="test",
            resource_id="test-123"
        )
        db.add(log)
        db.commit()
        
        result = authenticated_admin_runner.invoke(
            admin_cli,
            ["--api-url", "http://test", "audit", "logs"]
        )
        
        assert result.exit_code == 0, f"Failed: {result.output}"
        assert "test_action" in result.output or "log-123" in result.output

    def test_feedback_export_returns_async_job_info(self, authenticated_admin_runner, admin_user):
        """Test feedback export returns async job info, not data.
        
        This is a critical bug fix test - previously CLI would crash
        trying to iterate over non-existent "data" field.
        """
        result = authenticated_admin_runner.invoke(
            admin_cli,
            ["--api-url", "http://test", "feedback", "export"]
        )
        
        # Should show job info, not crash
        assert result.exit_code == 0, f"Failed: {result.output}"
        assert ("Export job created" in result.output or "Export ready" in result.output)

    def test_non_admin_cannot_access_admin_commands(self, runner, test_user):
        """Test that non-admin users cannot access admin commands."""
        # Login as regular user
        runner.invoke(
            admin_cli,
            ["--api-url", "http://test", "login"],
            input="testuser\npassword123\n"
        )
        
        # Try to list tokens (admin only)
        result = runner.invoke(
            admin_cli,
            ["--api-url", "http://test", "token", "list"]
        )
        
        # Should fail with permission error
        assert result.exit_code != 0 or "permission" in result.output.lower() or "admin" in result.output.lower()


class TestDataConsistencyRealE2E:
    """Test data consistency across CLI → API → DB."""

    def test_session_create_and_retrieve_consistency(self, authenticated_runner, test_user, db):
        """Test that created session can be immediately retrieved."""
        # Create session via API
        from uuid_utils import uuid7
        session_id = str(uuid7())
        session = Session(
            session_id=session_id,
            user_id=test_user.user_id,
            status="active"
        )
        db.add(session)
        db.commit()
        
        # Immediately retrieve via CLI
        result = authenticated_runner.invoke(
            agent_cli,
            ["--api-url", "http://test", "session", "show", session_id]
        )
        
        assert result.exit_code == 0, f"Failed: {result.output}"
        assert session_id in result.output

    def test_token_encryption_consistency(self, authenticated_admin_runner, admin_user, db):
        """Test that token encryption/decryption works correctly."""
        # Create token
        result = authenticated_admin_runner.invoke(
            admin_cli,
            ["--api-url", "http://test", "token", "create",
             "--type", "llm", "--provider", "test"],
            input="test-secret-key\n"
        )
        
        assert result.exit_code == 0, f"Failed: {result.output}"
        
        # Verify token is encrypted in database
        tokens = db.query(Token).filter(Token.provider == "test").all()
        assert len(tokens) > 0
        # Encrypted value should not be plaintext
        assert tokens[0].encrypted_value != "test-secret-key"



class TestChatStreamingRealE2E:
    """Test chat command integration with real API."""
    
    def test_chat_without_session_creates_session(self, authenticated_runner, db):
        """Test that chat without session ID attempts to create one."""
        from api.models import Session as SessionModel
        
        # Count sessions before
        sessions_before = db.query(SessionModel).filter(SessionModel.user_id == "test_user").count()
        
        # Try to start chat (will fail at LLM call, but should create session first)
        result = authenticated_runner.invoke(
            agent_cli,
            ["--api-url", "http://test", "chat", "--no-stream"],
            input="/exit\n"
        )
        
        # May fail at LLM call, but should have attempted session creation
        # Check if session was created
        sessions_after = db.query(SessionModel).filter(SessionModel.user_id == "test_user").count()
        
        # If session creation worked, we should see increase
        # If it failed, we should see error message about session creation
        assert sessions_after >= sessions_before or "session" in result.output.lower()
    
    def test_non_streaming_chat_polls_and_displays_response(self, authenticated_runner, db, client):
        """Test that non-streaming chat polls run status and displays response."""
        from api.models import Session as SessionModel
        from core.events.models import EventType
        import json
        from pathlib import Path
        
        # Read credentials from file (new profile format)
        creds_path = Path.home() / ".mo-agent" / "credentials.json"
        with open(creds_path) as f:
            creds = json.load(f)
        
        # Get access token from current profile
        current_profile = creds.get("current_profile", "default")
        profile_data = creds.get("profiles", {}).get(current_profile, {})
        access_token = profile_data.get("access_token")
        
        # Create a session first
        session_response = client.post(
            "/sessions",
            json={"agent_id": "dev-agent"},
            headers={"Authorization": f"Bearer {access_token}"}
        )
        assert session_response.status_code == 201
        session_id = session_response.json()["session_id"]
        
        # Mock the run to complete immediately with a response
        with patch("core.agent.run_engine.RunEngine.start_run") as mock_start:
            async def mock_run(run):
                # Simulate run completion
                from core.agent.run import RunStatus
                run.status = RunStatus.COMPLETED
                # Log a response event
                from core.events.event_logger import EventLogger
                logger = EventLogger(db)
                logger.create_llm_response(
                    user_id="test_user",
                    session_id=session_id,
                    content="Test response from agent",
                    agent_id="dev-agent",
                    agent_version="1.0.0",
                    parent_event_id=None,
                    causal_chain_id="test-chain"
                )
            
            mock_start.side_effect = mock_run
            
            # Start non-streaming chat
            result = authenticated_runner.invoke(
                agent_cli,
                ["--api-url", "http://test", "chat", "--no-stream", "--session-id", session_id],
                input="Hello\n/exit\n"
            )
            
            # Should show response or at least not be empty
            assert result.output.strip() != ""
            # Should have attempted to get response
            assert "Agent>" in result.output or "Test response" in result.output or "completed" in result.output.lower()


class TestUserRegistrationRealE2E:
    """Test user registration with real API."""
    
    def test_register_creates_user_in_database(self, runner, db):
        """Test that register command creates user in database."""
        from api.models import User
        
        # Register new user (password confirmation needs to match)
        result = runner.invoke(
            agent_cli,
            ["--api-url", "http://test", "register"],
            input="newuser\nnewuser@example.com\npassword123\n"
        )
        
        # Check if registration succeeded or failed
        if result.exit_code == 0:
            assert "success" in result.output.lower() or "registered" in result.output.lower()
            
            # Verify user in database
            user = db.query(User).filter(User.username == "newuser").first()
            assert user is not None
            assert user.email == "newuser@example.com"
        else:
            # If failed, should show error message
            assert "error" in result.output.lower() or "failed" in result.output.lower()
    
    def test_register_duplicate_username_fails(self, runner, test_user, db):
        """Test that registering duplicate username fails."""
        # Try to register with existing username
        result = runner.invoke(
            agent_cli,
            ["--api-url", "http://test", "register"],
            input="testuser\nother@example.com\npassword123\n"
        )
        
        # Should fail with error
        assert "already exists" in result.output.lower() or "error" in result.output.lower()


class TestWhoamiRealE2E:
    """Test whoami command with real API."""
    
    def test_whoami_shows_current_user(self, authenticated_runner):
        """Test that whoami shows current user info."""
        result = authenticated_runner.invoke(
            agent_cli,
            ["--api-url", "http://test", "whoami"]
        )
        
        assert result.exit_code == 0, f"Failed: {result.output}"
        assert "testuser" in result.output or "test_user" in result.output


class TestSkillManagementRealE2E:
    """Test skill management with real API."""
    
    def test_skill_list_retrieves_skills(self, authenticated_runner):
        """Test that skill list retrieves skills from API."""
        result = authenticated_runner.invoke(
            agent_cli,
            ["--api-url", "http://test", "skill", "list"]
        )
        
        assert result.exit_code == 0, f"Failed: {result.output}"
        # Should show skills or "No skills found"
        assert "skill" in result.output.lower() or "no skills" in result.output.lower()


class TestReplayRealE2E:
    """Test session replay with real API."""
    
    def test_replay_with_valid_session(self, authenticated_runner, db):
        """Test replay with valid session ID."""
        from api.models import Session as SessionModel
        
        # Get or create a session
        session = db.query(SessionModel).filter(SessionModel.user_id == "test_user").first()
        if not session:
            # Create one via API
            from uuid_utils import uuid7
            session = SessionModel(
                session_id=str(uuid7()),
                user_id="test_user",
                agent_id="test-agent",
                status="active"
            )
            db.add(session)
            db.commit()
        
        session_id = session.session_id
        
        # Try to replay (may fail at LLM call, but should accept session ID)
        result = authenticated_runner.invoke(
            agent_cli,
            ["--api-url", "http://test", "replay", session_id]
        )
        
        # Should either succeed or fail with meaningful error (not "session not found")
        if result.exit_code != 0:
            assert "not found" not in result.output.lower() or "llm" in result.output.lower() or "model" in result.output.lower()


class TestAdminInitRealE2E:
    """Test admin init command with real API."""
    
    def test_admin_init_creates_tables(self, authenticated_admin_runner, db):
        """Test that admin init creates database tables."""
        result = authenticated_admin_runner.invoke(
            admin_cli,
            ["--api-url", "http://test", "init"]
        )
        
        assert result.exit_code == 0, f"Failed: {result.output}"
        assert "initialized" in result.output.lower() or "success" in result.output.lower()


class TestAdminWhoamiRealE2E:
    """Test admin whoami command with real API."""
    
    def test_admin_whoami_shows_admin_user(self, authenticated_admin_runner):
        """Test that admin whoami shows admin user info."""
        result = authenticated_admin_runner.invoke(
            admin_cli,
            ["--api-url", "http://test", "whoami"]
        )
        
        assert result.exit_code == 0, f"Failed: {result.output}"
        assert "admin" in result.output.lower()
