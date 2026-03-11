"""Unit tests for mo_memory_cli.

Tests template loading/validation, tool detection, config writers,
CLI commands, and argument parsing.

CLI commands use lazy imports (inside function bodies), so we mock
at the source module level, not at the cli module level.
"""

from __future__ import annotations

import argparse
import json
import logging
import os
import tempfile
from pathlib import Path
from typing import Any
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
    _resolve_engine,
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
        # Default embedding config: local + all-MiniLM-L6-v2 + 384
        assert cfg["env"]["MEMORIA_DB_URL"] == ""
        assert cfg["env"]["EMBEDDING_PROVIDER"] == "local"
        assert cfg["env"]["EMBEDDING_MODEL"] == "all-MiniLM-L6-v2"
        assert cfg["env"]["EMBEDDING_DIM"] == "384"
        assert cfg["env"]["EMBEDDING_API_KEY"] == ""
        assert cfg["env"]["EMBEDDING_BASE_URL"] == ""

    def test_remote_mode(self) -> None:
        assert _mcp_config("remote") == {"url": "http://localhost:8100/mcp"}

    def test_db_url_in_env(self) -> None:
        cfg = _mcp_config("stdio", db_url="mysql+pymysql://u:p@h:6001/db")
        assert cfg["env"]["MEMORIA_DB_URL"] == "mysql+pymysql://u:p@h:6001/db"

    def test_embedding_opts_in_env(self) -> None:
        cfg = _mcp_config("stdio", provider="openai", model="BAAI/bge-m3")
        assert cfg["env"]["EMBEDDING_PROVIDER"] == "openai"
        assert cfg["env"]["EMBEDDING_MODEL"] == "BAAI/bge-m3"
        # dim auto-inferred from model name
        assert cfg["env"]["EMBEDDING_DIM"] == "1024"


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
            assert "memoria-lite" in mcp["mcpServers"]
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
            assert "memoria-lite" in mcp["mcpServers"]


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
            (Path(d) / "CLAUDE.md").write_text("# Has memoria-lite already")
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
        """MEMORIA_DB_URL env var is used when --db-url is absent."""
        args = argparse.Namespace(db_url=None)
        mock_engine = MagicMock()
        mock_sm = MagicMock(return_value=MagicMock())

        with patch.dict("os.environ", {"MEMORIA_DB_URL": "mysql://env"}), \
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
            os.environ.pop("MEMORIA_DB_URL", None)
            with patch.dict("sys.modules", {"api.database": MagicMock(SessionLocal=mock_sl)}):
                result = _get_db_factory(args)
        assert result is mock_sl

    def test_falls_back_to_default_db_url(self) -> None:
        """Falls back to DEFAULT_DB_URL when no DB URL and api.database import fails."""
        import sys as _sys

        args = argparse.Namespace(db_url=None)
        saved = _sys.modules.get("api.database")
        try:
            _sys.modules["api.database"] = None  # type: ignore[assignment]
            with patch.dict("os.environ", {}, clear=False), \
                 patch("sqlalchemy.create_engine") as mock_ce, \
                 patch("sqlalchemy.orm.sessionmaker") as mock_sm:
                os.environ.pop("MEMORIA_DB_URL", None)
                mock_engine = MagicMock()
                mock_ce.return_value = mock_engine
                mock_factory = MagicMock()
                mock_sm.return_value = mock_factory
                result = _get_db_factory(args)
            mock_ce.assert_called_once()
            assert "memoria" in mock_ce.call_args[0][0]
            assert result is mock_factory
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
            (mcp_dir / "mcp.json").write_text('{"mcpServers":{"memoria-lite":{}}}')

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


class TestCmdMigrate:
    """Test cmd_migrate passes force and dim to _create_tables."""

    def test_force_passed_to_create_tables(self) -> None:
        from cli.mo_memory_cli import cmd_migrate
        args = argparse.Namespace(db_url=None, dim="1536", force=True)
        with patch("cli.mo_memory_cli._resolve_engine", return_value=(MagicMock(), "d")), \
             patch("cli.mo_memory_cli._test_connection", return_value=True), \
             patch("cli.mo_memory_cli._create_tables", return_value=["t"] * 8) as mock_ct:
            cmd_migrate(args)
        mock_ct.assert_called_once()
        _, kwargs = mock_ct.call_args
        assert kwargs["dim"] == 1536
        assert kwargs["force"] is True

    def test_default_force_is_false(self) -> None:
        from cli.mo_memory_cli import cmd_migrate
        args = argparse.Namespace(db_url=None, dim=None, force=False)
        with patch("cli.mo_memory_cli._resolve_engine", return_value=(MagicMock(), "d")), \
             patch("cli.mo_memory_cli._test_connection", return_value=True), \
             patch("cli.mo_memory_cli._create_tables", return_value=["t"] * 8) as mock_ct:
            cmd_migrate(args)
        _, kwargs = mock_ct.call_args
        assert kwargs["force"] is False


# ── Argument parsing via main() ───────────────────────────────────────


class TestMain:
    def test_governance_args(self) -> None:
        with patch("sys.argv", ["memoria", "governance", "--user-id", "bob"]), \
             patch("cli.mo_memory_cli.cmd_governance") as mock_cmd:
            main()
        args = mock_cmd.call_args[0][0]
        assert args.user_id == "bob"

    def test_consolidate_requires_user_id(self) -> None:
        with patch("sys.argv", ["memoria", "consolidate"]), \
             pytest.raises(SystemExit):
            main()

    def test_reflect_requires_user_id(self) -> None:
        with patch("sys.argv", ["memoria", "reflect"]), \
             pytest.raises(SystemExit):
            main()

    def test_no_command_shows_help(self) -> None:
        with patch("sys.argv", ["memoria"]), \
             patch("argparse.ArgumentParser.print_help") as mock_help:
            main()
        mock_help.assert_called_once()

    def test_status_command(self) -> None:
        with patch("sys.argv", ["memoria", "status"]), \
             patch("cli.mo_memory_cli.cmd_status") as mock_cmd:
            main()
        mock_cmd.assert_called_once()

    def test_health_command(self) -> None:
        with patch("sys.argv", ["memoria", "health", "--api-url", "http://x:9000"]), \
             patch("cli.mo_memory_cli.cmd_health") as mock_cmd:
            main()
        assert mock_cmd.call_args[0][0].api_url == "http://x:9000"

    def test_migrate_force_flag(self) -> None:
        with patch("sys.argv", ["memoria", "migrate", "--force", "--dim", "1536"]), \
             patch("cli.mo_memory_cli.cmd_migrate") as mock_cmd:
            main()
        args = mock_cmd.call_args[0][0]
        assert args.force is True
        assert args.dim == "1536"

    def test_migrate_default_no_force(self) -> None:
        with patch("sys.argv", ["memoria", "migrate"]), \
             patch("cli.mo_memory_cli.cmd_migrate") as mock_cmd:
            main()
        args = mock_cmd.call_args[0][0]
        assert args.force is False


# ── Schema: ensure_database + ensure_tables ───────────────────────────


class TestEnsureDatabase:
    """Test that ensure_database creates the DB before tables."""

    def test_creates_database_via_root_connection(self) -> None:
        """ensure_database connects without DB name and runs CREATE DATABASE."""
        mock_engine = MagicMock()
        mock_engine.url.database = "memoria"
        mock_engine.url.set.return_value = "root_url"

        mock_root_engine = MagicMock()
        mock_conn = MagicMock()
        mock_root_engine.connect.return_value.__enter__ = Mock(return_value=mock_conn)
        mock_root_engine.connect.return_value.__exit__ = Mock(return_value=False)

        with patch("mo_memory_mcp.schema._create_engine", return_value=mock_root_engine) as mock_ce:
            from mo_memory_mcp.schema import ensure_database
            ensure_database(mock_engine)

        mock_engine.url.set.assert_called_once_with(database="")
        mock_ce.assert_called_once_with("root_url", pool_pre_ping=True)
        sql_arg = mock_conn.execute.call_args[0][0]
        assert "CREATE DATABASE IF NOT EXISTS" in sql_arg.text
        assert "memoria" in sql_arg.text
        mock_root_engine.dispose.assert_called_once()

    def test_skips_when_no_database_name(self) -> None:
        """No-op when engine URL has no database."""
        mock_engine = MagicMock()
        mock_engine.url.database = ""

        with patch("mo_memory_mcp.schema._create_engine") as mock_ce:
            from mo_memory_mcp.schema import ensure_database
            ensure_database(mock_engine)

        mock_ce.assert_not_called()


class TestEnsureTables:
    """Test that ensure_tables calls ensure_database first."""

    def test_calls_ensure_database_before_ddl(self) -> None:
        mock_engine = MagicMock()
        mock_conn = MagicMock()
        mock_engine.connect.return_value.__enter__ = Mock(return_value=mock_conn)
        mock_engine.connect.return_value.__exit__ = Mock(return_value=False)

        # _fix_embedding_dim runs SHOW COLUMNS and inspects the result.
        # Return matching dim so no ALTER is triggered.
        show_result = MagicMock()
        show_result.fetchone.return_value = ("embedding", "vecf32(384)", "YES", "", None, "")

        original_execute = mock_conn.execute

        def execute_side_effect(stmt: Any) -> Any:
            sql = getattr(stmt, "text", str(stmt))
            if "SHOW COLUMNS" in sql.upper():
                return show_result
            return original_execute(stmt)

        mock_conn.execute = MagicMock(side_effect=execute_side_effect)

        with patch("mo_memory_mcp.schema.ensure_database") as mock_edb:
            from mo_memory_mcp.schema import ensure_tables, TABLE_NAMES
            result = ensure_tables(mock_engine, dim=384)

        mock_edb.assert_called_once_with(mock_engine)
        assert result == TABLE_NAMES
        # 8 CREATE TABLE + 2 SHOW COLUMNS (mem_memories, memory_graph_nodes)
        assert mock_conn.execute.call_count == 10


class TestFixEmbeddingDim:
    """Test _fix_embedding_dim: warn/ALTER when embedding column dim mismatches."""

    def _make_conn(self, col_types: dict[str, str | None]) -> MagicMock:
        """Create a mock connection where SHOW COLUMNS returns given types.

        col_types maps table name → column type string (e.g. "vecf32(384)"),
        or None if the table has no embedding column.
        """
        tables = ("mem_memories", "memory_graph_nodes")

        def execute_side_effect(stmt: Any) -> MagicMock:
            sql = stmt.text if hasattr(stmt, "text") else str(stmt)
            for t in tables:
                if f"`{t}`" in sql and "SHOW COLUMNS" in sql.upper():
                    result = MagicMock()
                    ct = col_types.get(t)
                    if ct is None:
                        result.fetchone.return_value = None
                    else:
                        result.fetchone.return_value = ("embedding", ct, "YES", "", None, "")
                    return result
            return MagicMock()  # ALTER or other statements

        conn = MagicMock()
        conn.execute.side_effect = execute_side_effect
        return conn

    def test_no_action_when_dim_matches(self) -> None:
        from mo_memory_mcp.schema import _fix_embedding_dim
        conn = self._make_conn({"mem_memories": "vecf32(384)", "memory_graph_nodes": "vecf32(384)"})
        _fix_embedding_dim(conn, 384)
        # Only 2 SHOW COLUMNS, no ALTER
        assert conn.execute.call_count == 2

    def test_skips_when_no_embedding_column(self) -> None:
        from mo_memory_mcp.schema import _fix_embedding_dim
        conn = self._make_conn({"mem_memories": None, "memory_graph_nodes": None})
        _fix_embedding_dim(conn, 384)
        assert conn.execute.call_count == 2

    def test_warns_on_mismatch_without_force(self, caplog: pytest.LogCaptureFixture) -> None:
        from mo_memory_mcp.schema import _fix_embedding_dim
        conn = self._make_conn({"mem_memories": "vecf32(384)", "memory_graph_nodes": "vecf32(384)"})
        with caplog.at_level(logging.WARNING, logger="mo_memory_mcp.schema"):
            _fix_embedding_dim(conn, 1536, force=False)
        # No ALTER executed — only SHOW COLUMNS
        assert conn.execute.call_count == 2
        assert "dim mismatch" in caplog.text.lower()
        assert "memoria migrate" in caplog.text

    def test_alters_column_with_force(self) -> None:
        from mo_memory_mcp.schema import _fix_embedding_dim
        conn = self._make_conn({"mem_memories": "vecf32(384)", "memory_graph_nodes": "vecf32(384)"})
        _fix_embedding_dim(conn, 1536, force=True)
        # 2 SHOW COLUMNS + 2 ALTER TABLE
        assert conn.execute.call_count == 4
        alter_calls = [
            c for c in conn.execute.call_args_list
            if hasattr(c[0][0], "text") and "ALTER" in c[0][0].text
        ]
        assert len(alter_calls) == 2
        for call in alter_calls:
            assert "VECF32(1536)" in call[0][0].text

    def test_mixed_tables_one_matches_one_mismatches(self) -> None:
        from mo_memory_mcp.schema import _fix_embedding_dim
        conn = self._make_conn({"mem_memories": "vecf32(1536)", "memory_graph_nodes": "vecf32(384)"})
        _fix_embedding_dim(conn, 1536, force=True)
        # 2 SHOW COLUMNS + 1 ALTER (only memory_graph_nodes mismatches)
        assert conn.execute.call_count == 3


# ── CLI: effective_db_url written to MCP config ───────────────────────


class TestCmdInitEffectiveDbUrl:
    """Test that cmd_init writes the resolved db_url into MCP config."""

    def test_resolved_url_written_to_mcp_config(self) -> None:
        """When no --db-url, the resolved URL is still written to mcp.json."""
        with tempfile.TemporaryDirectory() as d:
            kiro_dir = Path(d) / ".kiro"
            kiro_dir.mkdir()

            mock_engine = MagicMock()
            mock_engine.url.render_as_string.return_value = (
                "mysql+pymysql://root:111@localhost:6001/memoria"
            )

            # cmd_init no longer creates tables — only resolves URL and writes config.
            # _test_connection is called for a non-fatal warning; _create_tables must NOT be called.
            with patch("cli.mo_memory_cli._resolve_engine",
                       return_value=(mock_engine, "default")), \
                 patch("cli.mo_memory_cli._test_connection", return_value=True), \
                 patch("cli.mo_memory_cli._create_tables") as mock_ct, \
                 patch("cli.mo_memory_cli._get_kiro_steering", return_value="# rule"):
                from cli.mo_memory_cli import cmd_init
                args = argparse.Namespace(
                    dir=d, mode="stdio", db_url=None,
                    embedding_provider=None, embedding_model=None,
                    embedding_dim=None, embedding_api_key=None,
                    embedding_base_url=None, tool=None, force=False,
                )
                cmd_init(args)
                mock_ct.assert_not_called()

            mcp_file = kiro_dir / "settings" / "mcp.json"
            config = json.loads(mcp_file.read_text())
            env = config["mcpServers"]["memoria-lite"].get("env", {})
            assert env.get("MEMORIA_DB_URL") == "mysql+pymysql://root:111@localhost:6001/memoria"

    def test_explicit_db_url_written_to_mcp_config(self) -> None:
        """When --db-url is given, it's written directly (no resolve, no connection test)."""
        with tempfile.TemporaryDirectory() as d:
            kiro_dir = Path(d) / ".kiro"
            kiro_dir.mkdir()

            # When db_url is explicit, _resolve_engine and _test_connection are NOT called.
            with patch("cli.mo_memory_cli._resolve_engine") as mock_re, \
                 patch("cli.mo_memory_cli._test_connection") as mock_tc, \
                 patch("cli.mo_memory_cli._create_tables") as mock_ct, \
                 patch("cli.mo_memory_cli._get_kiro_steering", return_value="# rule"):
                from cli.mo_memory_cli import cmd_init
                args = argparse.Namespace(
                    dir=d, mode="stdio",
                    db_url="mysql+pymysql://u:p@h:6001/mydb",
                    embedding_provider=None, embedding_model=None,
                    embedding_dim=None, embedding_api_key=None,
                    embedding_base_url=None, tool=None, force=False,
                )
                cmd_init(args)
                mock_re.assert_not_called()
                mock_tc.assert_not_called()
                mock_ct.assert_not_called()

            mcp_file = kiro_dir / "settings" / "mcp.json"
            config = json.loads(mcp_file.read_text())
            env = config["mcpServers"]["memoria-lite"].get("env", {})
            assert env.get("MEMORIA_DB_URL") == "mysql+pymysql://u:p@h:6001/mydb"

    def test_password_not_masked(self) -> None:
        """render_as_string(hide_password=False) is used, not str(url)."""
        with tempfile.TemporaryDirectory() as d:
            kiro_dir = Path(d) / ".kiro"
            kiro_dir.mkdir()

            mock_engine = MagicMock()
            mock_engine.url.render_as_string.return_value = (
                "mysql+pymysql://root:s3cret@host:6001/db"
            )

            with patch("cli.mo_memory_cli._resolve_engine",
                       return_value=(mock_engine, "default")), \
                 patch("cli.mo_memory_cli._test_connection", return_value=True), \
                 patch("cli.mo_memory_cli._get_kiro_steering", return_value="# rule"):
                from cli.mo_memory_cli import cmd_init
                args = argparse.Namespace(
                    dir=d, mode="stdio", db_url=None,
                    embedding_provider=None, embedding_model=None,
                    embedding_dim=None, embedding_api_key=None,
                    embedding_base_url=None, tool=None, force=False,
                )
                cmd_init(args)

            mock_engine.url.render_as_string.assert_called_once_with(hide_password=False)
            mcp_file = kiro_dir / "settings" / "mcp.json"
            config = json.loads(mcp_file.read_text())
            url = config["mcpServers"]["memoria-lite"]["env"]["MEMORIA_DB_URL"]
            assert "s3cret" in url
            assert "***" not in url


# ── CLI: embedding check in init ──────────────────────────────────────


class TestCmdInitEmbeddingCheck:
    """memoria init warns when sentence-transformers is not installed."""

    def _run_init(self, d: str, has_sentence_transformers: bool, capsys) -> str:
        kiro_dir = Path(d) / ".kiro"
        kiro_dir.mkdir(exist_ok=True)
        mock_engine = MagicMock()
        mock_engine.url.render_as_string.return_value = "mysql+pymysql://root:x@h:6001/db"

        import sys as _sys
        saved = _sys.modules.get("sentence_transformers")
        try:
            if not has_sentence_transformers:
                _sys.modules["sentence_transformers"] = None  # type: ignore[assignment]
            elif saved is None:
                # Simulate installed: put a dummy module
                import types
                _sys.modules["sentence_transformers"] = types.ModuleType("sentence_transformers")

            with patch("cli.mo_memory_cli._resolve_engine", return_value=(mock_engine, "d")), \
                 patch("cli.mo_memory_cli._test_connection", return_value=True), \
                 patch("cli.mo_memory_cli._get_kiro_steering", return_value="# rule"):
                from cli.mo_memory_cli import cmd_init
                args = argparse.Namespace(
                    dir=d, mode="stdio", db_url=None,
                    embedding_provider=None, embedding_model=None,
                    embedding_dim=None, embedding_api_key=None,
                    embedding_base_url=None, tool=None, force=False,
                )
                cmd_init(args)
        finally:
            if saved is not None:
                _sys.modules["sentence_transformers"] = saved
            else:
                _sys.modules.pop("sentence_transformers", None)

        return capsys.readouterr().out

    def test_warns_when_not_installed(self, capsys: pytest.CaptureFixture[str]) -> None:
        with tempfile.TemporaryDirectory() as d:
            out = self._run_init(d, has_sentence_transformers=False, capsys=capsys)
        assert "sentence-transformers not installed" in out
        assert "pip install" in out

    def test_no_warning_when_installed(self, capsys: pytest.CaptureFixture[str]) -> None:
        with tempfile.TemporaryDirectory() as d:
            out = self._run_init(d, has_sentence_transformers=True, capsys=capsys)
        assert "sentence-transformers installed" in out
        assert "not installed" not in out
