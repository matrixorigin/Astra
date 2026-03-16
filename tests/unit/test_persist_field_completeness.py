"""Tests that persisted events/records have all expected fields populated.

Covers:
- _persist_turn_events: llm_response gets model_used and token_usage
- _persist_turn_events: user_query gets correct causal_chain_id
- _persist_turn_events: tool_call events get correct metadata
- _persist_turn_events: tool_result events get correct metadata
- record_ctx_decision_audits: model_used column is set
- record_skill_selection: required fields are populated
- NullableJSON: None stored as SQL NULL, not JSON 'null'
"""

import json
from typing import Any
from unittest.mock import MagicMock, patch

import pytest


# ---------------------------------------------------------------------------
# _persist_turn_events — field completeness
# ---------------------------------------------------------------------------


class TestPersistFieldCompleteness:
    """Verify _persist_turn_events populates all expected fields on each event type."""

    def _run_persist(
        self,
        *,
        tool_calls=None,
        full_text="response",
        model_used="gpt-4o",
        token_usage=None,
        messages=None,
        tool_results=None,
        turn_chain_id=None,
        user_query_event_id=None,
    ):
        """Helper: run _persist_turn_events and capture all EventLogger calls."""
        tool_calls = tool_calls or []
        messages = messages or [{"role": "user", "content": "hello"}]
        captured = {"user_query": None, "llm_response": None, "stream_events": []}

        with (
            patch("api.routers.chat.SessionLocal"),
            patch("core.events.event_logger.EventLogger") as mock_el_cls,
            patch("core.agent.turn_hooks.TurnHooks"),
        ):
            mock_el = MagicMock()

            def capture_user_query(**kw):
                captured["user_query"] = kw
                return MagicMock(event_id="ev1", causal_chain_id="cc1")

            mock_el.create_user_query.side_effect = capture_user_query

            def capture_llm_response(**kw):
                captured["llm_response"] = kw
                return MagicMock()

            mock_el.create_llm_response.side_effect = capture_llm_response

            def capture_stream_event(**kw):
                captured["stream_events"].append(kw)
                return MagicMock()

            mock_el.create_stream_event.side_effect = capture_stream_event

            mock_el_cls.return_value = mock_el

            from api.routers.chat import _persist_turn_events

            _persist_turn_events(
                "u1",
                "s1",
                messages,
                tool_results,
                full_text,
                tool_calls,
                context_capture_id="snap1",
                model_used=model_used,
                token_usage=token_usage,
                turn_chain_id=turn_chain_id,
                user_query_event_id=user_query_event_id,
            )
        return captured

    # -- llm_response fields --

    def test_llm_response_has_model_used(self):
        """llm_response event must include llm_model_used."""
        c = self._run_persist(model_used="gpt-4o")
        assert c["llm_response"]["llm_model_used"] == "gpt-4o"

    def test_llm_response_has_token_usage(self):
        """llm_response event must include token_usage when available."""
        usage = {"prompt": 100, "completion": 50, "total": 150}
        c = self._run_persist(token_usage=usage)
        assert c["llm_response"]["token_usage"] == usage

    def test_llm_response_token_usage_none_when_absent(self):
        """llm_response token_usage is None when no usage data."""
        c = self._run_persist(token_usage=None)
        assert c["llm_response"]["token_usage"] is None

    def test_llm_response_model_used_none_when_absent(self):
        """llm_response llm_model_used is None when model unknown."""
        c = self._run_persist(model_used=None)
        assert c["llm_response"]["llm_model_used"] is None

    def test_llm_response_has_causal_chain(self):
        """llm_response must have causal_chain_id linking to user_query."""
        c = self._run_persist()
        assert c["llm_response"]["causal_chain_id"] == "cc1"

    def test_llm_response_has_parent_event(self):
        """llm_response must have parent_event_id linking to user_query."""
        c = self._run_persist()
        assert c["llm_response"]["parent_event_id"] == "ev1"

    def test_llm_response_has_agent_fields(self):
        """llm_response must have agent_id and agent_version."""
        c = self._run_persist()
        assert c["llm_response"]["agent_id"] == "dev-agent"
        assert c["llm_response"]["agent_version"] == "0.1.0"

    # -- user_query fields --

    def test_user_query_has_content(self):
        """user_query event must have the user's message content."""
        c = self._run_persist(messages=[{"role": "user", "content": "test query"}])
        assert c["user_query"]["content"] == "test query"

    def test_user_query_has_session_and_user(self):
        """user_query must have session_id and user_id."""
        c = self._run_persist()
        assert c["user_query"]["user_id"] == "u1"
        assert c["user_query"]["session_id"] == "s1"

    # -- turn_chain_id propagation --

    def test_turn_chain_id_used_when_provided(self):
        """When turn_chain_id is passed, create_user_query receives it."""
        c = self._run_persist(turn_chain_id="pre-generated-chain")
        assert c["user_query"]["causal_chain_id"] == "pre-generated-chain"

    def test_user_query_event_id_used_when_provided(self):
        """Pre-generated user_query_event_id is forwarded to create_user_query.

        Regression: event_id was generated inside the persist thread, so its
        uuid7 timestamp reflected persist time (potentially Turn N+1) rather
        than the time the turn actually started.
        """
        c = self._run_persist(user_query_event_id="pre-generated-event-id")
        assert c["user_query"]["event_id"] == "pre-generated-event-id"

    def test_user_query_event_id_none_when_not_provided(self):
        """When user_query_event_id is not passed, event_id is None (EventLogger generates it)."""
        c = self._run_persist()
        assert c["user_query"].get("event_id") is None

    # -- tool_call event fields --

    def test_tool_call_event_has_metadata(self):
        """tool_call events must have tool_call_id and name in metadata."""
        tc = [{"id": "tc1", "function": {"name": "read_file", "arguments": '{"path":"a.txt"}'}}]
        c = self._run_persist(tool_calls=tc)
        tc_events = [e for e in c["stream_events"] if e["event_type"] == "tool_call"]
        assert len(tc_events) == 1
        assert tc_events[0]["metadata"]["tool_call_id"] == "tc1"
        assert tc_events[0]["metadata"]["name"] == "read_file"
        assert tc_events[0]["skill_name"] == "read_file"

    def test_tool_call_event_content_is_valid_json(self):
        """tool_call event content must be parseable JSON with expected keys."""
        tc = [{"id": "tc1", "function": {"name": "bash", "arguments": '{"cmd":"ls"}'}}]
        c = self._run_persist(tool_calls=tc)
        tc_events = [e for e in c["stream_events"] if e["event_type"] == "tool_call"]
        content = json.loads(tc_events[0]["content"])
        assert content["tool_call_id"] == "tc1"
        assert content["name"] == "bash"
        assert content["arguments"] == '{"cmd":"ls"}'

    def test_tool_call_event_has_causal_chain(self):
        """tool_call events must share causal_chain_id with user_query."""
        tc = [{"id": "tc1", "function": {"name": "read_file", "arguments": "{}"}}]
        c = self._run_persist(tool_calls=tc)
        tc_events = [e for e in c["stream_events"] if e["event_type"] == "tool_call"]
        assert tc_events[0]["causal_chain_id"] == "cc1"

    def test_tool_events_have_skill_version_when_registry_available(self):
        """tool_call and tool_result events must include skill_version from registry."""
        # Clear module-level cache to ensure this test hits the mocked DB
        import api.routers.chat as _chat_mod

        _chat_mod._SKILL_VERSION_CACHE.clear()

        captured = {"stream_events": []}

        # Mock ORM query: db.query(SR.skill_name, SR.version).filter(...).order_by(...).all()
        mock_db = MagicMock()
        mock_query_chain = mock_db.query.return_value.filter.return_value.order_by.return_value
        mock_query_chain.all.return_value = [("read_file", "1.2.0")]

        mock_session_local = MagicMock()
        mock_session_local.return_value.__enter__ = MagicMock(return_value=mock_db)
        mock_session_local.return_value.__exit__ = MagicMock(return_value=False)

        with (
            patch("api.routers.chat.SessionLocal", mock_session_local),
            patch("core.events.event_logger.EventLogger") as mock_el_cls,
            patch("core.agent.turn_hooks.TurnHooks"),
            patch("core.context.manager.ContextManager.update_snapshot_llm_ids"),
        ):
            mock_el = MagicMock()
            mock_el.create_user_query.return_value = MagicMock(
                event_id="ev1", causal_chain_id="cc1"
            )
            mock_el.create_llm_response.return_value = MagicMock(event_id="ev_llm")

            def capture_stream(**kw):
                captured["stream_events"].append(kw)
                return MagicMock()

            mock_el.create_stream_event.side_effect = capture_stream
            mock_el_cls.return_value = mock_el

            from api.routers.chat import _persist_turn_events

            _persist_turn_events(
                "u1",
                "s1",
                [{"role": "user", "content": "hi"}],
                [{"tool_call_id": "tc1", "name": "read_file", "result": "ok"}],
                "done",
                [{"id": "tc1", "function": {"name": "read_file", "arguments": "{}"}}],
                context_capture_id="snap1",
                model_used="gpt-4o",
            )

        tc_events = [e for e in captured["stream_events"] if e["event_type"] == "tool_call"]
        tr_events = [e for e in captured["stream_events"] if e["event_type"] == "tool_result"]
        assert tc_events[0]["skill_version"] == "1.2.0"
        assert tr_events[0]["skill_version"] == "1.2.0"

    # -- tool_result event fields --

    def test_tool_result_event_has_metadata(self):
        """tool_result events must have source, tool_call_id, name in metadata."""
        tr = [{"tool_call_id": "tc1", "name": "read_file", "result": "file content"}]
        c = self._run_persist(tool_results=tr, messages=[])
        tr_events = [e for e in c["stream_events"] if e["event_type"] == "tool_result"]
        assert len(tr_events) == 1
        meta = tr_events[0]["metadata"]
        assert meta["source"] == "edge"
        assert meta["tool_call_id"] == "tc1"
        assert meta["name"] == "read_file"
        assert tr_events[0]["skill_name"] == "read_file"

    def test_tool_result_content_truncated_at_audit_limit(self):
        """tool_result content should truncate result to audit limit."""
        from core.context.compaction import MAX_TOOL_RESULT_AUDIT_CHARS

        long_result = "x" * (MAX_TOOL_RESULT_AUDIT_CHARS + 1000)
        tr = [{"tool_call_id": "tc1", "name": "bash", "result": long_result}]
        c = self._run_persist(tool_results=tr, messages=[])
        tr_events = [e for e in c["stream_events"] if e["event_type"] == "tool_result"]
        content = json.loads(tr_events[0]["content"])
        assert len(content["result"]) == MAX_TOOL_RESULT_AUDIT_CHARS

    def test_introspection_tool_result_flagged(self):
        """get_agent_info tool_result should have introspection=True in metadata."""
        tr = [{"tool_call_id": "tc1", "name": "get_agent_info", "result": "info"}]
        c = self._run_persist(tool_results=tr, messages=[])
        tr_events = [e for e in c["stream_events"] if e["event_type"] == "tool_result"]
        assert tr_events[0]["metadata"]["introspection"] is True


# ---------------------------------------------------------------------------
# TurnHooks.record_ctx_decision_audits — field completeness
# ---------------------------------------------------------------------------


class TestDecisionAuditFields:
    """Verify DecisionAudit records have all expected fields populated."""

    def _run_audit(self, tool_calls=None, model_used="gpt-4o", context_id="snap1"):
        tool_calls = tool_calls or []
        captured = {}

        with patch("core.agent.turn_hooks.TurnHooks._db") as mock_db_ctx:
            mock_db = MagicMock()
            mock_db_ctx.return_value.__enter__ = MagicMock(return_value=mock_db)
            mock_db_ctx.return_value.__exit__ = MagicMock(return_value=False)

            def capture_add(obj):
                for col in [
                    "decision_id",
                    "session_id",
                    "event_id",
                    "decision_type",
                    "decision_output",
                    "context_capture_id",
                    "model_used",
                ]:
                    captured[col] = getattr(obj, col, None)

            mock_db.add.side_effect = capture_add

            from core.agent.turn_hooks import TurnHooks

            hooks = TurnHooks(MagicMock())
            hooks.record_ctx_decision_audits(
                "s1",
                "ev1",
                tool_calls,
                "response text",
                context_id,
                model_used=model_used,
            )
        return captured

    def test_model_used_column_set(self):
        """DecisionAudit.model_used column must be set (not just in JSON)."""
        c = self._run_audit(model_used="claude-3.5-sonnet")
        assert c["model_used"] == "claude-3.5-sonnet"

    def test_model_used_in_decision_output(self):
        """model_used should also be in decision_output JSON for backward compat."""
        c = self._run_audit(model_used="gpt-4o")
        assert c["decision_output"]["model_used"] == "gpt-4o"

    def test_decision_type_tool_selection(self):
        """With tool_calls → decision_type should be 'tool_selection'."""
        tc = [{"function": {"name": "read_file"}}]
        c = self._run_audit(tool_calls=tc)
        assert c["decision_type"] == "tool_selection"

    def test_decision_type_response_generation(self):
        """Without tool_calls → decision_type should be 'response_generation'."""
        c = self._run_audit(tool_calls=[])
        assert c["decision_type"] == "response_generation"

    def test_context_capture_id_set(self):
        """context_capture_id must link to the snapshot."""
        c = self._run_audit(context_id="snap-abc")
        assert c["context_capture_id"] == "snap-abc"

    def test_event_id_set(self):
        """event_id must be set."""
        c = self._run_audit()
        assert c["event_id"] == "ev1"

    def test_decision_output_has_tool_names(self):
        """decision_output should list tool call names."""
        tc = [{"function": {"name": "bash"}}, {"function": {"name": "read_file"}}]
        c = self._run_audit(tool_calls=tc)
        assert c["decision_output"]["tool_calls"] == ["bash", "read_file"]

    def test_decision_output_text_truncated(self):
        """decision_output text should be truncated to 500 chars."""
        # The function truncates to [:500]
        long_text = "x" * 1000
        with patch("core.agent.turn_hooks.TurnHooks._db") as mock_db_ctx:
            mock_db = MagicMock()
            mock_db_ctx.return_value.__enter__ = MagicMock(return_value=mock_db)
            mock_db_ctx.return_value.__exit__ = MagicMock(return_value=False)
            captured = {}

            def capture_add(obj):
                captured["decision_output"] = obj.decision_output

            mock_db.add.side_effect = capture_add

            from core.agent.turn_hooks import TurnHooks

            hooks = TurnHooks(MagicMock())
            hooks.record_ctx_decision_audits("s1", "ev1", [], long_text, "snap1")
        assert len(captured["decision_output"]["text"]) == 500


# ---------------------------------------------------------------------------
# TurnHooks.record_skill_selection — field completeness
# ---------------------------------------------------------------------------


class TestSkillSelectionFields:
    """Verify SkillSelectionEvent records have all expected fields."""

    def _run_selection(self, tool_calls, user_content="test query"):
        captured = {}

        with patch("core.agent.turn_hooks.TurnHooks._db") as mock_db_ctx:
            mock_db = MagicMock()
            mock_db_ctx.return_value.__enter__ = MagicMock(return_value=mock_db)
            mock_db_ctx.return_value.__exit__ = MagicMock(return_value=False)

            def capture_add(obj):
                for col in [
                    "event_id",
                    "session_id",
                    "user_query",
                    "selected_skills",
                    "skill_name",
                    "selection_method",
                ]:
                    captured[col] = getattr(obj, col, None)

            mock_db.add.side_effect = capture_add

            from core.agent.turn_hooks import TurnHooks

            hooks = TurnHooks(MagicMock())
            hooks.record_skill_selection("s1", user_content, tool_calls)
        return captured

    def test_returns_none_for_no_tools(self):
        """No tool_calls → no event, returns None."""
        from core.agent.turn_hooks import TurnHooks

        hooks = TurnHooks(MagicMock())
        assert hooks.record_skill_selection("s1", "query", []) is None

    def test_selected_skills_populated(self):
        """selected_skills should list all tool names."""
        tc = [{"function": {"name": "bash"}}, {"function": {"name": "read_file"}}]
        c = self._run_selection(tc)
        assert c["selected_skills"] == ["bash", "read_file"]

    def test_skill_name_is_first_tool(self):
        """skill_name should be the first tool called."""
        tc = [{"function": {"name": "bash"}}, {"function": {"name": "read_file"}}]
        c = self._run_selection(tc)
        assert c["skill_name"] == "bash"

    def test_selection_method_set(self):
        """selection_method should be 'llm_tool_choice'."""
        tc = [{"function": {"name": "bash"}}]
        c = self._run_selection(tc)
        assert c["selection_method"] == "llm_tool_choice"

    def test_user_query_truncated(self):
        """user_query should be truncated to 2000 chars."""
        tc = [{"function": {"name": "bash"}}]
        long_query = "q" * 3000
        c = self._run_selection(tc, user_content=long_query)
        assert len(c["user_query"]) == 2000

    def test_session_id_set(self):
        """session_id must be set."""
        tc = [{"function": {"name": "bash"}}]
        c = self._run_selection(tc)
        assert c["session_id"] == "s1"

    def test_event_id_is_uuid(self):
        """event_id must be a non-empty string (uuid7)."""
        tc = [{"function": {"name": "bash"}}]
        c = self._run_selection(tc)
        assert c["event_id"] is not None
        assert len(c["event_id"]) > 0


# ---------------------------------------------------------------------------
# NullableJSON type — None → SQL NULL, not JSON 'null'
# ---------------------------------------------------------------------------


# ---------------------------------------------------------------------------
# _persist_turn_events — context snapshot linking
# ---------------------------------------------------------------------------


class TestSnapshotLinking:
    """Verify _persist_turn_events links LLM IDs to context snapshot."""

    def test_snapshot_llm_ids_linked(self):
        """update_snapshot_llm_ids called with correct snapshot_id and event IDs."""
        called_with: dict[str, Any] = {}

        with (
            patch("api.routers.chat.SessionLocal") as mock_sl,
            patch("core.events.event_logger.EventLogger") as mock_el_cls,
            patch("core.agent.turn_hooks.TurnHooks"),
            patch("core.context.manager.ContextManager.update_snapshot_llm_ids") as mock_link,
        ):
            mock_el = MagicMock()
            mock_el.create_user_query.return_value = MagicMock(
                event_id="ev1", causal_chain_id="cc1"
            )
            mock_el.create_llm_response.return_value = MagicMock(event_id="ev_llm")
            mock_el_cls.return_value = mock_el

            def capture_link(*a, **kw):
                called_with["args"] = a
                called_with.update(kw)

            mock_link.side_effect = capture_link

            from api.routers.chat import _persist_turn_events

            _persist_turn_events(
                "u1",
                "s1",
                [{"role": "user", "content": "hi"}],
                None,
                "response",
                [],
                context_capture_id="snap1",
            )

        mock_link.assert_called_once()
        # Verify db_factory and snapshot_id positional args
        assert called_with["args"][0] is mock_sl  # db_factory
        assert called_with["args"][1] == "snap1"  # context_capture_id
        assert called_with["llm_request_id"] == "ev1"
        assert called_with["llm_response_id"] == "ev_llm"

    def test_snapshot_not_linked_when_no_snapshot(self):
        """No linking when context_capture_id is None."""
        with (
            patch("api.routers.chat.SessionLocal"),
            patch("core.events.event_logger.EventLogger") as mock_el_cls,
            patch("core.agent.turn_hooks.TurnHooks"),
            patch("core.context.manager.ContextManager.update_snapshot_llm_ids") as mock_link,
        ):
            mock_el = MagicMock()
            mock_el.create_user_query.return_value = MagicMock(
                event_id="ev1", causal_chain_id="cc1"
            )
            mock_el.create_llm_response.return_value = MagicMock(event_id="ev_llm")
            mock_el_cls.return_value = mock_el

            from api.routers.chat import _persist_turn_events

            _persist_turn_events(
                "u1",
                "s1",
                [{"role": "user", "content": "hi"}],
                None,
                "response",
                [],
                context_capture_id=None,
            )

        mock_link.assert_not_called()

    def test_snapshot_not_linked_when_no_user_query(self):
        """No linking when parent_event_id is None (no user content)."""
        with (
            patch("api.routers.chat.SessionLocal"),
            patch("core.events.event_logger.EventLogger") as mock_el_cls,
            patch("core.agent.turn_hooks.TurnHooks"),
            patch("core.context.manager.ContextManager.update_snapshot_llm_ids") as mock_link,
        ):
            mock_el = MagicMock()
            mock_el.create_llm_response.return_value = MagicMock(event_id="ev_llm")
            mock_el_cls.return_value = mock_el

            from api.routers.chat import _persist_turn_events

            _persist_turn_events(
                "u1",
                "s1",
                [{"role": "system", "content": "you are helpful"}],
                None,
                "response",
                [],
                context_capture_id="snap1",
            )

        mock_link.assert_not_called()


class TestNullableJSON:
    """Verify NullableJSON stores Python None as SQL NULL."""

    def test_none_returns_none_from_bind_processor(self):
        """bind_processor(None) must return None (SQL NULL)."""
        from api.models._types import NullableJSON
        from unittest.mock import MagicMock

        nj = NullableJSON()
        dialect = MagicMock()
        nj.impl_instance = MagicMock()
        nj.impl_instance.bind_processor.return_value = lambda v: "null" if v is None else str(v)

        processor = nj.bind_processor(dialect)
        assert processor(None) is None  # SQL NULL, not 'null'

    def test_dict_passes_through_bind_processor(self):
        """bind_processor({'key': 'val'}) delegates to impl."""
        from api.models._types import NullableJSON
        from unittest.mock import MagicMock

        nj = NullableJSON()
        dialect = MagicMock()
        nj.impl_instance = MagicMock()
        nj.impl_instance.bind_processor.return_value = lambda v: f"json:{v}"

        processor = nj.bind_processor(dialect)
        assert processor({"key": "val"}) == "json:{'key': 'val'}"

    def test_no_process_result_value_override(self):
        """NullableJSON must NOT override process_result_value — no backward compat."""
        from api.models._types import NullableJSON

        assert "process_result_value" not in NullableJSON.__dict__


class TestSessionActivityUpdate:
    """Phase 5: _persist_turn_events updates session event_count."""

    def test_event_count_updated_after_persist(self):
        """session event_count is atomically incremented by events produced."""
        mock_db = MagicMock()

        with (
            patch("api.routers.chat.SessionLocal", return_value=mock_db),
            patch("core.events.event_logger.EventLogger") as mock_el_cls,
            patch("core.agent.turn_hooks.TurnHooks"),
        ):
            mock_el = MagicMock()
            mock_el.create_user_query.return_value = MagicMock(
                event_id="ev1",
                causal_chain_id="cc1",
            )
            mock_el.create_llm_response.return_value = MagicMock()
            mock_el.create_stream_event.return_value = MagicMock()
            mock_el_cls.return_value = mock_el

            from api.routers.chat import _persist_turn_events

            _persist_turn_events(
                "u1",
                "s1",
                [{"role": "user", "content": "hi"}],
                None,
                "response",
                [],
            )

        # Phase 5 issues UPDATE event_count = event_count + :n
        # 1 user_query + 1 llm_response = 2 events
        all_calls = mock_db.execute.call_args_list
        update_call = all_calls[-1]
        update_params = update_call[1][0] if update_call[1] else update_call[0][1]
        assert update_params["n"] == 2
        assert update_params["sid"] == "s1"


    def test_last_event_id_updated_after_persist(self):
        """Phase 5 UPDATE must include last_event_id = COALESCE(:last_eid, last_event_id)."""
        mock_db = MagicMock()

        with (
            patch("api.routers.chat.SessionLocal", return_value=mock_db),
            patch("core.events.event_logger.EventLogger") as mock_el_cls,
            patch("core.agent.turn_hooks.TurnHooks"),
        ):
            mock_el = MagicMock()
            llm_ev = MagicMock()
            llm_ev.event_id = "ev-llm-42"
            mock_el.create_user_query.return_value = MagicMock(
                event_id="ev-user-1", causal_chain_id="cc1"
            )
            mock_el.create_llm_response.return_value = llm_ev
            mock_el.create_stream_event.return_value = MagicMock()
            mock_el_cls.return_value = mock_el

            from api.routers.chat import _persist_turn_events

            _persist_turn_events(
                "u1",
                "s1",
                [{"role": "user", "content": "hi"}],
                None,
                "response",
                [],
            )

        all_calls = mock_db.execute.call_args_list
        update_call = all_calls[-1]
        update_params = update_call[1][0] if update_call[1] else update_call[0][1]
        assert "last_eid" in update_params, "Phase 5 must pass last_eid to UPDATE"
        assert update_params["last_eid"] is not None, "last_eid must not be None when LLM responded"
        # The SQL must reference last_event_id
        sql_text = str(update_call[0][0])
        assert "last_event_id" in sql_text


class TestGetMemoriaStorageImpersonation:
    """get_memoria_storage must reject api_key-only config (cannot impersonate user_id)."""

    def test_api_key_only_raises(self):
        """MEMORIA_API_KEY without MEMORIA_MASTER_KEY must raise RuntimeError."""
        from core.memory.backends import get_memoria_storage
        from core.config import MemoriaConfig

        cfg = MemoriaConfig(base_url="http://localhost:8100", api_key="sk-test", master_key=None)
        with patch("core.config.get_memoria_config", return_value=cfg):
            with pytest.raises(RuntimeError, match="MEMORIA_MASTER_KEY"):
                get_memoria_storage("alice")

    def test_master_key_succeeds(self):
        """MEMORIA_MASTER_KEY must allow creating storage without error."""
        from core.memory.backends import get_memoria_storage
        from core.config import MemoriaConfig

        cfg = MemoriaConfig(
            base_url="http://localhost:8100", master_key="mk-test", api_key=None
        )
        with patch("core.config.get_memoria_config", return_value=cfg):
            svc = get_memoria_storage("alice")
        assert svc is not None

    def test_no_auth_raises(self):
        """No auth key at all must raise RuntimeError."""
        from core.memory.backends import get_memoria_storage
        from core.config import MemoriaConfig

        cfg = MemoriaConfig(base_url="http://localhost:8100", master_key=None, api_key=None)
        with patch("core.config.get_memoria_config", return_value=cfg):
            with pytest.raises(RuntimeError):
                get_memoria_storage("alice")


class TestCloseSessionSummaryFields:
    """close_session must write summary_status and summary_job_id to DB."""

    def _run_close(self, session_row, mock_svc):
        """Run close_session with all side-effects patched except the summary path."""
        from core.events.session_manager import SessionManager

        db = MagicMock()
        db.query.return_value.filter.return_value.first.return_value = session_row
        # events query (order_by chain)
        user_ev = MagicMock()
        user_ev.event_type = "user_query"
        user_ev.content = "hello"
        user_ev.causal_chain_id = "cc1"
        llm_ev = MagicMock()
        llm_ev.event_type = "llm_response"
        llm_ev.content = "world"
        llm_ev.causal_chain_id = "cc1"
        db.query.return_value.filter.return_value.order_by.return_value.limit.return_value.all.return_value = [
            user_ev,
            llm_ev,
        ]

        with (
            patch("core.memory.backends.get_memoria_storage", return_value=mock_svc),
            patch("core.evaluation.multi_level_scorer.score_session"),
            patch("skills.knowledge.api.KnowledgeExtractor"),
            patch("core.events.event_logger.EventLogger.from_session"),
        ):
            mgr = SessionManager.__new__(SessionManager)
            mgr._db_session = db
            mgr._get_session = lambda: db
            mgr._owns_session = False
            mgr.close_session("sess1")

        return session_row, db

    def test_summary_status_set_to_pending_on_close(self):
        """close_session must set summary_status='pending' after requesting summary."""
        session_row = MagicMock()
        session_row.session_id = "sess1"
        session_row.user_id = "alice"
        session_row.summary_status = None
        session_row.summary_job_id = None

        mock_svc = MagicMock()
        mock_svc.request_session_summary.return_value = {"task_id": "task-999"}

        row, db = self._run_close(session_row, mock_svc)

        assert row.summary_status == "pending"
        assert row.summary_job_id == "task-999"
        db.commit.assert_called()

    def test_summary_job_id_none_when_no_task_id(self):
        """If Memoria returns no task_id, summary_job_id stays None but status is pending."""
        session_row = MagicMock()
        session_row.session_id = "sess1"
        session_row.user_id = "alice"
        session_row.summary_status = None
        session_row.summary_job_id = None

        mock_svc = MagicMock()
        mock_svc.request_session_summary.return_value = {}  # no task_id

        row, db = self._run_close(session_row, mock_svc)

        assert row.summary_status == "pending"
        assert row.summary_job_id is None
