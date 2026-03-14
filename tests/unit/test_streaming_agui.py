"""Unit tests for core/streaming/agui_protocol.py and multi_agent_aggregator.py."""

import asyncio
import pytest

from core.streaming.agui_protocol import AGUIProtocolValidator, EventTypeCategory


class TestAGUIProtocolValidator:
    @pytest.fixture
    def v(self):
        return AGUIProtocolValidator()

    def test_valid_session_info(self, v):
        event = {"event_type": "session_info", "data": {"session_id": "s1", "run_id": "r1"}}
        assert v.validate_event(event) is True
        assert v.validation_errors == []

    def test_missing_event_type(self, v):
        assert v.validate_event({}) is False
        assert "Missing event_type" in v.validation_errors[0]

    def test_unknown_event_type_allowed(self, v):
        event = {"event_type": "custom_event", "data": {}}
        assert v.validate_event(event) is True
        assert len(v.validation_warnings) == 1

    def test_missing_required_field(self, v):
        event = {"event_type": "session_info", "data": {"session_id": "s1"}}  # missing run_id
        assert v.validate_event(event) is False
        assert any("run_id" in e for e in v.validation_errors)

    def test_unexpected_field_warning(self, v):
        event = {
            "event_type": "session_info",
            "data": {"session_id": "s1", "run_id": "r1", "extra": "x"},
        }
        assert v.validate_event(event) is True
        assert any("extra" in w for w in v.validation_warnings)

    def test_validate_stream_counts(self, v):
        events = [
            {"event_type": "session_info", "data": {"session_id": "s1", "run_id": "r1"}},
            {"event_type": "session_info", "data": {"session_id": "s2"}},  # missing run_id
            {"event_type": "run_finished", "data": {"run_id": "r1"}},
        ]
        report = v.validate_stream(events)
        assert report["total_events"] == 3
        assert report["valid_events"] == 2
        assert report["invalid_events"] == 1
        assert report["event_type_distribution"]["session_info"] == 2

    def test_validate_stream_empty(self, v):
        report = v.validate_stream([])
        assert report["total_events"] == 0
        assert report["valid_events"] == 0

    def test_get_schema_known(self, v):
        schema = v.get_schema("session_info")
        assert schema is not None
        assert schema.event_type == "session_info"

    def test_get_schema_unknown(self, v):
        assert v.get_schema("nonexistent") is None

    def test_list_event_types(self, v):
        types = v.list_event_types()
        assert "session_info" in types
        assert "run_started" in types
        assert len(types) > 5

    def test_errors_cleared_between_calls(self, v):
        v.validate_event({})  # sets errors
        assert len(v.validation_errors) > 0
        v.validate_event({"event_type": "session_info", "data": {"session_id": "s", "run_id": "r"}})
        assert v.validation_errors == []


class TestMultiAgentAggregator:
    @pytest.fixture
    def agg(self):
        from core.streaming.multi_agent_aggregator import MultiAgentAggregator

        return MultiAgentAggregator(run_id="run-1")

    def test_init(self, agg):
        assert agg.run_id == "run-1"

    def test_get_stats_empty(self, agg):
        stats = agg.get_stats()
        assert "registered_agents" in stats
        assert stats["registered_agents"] == 0

    def test_register_agent_stream(self, agg):
        async def gen():
            yield {"event_type": "text_delta", "data": {"chunk": "hi"}}

        agg.register_agent_stream("agent-1", gen())
        stats = agg.get_stats()
        assert stats["registered_agents"] == 1

    @pytest.mark.asyncio
    async def test_aggregate_single_agent(self, agg):
        async def gen():
            yield {"event_type": "text_delta", "data": {"chunk": "hello"}}
            yield {"event_type": "run_finished", "data": {"run_id": "run-1"}}

        agg.register_agent_stream("agent-1", gen())
        events = []
        async for event in agg.aggregate():
            events.append(event)

        types = [e.get("event_type") for e in events]
        assert "text_delta" in types
        assert "run_finished" in types

    @pytest.mark.asyncio
    async def test_aggregate_empty(self, agg):
        events = []
        async for event in agg.aggregate():
            events.append(event)
        assert events == []
