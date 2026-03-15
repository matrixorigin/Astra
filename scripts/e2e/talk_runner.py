"""Talk session runner — drives mo-agent chat -m via real CLI subprocess."""

from __future__ import annotations

import json
import logging
import os
import subprocess
import sys
import uuid
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from sqlalchemy import text

logger = logging.getLogger(__name__)

PROJECT_ROOT = Path(__file__).parent.parent.parent
CLI_SCRIPT = PROJECT_ROOT / "cli" / "mo_agent_api.py"
PYTHON = sys.executable


@dataclass
class TurnRecord:
    """Result of one conversation turn."""

    user_message: str
    response: str = ""
    tool_calls: list[dict] = field(default_factory=list)
    error: str | None = None


class TalkSession:
    """Drives real CLI (mo-agent chat -m) for E2E verification.

    Each say() call is a subprocess: same as a user running
        mo-agent chat -m "message" --session-id xxx
    """

    def __init__(
        self,
        api_url: str,
        db_factory: Any,
        *,
        model: str | None = None,
        profile: str | None = None,
    ):
        self.api_url = api_url
        self.db_factory = db_factory
        self.model = model
        self.profile = profile or f"verify_{uuid.uuid4().hex[:8]}"
        self.session_id: str | None = None
        self.username: str = f"__verify_{uuid.uuid4().hex[:8]}"
        self.password: str = "verify_pass_123!"
        self.user_uuid: str | None = None  # JWT UUID, set after login
        self.turns: list[TurnRecord] = []

    def setup(self) -> None:
        """Register and login via CLI."""
        # Register
        self._cli(
            "register",
            "--email",
            f"{self.username}@example.com",
            "--password",
            self.password,
            "--username",
            self.username,
            check=False,
        )  # may already exist

        # Login
        self._cli("login", "--username", self.username, "--password", self.password)

        # Fetch JWT UUID so DB checks use the same user_id as memory writes
        try:
            with self.db_factory() as db:
                row = db.execute(
                    text("SELECT user_id FROM auth_users WHERE username = :u"),
                    {"u": self.username},
                ).fetchone()
                self.user_uuid = row[0] if row else self.username
        except Exception:
            self.user_uuid = self.username

    def say(self, message: str) -> TurnRecord:
        """Send a message via mo-agent chat -m, return result."""
        record = TurnRecord(user_message=message)

        # Count events before this turn
        events_before = self._count_events() if self.session_id else 0
        time_before = self._snapshot_time() if self.session_id else None

        args = ["chat", "-m", message, "--auto-approve", "--user-id", self.username]
        if self.session_id:
            args.extend(["--session-id", self.session_id])
        if self.model:
            args.extend(["--model", self.model])

        try:
            result = self._cli(*args)
            record.response = result.stdout.strip()

            # Capture session_id from profile if first turn
            if not self.session_id:
                self.session_id = self._read_session_id()

            # Get tool calls from DB (ground truth, this turn only)
            if self.session_id:
                record.tool_calls = self._get_tool_calls(events_before, since=time_before)

        except Exception as e:
            record.error = str(e)

        self.turns.append(record)
        return record

    def _cli(self, *args: str, check: bool = True) -> subprocess.CompletedProcess:
        """Run mo-agent CLI command."""
        cmd = [PYTHON, str(CLI_SCRIPT), "--api-url", self.api_url, "--profile", self.profile, *args]

        env = os.environ.copy()
        # Remove all proxy settings to avoid connection issues
        for proxy_var in ["http_proxy", "https_proxy", "HTTP_PROXY", "HTTPS_PROXY", "all_proxy", "ALL_PROXY"]:
            env.pop(proxy_var, None)
        env["HF_HUB_OFFLINE"] = "1"
        env["TRANSFORMERS_OFFLINE"] = "1"
        env["NO_PROXY"] = "localhost,127.0.0.1,::1"
        result = subprocess.run(
            cmd,
            capture_output=True,
            text=True,
            env=env,
            cwd=PROJECT_ROOT,
            timeout=180,
        )

        if check and result.returncode != 0:
            raise RuntimeError(f"CLI failed: {result.stderr.strip() or result.stdout.strip()}")
        return result

    def _read_session_id(self) -> str | None:
        """Read last_session_id from CLI profile."""
        try:
            from cli.api_client import APIClient

            profile = APIClient.load_profile(profile=self.profile)
            return profile.get("last_session_id")
        except Exception:
            return None

    def _snapshot_time(self) -> str | None:
        """Return current max created_at for this session (used to filter new events)."""
        if not self.session_id:
            return None
        with self.db_factory() as db:
            return db.execute(
                text("SELECT MAX(created_at) FROM agent_events WHERE session_id = :sid"),
                {"sid": self.session_id},
            ).scalar()

    def _count_events(self) -> int:
        if not self.session_id:
            return 0
        with self.db_factory() as db:
            return (
                db.execute(
                    text("SELECT COUNT(*) FROM agent_events WHERE session_id = :sid"),
                    {"sid": self.session_id},
                ).scalar()
                or 0
            )

    def _get_tool_calls(self, after_count: int, since: str | None = None) -> list[dict]:
        """Get tool calls from agent_events for this turn only."""
        if not self.session_id:
            return []
        with self.db_factory() as db:
            if since:
                rows = db.execute(
                    text(
                        "SELECT content, metadata FROM agent_events "
                        "WHERE session_id = :sid AND event_type = 'tool_call' "
                        "AND created_at > :since ORDER BY created_at"
                    ),
                    {"sid": self.session_id, "since": since},
                ).fetchall()
            else:
                rows = db.execute(
                    text(
                        "SELECT content, metadata FROM agent_events "
                        "WHERE session_id = :sid AND event_type = 'tool_call' "
                        "ORDER BY created_at"
                    ),
                    {"sid": self.session_id},
                ).fetchall()
            results = []
            for r in rows:
                meta = r.metadata if isinstance(r.metadata, dict) else {}
                if isinstance(r.metadata, str):
                    try:
                        meta = json.loads(r.metadata)
                    except Exception:
                        meta = {}
                results.append(
                    {"name": meta.get("name", meta.get("tool_name", "")), "args": r.content}
                )
            return results

    def new_session(self) -> None:
        """Start a fresh session (same user, new session_id)."""
        self.session_id = None

    def cleanup(self) -> None:
        """Clean up credentials file."""
        cred_path = Path.home() / ".mo-agent" / "credentials.json"
        if cred_path.exists():
            try:
                data = json.loads(cred_path.read_text())
                profiles = data.get("profiles", {})
                if self.profile in profiles:
                    del profiles[self.profile]
                    cred_path.write_text(json.dumps(data, indent=2))
            except Exception:
                pass
