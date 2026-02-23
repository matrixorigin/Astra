"""API client for mo-agent CLI.

Provides typed methods for all API endpoints, handles authentication,
and manages JWT token lifecycle.
"""

import json
import os
from collections.abc import AsyncIterator
from pathlib import Path
from typing import Any

import httpx
from httpx_sse import aconnect_sse


class APIClient:
    """Client for mo-agent API server."""

    def __init__(
        self,
        base_url: str | None = None,
        credentials_path: Path | None = None,
    ):
        """Initialize API client.

        Args:
            base_url: API server URL. Defaults to MO_AGENT_API_URL env var or http://localhost:8000
            credentials_path: Path to credentials file. Defaults to ~/.mo-agent/credentials.json
        """
        self.base_url = (
            base_url or os.getenv("MO_AGENT_API_URL", "http://localhost:8000")
        ).rstrip("/")
        self.credentials_path = credentials_path or Path.home() / ".mo-agent" / "credentials.json"
        self._client: httpx.AsyncClient | None = None
        self._access_token: str | None = None
        self._refresh_token: str | None = None

    async def __aenter__(self) -> "APIClient":
        """Async context manager entry."""
        self._client = httpx.AsyncClient(timeout=30.0)
        await self._load_credentials()
        return self

    async def __aexit__(self, *args: Any) -> None:
        """Async context manager exit."""
        if self._client:
            await self._client.aclose()

    async def _load_credentials(self) -> None:
        """Load JWT tokens from credentials file."""
        if not self.credentials_path.exists():
            return
        try:
            data = json.loads(self.credentials_path.read_text())
            self._access_token = data.get("access_token")
            self._refresh_token = data.get("refresh_token")
        except Exception:
            pass

    async def _save_credentials(self) -> None:
        """Save JWT tokens to credentials file."""
        self.credentials_path.parent.mkdir(parents=True, exist_ok=True)
        data = {
            "access_token": self._access_token,
            "refresh_token": self._refresh_token,
        }
        self.credentials_path.write_text(json.dumps(data, indent=2))
        self.credentials_path.chmod(0o600)

    async def _request(
        self,
        method: str,
        path: str,
        **kwargs: Any,
    ) -> httpx.Response:
        """Make HTTP request with auto-refresh on 401.

        Args:
            method: HTTP method
            path: API path (without base_url)
            **kwargs: Additional arguments for httpx.request

        Returns:
            HTTP response

        Raises:
            httpx.HTTPStatusError: On non-2xx status
        """
        if not self._client:
            raise RuntimeError("Client not initialized. Use async context manager.")

        headers = kwargs.pop("headers", {})
        if self._access_token:
            headers["Authorization"] = f"Bearer {self._access_token}"

        url = f"{self.base_url}{path}"
        response = await self._client.request(method, url, headers=headers, **kwargs)

        # Auto-refresh on 401
        if response.status_code == 401 and self._refresh_token:
            await self._refresh_access_token()
            headers["Authorization"] = f"Bearer {self._access_token}"
            response = await self._client.request(method, url, headers=headers, **kwargs)

        response.raise_for_status()
        return response

    async def _refresh_access_token(self) -> None:
        """Refresh access token using refresh token."""
        if not self._client or not self._refresh_token:
            raise RuntimeError("No refresh token available")

        response = await self._client.post(
            f"{self.base_url}/auth/refresh",
            json={"refresh_token": self._refresh_token},
        )
        response.raise_for_status()
        data = response.json()
        self._access_token = data["access_token"]
        # Update refresh token if server returns a new one
        if "refresh_token" in data:
            self._refresh_token = data["refresh_token"]
        await self._save_credentials()

    # ============================================================================
    # Authentication
    # ============================================================================

    async def ensure_authenticated(self) -> bool:
        """Check if user is authenticated.
        
        Returns:
            True if authenticated, False otherwise
        """
        if not self._access_token:
            print(f"[DEBUG] ensure_authenticated: No access token")
            return False
        try:
            user = await self.get_current_user()
            print(f"[DEBUG] ensure_authenticated: Success, user={user.get('username')}")
            return True
        except Exception as e:
            print(f"[DEBUG] ensure_authenticated: Failed with {type(e).__name__}: {e}")
            import traceback
            traceback.print_exc()
            return False

    async def register(self, username: str, password: str, email: str) -> dict[str, Any]:
        """Register new user."""
        response = await self._request(
            "POST",
            "/auth/register",
            json={"username": username, "password": password, "email": email},
        )
        return response.json()

    async def login(self, username: str, password: str) -> dict[str, Any]:
        """Login and get JWT tokens."""
        response = await self._request(
            "POST",
            "/auth/login",
            json={"username": username, "password": password},
        )
        data = response.json()
        self._access_token = data["access_token"]
        self._refresh_token = data["refresh_token"]
        await self._save_credentials()
        return data

    async def get_current_user(self) -> dict[str, Any]:
        """Get current user info."""
        response = await self._request("GET", "/auth/me")
        return response.json()

    # ============================================================================
    # Chat
    # ============================================================================

    async def chat(
        self,
        message: str,
        session_id: str | None = None,
        agent_id: str | None = None,
    ) -> dict[str, Any]:
        """Send chat message and get response."""
        response = await self._request(
            "POST",
            "/chat",
            json={
                "message": message,
                "session_id": session_id,
                "agent_id": agent_id,
            },
        )
        return response.json()

    async def chat_stream(
        self,
        message: str,
        session_id: str | None = None,
        agent_id: str | None = None,
    ) -> AsyncIterator[dict[str, Any]]:
        """Stream chat response as SSE."""
        if not self._client:
            raise RuntimeError("Client not initialized")

        headers = {}
        if self._access_token:
            headers["Authorization"] = f"Bearer {self._access_token}"

        url = f"{self.base_url}/chat/stream"
        async with aconnect_sse(
            self._client,
            "POST",
            url,
            json={
                "message": message,
                "session_id": session_id,
                "agent_id": agent_id,
            },
            headers=headers,
        ) as event_source:
            async for sse in event_source.aiter_sse():
                yield json.loads(sse.data)

    async def get_run_status(self, run_id: str) -> dict[str, Any]:
        """Get run status and progress."""
        response = await self._request("GET", f"/chat/runs/{run_id}")
        return response.json()

    async def stream_run_events(self, run_id: str) -> AsyncIterator[dict[str, Any]]:
        """Stream run events (supports reconnection)."""
        if not self._client:
            raise RuntimeError("Client not initialized")

        headers = {}
        if self._access_token:
            headers["Authorization"] = f"Bearer {self._access_token}"

        url = f"{self.base_url}/chat/runs/{run_id}/stream"
        async with aconnect_sse(self._client, "GET", url, headers=headers) as event_source:
            async for sse in event_source.aiter_sse():
                yield json.loads(sse.data)

    async def cancel_run(self, run_id: str) -> dict[str, Any]:
        """Cancel a running task."""
        response = await self._request("DELETE", f"/chat/runs/{run_id}")
        return response.json()

    # ============================================================================
    # Sessions
    # ============================================================================

    async def create_session(
        self,
        agent_id: str,
        metadata: dict[str, Any] | None = None,
    ) -> dict[str, Any]:
        """Create new session."""
        response = await self._request(
            "POST",
            "/sessions",
            json={"agent_id": agent_id, "metadata": metadata or {}},
        )
        return response.json()

    async def list_sessions(
        self,
        agent_id: str | None = None,
        status: str | None = None,
        limit: int = 50,
    ) -> list[dict[str, Any]]:
        """List sessions."""
        params = {"limit": limit}
        if agent_id:
            params["agent_id"] = agent_id
        if status:
            params["status"] = status
        response = await self._request("GET", "/sessions", params=params)
        return response.json()

    async def get_session(self, session_id: str) -> dict[str, Any]:
        """Get session details."""
        response = await self._request("GET", f"/sessions/{session_id}")
        return response.json()

    async def update_session(
        self,
        session_id: str,
        metadata: dict[str, Any] | None = None,
    ) -> dict[str, Any]:
        """Update session metadata."""
        response = await self._request(
            "PUT",
            f"/sessions/{session_id}",
            json={"metadata": metadata or {}},
        )
        return response.json()

    async def close_session(self, session_id: str) -> dict[str, Any]:
        """Close session."""
        response = await self._request("POST", f"/sessions/{session_id}/close")
        return response.json()

    async def delete_session(self, session_id: str) -> dict[str, Any]:
        """Delete session."""
        response = await self._request("DELETE", f"/sessions/{session_id}")
        return response.json()

    # ============================================================================
    # Skills
    # ============================================================================

    async def register_skill(self, skill_data: dict[str, Any]) -> dict[str, Any]:
        """Register new skill."""
        response = await self._request("POST", "/skills", json=skill_data)
        return response.json()

    async def list_skills(
        self,
        category: str | None = None,
        limit: int = 100,
    ) -> list[dict[str, Any]]:
        """List skills."""
        params = {"limit": limit}
        if category:
            params["category"] = category
        response = await self._request("GET", "/skills", params=params)
        return response.json()

    async def get_skill(self, skill_id: str) -> dict[str, Any]:
        """Get skill details."""
        response = await self._request("GET", f"/skills/{skill_id}")
        return response.json()

    async def get_skill_versions(self, skill_id: str) -> list[dict[str, Any]]:
        """List skill versions."""
        response = await self._request("GET", f"/skills/{skill_id}/versions")
        return response.json()

    # ============================================================================
    # Replay
    # ============================================================================

    async def replay_session(
        self,
        session_id: str,
        sandbox_name: str | None = None,
    ) -> dict[str, Any]:
        """Replay session."""
        response = await self._request(
            "POST",
            f"/sessions/{session_id}/replay",
            json={"sandbox_name": sandbox_name} if sandbox_name else {},
        )
        return response.json()

    async def compare_replay(self, session_id: str) -> dict[str, Any]:
        """Compare replay results."""
        response = await self._request("GET", f"/sessions/{session_id}/replay/compare")
        return response.json()

    # ============================================================================
    # Events
    # ============================================================================

    async def list_events(
        self,
        session_id: str | None = None,
        event_type: str | None = None,
        limit: int = 100,
    ) -> list[dict[str, Any]]:
        """List events."""
        params = {"limit": limit}
        if session_id:
            params["session_id"] = session_id
        if event_type:
            params["event_type"] = event_type
        response = await self._request("GET", "/events", params=params)
        return response.json()

    async def get_event(self, event_id: str) -> dict[str, Any]:
        """Get event details."""
        response = await self._request("GET", f"/events/{event_id}")
        return response.json()

    # ============================================================================
    # Agents
    # ============================================================================

    async def create_agent(self, agent_data: dict[str, Any]) -> dict[str, Any]:
        """Create new agent."""
        response = await self._request("POST", "/agents", json=agent_data)
        return response.json()

    async def list_agents(self, limit: int = 50) -> list[dict[str, Any]]:
        """List agents."""
        response = await self._request("GET", "/agents", params={"limit": limit})
        return response.json()

    async def get_agent(self, agent_id: str) -> dict[str, Any]:
        """Get agent details."""
        response = await self._request("GET", f"/agents/{agent_id}")
        return response.json()

    async def update_agent(
        self,
        agent_id: str,
        agent_data: dict[str, Any],
    ) -> dict[str, Any]:
        """Update agent."""
        response = await self._request("PUT", f"/agents/{agent_id}", json=agent_data)
        return response.json()

    async def delete_agent(self, agent_id: str) -> dict[str, Any]:
        """Delete agent."""
        response = await self._request("DELETE", f"/agents/{agent_id}")
        return response.json()

    # ============================================================================
    # Admin
    # ============================================================================

    async def admin_init(self) -> dict[str, Any]:
        """Initialize database (run DDL migrations)."""
        response = await self._request("POST", "/admin/init")
        return response.json()

    async def admin_create_token(
        self,
        token_type: str,
        provider: str | None = None,
        scope: str = "global",
        scope_id: str | None = None,
        token_value: str | None = None,
    ) -> dict[str, Any]:
        """Create API/LLM token."""
        response = await self._request(
            "POST",
            "/admin/tokens",
            json={
                "token_type": token_type,
                "provider": provider,
                "scope": scope,
                "scope_id": scope_id,
                "token_value": token_value,
            },
        )
        return response.json()

    async def admin_list_tokens(
        self,
        token_type: str | None = None,
        scope: str | None = None,
    ) -> list[dict[str, Any]]:
        """List tokens."""
        params = {}
        if token_type:
            params["token_type"] = token_type
        if scope:
            params["scope"] = scope
        response = await self._request("GET", "/admin/tokens", params=params)
        return response.json()

    async def admin_audit_logs(
        self,
        user_id: str | None = None,
        since: str | None = None,
        limit: int = 100,
    ) -> list[dict[str, Any]]:
        """Query audit logs."""
        params = {"limit": limit}
        if user_id:
            params["user_id"] = user_id
        if since:
            params["since"] = since
        response = await self._request("GET", "/admin/audit", params=params)
        return response.json()

    async def admin_optimize_prompt(
        self,
        agent_id: str,
        optimization_type: str = "compression",
    ) -> dict[str, Any]:
        """Trigger prompt optimization."""
        response = await self._request(
            "POST",
            "/admin/prompts/optimize",
            json={"agent_id": agent_id, "optimization_type": optimization_type},
        )
        return response.json()

    async def admin_feedback_stats(
        self,
        agent_id: str | None = None,
        since: str | None = None,
    ) -> dict[str, Any]:
        """Get feedback statistics."""
        params = {}
        if agent_id:
            params["agent_id"] = agent_id
        if since:
            params["since"] = since
        response = await self._request("GET", "/admin/feedback/stats", params=params)
        return response.json()

    async def admin_feedback_export(
        self,
        agent_id: str | None = None,
        format: str = "jsonl",
    ) -> dict[str, Any]:
        """Export training data."""
        response = await self._request(
            "POST",
            "/admin/feedback/export",
            json={"agent_id": agent_id, "format": format},
        )
        return response.json()
