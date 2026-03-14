"""Real E2E tests for Tool Result Quality Firewall.

These tests exercise the ACTUAL code paths in chat.py:
  - _build_turn_messages: quality assessment → annotation → history merge
  - _persist_turn_events: Phase 2b → tool_result_quality event emission

Unlike test_tool_quality_e2e.py which only tests pure functions in isolation,
these tests call the real chat.py functions with mocked DB/cache dependencies.
"""

from __future__ import annotations

import json
from unittest.mock import MagicMock, patch

import pytest


# ---------------------------------------------------------------------------
# Helper: build a session cache entry with history containing a pending
# tool_call, so _build_turn_messages will merge tool_results into it.
# ---------------------------------------------------------------------------


def _make_cache_entry_with_pending_tool_call(tool_call_id: str = "tc_019ca950"):
    """Session cache entry where the last assistant message has a pending tool_call."""
    return {
        "history": [
            {"role": "system", "content": "You are a helpful assistant."},
            {"role": "user", "content": "中信证券建议买吗？"},
            {
                "role": "assistant",
                "content": "",
                "tool_calls": [
                    {
                        "id": tool_call_id,
                        "type": "function",
                        "function": {"name": "stock_assistant", "arguments": '{"code":"600030"}'},
                    },
                ],
            },
        ],
        "tools": [],
        "turn_count": 1,
        "sections": {"identity": "system"},
    }


# ---------------------------------------------------------------------------
# Test 1: _build_turn_messages annotates degraded tool results in history
# ---------------------------------------------------------------------------


class TestBuildTurnMessagesQualityAnnotation:
    """Verify that _build_turn_messages runs quality assessment and the
    annotation actually appears in the history that gets sent to the LLM."""

    def test_degraded_result_annotated_in_merged_history(self):
        """019ca950 pattern: stock_assistant returns empty data.
        After _build_turn_messages, the tool message in history must contain
        the quality annotation."""
        session_id = "test-session-quality-e2e"
        tool_call_id = "tc_019ca950"

        degraded_result = json.dumps(
            {
                "stock_code": "600030",
                "stock_name": "中信证券",
                "current_price": 0,
                "price_change": 0,
                "technical_indicators": {},
                "trend_analysis": {},
                "risk_score": 0,
                "risk_factors": [],
                "recommendation": "",
                "confidence": 50,
            }
        )

        tool_results = [
            {"name": "stock_assistant", "tool_call_id": tool_call_id, "result": degraded_result},
        ]

        cache_entry = _make_cache_entry_with_pending_tool_call(tool_call_id)

        with (
            patch("api.routers.chat._session_cache") as mock_cache,
            patch("api.routers.chat.SessionLocal"),
        ):
            mock_cache.get.return_value = cache_entry
            mock_cache.__setitem__ = MagicMock()
            mock_cache.__contains__ = lambda self, k: True

            from api.routers.chat import _build_turn_messages

            history, _, _ = _build_turn_messages(
                db=MagicMock(),
                user_id="test-user",
                session_id=session_id,
                messages=[],  # no new user message this turn
                tool_results=tool_results,
                project_rules=None,
            )

        # Find the tool message in history
        tool_msgs = [m for m in history if m.get("role") == "tool"]
        assert len(tool_msgs) >= 1, (
            f"Expected tool message in history, got: {[m['role'] for m in history]}"
        )

        tool_content = tool_msgs[0]["content"]
        assert "[TOOL QUALITY:" in tool_content, (
            f"Quality annotation missing from tool message content.\nContent: {tool_content[:200]}"
        )
        assert "Respond honestly" in tool_content

        # Verify assessments were stored in cache entry
        assessments = cache_entry.get("tool_quality_assessments", [])
        assert len(assessments) == 1
        assert assessments[0]["grade"] != "complete"
        assert assessments[0]["tool_name"] == "stock_assistant"

    def test_healthy_result_not_annotated_in_merged_history(self):
        """A complete tool result should pass through without annotation."""
        session_id = "test-session-healthy-e2e"
        tool_call_id = "tc_healthy"

        healthy_result = json.dumps(
            {
                "temperature": 22.5,
                "humidity": 65,
                "wind_speed": 12,
                "city": "Beijing",
                "forecast": "sunny",
            }
        )

        tool_results = [
            {"name": "weather", "tool_call_id": tool_call_id, "result": healthy_result},
        ]

        cache_entry = {
            "history": [
                {"role": "system", "content": "system"},
                {"role": "user", "content": "What's the weather?"},
                {
                    "role": "assistant",
                    "content": "",
                    "tool_calls": [
                        {
                            "id": tool_call_id,
                            "type": "function",
                            "function": {"name": "weather", "arguments": "{}"},
                        },
                    ],
                },
            ],
            "tools": [],
            "turn_count": 1,
            "sections": {"identity": "system"},
        }

        with (
            patch("api.routers.chat._session_cache") as mock_cache,
            patch("api.routers.chat.SessionLocal"),
        ):
            mock_cache.get.return_value = cache_entry
            mock_cache.__setitem__ = MagicMock()

            from api.routers.chat import _build_turn_messages

            history, _, _ = _build_turn_messages(
                db=MagicMock(),
                user_id="u",
                session_id=session_id,
                messages=[],
                tool_results=tool_results,
                project_rules=None,
            )

        tool_msgs = [m for m in history if m.get("role") == "tool"]
        assert len(tool_msgs) >= 1
        assert "[TOOL QUALITY:" not in tool_msgs[0]["content"]

    def test_feature_flag_off_skips_annotation(self):
        """When ENABLE_TOOL_QUALITY_FIREWALL=false, no annotation happens."""
        session_id = "test-session-flag-off"
        tool_call_id = "tc_flag"

        tool_results = [
            {
                "name": "stock_assistant",
                "tool_call_id": tool_call_id,
                "result": json.dumps({"data": {}, "info": {}}),
            },
        ]

        cache_entry = _make_cache_entry_with_pending_tool_call(tool_call_id)

        with (
            patch("api.routers.chat._session_cache") as mock_cache,
            patch("api.routers.chat.SessionLocal"),
            patch("api.routers.chat._TOOL_QUALITY_ENABLED", False),
        ):
            mock_cache.get.return_value = cache_entry
            mock_cache.__setitem__ = MagicMock()

            from api.routers.chat import _build_turn_messages

            history, _, _ = _build_turn_messages(
                db=MagicMock(),
                user_id="u",
                session_id=session_id,
                messages=[],
                tool_results=tool_results,
                project_rules=None,
            )

        tool_msgs = [m for m in history if m.get("role") == "tool"]
        assert len(tool_msgs) >= 1
        assert "[TOOL QUALITY:" not in tool_msgs[0]["content"]
        # No assessments stored
        assert cache_entry.get("tool_quality_assessments", []) == []


# ---------------------------------------------------------------------------
# Test 2: _persist_turn_events emits tool_result_quality events
# ---------------------------------------------------------------------------


class TestPersistTurnEventsQualityEvent:
    """Verify Phase 2b in _persist_turn_events actually calls create_stream_event
    with event_type='tool_result_quality' for degraded assessments."""

    def test_quality_event_emitted_for_degraded_assessment(self):
        """When session cache has a degraded assessment, _persist_turn_events
        should emit a tool_result_quality event."""
        session_id = "test-persist-quality"
        created_events = []

        # Pre-populate session cache with degraded assessment
        fake_cache = {
            session_id: {
                "tool_quality_assessments": [
                    {
                        "tool_name": "stock_assistant",
                        "score": 0.3,
                        "grade": "degraded",
                        "signals": ["empty_containers: 3/4 fields empty"],
                        "stale": False,
                    }
                ],
            },
        }

        with (
            patch("api.routers.chat.SessionLocal") as mock_sl,
            patch("core.events.event_logger.EventLogger") as mock_el_cls,
            patch("core.agent.turn_hooks.TurnHooks"),
            patch("api.routers.chat._session_cache", fake_cache),
            patch("api.routers.chat._TOOL_QUALITY_ENABLED", True),
        ):
            mock_el = MagicMock()
            mock_el.create_user_query.return_value = MagicMock(
                event_id="ev1",
                causal_chain_id="cc1",
            )
            mock_el.create_stream_event.side_effect = lambda **kw: (
                created_events.append(kw) or MagicMock()
            )
            mock_el.create_llm_response.return_value = MagicMock()
            mock_el_cls.return_value = mock_el

            from api.routers.chat import _persist_turn_events

            _persist_turn_events(
                user_id="u1",
                session_id=session_id,
                messages=[{"role": "user", "content": "中信证券建议买吗？"}],
                tool_results=[{"name": "stock_assistant", "tool_call_id": "tc1", "result": "{}"}],
                full_text="建议持有",
                tool_calls=[],
            )

        # Find the quality event
        quality_events = [e for e in created_events if e.get("event_type") == "tool_result_quality"]
        assert len(quality_events) == 1, (
            f"Expected 1 tool_result_quality event, got {len(quality_events)}. "
            f"All events: {[e.get('event_type') for e in created_events]}"
        )
        content = json.loads(quality_events[0]["content"])
        assert content["tool_name"] == "stock_assistant"
        assert content["grade"] == "degraded"
        assert content["score"] == 0.3

    def test_no_quality_event_when_all_complete(self):
        """When all assessments are 'complete', no quality event is emitted."""
        session_id = "test-persist-complete"
        created_events = []

        fake_cache = {
            session_id: {
                "tool_quality_assessments": [
                    {
                        "tool_name": "weather",
                        "score": 1.0,
                        "grade": "complete",
                        "signals": [],
                        "stale": False,
                    }
                ],
            },
        }

        with (
            patch("api.routers.chat.SessionLocal"),
            patch("core.events.event_logger.EventLogger") as mock_el_cls,
            patch("core.agent.turn_hooks.TurnHooks"),
            patch("api.routers.chat._session_cache", fake_cache),
            patch("api.routers.chat._TOOL_QUALITY_ENABLED", True),
        ):
            mock_el = MagicMock()
            mock_el.create_user_query.return_value = MagicMock(
                event_id="ev1",
                causal_chain_id="cc1",
            )
            mock_el.create_stream_event.side_effect = lambda **kw: (
                created_events.append(kw) or MagicMock()
            )
            mock_el.create_llm_response.return_value = MagicMock()
            mock_el_cls.return_value = mock_el

            from api.routers.chat import _persist_turn_events

            _persist_turn_events(
                user_id="u1",
                session_id=session_id,
                messages=[{"role": "user", "content": "weather?"}],
                tool_results=[{"name": "weather", "tool_call_id": "tc1", "result": "{}"}],
                full_text="It's sunny",
                tool_calls=[],
            )

        quality_events = [e for e in created_events if e.get("event_type") == "tool_result_quality"]
        assert len(quality_events) == 0

    def test_no_quality_event_when_flag_disabled(self):
        """Feature flag off → no quality events regardless of assessment."""
        session_id = "test-persist-disabled"
        created_events = []

        fake_cache = {
            session_id: {
                "tool_quality_assessments": [
                    {
                        "tool_name": "stock_assistant",
                        "score": 0.0,
                        "grade": "empty",
                        "signals": ["all_empty"],
                        "stale": False,
                    }
                ],
            },
        }

        with (
            patch("api.routers.chat.SessionLocal"),
            patch("core.events.event_logger.EventLogger") as mock_el_cls,
            patch("core.agent.turn_hooks.TurnHooks"),
            patch("api.routers.chat._session_cache", fake_cache),
            patch("api.routers.chat._TOOL_QUALITY_ENABLED", False),
        ):
            mock_el = MagicMock()
            mock_el.create_user_query.return_value = MagicMock(
                event_id="ev1",
                causal_chain_id="cc1",
            )
            mock_el.create_stream_event.side_effect = lambda **kw: (
                created_events.append(kw) or MagicMock()
            )
            mock_el.create_llm_response.return_value = MagicMock()
            mock_el_cls.return_value = mock_el

            from api.routers.chat import _persist_turn_events

            _persist_turn_events(
                user_id="u1",
                session_id=session_id,
                messages=[{"role": "user", "content": "test"}],
                tool_results=[],
                full_text="ok",
                tool_calls=[],
            )

        quality_events = [e for e in created_events if e.get("event_type") == "tool_result_quality"]
        assert len(quality_events) == 0
