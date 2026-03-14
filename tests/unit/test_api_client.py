"""Unit tests for API client."""

import json
from pathlib import Path
from unittest.mock import AsyncMock, MagicMock, patch

import httpx
import pytest

from cli.api_client import APIClient, AuthenticationError


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
        },
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

            with pytest.raises(AuthenticationError, match="Session expired"):
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
    creds_path.write_text(
        json.dumps(
            {
                "current_profile": "alice",
                "profiles": {
                    "alice": {
                        "username": "alice",
                        "access_token": "old_access",
                        "refresh_token": "old_refresh",
                    }
                },
            }
        )
    )

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
            with pytest.raises(AuthenticationError, match="Session expired"):
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


# ============================================================================
# SSE auth: _sse_stream auto-refresh and AuthenticationError
# ============================================================================


class FakeSSE:
    """Minimal SSE event object matching httpx_sse interface."""

    def __init__(self, data: str):
        self.data = data


class FakeEventSource:
    """Async context manager that yields FakeSSE events."""

    def __init__(self, events: list[dict]):
        self._events = events

    async def __aenter__(self):
        return self

    async def __aexit__(self, *args):
        pass

    async def aiter_sse(self):
        for e in self._events:
            yield FakeSSE(json.dumps(e))


@pytest.mark.asyncio
async def test_sse_stream_auth_error_no_refresh_raises(mock_credentials_path: Path):
    """AUTH_ERROR with no refresh token → AuthenticationError immediately."""
    with patch("httpx.AsyncClient") as mock_client_class:
        mock_client_class.return_value = AsyncMock()

        async with APIClient(credentials_path=mock_credentials_path) as client:
            client._access_token = "bad_token"
            client._refresh_token = None  # no refresh available

            with patch("cli.api_client.aconnect_sse") as mock_sse:
                mock_sse.return_value = FakeEventSource(
                    [
                        {
                            "type": "error",
                            "code": "AUTH_ERROR",
                            "message": "Could not validate credentials",
                        },
                    ]
                )

                with pytest.raises(AuthenticationError, match="Session expired"):
                    async for _ in client._sse_stream("POST", "/chat/turn", json={}):
                        pass

            # Tokens should be cleared
            assert client._access_token is None
            assert client._refresh_token is None


@pytest.mark.asyncio
async def test_sse_stream_auth_error_refresh_fails_raises(mock_credentials_path: Path):
    """AUTH_ERROR + refresh fails → AuthenticationError."""
    with patch("httpx.AsyncClient") as mock_client_class:
        mock_client = AsyncMock()
        # Refresh endpoint returns 401
        mock_refresh_resp = MagicMock()
        mock_refresh_resp.status_code = 401
        mock_refresh_resp.raise_for_status.side_effect = httpx.HTTPStatusError(
            "401",
            request=MagicMock(),
            response=mock_refresh_resp,
        )
        mock_client.post.return_value = mock_refresh_resp
        mock_client_class.return_value = mock_client

        async with APIClient(credentials_path=mock_credentials_path) as client:
            client._access_token = "bad_token"
            client._refresh_token = "also_bad_refresh"

            with patch("cli.api_client.aconnect_sse") as mock_sse:
                mock_sse.return_value = FakeEventSource(
                    [
                        {
                            "type": "error",
                            "code": "AUTH_ERROR",
                            "message": "Could not validate credentials",
                        },
                    ]
                )

                with pytest.raises(AuthenticationError, match="Session expired"):
                    async for _ in client._sse_stream("POST", "/chat/turn", json={}):
                        pass

            assert client._access_token is None
            assert client._refresh_token is None


@pytest.mark.asyncio
async def test_sse_stream_auth_error_refresh_succeeds(mock_credentials_path: Path):
    """AUTH_ERROR + refresh succeeds → retry with new token, yield events."""
    with patch("httpx.AsyncClient") as mock_client_class:
        mock_client = AsyncMock()
        # Refresh endpoint returns new token
        mock_refresh_resp = MagicMock()
        mock_refresh_resp.status_code = 200
        mock_refresh_resp.json.return_value = {"access_token": "new_token"}
        mock_refresh_resp.raise_for_status = MagicMock()
        mock_client.post.return_value = mock_refresh_resp
        mock_client_class.return_value = mock_client

        async with APIClient(credentials_path=mock_credentials_path) as client:
            client._access_token = "expired_token"
            client._refresh_token = "valid_refresh"

            call_count = 0

            def make_event_source(*args, **kwargs):
                nonlocal call_count
                call_count += 1
                if call_count == 1:
                    # First call: auth error
                    return FakeEventSource(
                        [
                            {"type": "error", "code": "AUTH_ERROR", "message": "expired"},
                        ]
                    )
                else:
                    # Second call (after refresh): success
                    return FakeEventSource(
                        [
                            {"type": "text_delta", "content": "Hello"},
                            {"type": "turn_complete", "has_tool_calls": False},
                        ]
                    )

            with patch("cli.api_client.aconnect_sse", side_effect=make_event_source):
                events = []
                async for event in client._sse_stream("POST", "/chat/turn", json={}):
                    events.append(event)

            assert client._access_token == "new_token"
            assert len(events) == 2
            assert events[0]["type"] == "text_delta"
            assert events[1]["type"] == "turn_complete"


@pytest.mark.asyncio
async def test_sse_stream_normal_events_pass_through(mock_credentials_path: Path):
    """Non-error SSE events are yielded transparently."""
    with patch("httpx.AsyncClient") as mock_client_class:
        mock_client_class.return_value = AsyncMock()

        async with APIClient(credentials_path=mock_credentials_path) as client:
            client._access_token = "good_token"

            with patch("cli.api_client.aconnect_sse") as mock_sse:
                mock_sse.return_value = FakeEventSource(
                    [
                        {"type": "session_info", "session_id": "s1"},
                        {"type": "text_delta", "content": "Hi"},
                        {"type": "turn_complete", "has_tool_calls": False},
                    ]
                )

                events = []
                async for event in client._sse_stream("POST", "/chat/turn", json={}):
                    events.append(event)

            assert len(events) == 3
            assert events[0]["type"] == "session_info"


@pytest.mark.asyncio
async def test_sse_stream_non_auth_error_passes_through(mock_credentials_path: Path):
    """Non-AUTH_ERROR error events are yielded, not intercepted."""
    with patch("httpx.AsyncClient") as mock_client_class:
        mock_client_class.return_value = AsyncMock()

        async with APIClient(credentials_path=mock_credentials_path) as client:
            client._access_token = "good_token"

            with patch("cli.api_client.aconnect_sse") as mock_sse:
                mock_sse.return_value = FakeEventSource(
                    [
                        {"type": "error", "code": "RATE_LIMIT", "message": "Too many requests"},
                    ]
                )

                events = []
                async for event in client._sse_stream("POST", "/chat/turn", json={}):
                    events.append(event)

            assert len(events) == 1
            assert events[0]["code"] == "RATE_LIMIT"


@pytest.mark.asyncio
async def test_sse_stream_clears_file_tokens_on_auth_failure(tmp_path: Path):
    """AuthenticationError also clears tokens from the credentials file."""
    creds_path = tmp_path / "credentials.json"
    creds_path.write_text(
        json.dumps(
            {
                "current_profile": "alice",
                "profiles": {
                    "alice": {
                        "username": "alice",
                        "access_token": "old_access",
                        "refresh_token": "old_refresh",
                    }
                },
            }
        )
    )

    async with APIClient(credentials_path=creds_path) as client:
        with patch("cli.api_client.aconnect_sse") as mock_sse:
            mock_sse.return_value = FakeEventSource(
                [
                    {"type": "error", "code": "AUTH_ERROR", "message": "bad"},
                ]
            )

            with pytest.raises(AuthenticationError):
                async for _ in client._sse_stream("POST", "/chat/turn", json={}):
                    pass

    data = json.loads(creds_path.read_text())
    assert data["profiles"]["alice"]["access_token"] is None
    assert data["profiles"]["alice"]["refresh_token"] is None


@pytest.mark.asyncio
async def test_authentication_error_is_runtime_error():
    """AuthenticationError is a subclass of RuntimeError for backward compat."""
    err = AuthenticationError("test")
    assert isinstance(err, RuntimeError)


@pytest.mark.asyncio
async def test_proactive_refresh_when_token_near_expiry(mock_credentials_path: Path):
    """Token is refreshed proactively when < 5 min remaining."""
    import time, jwt as pyjwt

    # Create a token expiring in 2 minutes
    near_expiry_token = pyjwt.encode(
        {"sub": "u1", "exp": time.time() + 120}, "x" * 32, algorithm="HS256"
    )

    with patch("httpx.AsyncClient") as mock_client_class:
        mock_client = AsyncMock()
        # refresh response
        mock_refresh_resp = MagicMock()
        mock_refresh_resp.status_code = 200
        mock_refresh_resp.raise_for_status = MagicMock()
        mock_refresh_resp.json.return_value = {
            "access_token": "new_access",
            "refresh_token": "new_refresh",
        }
        # normal request response
        mock_ok = MagicMock()
        mock_ok.status_code = 200
        mock_ok.json.return_value = {"ok": True}

        mock_client.post.return_value = mock_refresh_resp
        mock_client.request.return_value = mock_ok
        mock_client_class.return_value = mock_client

        async with APIClient(credentials_path=mock_credentials_path) as client:
            client._access_token = near_expiry_token
            client._refresh_token = "valid_refresh"
            await client._request("GET", "/test")
            # Should have called refresh endpoint
            mock_client.post.assert_called_once()
            assert client._access_token == "new_access"


@pytest.mark.asyncio
async def test_proactive_refresh_skipped_when_token_fresh(mock_credentials_path: Path):
    """No proactive refresh when token has plenty of time left."""
    import time, jwt as pyjwt

    fresh_token = pyjwt.encode(
        {"sub": "u1", "exp": time.time() + 3600}, "x" * 32, algorithm="HS256"
    )

    with patch("httpx.AsyncClient") as mock_client_class:
        mock_client = AsyncMock()
        mock_ok = MagicMock()
        mock_ok.status_code = 200
        mock_client.request.return_value = mock_ok
        mock_client_class.return_value = mock_client

        async with APIClient(credentials_path=mock_credentials_path) as client:
            client._access_token = fresh_token
            client._refresh_token = "valid_refresh"
            await client._request("GET", "/test")
            # Should NOT have called refresh
            mock_client.post.assert_not_called()
