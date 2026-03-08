"""Unit tests for per-user strategy binding (Phase 1).

Tests:
- DB-backed resolution order
- set_user_strategy / switch_user_strategy
- Backfill-during-switch flow
- Backfilling status falls through to default
"""

from __future__ import annotations

from unittest.mock import MagicMock, patch

import pytest


class TestPerUserResolution:
    """Factory resolves strategy from DB when user_id is provided."""

    def test_explicit_strategy_wins_over_db(self):
        """Explicit strategy= param always wins, even if DB has a row."""
        from core.memory.factory import _resolve_strategy

        result = _resolve_strategy(
            db_factory=MagicMock(), user_id="alice",
            backend=None, strategy="activation:v1",
        )
        assert result == "activation:v1"

    def test_backend_wins_over_db(self):
        """Legacy backend= param wins over DB lookup."""
        from core.memory.factory import _resolve_strategy

        result = _resolve_strategy(
            db_factory=MagicMock(), user_id="alice",
            backend="graph", strategy=None,
        )
        assert result == "activation:v1"

    def test_db_lookup_used_when_no_explicit(self):
        """When no strategy/backend, look up from DB."""
        from core.memory.factory import _resolve_strategy

        mock_db = MagicMock()
        mock_row = MagicMock()
        mock_row.strategy_key = "activation:v1"
        mock_row.index_status = "ready"
        mock_db.__enter__ = MagicMock(return_value=mock_db)
        mock_db.__exit__ = MagicMock(return_value=False)
        mock_db.execute.return_value.fetchone.return_value = mock_row

        result = _resolve_strategy(
            db_factory=lambda: mock_db, user_id="alice",
            backend=None, strategy=None,
        )
        assert result == "activation:v1"

    def test_db_backfilling_falls_through(self):
        """If index_status='backfilling', fall through to env/default."""
        from core.memory.factory import _resolve_strategy

        mock_db = MagicMock()
        mock_row = MagicMock()
        mock_row.strategy_key = "activation:v1"
        mock_row.index_status = "backfilling"
        mock_db.__enter__ = MagicMock(return_value=mock_db)
        mock_db.__exit__ = MagicMock(return_value=False)
        mock_db.execute.return_value.fetchone.return_value = mock_row

        result = _resolve_strategy(
            db_factory=lambda: mock_db, user_id="alice",
            backend=None, strategy=None,
        )
        # Should fall through to env/default, not return activation:v1
        assert result == "vector:v1"

    def test_db_backfilling_without_migrated_from_falls_through(self):
        """Backfilling with no migrated_from still falls through."""
        from core.memory.factory import _resolve_strategy

        mock_db = MagicMock()
        mock_row = MagicMock()
        mock_row.strategy_key = "activation:v1"
        mock_row.index_status = "backfilling"
        mock_row.migrated_from = None  # first-time setup
        mock_db.__enter__ = MagicMock(return_value=mock_db)
        mock_db.__exit__ = MagicMock(return_value=False)
        mock_db.execute.return_value.fetchone.return_value = mock_row

        result = _resolve_strategy(
            db_factory=lambda: mock_db, user_id="alice",
            backend=None, strategy=None,
        )
        assert result == "vector:v1"

    def test_no_db_row_falls_through(self):
        """No DB row → fall through to env/default."""
        from core.memory.factory import _resolve_strategy

        mock_db = MagicMock()
        mock_db.__enter__ = MagicMock(return_value=mock_db)
        mock_db.__exit__ = MagicMock(return_value=False)
        mock_db.execute.return_value.fetchone.return_value = None

        result = _resolve_strategy(
            db_factory=lambda: mock_db, user_id="alice",
            backend=None, strategy=None,
        )
        assert result == "vector:v1"

    def test_no_user_id_skips_db(self):
        """No user_id → skip DB lookup entirely."""
        from core.memory.factory import _resolve_strategy

        result = _resolve_strategy(
            db_factory=MagicMock(), user_id=None,
            backend=None, strategy=None,
        )
        assert result == "vector:v1"

    def test_db_error_falls_through(self):
        """DB error → gracefully fall through to default."""
        from core.memory.factory import _resolve_strategy

        def bad_factory():
            raise RuntimeError("DB down")

        result = _resolve_strategy(
            db_factory=bad_factory, user_id="alice",
            backend=None, strategy=None,
        )
        assert result == "vector:v1"


class TestSwitchUserStrategy:
    """switch_user_strategy handles backfill and status transitions."""

    def test_same_strategy_is_noop(self):
        from core.memory.factory import switch_user_strategy

        mock_db = MagicMock()
        mock_db.__enter__ = MagicMock(return_value=mock_db)
        mock_db.__exit__ = MagicMock(return_value=False)
        # Return current strategy = same as requested
        mock_row = MagicMock()
        mock_row.strategy_key = "vector:v1"
        mock_db.execute.return_value.fetchone.return_value = mock_row

        result = switch_user_strategy(lambda: mock_db, "alice", "vector:v1")
        assert result.status == "ready"
        assert result.strategy_key == "vector:v1"
        assert result.previous_key == "vector:v1"

    @patch("core.memory.factory._upsert_user_config")
    @patch("core.memory.factory._registry")
    def test_switch_with_backfill(self, mock_registry, mock_upsert):
        from core.memory.factory import switch_user_strategy

        # Mock: index manager says backfill needed
        mock_index = MagicMock()
        mock_index.backfill_needed.return_value = True
        mock_index.backfill.return_value = MagicMock(processed=10, skipped=0, errors=[])
        mock_registry.create_strategy.return_value = MagicMock()
        mock_registry.create_index_manager.return_value = mock_index

        mock_db = MagicMock()
        mock_db.__enter__ = MagicMock(return_value=mock_db)
        mock_db.__exit__ = MagicMock(return_value=False)
        mock_db.execute.return_value.fetchone.return_value = None  # no existing row

        result = switch_user_strategy(lambda: mock_db, "alice", "activation:v1")
        assert result.status == "ready"
        assert result.strategy_key == "activation:v1"
        mock_index.backfill.assert_called_once_with("alice")
        # Should have been called twice: once for backfilling, once for ready
        assert mock_upsert.call_count == 2

    @patch("core.memory.factory._upsert_user_config")
    @patch("core.memory.factory._registry")
    def test_switch_backfill_failure_reverts(self, mock_registry, mock_upsert):
        from core.memory.factory import switch_user_strategy

        mock_index = MagicMock()
        mock_index.backfill_needed.return_value = True
        mock_index.backfill.side_effect = RuntimeError("backfill failed")
        mock_registry.create_strategy.return_value = MagicMock()
        mock_registry.create_index_manager.return_value = mock_index

        mock_db = MagicMock()
        mock_db.__enter__ = MagicMock(return_value=mock_db)
        mock_db.__exit__ = MagicMock(return_value=False)
        mock_row = MagicMock()
        mock_row.strategy_key = "vector:v1"
        mock_db.execute.return_value.fetchone.return_value = mock_row

        with pytest.raises(RuntimeError, match="backfill failed"):
            switch_user_strategy(lambda: mock_db, "alice", "activation:v1")

        # Last upsert should revert to old strategy with failed status
        last_call = mock_upsert.call_args
        assert last_call[0][2] == "vector:v1"  # strategy_key
        assert last_call[1]["index_status"] == "failed"

    @patch("core.memory.factory.set_user_strategy")
    @patch("core.memory.factory._registry")
    def test_switch_no_backfill_needed(self, mock_registry, mock_set):
        from core.memory.factory import switch_user_strategy

        mock_index = MagicMock()
        mock_index.backfill_needed.return_value = False
        mock_registry.create_strategy.return_value = MagicMock()
        mock_registry.create_index_manager.return_value = mock_index

        mock_db = MagicMock()
        mock_db.__enter__ = MagicMock(return_value=mock_db)
        mock_db.__exit__ = MagicMock(return_value=False)
        mock_db.execute.return_value.fetchone.return_value = None

        result = switch_user_strategy(lambda: mock_db, "alice", "vector:v1")
        assert result.status == "ready"
        mock_set.assert_called_once()
