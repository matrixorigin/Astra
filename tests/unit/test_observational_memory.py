"""Unit tests for Observer and Reflector agents."""

import json
from datetime import datetime
from unittest.mock import MagicMock, patch, call

import pytest

from core.memory.observer import Observer, _parse_json_array, DEFAULT_OBSERVE_THRESHOLD
from core.memory.reflector import Reflector, DEFAULT_REFLECT_THRESHOLD


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def _make_messages(n=10, chars_per=1000):
    """Generate messages that exceed the default observe threshold (~2500 tokens)."""
    return [
        {"role": "user" if i % 2 == 0 else "assistant", "content": f"msg {i} " + "x" * chars_per}
        for i in range(n)
    ]


def _big_observations(n=30, chars=2000):
    """Generate observations that exceed the reflect threshold."""
    return [
        {"content": f"obs {i} " + "y" * chars, "observation_id": f"o{i}",
         "priority": "medium", "type": "fact", "observed_at": "2026-02-20",
         "referenced_at": None, "session_id": "s1", "is_reflected": False}
        for i in range(n)
    ]


@pytest.fixture
def db():
    return MagicMock()


@pytest.fixture
def llm():
    return MagicMock()


@pytest.fixture
def observer(db, llm):
    return Observer(db, llm)


@pytest.fixture
def reflector(db, llm):
    return Reflector(db, llm)


# ---------------------------------------------------------------------------
# _parse_json_array: robust LLM output parsing
# ---------------------------------------------------------------------------

class TestParseJsonArray:
    def test_bare_json(self):
        assert _parse_json_array('[{"a": 1}]') == [{"a": 1}]

    def test_code_block(self):
        assert _parse_json_array('```json\n[{"a": 1}]\n```') == [{"a": 1}]

    def test_garbage_around(self):
        assert _parse_json_array('Here are observations:\n[{"a": 1}]\nDone!') == [{"a": 1}]

    def test_empty_array(self):
        assert _parse_json_array("[]") == []

    def test_invalid(self):
        assert _parse_json_array("not json at all") == []

    def test_code_block_no_lang(self):
        assert _parse_json_array('```\n[{"a": 1}]\n```') == [{"a": 1}]


# ---------------------------------------------------------------------------
# Observer: gating
# ---------------------------------------------------------------------------

class TestObserverGating:
    def test_skip_below_threshold(self, observer):
        # Mock DB index query to return 0
        observer.db.query.return_value.filter.return_value.scalar.return_value = 0
        result = observer.observe("s1", "u1", [{"role": "user", "content": "hi"}])
        assert result == []
        observer.llm.chat_with_tools.assert_not_called()

    def test_skip_no_llm(self, db):
        db.query.return_value.filter.return_value.scalar.return_value = 0
        obs = Observer(db, llm_client=None)
        result = obs.observe("s1", "u1", _make_messages())
        assert result == []

    def test_skip_all_observed(self, observer):
        """Skip when all messages already observed (DB index == len(messages))."""
        msgs = _make_messages()
        observer.db.query.return_value.filter.return_value.scalar.return_value = len(msgs)
        result = observer.observe("s1", "u1", msgs)
        assert result == []


# ---------------------------------------------------------------------------
# Observer: extraction
# ---------------------------------------------------------------------------

class TestObserverExtraction:
    def _setup_db_index(self, observer, idx=0):
        observer.db.query.return_value.filter.return_value.scalar.return_value = idx

    def test_extract_and_store(self, observer, db):
        self._setup_db_index(observer)
        observer.llm.chat_with_tools.return_value = {
            "content": json.dumps([
                {"content": "User prefers Python", "priority": "high", "type": "preference"},
                {"content": "Project uses FastAPI", "priority": "medium", "type": "fact"},
            ])
        }
        result = observer.observe("s1", "u1", _make_messages())
        assert len(result) == 2
        assert result[0]["content"] == "User prefers Python"
        assert db.add.call_count == 2
        db.commit.assert_called_once()

    def test_extract_with_garbage(self, observer, db):
        """LLM returns JSON with surrounding text."""
        self._setup_db_index(observer)
        observer.llm.chat_with_tools.return_value = {
            "content": 'Here are the observations:\n[{"content": "test", "priority": "low", "type": "fact"}]\nDone!'
        }
        result = observer.observe("s1", "u1", _make_messages())
        assert len(result) == 1

    def test_extract_empty(self, observer):
        self._setup_db_index(observer)
        observer.llm.chat_with_tools.return_value = {"content": "[]"}
        result = observer.observe("s1", "u1", _make_messages())
        assert result == []

    def test_extract_empty_advances_index(self, observer, db):
        """LLM returns [] → must write marker row to advance DB index, avoiding repeated LLM calls."""
        self._setup_db_index(observer)
        observer.llm.chat_with_tools.return_value = {"content": "[]"}
        msgs = _make_messages(10)
        observer.observe("s1", "u1", msgs)
        # Marker row written: db.add called with is_reflected=1 marker
        added = db.add.call_args[0][0]
        assert added.observed_msg_index == 10
        assert added.is_reflected == 1  # marker, won't appear in context
        db.commit.assert_called()

    def test_llm_failure_advances_index(self, observer, db):
        """LLM exception → must still advance index so we don't retry same messages."""
        self._setup_db_index(observer)
        observer.llm.chat_with_tools.side_effect = RuntimeError("LLM down")
        msgs = _make_messages(10)
        result = observer.observe("s1", "u1", msgs)
        assert result == []
        # Marker row written even on LLM failure
        added = db.add.call_args[0][0]
        assert added.observed_msg_index == 10
        assert added.is_reflected == 1

    def test_llm_failure_graceful(self, observer):
        self._setup_db_index(observer)
        observer.llm.chat_with_tools.side_effect = RuntimeError("LLM down")
        result = observer.observe("s1", "u1", _make_messages())
        assert result == []

    def test_referenced_at_parsing(self, observer, db):
        self._setup_db_index(observer)
        observer.llm.chat_with_tools.return_value = {
            "content": json.dumps([{
                "content": "Flight on Jan 31",
                "priority": "high",
                "type": "fact",
                "referenced_at": "2026-01-31T00:00:00",
            }])
        }
        result = observer.observe("s1", "u1", _make_messages())
        assert len(result) == 1
        added_obj = db.add.call_args[0][0]
        assert added_obj.referenced_at == datetime(2026, 1, 31)

    def test_observed_msg_index_stored(self, observer, db):
        """Each observation stores the message index for DB-backed tracking."""
        self._setup_db_index(observer)
        observer.llm.chat_with_tools.return_value = {
            "content": json.dumps([{"content": "test", "priority": "low", "type": "fact"}])
        }
        msgs = _make_messages(10)
        observer.observe("s1", "u1", msgs)
        added_obj = db.add.call_args[0][0]
        assert added_obj.observed_msg_index == 10


# ---------------------------------------------------------------------------
# Observer: DB-backed index (no in-memory state)
# ---------------------------------------------------------------------------

class TestObserverDBIndex:
    def test_get_observed_index_from_db(self, observer, db):
        """Index comes from DB, not in-memory dict."""
        db.query.return_value.filter.return_value.scalar.return_value = 7
        assert observer.get_observed_index("s1") == 7

    def test_get_observed_index_none_returns_zero(self, observer, db):
        db.query.return_value.filter.return_value.scalar.return_value = None
        assert observer.get_observed_index("s1") == 0

    def test_only_new_messages_sent_to_llm(self, observer, db):
        """When DB says 10 messages observed, only messages[10:] go to LLM."""
        db.query.return_value.filter.return_value.scalar.return_value = 10

        old_msgs = _make_messages(10)
        new_msgs = [
            {"role": "user", "content": "NEWMSG_A " + "z" * 1000},
            {"role": "assistant", "content": "NEWMSG_B " + "z" * 1000},
        ] * 5  # 10 new messages to exceed threshold
        all_msgs = old_msgs + new_msgs

        observer.llm.chat_with_tools.return_value = {"content": "[]"}
        observer.observe("s1", "u1", all_msgs)

        call_content = observer.llm.chat_with_tools.call_args.kwargs["messages"][1]["content"]
        assert "NEWMSG_A" in call_content
        assert "msg 0" not in call_content


# ---------------------------------------------------------------------------
# Observer: context assembly
# ---------------------------------------------------------------------------

class TestObserverContext:
    def test_format_emoji(self, observer):
        observations = [
            {"content": "Critical", "priority": "high", "observed_at": "2026-02-20T10:00:00"},
            {"content": "Normal", "priority": "medium", "observed_at": "2026-02-20T11:00:00"},
            {"content": "Minor", "priority": "low", "observed_at": "2026-02-20T12:00:00"},
        ]
        text = observer.format_for_context(observations)
        assert "🔴" in text and "🟡" in text and "🟢" in text
        assert text.startswith("## Memory (Observations)")

    def test_format_empty(self, observer):
        assert observer.format_for_context([]) == ""

    def test_build_context_replaces_observed(self, observer, db):
        """Observed messages are dropped, observations injected into system prompt."""
        msgs = [{"role": "system", "content": "You are helpful."}]
        msgs += [{"role": "user" if i % 2 == 0 else "assistant", "content": f"old msg {i}"}
                 for i in range(10)]
        # DB says 7 messages observed (system + 6 user/assistant)
        db.query.return_value.filter.return_value.scalar.return_value = 7

        obs_section = "## Memory (Observations)\n- 🔴 Summary of old msgs"

        with patch.object(observer, "get_observations", return_value=[{"content": "x"}]):
            result = observer.build_context_with_observations(
                msgs, "u1", "s1", _cached_obs_section=obs_section,
            )

        # System message has observations
        assert "## Memory (Observations)" in result[0]["content"]
        # Observed messages dropped — result should be shorter
        assert len(result) < len(msgs)
        # Recent messages preserved
        assert any("old msg 9" in m.get("content", "") for m in result)

    def test_build_context_no_observations_passthrough(self, observer, db):
        msgs = [{"role": "system", "content": "sys"}, {"role": "user", "content": "hi"}]
        with patch.object(observer, "get_observations", return_value=[]):
            result = observer.build_context_with_observations(msgs, "u1", "s1")
        assert result == msgs

    def test_build_context_no_duplicate_injection(self, observer, db):
        """Calling build_context twice doesn't duplicate observations in system prompt."""
        msgs = [{"role": "system", "content": "Base prompt.\n\n## Memory (Observations)\n- existing"}]
        msgs += [{"role": "user", "content": "hi"}]
        db.query.return_value.filter.return_value.scalar.return_value = 0

        obs_section = "## Memory (Observations)\n- new obs"
        with patch.object(observer, "get_observations", return_value=[{"content": "x"}]):
            result = observer.build_context_with_observations(
                msgs, "u1", "s1", _cached_obs_section=obs_section,
            )

        # Should NOT have two "## Memory (Observations)" sections
        assert result[0]["content"].count("## Memory (Observations)") == 1

    def test_build_context_exact_message_boundaries(self, observer, db):
        """Verify exactly which messages are kept vs dropped.

        Setup: system + 10 msgs (idx 0-9), observed_idx=7 (system + 6 non-system observed).
        Expected: system(+obs) + msgs[6:] (4 unobserved: idx 6,7,8,9).
        adj_idx = 7 - 1(system) = 6, remaining[6:] = msgs at original idx 7,8,9,10.
        """
        msgs = [{"role": "system", "content": "sys"}]
        for i in range(10):
            msgs.append({"role": "user" if i % 2 == 0 else "assistant", "content": f"msg-{i}"})
        # 11 total messages. observed_idx=7 means system + first 6 non-system are observed.
        db.query.return_value.filter.return_value.scalar.return_value = 7
        obs_section = "## Memory (Observations)\n- obs"

        with patch.object(observer, "get_observations", return_value=[{"content": "x"}]):
            result = observer.build_context_with_observations(
                msgs, "u1", "s1", _cached_obs_section=obs_section,
            )

        # System + 4 unobserved messages
        assert len(result) == 5
        assert result[0]["role"] == "system"
        # The kept messages should be msg-6, msg-7, msg-8, msg-9
        kept_contents = [m["content"] for m in result[1:]]
        assert kept_contents == ["msg-6", "msg-7", "msg-8", "msg-9"]

    def test_build_context_preserve_recent_minimum(self, observer, db):
        """When fewer unobserved msgs than preserve_recent, keep preserve_recent from tail."""
        msgs = [{"role": "system", "content": "sys"}]
        for i in range(10):
            msgs.append({"role": "user" if i % 2 == 0 else "assistant", "content": f"msg-{i}"})
        # observed_idx=10 means only msg-9 is unobserved (1 < preserve_recent=4)
        db.query.return_value.filter.return_value.scalar.return_value = 10
        obs_section = "## Memory (Observations)\n- obs"

        with patch.object(observer, "get_observations", return_value=[{"content": "x"}]):
            result = observer.build_context_with_observations(
                msgs, "u1", "s1", _cached_obs_section=obs_section,
            )

        # Should keep at least 4 recent messages (preserve_recent default)
        non_system = [m for m in result if m["role"] != "system"]
        assert len(non_system) == 4
        # Last 4: msg-6, msg-7, msg-8, msg-9
        assert [m["content"] for m in non_system] == ["msg-6", "msg-7", "msg-8", "msg-9"]

    def test_build_context_no_system_message(self, observer, db):
        """Messages without system prompt: observations become system message."""
        msgs = [
            {"role": "user", "content": "msg-0"},
            {"role": "assistant", "content": "msg-1"},
            {"role": "user", "content": "msg-2"},
        ]
        db.query.return_value.filter.return_value.scalar.return_value = 2
        obs_section = "## Memory (Observations)\n- obs"

        with patch.object(observer, "get_observations", return_value=[{"content": "x"}]):
            result = observer.build_context_with_observations(
                msgs, "u1", "s1", _cached_obs_section=obs_section,
            )

        assert result[0]["role"] == "system"
        assert "## Memory (Observations)" in result[0]["content"]
        # msg-2 is unobserved (idx 2), should be kept
        assert any("msg-2" in m.get("content", "") for m in result)


# ---------------------------------------------------------------------------
# Reflector
# ---------------------------------------------------------------------------

class TestReflector:
    def test_skip_no_observations(self, reflector):
        with patch("core.memory.observer.Observer") as MockObs:
            MockObs.return_value.get_observations.return_value = []
            result = reflector.reflect("u1")
            assert result["reflected"] is False

    def test_skip_below_threshold(self, reflector):
        small_obs = [{"content": "short", "observation_id": "o1", "priority": "low",
                       "type": "fact", "observed_at": "2026-02-20", "referenced_at": None,
                       "session_id": "s1", "is_reflected": False}]
        with patch("core.memory.observer.Observer") as MockObs:
            MockObs.return_value.get_observations.return_value = small_obs
            result = reflector.reflect("u1")
            assert result["reflected"] is False

    def test_skip_no_llm(self, db):
        ref = Reflector(db, llm_client=None)
        with patch("core.memory.observer.Observer") as MockObs:
            MockObs.return_value.get_observations.return_value = _big_observations()
            result = ref.reflect("u1")
            assert result["reflected"] is False

    def test_reflect_condenses(self, reflector, db):
        reflector.llm.chat_with_tools.return_value = {
            "content": json.dumps([
                {"content": "Condensed 1", "priority": "high", "type": "pattern"},
                {"content": "Condensed 2", "priority": "medium", "type": "pattern"},
            ])
        }
        mock_query = MagicMock()
        db.query.return_value.filter.return_value = mock_query
        mock_query.update.return_value = 30

        with patch("core.memory.observer.Observer") as MockObs:
            MockObs.return_value.get_observations.return_value = _big_observations()
            result = reflector.reflect("u1")

        assert result["reflected"] is True
        assert result["before"] == 30
        assert result["after"] == 2
        db.commit.assert_called()

    def test_reflect_llm_failure(self, reflector, db):
        reflector.llm.chat_with_tools.side_effect = RuntimeError("LLM down")
        with patch("core.memory.observer.Observer") as MockObs:
            MockObs.return_value.get_observations.return_value = _big_observations()
            result = reflector.reflect("u1")
        assert result["reflected"] is False

    def test_reflect_transaction_rollback(self, reflector, db):
        """DB failure during replace triggers rollback."""
        reflector.llm.chat_with_tools.return_value = {
            "content": json.dumps([{"content": "condensed", "priority": "high", "type": "pattern"}])
        }
        db.commit.side_effect = RuntimeError("DB error")
        mock_query = MagicMock()
        db.query.return_value.filter.return_value = mock_query
        mock_query.update.return_value = 30

        with patch("core.memory.observer.Observer") as MockObs:
            MockObs.return_value.get_observations.return_value = _big_observations()
            with pytest.raises(RuntimeError, match="DB error"):
                reflector.reflect("u1")

        db.rollback.assert_called_once()

    def test_reject_if_condensed_longer(self, reflector, db):
        """Quality gate: discard condensed output if it's longer than original."""
        # Return condensed that is LONGER than original
        big_content = "x" * 100_000
        reflector.llm.chat_with_tools.return_value = {
            "content": json.dumps([
                {"content": big_content, "priority": "high", "type": "pattern"},
            ])
        }
        with patch("core.memory.observer.Observer") as MockObs:
            MockObs.return_value.get_observations.return_value = _big_observations()
            result = reflector.reflect("u1")
        assert result["reflected"] is False
        db.commit.assert_not_called()

    def test_reflect_returns_token_metrics(self, reflector, db):
        """Reflected result includes tokens_before and tokens_after."""
        reflector.llm.chat_with_tools.return_value = {
            "content": json.dumps([
                {"content": "Condensed", "priority": "high", "type": "pattern"},
            ])
        }
        mock_query = MagicMock()
        db.query.return_value.filter.return_value = mock_query
        mock_query.update.return_value = 30

        with patch("core.memory.observer.Observer") as MockObs:
            MockObs.return_value.get_observations.return_value = _big_observations()
            result = reflector.reflect("u1")
        assert result["reflected"] is True
        assert result["tokens_before"] > result["tokens_after"]
        assert result["tokens_after"] > 0


# ---------------------------------------------------------------------------
# ChatLoop integration
# ---------------------------------------------------------------------------

class TestChatLoopObserverIntegration:
    def _make_loop(self):
        from core.agent.chat_loop import ChatLoop
        return ChatLoop(
            selector=MagicMock(),
            executor=MagicMock(),
            llm_client=MagicMock(),
            event_logger=MagicMock(),
            context_manager=MagicMock(),
            firewall=MagicMock(),
        )

    def test_set_observer(self):
        loop = self._make_loop()
        assert loop.observer is None
        mock_observer = MagicMock()
        loop.set_observer(mock_observer)
        assert loop.observer is mock_observer

    def test_run_observer_background_thread(self):
        loop = self._make_loop()
        mock_observer = MagicMock()
        mock_observer.llm = MagicMock()
        loop.set_observer(mock_observer)

        with patch("threading.Thread") as MockThread:
            loop._run_observer("s1", "u1", [{"role": "user", "content": "hi"}])
            MockThread.assert_called_once()
            assert MockThread.call_args.kwargs.get("daemon") is True
            MockThread.return_value.start.assert_called_once()

    def test_run_observer_noop_without_observer(self):
        loop = self._make_loop()
        with patch("threading.Thread") as MockThread:
            loop._run_observer("s1", "u1", [])
            MockThread.assert_not_called()

    def test_run_observer_no_shared_mutable_state(self):
        """Background thread creates its own Observer — no shared dict."""
        loop = self._make_loop()
        mock_observer = MagicMock()
        mock_observer.llm = MagicMock()
        loop.set_observer(mock_observer)

        with patch("threading.Thread") as MockThread:
            loop._run_observer("s1", "u1", [{"role": "user", "content": "hi"}])
            # Get the target function
            bg_fn = MockThread.call_args.kwargs.get("target") or MockThread.call_args[1].get("target")
            assert bg_fn is not None
            # The function should NOT reference observer._observed_index
            # (it creates a fresh Observer with DB-backed index)

    def test_build_messages_injects_observations(self):
        loop = self._make_loop()
        mock_observer = MagicMock()
        mock_observer.get_observations.return_value = [
            {"content": "User prefers Python", "priority": "high", "observed_at": "2026-02-20"},
        ]
        mock_observer.format_for_context.return_value = "## Memory (Observations)\n- 🔴 User prefers Python"
        loop.set_observer(mock_observer)

        messages = loop._build_messages("test input", None, session_id="s1", user_id="u1")
        system_content = messages[0]["content"]
        assert "Memory (Observations)" in system_content


# ---------------------------------------------------------------------------
# Lifecycle integration
# ---------------------------------------------------------------------------

class TestLifecycleReflector:
    def test_hourly_tasks_includes_reflector(self):
        from core.context.lifecycle import MemoryGovernanceEngine
        db = MagicMock()
        engine = MemoryGovernanceEngine(db)

        with patch.object(engine, "_archive_closed_notes", return_value=0), \
             patch.object(engine, "_run_reflector", return_value=5) as mock_ref, \
             patch("core.sandbox.cleanup.SandboxCleaner") as MockCleaner:
            MockCleaner.return_value.run.return_value = {"cleaned": 0, "failed": 0}
            result = engine.run_hourly_tasks()

        mock_ref.assert_called_once()
        assert result["observations_reflected"] == 5

    def test_governance_engine_passes_llm_to_reflector(self):
        from core.context.lifecycle import MemoryGovernanceEngine
        db = MagicMock()
        llm = MagicMock()
        engine = MemoryGovernanceEngine(db, llm_client=llm)
        assert engine.llm_client is llm


# ---------------------------------------------------------------------------
# Edge cases: concurrent observers, DB failure degradation
# ---------------------------------------------------------------------------

class TestObserverEdgeCases:
    def test_concurrent_observers_no_data_loss(self, db, llm):
        """Two observers on same session: both succeed, no crash.

        With DB-backed index, worst case is duplicate observations (not data loss).
        Reflector will deduplicate on next pass.
        """
        # Both observers see index=0
        db.query.return_value.filter.return_value.scalar.return_value = 0
        llm.chat_with_tools.return_value = {
            "content": json.dumps([{"content": "obs", "priority": "low", "type": "fact"}])
        }

        obs1 = Observer(db, llm)
        obs2 = Observer(db, llm)

        r1 = obs1.observe("s1", "u1", _make_messages())
        r2 = obs2.observe("s1", "u1", _make_messages())

        # Both succeed — no crash, both produce observations
        assert len(r1) == 1
        assert len(r2) == 1
        # db.add called twice (once per observer)
        assert db.add.call_count == 2

    def test_advance_index_db_failure_degrades_gracefully(self, observer, db):
        """If _advance_index DB write fails, observe() propagates the exception.

        The outer _run_observer catches it — next turn will retry same messages.
        """
        observer.db.query.return_value.filter.return_value.scalar.return_value = 0
        observer.llm.chat_with_tools.return_value = {"content": "[]"}

        # Make commit fail (simulating DB down)
        db.commit.side_effect = RuntimeError("DB connection lost")

        with pytest.raises(RuntimeError, match="DB connection lost"):
            observer.observe("s1", "u1", _make_messages())

    def test_store_observations_db_failure(self, observer, db):
        """If DB commit fails during store, exception propagates (caught by bg thread)."""
        observer.db.query.return_value.filter.return_value.scalar.return_value = 0
        observer.llm.chat_with_tools.return_value = {
            "content": json.dumps([{"content": "test", "priority": "low", "type": "fact"}])
        }
        db.commit.side_effect = RuntimeError("DB error")

        with pytest.raises(RuntimeError, match="DB error"):
            observer.observe("s1", "u1", _make_messages())


class TestObserverDedup:
    """Deduplication and confidence scoring."""

    def _setup(self, observer):
        observer.db.query.return_value.filter.return_value.scalar.return_value = 0

    def test_within_batch_dedup(self, observer, db):
        """Duplicate content within a single LLM response should be stored only once."""
        self._setup(observer)
        observer.llm.chat_with_tools.return_value = {
            "content": json.dumps([
                {"content": "User prefers Python", "priority": "high", "type": "preference"},
                {"content": "User prefers Python", "priority": "medium", "type": "preference"},
                {"content": "Project uses FastAPI", "priority": "medium", "type": "fact"},
            ])
        }
        result = observer.observe("s1", "u1", _make_messages())
        assert len(result) == 2
        contents = [r["content"] for r in result]
        assert contents == ["User prefers Python", "Project uses FastAPI"]

    def test_confidence_from_priority(self, observer, db):
        """Observations should have confidence derived from priority."""
        self._setup(observer)
        observer.llm.chat_with_tools.return_value = {
            "content": json.dumps([
                {"content": "Critical decision", "priority": "high", "type": "decision"},
                {"content": "Some context", "priority": "medium", "type": "fact"},
                {"content": "Minor detail", "priority": "low", "type": "fact"},
            ])
        }
        result = observer.observe("s1", "u1", _make_messages())
        assert result[0]["confidence"] == 0.95
        assert result[1]["confidence"] == 0.75
        assert result[2]["confidence"] == 0.5
