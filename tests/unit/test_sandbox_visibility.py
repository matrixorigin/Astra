from unittest.mock import MagicMock

import pytest

from core.sandbox.sandbox import Sandbox


def _make_row(value: str):
    row = MagicMock()
    row._mapping = {"name": value}
    return row


def test_wait_until_database_visible_retries_until_visible(monkeypatch):
    sandbox = Sandbox(lambda: MagicMock())
    attempts = {"n": 0}

    def factory():
        db = MagicMock()

        def fetchall():
            attempts["n"] += 1
            return [] if attempts["n"] < 3 else [_make_row("target_db")]

        db.execute.return_value.fetchall.side_effect = fetchall
        return db

    sleep_calls: list[float] = []
    monkeypatch.setattr("core.sandbox.sandbox.time.sleep", lambda delay: sleep_calls.append(delay))

    sandbox.wait_until_database_visible("target_db", attempts=5, session_factory=factory)

    assert attempts["n"] == 3
    assert sleep_calls == [0.05, 0.1]


def test_wait_until_table_visible_raises_when_never_visible(monkeypatch):
    sandbox = Sandbox(lambda: MagicMock())

    def factory():
        db = MagicMock()
        db.execute.return_value.fetchall.return_value = []
        return db

    monkeypatch.setattr("core.sandbox.sandbox.time.sleep", lambda delay: None)

    with pytest.raises(RuntimeError, match="Table exp_db.experiment_config was not visible"):
        sandbox.wait_until_table_visible(
            "exp_db",
            "experiment_config",
            attempts=3,
            session_factory=factory,
        )
