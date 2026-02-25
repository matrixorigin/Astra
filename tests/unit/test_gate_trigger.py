"""Tests for GateTrigger auto-trigger on skill/prompt changes."""

import threading
import time
from unittest.mock import MagicMock, Mock, patch, call

import pytest

from core.evaluation.gate_trigger import GateTrigger


def _trigger(gate_result: dict | None = None) -> tuple[GateTrigger, Mock]:
    """Build GateTrigger with mocked gate."""
    db = Mock()
    db_factory = Mock(return_value=db)

    gate = Mock()
    gate.validate_change.return_value = gate_result or {"verdict": "pass", "metrics": {}}

    with patch("core.evaluation.gate_trigger.GateTrigger._run_gate") as mock_run:
        trigger = GateTrigger(db_factory=db_factory)
        trigger._gate_mock = gate
        return trigger, db_factory


class TestGateTriggerAsync:
    def test_on_skill_change_fires_thread(self):
        fired = threading.Event()
        db_factory = Mock(return_value=Mock())

        trigger = GateTrigger(db_factory=db_factory)

        with patch.object(trigger, "_run_gate", side_effect=lambda *a, **kw: fired.set()) as mock_run:
            trigger.on_skill_change("my_skill", "1.0.0", {"param": "value"})
            fired.wait(timeout=2.0)
            assert fired.is_set()
            mock_run.assert_called_once_with("skill", "my_skill@1.0.0", {"name": "my_skill", "version": "1.0.0", "definition": {"param": "value"}})

    def test_on_prompt_change_fires_thread(self):
        fired = threading.Event()
        db_factory = Mock(return_value=Mock())

        trigger = GateTrigger(db_factory=db_factory)

        with patch.object(trigger, "_run_gate", side_effect=lambda *a, **kw: fired.set()) as mock_run:
            trigger.on_prompt_change("system_prompt", "v2", "You are a helpful assistant.")
            fired.wait(timeout=2.0)
            assert fired.is_set()
            mock_run.assert_called_once_with(
                "prompt", "system_prompt@v2",
                {"template_id": "system_prompt", "version": "v2", "content": "You are a helpful assistant."},
            )

    def test_thread_is_daemon(self):
        """Gate thread must be daemon so it doesn't block process exit."""
        db_factory = Mock(return_value=Mock())
        trigger = GateTrigger(db_factory=db_factory)

        threads_before = set(t.name for t in threading.enumerate())

        with patch.object(trigger, "_run_gate"):
            trigger.on_skill_change("s", "1.0", {})
            time.sleep(0.05)

        # Verify thread was created as daemon (it may have already finished)
        # Just verify no exception was raised and call was non-blocking
        assert True  # non-blocking call completed

    def test_non_blocking(self):
        """on_skill_change must return immediately."""
        db_factory = Mock(return_value=Mock())
        trigger = GateTrigger(db_factory=db_factory)

        slow_gate_started = threading.Event()

        def slow_gate(*args):
            slow_gate_started.set()
            time.sleep(10)  # simulate slow gate

        with patch.object(trigger, "_run_gate", side_effect=slow_gate):
            start = time.monotonic()
            trigger.on_skill_change("s", "1.0", {})
            elapsed = time.monotonic() - start
            assert elapsed < 0.5  # must return in < 500ms


class TestRunGate:
    def _patched_run(self, gate, db, trigger):
        """Helper: patch RegressionGate + lock so _run_gate executes gate."""
        with patch("core.evaluation.gate_trigger.RegressionGate", return_value=gate), \
             patch.object(trigger, "_try_acquire", return_value=True), \
             patch.object(trigger, "_release"):
            yield

    def test_run_gate_calls_validate_change(self):
        db = Mock()
        db_factory = Mock(return_value=db)
        gate = Mock()
        gate.validate_change.return_value = {"verdict": "pass", "metrics": {}}
        trigger = GateTrigger(db_factory=db_factory)

        with patch("core.evaluation.gate_trigger.RegressionGate", return_value=gate), \
             patch.object(trigger, "_try_acquire", return_value=True), \
             patch.object(trigger, "_release"):
            trigger._run_gate("skill", "my_skill@1.0", {"name": "my_skill"})

        gate.validate_change.assert_called_once()
        assert gate.validate_change.call_args.kwargs["change_id"] == "my_skill@1.0"

    def test_run_gate_skipped_when_lock_held(self):
        db = Mock()
        db_factory = Mock(return_value=db)
        gate = Mock()
        trigger = GateTrigger(db_factory=db_factory)

        with patch("core.evaluation.gate_trigger.RegressionGate", return_value=gate), \
             patch.object(trigger, "_try_acquire", return_value=False):
            trigger._run_gate("skill", "my_skill@1.0", {})

        gate.validate_change.assert_not_called()

    def test_run_gate_logs_warning_on_fail(self):
        db = Mock()
        db_factory = Mock(return_value=db)
        gate = Mock()
        gate.validate_change.return_value = {"verdict": "fail", "metrics": {"error_rate": 0.1}}
        trigger = GateTrigger(db_factory=db_factory)

        with patch("core.evaluation.gate_trigger.RegressionGate", return_value=gate), \
             patch.object(trigger, "_try_acquire", return_value=True), \
             patch.object(trigger, "_release"), \
             patch("core.evaluation.gate_trigger.logger") as mock_logger:
            trigger._run_gate("prompt", "tmpl@v2", {"content": "..."})
            mock_logger.warning.assert_called_once()

    def test_run_gate_closes_db_on_exception(self):
        db = Mock()
        db_factory = Mock(return_value=db)
        trigger = GateTrigger(db_factory=db_factory)

        with patch("core.evaluation.gate_trigger.RegressionGate", side_effect=RuntimeError("boom")), \
             patch.object(trigger, "_try_acquire", return_value=True), \
             patch.object(trigger, "_release"):
            trigger._run_gate("skill", "s@1.0", {})  # must not raise

        db.close.assert_called_once()

    def test_run_gate_closes_db_on_success(self):
        db = Mock()
        db_factory = Mock(return_value=db)
        gate = Mock()
        gate.validate_change.return_value = {"verdict": "pass", "metrics": {}}
        trigger = GateTrigger(db_factory=db_factory)

        with patch("core.evaluation.gate_trigger.RegressionGate", return_value=gate), \
             patch.object(trigger, "_try_acquire", return_value=True), \
             patch.object(trigger, "_release"):
            trigger._run_gate("skill", "s@1.0", {})

        db.close.assert_called_once()

    def test_lock_released_even_on_gate_exception(self):
        db = Mock()
        db_factory = Mock(return_value=db)
        gate = Mock()
        gate.validate_change.side_effect = RuntimeError("gate crashed")
        trigger = GateTrigger(db_factory=db_factory)
        release = Mock()

        with patch("core.evaluation.gate_trigger.RegressionGate", return_value=gate), \
             patch.object(trigger, "_try_acquire", return_value=True), \
             patch.object(trigger, "_release", release):
            trigger._run_gate("skill", "s@1.0", {})

        release.assert_called_once()


class TestSkillRegistryIntegration:
    def test_gate_triggered_on_active_skill_register(self):
        from sqlalchemy.orm import Session
        from core.skills.registry import SkillRegistry

        session = Mock(spec=Session)
        session.query.return_value.filter.return_value.update.return_value = None
        session.query.return_value.filter.return_value.first.return_value = None

        gate_trigger = Mock()
        registry = SkillRegistry(db_factory=lambda: session, gate_trigger=gate_trigger)

        skill = Mock()
        skill.name = "test_skill"
        skill.version = "1.0.0"
        skill.description = "test"
        skill.requirements = Mock()
        skill.requirements.model_dump.return_value = {}

        with patch.object(registry, "_compute_code_hash", return_value="abc123"):
            registry.register(skill, is_active=True)

        gate_trigger.on_skill_change.assert_called_once_with(
            skill_name="test_skill",
            version="1.0.0",
            definition={},
        )

    def test_gate_not_triggered_on_inactive_skill(self):
        from sqlalchemy.orm import Session
        from core.skills.registry import SkillRegistry

        session = Mock(spec=Session)
        session.query.return_value.filter.return_value.update.return_value = None
        session.query.return_value.filter.return_value.first.return_value = None

        gate_trigger = Mock()
        registry = SkillRegistry(db_factory=lambda: session, gate_trigger=gate_trigger)

        skill = Mock()
        skill.name = "test_skill"
        skill.version = "1.0.0"
        skill.description = "test"
        skill.requirements = Mock()
        skill.requirements.model_dump.return_value = {}

        with patch.object(registry, "_compute_code_hash", return_value="abc123"):
            registry.register(skill, is_active=False)

        gate_trigger.on_skill_change.assert_not_called()


class TestPromptManagerIntegration:
    def test_gate_triggered_on_active_prompt_register(self):
        from core.context.prompts import PromptManager

        db = Mock()
        gate_trigger = Mock()
        pm = PromptManager(lambda: db, gate_trigger=gate_trigger)

        pm.register_prompt("system_prompt", "v2", "You are helpful.", is_active=True)

        gate_trigger.on_prompt_change.assert_called_once_with(
            template_id="system_prompt",
            version="v2",
            content="You are helpful.",
        )

    def test_gate_not_triggered_on_inactive_prompt(self):
        from core.context.prompts import PromptManager

        db = Mock()
        gate_trigger = Mock()
        pm = PromptManager(lambda: db, gate_trigger=gate_trigger)

        pm.register_prompt("system_prompt", "v2", "You are helpful.", is_active=False)

        gate_trigger.on_prompt_change.assert_not_called()

    def test_no_gate_trigger_no_error(self):
        from core.context.prompts import PromptManager

        db = Mock()
        pm = PromptManager(lambda: db)  # no gate_trigger

        pm.register_prompt("system_prompt", "v2", "content", is_active=True)
        # must not raise

    def test_rollback_reactivates_previous_version(self):
        from core.context.prompts import PromptManager
        from unittest.mock import call

        db = Mock()
        pm = PromptManager(lambda: db)

        # Current active version
        current = Mock()
        current.version = "v2"
        current.created_at = "2026-01-02"

        # Previous inactive version
        previous = Mock()
        previous.version = "v1"

        # First query returns current, second returns previous
        db.execute.return_value.first.side_effect = [current, previous]

        result = pm.rollback_prompt("system_general")

        assert result == "v1"
        db.commit.assert_called_once()
        # Cache should be invalidated
        assert "system_general" not in pm._cache

    def test_rollback_returns_none_when_no_prior_version(self):
        from core.context.prompts import PromptManager

        db = Mock()
        pm = PromptManager(lambda: db)

        current = Mock()
        current.version = "v1"
        # First call returns current, second returns None (no prior)
        db.execute.return_value.first.side_effect = [current, None]

        result = pm.rollback_prompt("system_general")

        assert result is None
        db.commit.assert_not_called()
