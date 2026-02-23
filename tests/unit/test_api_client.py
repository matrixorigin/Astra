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
    # Write credentials
    credentials = {
        "access_token": "test_access",
        "refresh_token": "test_refresh",
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
        await client._save_credentials()

    # Verify file contents
    data = json.loads(mock_credentials_path.read_text())
    assert data["access_token"] == "new_access"
    assert data["refresh_token"] == "new_refresh"

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

            # Verify credentials were saved
            data = json.loads(mock_credentials_path.read_text())
            assert data["access_token"] == "access_123"


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
        assert hasattr(client, "admin_audit_logs")
        assert hasattr(client, "admin_optimize_prompt")
        assert hasattr(client, "admin_feedback_stats")
        assert hasattr(client, "admin_feedback_export")
