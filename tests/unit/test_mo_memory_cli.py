"""Unit tests for mo_memory_cli.

Tests template loading/validation, tool detection, config writers,
CLI commands, and argument parsing.

CLI commands use lazy imports (inside function bodies), so we mock
at the source module level, not at the cli module level.
"""

from __future__ import annotations

import argparse
import json
import os
import tempfile
from pathlib import Path
from unittest.mock import MagicMock, Mock, patch

import pytest

from cli.mo_memory_cli import (
    _detect_tools,
    _get_claude_rule,
    _get_cursor_rule,
    _get_db_factory,
    _get_kiro_steering,
    _load_template,
    _mcp_config,
    _write_claude,
    _write_cursor,
    _write_kiro,
    cmd_consolidate,
    cmd_governance,
    cmd_health,
    cmd_reflect,
    cmd_status,
    main,
)

# ── Template loading & validation ─────────────────────────────────────


class TestTemplateLoading:
    """Test template loading with validation."""

    def test_load_existing_template(self) -> None:
        """Real templates load without error."""
        content = _load_template("kiro_steering.md")
        assert "Memory Integration" in content
        assert "memory_retrieve" in content

    def test_load_nonexistent_template_raises(self) -> None:
        with pytest.raises(FileNotFoundError, match="Template not found"):
            _load_template("nonexistent.md")

    def test_load_empty_template_raises(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            empty = Path(tmpdir) / "empty.md"
            empty.write_text("")
            with patch("cli.mo_memory_cli._TEMPLATES_DIR", Path(tmpdir)), \
                 pytest.raises(ValueError, match="empty"):
                _load_template("empty.md")

    def test_load_template_missing_required_keyword_raises(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            bad = Path(tmpdir) / "bad.md"
            bad.write_text("# Some unrelated content\nNo required keywords here.")
            with patch("cli.mo_memory_cli._TEMPLATES_DIR", Path(tmpdir)), \
                 pytest.raises(ValueError, match="missing required section"):
                _load_template("bad.md")

    def test_all_templates_load_successfully(self) -> None:
        """All three shipped templates pass validation."""
        for fn in (_get_kiro_steering, _get_cursor_rule, _get_claude_rule):
            content = fn()
            assert len(content) > 100
            assert "memory_retrieve" in content


# ── MCP config generation ─────────────────────────────────────────────


class TestMCPConfig:
    def test_stdio_mode(self) -> None:
        cfg = _mcp_config("stdio")
        assert "command" in cfg
        assert cfg["args"] == ["-m", "mo_memory_mcp"]
        assert "env" not in cfg  # no env when no extras

    def test_remote_mode(self) -> None:
        assert _mcp_config("remote") == {"url": "http://localhost:8100/mcp"}

    def test_db_url_in_env(self) -> None:
        cfg = _mcp_config("stdio", db_url="mysql+pymysql://u:p@h:6001/db")
        assert cfg["env"]["MO_MEMORY_DB_URL"] == "mysql+pymysql://u:p@h:6001/db"

    def test_embedding_opts_in_env(self) -> None:
        cfg = _mcp_config("stdio", provider="openai", model="ada-002")
        assert cfg["env"]["MO_MEMORY_EMBEDDING_PROVIDER"] == "openai"
        assert cfg["env"]["MO_MEMORY_EMBEDDING_MODEL"] == "ada-002"


# ── Tool detection ────────────────────────────────────────────────────


class TestToolDetection:
    def test_no_tools(self) -> None:
        with tempfile.TemporaryDirectory() as d:
            assert _detect_tools(Path(d)) == {"kiro": False, "cursor": False, "claude": False}

    def test_kiro_detected(self) -> None:
        with tempfile.TemporaryDirectory() as d:
            (Path(d) / ".kiro").mkdir()
            assert _detect_tools(Path(d))["kiro"] is True

    def test_cursor_dir_detected(self) -> None:
        with tempfile.TemporaryDirectory() as d:
            (Path(d) / ".cursor").mkdir()
            assert _detect_tools(Path(d))["cursor"] is True

    def test_cursor_rc_detected(self) -> None:
        with tempfile.TemporaryDirectory() as d:
            (Path(d) / ".cursorrc").touch()
            assert _detect_tools(Path(d))["cursor"] is True

    def test_claude_md_detected(self) -> None:
        with tempfile.TemporaryDirectory() as d:
            (Path(d) / "CLAUDE.md").touch()
            assert _detect_tools(Path(d))["claude"] is True

    def test_claude_dir_detected(self) -> None:
        with tempfile.TemporaryDirectory() as d:
            (Path(d) / ".claude").mkdir()
            assert _detect_tools(Path(d))["claude"] is True


# ── Config writers ────────────────────────────────────────────────────


class TestWriteKiro:
    @patch("cli.mo_memory_cli._get_kiro_steering", return_value="# kiro rule")
    def test_creates_mcp_and_steering(self, _mock: Mock) -> None:
        with tempfile.TemporaryDirectory() as d:
            actions = _write_kiro(Path(d), "stdio")
            mcp = json.loads((Path(d) / ".kiro" / "settings" / "mcp.json").read_text())
            assert "mo-memory" in mcp["mcpServers"]
            assert (Path(d) / ".kiro" / "steering" / "memory.md").read_text() == "# kiro rule"
            assert len(actions) == 2

    @patch("cli.mo_memory_cli._get_kiro_steering", return_value="# kiro rule")
    def test_merges_existing_mcp_config(self, _mock: Mock) -> None:
        with tempfile.TemporaryDirectory() as d:
            mcp_dir = Path(d) / ".kiro" / "settings"
            mcp_dir.mkdir(parents=True)
            (mcp_dir / "mcp.json").write_text('{"mcpServers":{"other":{}}}')
            _write_kiro(Path(d), "stdio")
            mcp = json.loads((mcp_dir / "mcp.json").read_text())
            assert "other" in mcp["mcpServers"]
            assert "mo-memory" in mcp["mcpServers"]


class TestWriteCursor:
    @patch("cli.mo_memory_cli._get_cursor_rule", return_value="# cursor rule")
    def test_creates_mcp_and_rule(self, _mock: Mock) -> None:
        with tempfile.TemporaryDirectory() as d:
            actions = _write_cursor(Path(d), "stdio")
            assert (Path(d) / ".cursor" / "mcp.json").exists()
            assert (Path(d) / ".cursor" / "rules" / "memory.mdc").read_text() == "# cursor rule"
            assert len(actions) == 2


class TestWriteClaude:
    @patch("cli.mo_memory_cli._get_claude_rule", return_value="\n# claude rule")
    def test_creates_new_claude_md(self, _mock: Mock) -> None:
        with tempfile.TemporaryDirectory() as d:
            _write_claude(Path(d), "stdio")
            assert (Path(d) / "CLAUDE.md").read_text() == "# claude rule"

    @patch("cli.mo_memory_cli._get_claude_rule", return_value="\n# claude rule")
    def test_appends_to_existing_claude_md(self, _mock: Mock) -> None:
        with tempfile.TemporaryDirectory() as d:
            (Path(d) / "CLAUDE.md").write_text("# Existing")
            _write_claude(Path(d), "stdio")
            content = (Path(d) / "CLAUDE.md").read_text()
            assert "# Existing" in content
            assert "# claude rule" in content

    @patch("cli.mo_memory_cli._get_claude_rule", return_value="\n# claude rule")
    def test_skips_if_already_configured(self, _mock: Mock) -> None:
        with tempfile.TemporaryDirectory() as d:
            (Path(d) / "CLAUDE.md").write_text("# Has mo-memory already")
            actions = _write_claude(Path(d), "stdio")
            assert any("already configured" in a for a in actions)


# ── _get_db_factory ───────────────────────────────────────────────────


class TestGetDbFactory:
    def test_with_db_url_arg(self) -> None:
        """--db-url creates engine + sessionmaker."""
        args = argparse.Namespace(db_url="mysql+pymysql://u:p@h:6001/db")
        mock_engine = MagicMock()
        mock_sm = MagicMock(return_value=MagicMock())

        with patch("sqlalchemy.create_engine", return_value=mock_engine) as mock_ce, \
             patch("sqlalchemy.orm.sessionmaker", mock_sm):
            _get_db_factory(args)

        mock_ce.assert_called_once_with("mysql+pymysql://u:p@h:6001/db", pool_pre_ping=True)
        mock_sm.assert_called_once_with(bind=mock_engine)

    def test_with_env_var(self) -> None:
        """MO_MEMORY_DB_URL env var is used when --db-url is absent."""
        args = argparse.Namespace(db_url=None)
        mock_engine = MagicMock()
        mock_sm = MagicMock(return_value=MagicMock())

        with patch.dict("os.environ", {"MO_MEMORY_DB_URL": "mysql://env"}), \
             patch("sqlalchemy.create_engine", return_value=mock_engine) as mock_ce, \
             patch("sqlalchemy.orm.sessionmaker", mock_sm):
            _get_db_factory(args)

        mock_ce.assert_called_once_with("mysql://env", pool_pre_ping=True)
        mock_sm.assert_called_once_with(bind=mock_engine)

    def test_fallback_to_session_local(self) -> None:
        """Falls back to api.database.SessionLocal when no URL given."""
        args = argparse.Namespace(db_url=None)
        mock_sl = MagicMock()
        # Remove env var if present, then patch api.database module
        with patch.dict("os.environ", {}, clear=False):
            os.environ.pop("MO_MEMORY_DB_URL", None)
            with patch.dict("sys.modules", {"api.database": MagicMock(SessionLocal=mock_sl)}):
                result = _get_db_factory(args)
        assert result is mock_sl

    def test_exits_when_no_db_available(self) -> None:
        """sys.exit(1) when no DB URL and api.database import fails."""
        import sys as _sys

        args = argparse.Namespace(db_url=None)
        saved = _sys.modules.get("api.database")
        try:
            # Force ImportError by making the module None
            _sys.modules["api.database"] = None  # type: ignore[assignment]
            with patch.dict("os.environ", {}, clear=False), \
                 pytest.raises(SystemExit, match="1"):
                os.environ.pop("MO_MEMORY_DB_URL", None)
                _get_db_factory(args)
        finally:
            if saved is not None:
                _sys.modules["api.database"] = saved
            else:
                _sys.modules.pop("api.database", None)


# ── CLI commands ──────────────────────────────────────────────────────


class TestCmdGovernance:
    def test_runs_governance_cycle(self) -> None:
        mock_result = MagicMock(
            quarantined=5, cleaned_stale=3, scenes_created=2,
            vector_index_health={"mem_memories": {"ratio": 0.8}},
            errors=[],
        )
        mock_scheduler = MagicMock()
        mock_scheduler.run_cycle.return_value = mock_result

        args = argparse.Namespace(user_id="alice", db_url=None)
        with patch("cli.mo_memory_cli._get_db_factory", return_value=MagicMock()), \
             patch("core.memory.tabular.governance.GovernanceScheduler", return_value=mock_scheduler), \
             patch("builtins.print"):
            cmd_governance(args)

        mock_scheduler.run_cycle.assert_called_once_with("alice")

    def test_default_user_id_is_all(self) -> None:
        mock_scheduler = MagicMock()
        mock_scheduler.run_cycle.return_value = MagicMock(
            quarantined=0, cleaned_stale=0, scenes_created=0,
            vector_index_health={}, errors=[],
        )
        args = argparse.Namespace(user_id=None, db_url=None)
        with patch("cli.mo_memory_cli._get_db_factory", return_value=MagicMock()), \
             patch("core.memory.tabular.governance.GovernanceScheduler", return_value=mock_scheduler), \
             patch("builtins.print"):
            cmd_governance(args)

        mock_scheduler.run_cycle.assert_called_once_with("all")

    def test_prints_errors(self, capsys: pytest.CaptureFixture[str]) -> None:
        mock_scheduler = MagicMock()
        mock_scheduler.run_cycle.return_value = MagicMock(
            quarantined=0, cleaned_stale=0, scenes_created=0,
            vector_index_health={}, errors=["something broke"],
        )
        args = argparse.Namespace(user_id="alice", db_url=None)
        with patch("cli.mo_memory_cli._get_db_factory", return_value=MagicMock()), \
             patch("core.memory.tabular.governance.GovernanceScheduler", return_value=mock_scheduler):
            cmd_governance(args)

        captured = capsys.readouterr()
        assert "❌ something broke" in captured.out


class TestCmdConsolidate:
    def test_runs_consolidation(self) -> None:
        mock_result = MagicMock(
            merged_nodes=3, conflicts_detected=1, orphaned_scenes=2,
            promoted=1, demoted=0, errors=[],
        )
        mock_gc = MagicMock()
        mock_gc.consolidate.return_value = mock_result

        args = argparse.Namespace(user_id="alice", db_url=None)
        with patch("cli.mo_memory_cli._get_db_factory", return_value=MagicMock()), \
             patch("core.memory.graph.consolidation.GraphConsolidator", return_value=mock_gc), \
             patch("builtins.print"):
            cmd_consolidate(args)

        mock_gc.consolidate.assert_called_once_with("alice")


class TestCmdReflect:
    def test_runs_reflection(self) -> None:
        mock_result = MagicMock(
            candidates_found=5, scenes_created=3, llm_calls=2, errors=[],
        )
        mock_engine = MagicMock()
        mock_engine.reflect.return_value = mock_result

        args = argparse.Namespace(user_id="alice", db_url=None)
        with patch("cli.mo_memory_cli._get_db_factory", return_value=MagicMock()), \
             patch("core.memory.graph.candidates.GraphCandidateProvider"), \
             patch("core.memory.graph.service.GraphMemoryService"), \
             patch("core.memory.reflection.engine.ReflectionEngine", return_value=mock_engine), \
             patch("core.llm.client.LLMClient"), \
             patch("builtins.print"):
            cmd_reflect(args)

        mock_engine.reflect.assert_called_once_with("alice")

    def test_exits_when_llm_unavailable(self) -> None:
        args = argparse.Namespace(user_id="alice", db_url=None)
        with patch("cli.mo_memory_cli._get_db_factory", return_value=MagicMock()), \
             patch("core.llm.client.LLMClient", side_effect=ImportError("no llm")), \
             pytest.raises(SystemExit, match="1"):
            cmd_reflect(args)


# ── cmd_status ────────────────────────────────────────────────────────


class TestCmdStatus:
    def test_shows_configured_and_not_detected(self, capsys: pytest.CaptureFixture[str]) -> None:
        with tempfile.TemporaryDirectory() as d:
            # Set up kiro as configured
            mcp_dir = Path(d) / ".kiro" / "settings"
            mcp_dir.mkdir(parents=True)
            (mcp_dir / "mcp.json").write_text('{"mcpServers":{"mo-memory":{}}}')

            args = argparse.Namespace(dir=d)
            with patch("cli.mo_memory_cli._detect_tools", return_value={
                "kiro": True, "cursor": False, "claude": False,
            }):
                cmd_status(args)

            out = capsys.readouterr().out
            assert "kiro: ✅ configured" in out
            assert "cursor: — not detected" in out


# ── cmd_health ────────────────────────────────────────────────────────


class TestCmdHealth:
    def test_success(self, capsys: pytest.CaptureFixture[str]) -> None:
        mock_resp = MagicMock()
        mock_resp.read.return_value = json.dumps({"status": "healthy", "database": "ok"}).encode()
        mock_resp.__enter__ = lambda s: s
        mock_resp.__exit__ = MagicMock(return_value=False)

        args = argparse.Namespace(api_url="http://localhost:8100")
        with patch("urllib.request.urlopen", return_value=mock_resp):
            cmd_health(args)

        out = capsys.readouterr().out
        assert "Memory service: healthy" in out

    def test_failure(self, capsys: pytest.CaptureFixture[str]) -> None:
        args = argparse.Namespace(api_url="http://localhost:8100")
        with patch("urllib.request.urlopen", side_effect=Exception("refused")):
            cmd_health(args)

        out = capsys.readouterr().out
        assert "❌ Cannot reach memory service" in out


# ── Argument parsing via main() ───────────────────────────────────────


class TestMain:
    def test_governance_args(self) -> None:
        with patch("sys.argv", ["mo-memory", "governance", "--user-id", "bob"]), \
             patch("cli.mo_memory_cli.cmd_governance") as mock_cmd:
            main()
        args = mock_cmd.call_args[0][0]
        assert args.user_id == "bob"

    def test_consolidate_requires_user_id(self) -> None:
        with patch("sys.argv", ["mo-memory", "consolidate"]), \
             pytest.raises(SystemExit):
            main()

    def test_reflect_requires_user_id(self) -> None:
        with patch("sys.argv", ["mo-memory", "reflect"]), \
             pytest.raises(SystemExit):
            main()

    def test_no_command_shows_help(self) -> None:
        with patch("sys.argv", ["mo-memory"]), \
             patch("argparse.ArgumentParser.print_help") as mock_help:
            main()
        mock_help.assert_called_once()

    def test_status_command(self) -> None:
        with patch("sys.argv", ["mo-memory", "status"]), \
             patch("cli.mo_memory_cli.cmd_status") as mock_cmd:
            main()
        mock_cmd.assert_called_once()

    def test_health_command(self) -> None:
        with patch("sys.argv", ["mo-memory", "health", "--api-url", "http://x:9000"]), \
             patch("cli.mo_memory_cli.cmd_health") as mock_cmd:
            main()
        assert mock_cmd.call_args[0][0].api_url == "http://x:9000"
