"""Unit tests for API client."""

import json
from pathlib import Path
from unittest.mock import AsyncMock, MagicMock, patch

import httpx
import pytest

from cli.api_client import APIClient


@pytest.fixture
def mock_credentials_path(tmp_path: Path) -> Path:
    """Create temporary credentials path."""
    return tmp_path / "credentials.json"


@pytest.fixture
def mock_httpx_client():
    """Mock httpx.AsyncClient."""
    client = AsyncMock(spec=httpx.AsyncClient)
    return client


@pytest.mark.asyncio
async def test_api_client_initialization(mock_credentials_path: Path):
    """Test API client initialization."""
    client = APIClient(
        base_url="http://test:8000",
        credentials_path=mock_credentials_path,
    )
    assert client.base_url == "http://test:8000"
    assert client.credentials_path == mock_credentials_path


@pytest.mark.asyncio
async def test_load_credentials(mock_credentials_path: Path):
    """Test loading credentials from file."""
    # Write credentials in new profile format
    credentials = {
        "current_profile": "default",
        "profiles": {
            "default": {
                "username": "test_user",
                "access_token": "test_access",
                "refresh_token": "test_refresh",
            }
        }
    }
    mock_credentials_path.write_text(json.dumps(credentials))

    async with APIClient(credentials_path=mock_credentials_path) as client:
        assert client._access_token == "test_access"
        assert client._refresh_token == "test_refresh"


@pytest.mark.asyncio
async def test_save_credentials(mock_credentials_path: Path):
    """Test saving credentials to file."""
    async with APIClient(credentials_path=mock_credentials_path) as client:
        client._access_token = "new_access"
        client._refresh_token = "new_refresh"
        await client._save_credentials(username="test_user")

    # Verify file contents
    data = json.loads(mock_credentials_path.read_text())
    assert data["current_profile"] == "test_user"
    assert data["profiles"]["test_user"]["access_token"] == "new_access"
    assert data["profiles"]["test_user"]["refresh_token"] == "new_refresh"

    # Verify file permissions
    assert mock_credentials_path.stat().st_mode & 0o777 == 0o600


@pytest.mark.asyncio
async def test_request_with_auth(mock_credentials_path: Path):
    """Test request with authentication header."""
    with patch("httpx.AsyncClient") as mock_client_class:
        mock_client = AsyncMock()
        mock_response = MagicMock()
        mock_response.status_code = 200
        mock_response.json.return_value = {"result": "success"}
        mock_client.request.return_value = mock_response
        mock_client_class.return_value = mock_client

        async with APIClient(credentials_path=mock_credentials_path) as client:
            client._access_token = "test_token"
            response = await client._request("GET", "/test")

            # Verify request was made with auth header
            mock_client.request.assert_called_once()
            call_args = mock_client.request.call_args
            assert call_args[1]["headers"]["Authorization"] == "Bearer test_token"
            assert response.status_code == 200


@pytest.mark.asyncio
async def test_auto_refresh_on_401(mock_credentials_path: Path):
    """Test automatic token refresh on 401."""
    with patch("httpx.AsyncClient") as mock_client_class:
        mock_client = AsyncMock()

        # First request returns 401
        mock_401_response = MagicMock()
        mock_401_response.status_code = 401

        # Refresh request returns new token
        mock_refresh_response = MagicMock()
        mock_refresh_response.status_code = 200
        mock_refresh_response.json.return_value = {"access_token": "new_token"}

        # Retry request succeeds
        mock_success_response = MagicMock()
        mock_success_response.status_code = 200
        mock_success_response.json.return_value = {"result": "success"}

        # Setup mock responses
        mock_client.request.side_effect = [mock_401_response, mock_success_response]
        mock_client.post.return_value = mock_refresh_response
        mock_client_class.return_value = mock_client

        async with APIClient(credentials_path=mock_credentials_path) as client:
            client._access_token = "old_token"
            client._refresh_token = "refresh_token"

            response = await client._request("GET", "/test")

            # Verify refresh was called
            mock_client.post.assert_called_once()
            assert "refresh" in mock_client.post.call_args[0][0]

            # Verify token was updated
            assert client._access_token == "new_token"


@pytest.mark.asyncio
async def test_login(mock_credentials_path: Path):
    """Test login method."""
    with patch("httpx.AsyncClient") as mock_client_class:
        mock_client = AsyncMock()
        mock_response = MagicMock()
        mock_response.status_code = 200
        mock_response.json.return_value = {
            "access_token": "access_123",
            "refresh_token": "refresh_123",
        }
        mock_client.request.return_value = mock_response
        mock_client_class.return_value = mock_client

        async with APIClient(credentials_path=mock_credentials_path) as client:
            result = await client.login("testuser", "testpass")

            assert result["access_token"] == "access_123"
            assert client._access_token == "access_123"
            assert client._refresh_token == "refresh_123"

            # Verify credentials were saved in profile format
            data = json.loads(mock_credentials_path.read_text())
            assert data["current_profile"] == "testuser"
            assert data["profiles"]["testuser"]["access_token"] == "access_123"


@pytest.mark.asyncio
async def test_chat_stream(mock_credentials_path: Path):
    """Test chat streaming method."""
    # This is a simplified test - full SSE testing would require more setup
    with patch("httpx.AsyncClient") as mock_client_class:
        mock_client = AsyncMock()
        mock_client_class.return_value = mock_client

        async with APIClient(credentials_path=mock_credentials_path) as client:
            client._access_token = "test_token"

            # Just verify the method exists and can be called
            # Full SSE testing would require mocking httpx_sse.aconnect_sse
            assert hasattr(client, "chat_stream")


@pytest.mark.asyncio
async def test_admin_methods(mock_credentials_path: Path):
    """Test admin API methods exist."""
    async with APIClient(credentials_path=mock_credentials_path) as client:
        # Verify all admin methods exist
        assert hasattr(client, "admin_init")
        assert hasattr(client, "admin_create_token")
        assert hasattr(client, "admin_list_tokens")
        assert hasattr(client, "admin_auth_audit_logs")
        assert hasattr(client, "admin_optimize_prompt")
        assert hasattr(client, "admin_feedback_stats")
        assert hasattr(client, "admin_feedback_export")


@pytest.mark.asyncio
async def test_refresh_failure_raises_session_expired(mock_credentials_path: Path):
    """Test that when refresh token is also expired, RuntimeError is raised."""
    with patch("httpx.AsyncClient") as mock_client_class:
        mock_client = AsyncMock()

        # First request returns 401
        mock_401 = MagicMock()
        mock_401.status_code = 401

        # Refresh also returns 401
        mock_refresh_401 = MagicMock()
        mock_refresh_401.status_code = 401
        mock_refresh_401.raise_for_status.side_effect = httpx.HTTPStatusError(
            "401", request=MagicMock(), response=mock_refresh_401
        )

        mock_client.request.return_value = mock_401
        mock_client.post.return_value = mock_refresh_401
        mock_client_class.return_value = mock_client

        async with APIClient(credentials_path=mock_credentials_path) as client:
            client._access_token = "expired_access"
            client._refresh_token = "expired_refresh"

            with pytest.raises(RuntimeError, match="Session expired"):
                await client._request("GET", "/test")

            # Tokens should be cleared
            assert client._access_token is None
            assert client._refresh_token is None


@pytest.mark.asyncio
async def test_401_without_refresh_token_returns_401(mock_credentials_path: Path):
    """Test that 401 without refresh token doesn't attempt refresh."""
    with patch("httpx.AsyncClient") as mock_client_class:
        mock_client = AsyncMock()

        mock_401 = MagicMock()
        mock_401.status_code = 401
        mock_401.json.return_value = {"detail": "Not authenticated"}
        mock_401.text = "Not authenticated"
        mock_401.reason_phrase = "Unauthorized"
        mock_401.request = MagicMock()

        mock_client.request.return_value = mock_401
        mock_client_class.return_value = mock_client

        async with APIClient(credentials_path=mock_credentials_path) as client:
            client._access_token = "expired_access"
            client._refresh_token = None  # No refresh token

            with pytest.raises(httpx.HTTPStatusError):
                await client._request("GET", "/test")

            # Should not have attempted refresh
            mock_client.post.assert_not_called()


@pytest.mark.asyncio
async def test_ensure_authenticated_no_token(mock_credentials_path: Path):
    """Test ensure_authenticated returns False when no token."""
    async with APIClient(credentials_path=mock_credentials_path) as client:
        result = await client.ensure_authenticated()
        assert result is False


@pytest.mark.asyncio
async def test_ensure_authenticated_valid_token(mock_credentials_path: Path):
    """Test ensure_authenticated returns True when token is valid."""
    with patch("httpx.AsyncClient") as mock_client_class:
        mock_client = AsyncMock()
        mock_response = MagicMock()
        mock_response.status_code = 200
        mock_response.json.return_value = {"user_id": "u1", "username": "test"}
        mock_client.request.return_value = mock_response
        mock_client_class.return_value = mock_client

        async with APIClient(credentials_path=mock_credentials_path) as client:
            client._access_token = "valid_token"
            result = await client.ensure_authenticated()
            assert result is True


@pytest.mark.asyncio
async def test_ensure_authenticated_expired_session(mock_credentials_path: Path):
    """Test ensure_authenticated returns 'session_expired' when both tokens expired."""
    with patch("httpx.AsyncClient") as mock_client_class:
        mock_client = AsyncMock()

        # get_current_user → _request → 401 → refresh fails → RuntimeError
        mock_401 = MagicMock()
        mock_401.status_code = 401
        mock_refresh_401 = MagicMock()
        mock_refresh_401.status_code = 401
        mock_refresh_401.raise_for_status.side_effect = httpx.HTTPStatusError(
            "401", request=MagicMock(), response=mock_refresh_401
        )

        mock_client.request.return_value = mock_401
        mock_client.post.return_value = mock_refresh_401
        mock_client_class.return_value = mock_client

        async with APIClient(credentials_path=mock_credentials_path) as client:
            client._access_token = "expired"
            client._refresh_token = "also_expired"
            result = await client.ensure_authenticated()
            assert result == "session_expired"


@pytest.mark.asyncio
async def test_request_without_client_raises(mock_credentials_path: Path):
    """Test _request raises when client not initialized."""
    client = APIClient(credentials_path=mock_credentials_path)
    # Don't use async with — _client stays None
    with pytest.raises(RuntimeError, match="Client not initialized"):
        await client._request("GET", "/test")


@pytest.mark.asyncio
async def test_refresh_no_refresh_token_raises(mock_credentials_path: Path):
    """Test _refresh_access_token raises when no refresh token."""
    with patch("httpx.AsyncClient") as mock_client_class:
        mock_client_class.return_value = AsyncMock()

        async with APIClient(credentials_path=mock_credentials_path) as client:
            client._refresh_token = None
            with pytest.raises(RuntimeError, match="No refresh token"):
                await client._refresh_access_token()


@pytest.mark.asyncio
async def test_save_credentials_creates_directory(tmp_path: Path):
    """Test _save_credentials creates parent directory if missing."""
    creds_path = tmp_path / "subdir" / "credentials.json"
    async with APIClient(credentials_path=creds_path) as client:
        client._access_token = "tok"
        client._refresh_token = "ref"
        await client._save_credentials(username="u1")

    assert creds_path.exists()
    data = json.loads(creds_path.read_text())
    assert data["profiles"]["u1"]["access_token"] == "tok"


@pytest.mark.asyncio
async def test_load_credentials_corrupt_file(tmp_path: Path):
    """Test _load_credentials handles corrupt file gracefully."""
    creds_path = tmp_path / "credentials.json"
    creds_path.write_text("not json{{{")

    async with APIClient(credentials_path=creds_path) as client:
        # Should not crash, tokens stay None
        assert client._access_token is None
        assert client._refresh_token is None


@pytest.mark.asyncio
async def test_request_non_401_error(mock_credentials_path: Path):
    """Test _request raises HTTPStatusError for non-401 errors."""
    with patch("httpx.AsyncClient") as mock_client_class:
        mock_client = AsyncMock()
        mock_500 = MagicMock()
        mock_500.status_code = 500
        mock_500.json.return_value = {"detail": "Internal error"}
        mock_500.text = "Internal error"
        mock_500.reason_phrase = "Internal Server Error"
        mock_500.request = MagicMock()
        mock_client.request.return_value = mock_500
        mock_client_class.return_value = mock_client

        async with APIClient(credentials_path=mock_credentials_path) as client:
            client._access_token = "tok"
            with pytest.raises(httpx.HTTPStatusError, match="500"):
                await client._request("GET", "/test")


# ============================================================================
# Task 2: Refresh failure clears file tokens
# ============================================================================

@pytest.mark.asyncio
async def test_refresh_failure_clears_file_tokens(tmp_path: Path):
    """When refresh fails, tokens in credentials file should be cleared."""
    creds_path = tmp_path / "credentials.json"
    # Pre-populate with tokens
    creds_path.write_text(json.dumps({
        "current_profile": "alice",
        "profiles": {"alice": {
            "username": "alice",
            "access_token": "old_access",
            "refresh_token": "old_refresh",
        }}
    }))

    with patch("httpx.AsyncClient") as mock_client_class:
        mock_client = AsyncMock()
        mock_401 = MagicMock()
        mock_401.status_code = 401
        mock_refresh_fail = MagicMock()
        mock_refresh_fail.status_code = 401
        mock_refresh_fail.raise_for_status.side_effect = httpx.HTTPStatusError(
            "401", request=MagicMock(), response=mock_refresh_fail
        )
        mock_client.request.return_value = mock_401
        mock_client.post.return_value = mock_refresh_fail
        mock_client_class.return_value = mock_client

        async with APIClient(credentials_path=creds_path) as client:
            with pytest.raises(RuntimeError, match="Session expired"):
                await client._request("GET", "/test")

    # File should have None tokens
    data = json.loads(creds_path.read_text())
    assert data["profiles"]["alice"]["access_token"] is None
    assert data["profiles"]["alice"]["refresh_token"] is None


# ============================================================================
# Task 4: Profile settings persistence
# ============================================================================

@pytest.mark.asyncio
async def test_save_and_load_default_model(tmp_path: Path):
    """default_model persists across save/load cycles."""
    creds_path = tmp_path / "credentials.json"
    async with APIClient(credentials_path=creds_path) as client:
        client._access_token = "tok"
        client._refresh_token = "ref"
        client._default_model = "gpt-4"
        await client._save_credentials(username="alice")

    async with APIClient(credentials_path=creds_path) as client2:
        assert client2._default_model == "gpt-4"


@pytest.mark.asyncio
async def test_save_and_load_last_session_id(tmp_path: Path):
    """last_session_id persists across save/load cycles."""
    creds_path = tmp_path / "credentials.json"
    async with APIClient(credentials_path=creds_path) as client:
        client._access_token = "tok"
        client._refresh_token = "ref"
        client._last_session_id = "sess_123"
        await client._save_credentials(username="bob")

    async with APIClient(credentials_path=creds_path) as client2:
        assert client2._last_session_id == "sess_123"


@pytest.mark.asyncio
async def test_save_profile_setting(tmp_path: Path):
    """save_profile_setting updates specific fields without clobbering others."""
    creds_path = tmp_path / "credentials.json"
    async with APIClient(credentials_path=creds_path) as client:
        client._access_token = "tok"
        client._refresh_token = "ref"
        await client._save_credentials(username="alice")
        await client.save_profile_setting(default_model="claude-3")

    data = json.loads(creds_path.read_text())
    assert data["profiles"]["alice"]["default_model"] == "claude-3"
    assert data["profiles"]["alice"]["access_token"] == "tok"


@pytest.mark.asyncio
async def test_logout_clears_tokens_keeps_settings(tmp_path: Path):
    """logout clears tokens but preserves default_model and other settings."""
    creds_path = tmp_path / "credentials.json"
    async with APIClient(credentials_path=creds_path) as client:
        client._access_token = "tok"
        client._refresh_token = "ref"
        client._default_model = "gpt-4"
        await client._save_credentials(username="alice")
        await client.logout()

    data = json.loads(creds_path.read_text())
    assert data["profiles"]["alice"]["access_token"] is None
    assert data["profiles"]["alice"]["refresh_token"] is None
    assert data["profiles"]["alice"]["default_model"] == "gpt-4"


@pytest.mark.asyncio
async def test_profile_name_consistency(tmp_path: Path):
    """save and load use the same profile key."""
    creds_path = tmp_path / "credentials.json"
    # Save with username "alice" (no explicit profile)
    async with APIClient(credentials_path=creds_path) as client:
        client._access_token = "tok"
        client._refresh_token = "ref"
        await client._save_credentials(username="alice")

    # Load without explicit profile — should find "alice" via current_profile
    async with APIClient(credentials_path=creds_path) as client2:
        assert client2._access_token == "tok"
        assert client2._current_username == "alice"
