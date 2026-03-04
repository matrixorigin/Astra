"""Integration tests: verify the full turn event chain in the real DB.

Tests that _persist_turn_events writes every event with correct:
- event_id (pre-generated, reflects turn start time)
- causal_chain_id (all events in a turn share the same chain)
- parent_event_id (llm_response → user_query, tool_call → user_query)
- event_count on agent_sessions updated after persist
- continuation turns (tool_results without new user_query) reuse chain
"""
import time
import pytest
import sqlalchemy as sa
from uuid_utils import uuid7


@pytest.fixture
def turn_session(db):
    """Create a fresh session for turn event tests."""
    from api.models.agent import Session as SessionModel
    sid = str(uuid7())
    uid = str(uuid7())
    db.add(SessionModel(session_id=sid, user_id=uid, agent_id="test", status="active"))
    db.commit()
    yield sid, uid
    # cleanup
    db.execute(sa.text("DELETE FROM agent_events WHERE session_id = :sid"), {"sid": sid})
    db.execute(sa.text("DELETE FROM agent_sessions WHERE session_id = :sid"), {"sid": sid})
    db.commit()


def _run_persist(session_id, user_id, turn_chain_id, user_query_event_id, **kwargs):
    """Run _persist_turn_events and wait for the background thread."""
    from api.routers.chat import _persist_turn_events
    _persist_turn_events(
        user_id, session_id,
        messages=kwargs.get("messages", [{"role": "user", "content": "hello"}]),
        tool_results=kwargs.get("tool_results", []),
        full_text=kwargs.get("full_text", "response"),
        tool_calls=kwargs.get("tool_calls", []),
        turn_chain_id=turn_chain_id,
        user_query_event_id=user_query_event_id,
    )
    time.sleep(0.5)  # wait for daemon thread


def _fetch_events(db, session_id):
    rows = db.execute(sa.text("""
        SELECT event_id, event_type, causal_chain_id, parent_event_id, created_at
        FROM agent_events WHERE session_id = :sid ORDER BY created_at
    """), {"sid": session_id}).fetchall()
    return [dict(r._mapping) for r in rows]


class TestTurnEventChain:
    """Verify every DB field in the turn event chain."""

    def test_all_events_share_turn_chain_id(self, db, turn_session):
        """Every event in a turn must have the same causal_chain_id."""
        sid, uid = turn_session
        chain = str(uuid7())
        uq_eid = str(uuid7())
        _run_persist(sid, uid, chain, uq_eid,
                     tool_calls=[{"id": "tc1", "function": {"name": "bash", "arguments": "{}"}}])

        events = _fetch_events(db, sid)
        assert len(events) >= 2
        chains = {e["causal_chain_id"] for e in events}
        assert chains == {chain}, f"Multiple chains found: {chains}"

    def test_user_query_event_id_is_pregenerated(self, db, turn_session):
        """user_query.event_id must equal the pre-generated ID, not a new uuid7."""
        sid, uid = turn_session
        chain = str(uuid7())
        uq_eid = str(uuid7())
        _run_persist(sid, uid, chain, uq_eid)

        events = _fetch_events(db, sid)
        uq = next(e for e in events if e["event_type"] == "user_query")
        assert uq["event_id"] == uq_eid, (
            f"user_query.event_id {uq['event_id']} != pre-generated {uq_eid}. "
            "event_id must be generated at turn-start time, not persist-thread time."
        )

    def test_user_query_has_no_parent(self, db, turn_session):
        """user_query is the root event — parent_event_id must be None."""
        sid, uid = turn_session
        _run_persist(sid, uid, str(uuid7()), str(uuid7()))

        events = _fetch_events(db, sid)
        uq = next(e for e in events if e["event_type"] == "user_query")
        assert uq["parent_event_id"] is None

    def test_llm_response_parent_is_user_query(self, db, turn_session):
        """llm_response.parent_event_id must equal user_query.event_id."""
        sid, uid = turn_session
        chain = str(uuid7())
        uq_eid = str(uuid7())
        _run_persist(sid, uid, chain, uq_eid)

        events = _fetch_events(db, sid)
        uq = next(e for e in events if e["event_type"] == "user_query")
        lr = next(e for e in events if e["event_type"] == "llm_response")
        assert lr["parent_event_id"] == uq["event_id"], (
            f"llm_response.parent {lr['parent_event_id']} != user_query.event_id {uq['event_id']}"
        )

    def test_tool_call_parent_is_user_query(self, db, turn_session):
        """tool_call.parent_event_id must equal user_query.event_id."""
        sid, uid = turn_session
        chain = str(uuid7())
        uq_eid = str(uuid7())
        _run_persist(sid, uid, chain, uq_eid,
                     tool_calls=[{"id": "tc1", "function": {"name": "bash", "arguments": "{}"}}])

        events = _fetch_events(db, sid)
        uq = next(e for e in events if e["event_type"] == "user_query")
        tc = next(e for e in events if e["event_type"] == "tool_call")
        assert tc["parent_event_id"] == uq["event_id"]

    def test_tool_result_chain_matches_turn(self, db, turn_session):
        """tool_result must share the turn's causal_chain_id."""
        sid, uid = turn_session
        chain = str(uuid7())
        uq_eid = str(uuid7())
        _run_persist(sid, uid, chain, uq_eid,
                     tool_results=[{"name": "read_file", "result": "content", "tool_call_id": "tc1"}])

        events = _fetch_events(db, sid)
        tr = next((e for e in events if e["event_type"] == "tool_result"), None)
        assert tr is not None
        assert tr["causal_chain_id"] == chain

    def test_session_event_count_updated(self, db, turn_session):
        """agent_sessions.event_count must equal actual event count after persist."""
        sid, uid = turn_session
        _run_persist(sid, uid, str(uuid7()), str(uuid7()),
                     tool_calls=[{"id": "tc1", "function": {"name": "bash", "arguments": "{}"}}])

        events = _fetch_events(db, sid)
        sess = db.execute(
            sa.text("SELECT event_count FROM agent_sessions WHERE session_id = :sid"),
            {"sid": sid},
        ).fetchone()
        assert sess[0] == len(events), f"event_count {sess[0]} != actual {len(events)}"

    def test_two_turns_have_different_chains(self, db, turn_session):
        """Events from different turns must have different causal_chain_ids."""
        sid, uid = turn_session
        chain1 = str(uuid7())
        chain2 = str(uuid7())
        _run_persist(sid, uid, chain1, str(uuid7()))
        _run_persist(sid, uid, chain2, str(uuid7()))

        events = _fetch_events(db, sid)
        chains = {e["causal_chain_id"] for e in events}
        assert len(chains) == 2, f"Expected 2 distinct chains, got: {chains}"
        assert chain1 in chains
        assert chain2 in chains

    def test_user_query_event_id_timestamp_before_persist(self, db, turn_session):
        """Pre-generated event_id uuid7 timestamp must be <= persist thread's uuid7.

        This ensures event_id reflects turn-start time, not persist time.
        """
        sid, uid = turn_session
        chain = str(uuid7())
        uq_eid = str(uuid7())
        # Small delay to ensure persist thread runs later
        time.sleep(0.01)
        _run_persist(sid, uid, chain, uq_eid)

        events = _fetch_events(db, sid)
        uq = next(e for e in events if e["event_type"] == "user_query")
        lr = next(e for e in events if e["event_type"] == "llm_response")

        # user_query.event_id (pre-generated) should sort before llm_response.event_id
        # since both are uuid7 and user_query was "generated" first
        assert uq["event_id"] < lr["event_id"], (
            "user_query event_id should be lexicographically earlier than llm_response "
            "(both uuid7, user_query pre-generated at turn start)"
        )


class TestContinuationTurnChain:
    """Verify causal chain integrity across multi-step tool loops.

    When the LLM returns tool_calls, the edge executes them and sends
    tool_results back in a new /chat/turn request (no new user_query).
    These "continuation turns" must share the same causal_chain_id as
    the original turn so the entire multi-step loop is one causal chain.
    """

    def test_continuation_turn_shares_chain_with_original(self, db, turn_session):
        """tool_results-only turn must reuse the original turn's causal_chain_id."""
        sid, uid = turn_session
        chain = str(uuid7())
        uq_eid = str(uuid7())

        # Turn 1: user query + tool_call
        _run_persist(sid, uid, chain, uq_eid,
                     tool_calls=[{"id": "tc1", "function": {"name": "bash", "arguments": "{}"}}],
                     full_text="Let me run that.")

        # Turn 1 continuation: tool_results come back, no new user_query.
        # Reuse same chain and user_query_event_id.
        _run_persist(sid, uid, chain, uq_eid,
                     tool_results=[{"name": "bash", "result": "ok", "tool_call_id": "tc1"}],
                     full_text="Done.")

        events = _fetch_events(db, sid)
        chains = {e["causal_chain_id"] for e in events}
        assert chains == {chain}, (
            f"All events in a multi-step tool loop must share one chain. "
            f"Found {len(chains)} chains: {chains}"
        )

    def test_continuation_tool_result_has_parent(self, db, turn_session):
        """tool_result in continuation turn must have parent_event_id = user_query."""
        sid, uid = turn_session
        chain = str(uuid7())
        uq_eid = str(uuid7())

        # Turn 1: user query + tool_call
        _run_persist(sid, uid, chain, uq_eid,
                     tool_calls=[{"id": "tc1", "function": {"name": "bash", "arguments": "{}"}}],
                     full_text="Running.")

        # Continuation: tool_results only
        _run_persist(sid, uid, chain, uq_eid,
                     tool_results=[{"name": "bash", "result": "output", "tool_call_id": "tc1"}],
                     full_text="Here is the result.")

        events = _fetch_events(db, sid)
        tool_results = [e for e in events if e["event_type"] == "tool_result"]
        assert len(tool_results) >= 1, "Expected at least one tool_result event"

        for tr in tool_results:
            assert tr["parent_event_id"] == uq_eid, (
                f"tool_result.parent_event_id {tr['parent_event_id']} != "
                f"user_query.event_id {uq_eid}. "
                "Continuation turn events must point to the original user_query."
            )
            assert tr["causal_chain_id"] == chain

    def test_continuation_llm_response_has_parent(self, db, turn_session):
        """llm_response in continuation turn must have parent_event_id = user_query."""
        sid, uid = turn_session
        chain = str(uuid7())
        uq_eid = str(uuid7())

        # Turn 1: user query + tool_call
        _run_persist(sid, uid, chain, uq_eid,
                     tool_calls=[{"id": "tc1", "function": {"name": "bash", "arguments": "{}"}}],
                     full_text="Running.")

        # Continuation: tool_results + final response
        _run_persist(sid, uid, chain, uq_eid,
                     tool_results=[{"name": "bash", "result": "output", "tool_call_id": "tc1"}],
                     full_text="Here is the result.")

        events = _fetch_events(db, sid)
        llm_responses = [e for e in events if e["event_type"] == "llm_response"]
        assert len(llm_responses) == 2, f"Expected 2 llm_response events, got {len(llm_responses)}"

        for lr in llm_responses:
            assert lr["parent_event_id"] == uq_eid, (
                f"llm_response.parent_event_id {lr['parent_event_id']} != "
                f"user_query.event_id {uq_eid}"
            )
            assert lr["causal_chain_id"] == chain

    def test_three_step_tool_loop_single_chain(self, db, turn_session):
        """A 3-step tool loop (query → tool → result → tool → result → answer)
        must produce exactly one causal chain."""
        sid, uid = turn_session
        chain = str(uuid7())
        uq_eid = str(uuid7())

        # Step 1: user query → LLM returns tool_call
        _run_persist(sid, uid, chain, uq_eid,
                     tool_calls=[{"id": "tc1", "function": {"name": "read_file", "arguments": "{}"}}],
                     full_text="")

        # Step 2: tool_result → LLM returns another tool_call
        _run_persist(sid, uid, chain, uq_eid,
                     tool_results=[{"name": "read_file", "result": "file content", "tool_call_id": "tc1"}],
                     tool_calls=[{"id": "tc2", "function": {"name": "write_file", "arguments": "{}"}}],
                     full_text="")

        # Step 3: tool_result → LLM returns final text
        _run_persist(sid, uid, chain, uq_eid,
                     tool_results=[{"name": "write_file", "result": "ok", "tool_call_id": "tc2"}],
                     full_text="Done, file updated.")

        events = _fetch_events(db, sid)
        chains = {e["causal_chain_id"] for e in events}
        assert chains == {chain}, (
            f"3-step tool loop must have exactly 1 chain. Found: {chains}"
        )

        # Verify all events have parent pointing to user_query
        non_uq = [e for e in events if e["event_type"] != "user_query"]
        for e in non_uq:
            assert e["parent_event_id"] == uq_eid, (
                f"{e['event_type']}.parent_event_id = {e['parent_event_id']}, "
                f"expected {uq_eid}"
            )

    def test_new_user_query_starts_new_chain(self, db, turn_session):
        """A new user message after a tool loop must start a fresh chain."""
        sid, uid = turn_session
        chain1 = str(uuid7())
        uq_eid1 = str(uuid7())
        chain2 = str(uuid7())
        uq_eid2 = str(uuid7())

        # Turn 1: user query + tool loop
        _run_persist(sid, uid, chain1, uq_eid1,
                     tool_calls=[{"id": "tc1", "function": {"name": "bash", "arguments": "{}"}}],
                     full_text="Running.")
        _run_persist(sid, uid, chain1, uq_eid1,
                     tool_results=[{"name": "bash", "result": "ok", "tool_call_id": "tc1"}],
                     full_text="Done.")

        # Turn 2: new user query — must be a different chain
        _run_persist(sid, uid, chain2, uq_eid2, full_text="Sure thing.")

        events = _fetch_events(db, sid)
        chains = {e["causal_chain_id"] for e in events}
        assert len(chains) == 2, f"Expected 2 chains (2 user intents), got: {chains}"
        assert chain1 in chains
        assert chain2 in chains

        # Verify chain assignment
        for e in events:
            if e["event_type"] == "user_query":
                if e["event_id"] == uq_eid1:
                    assert e["causal_chain_id"] == chain1
                elif e["event_id"] == uq_eid2:
                    assert e["causal_chain_id"] == chain2

    def test_tool_result_parent_not_none_in_continuation(self, db, turn_session):
        """Regression: tool_result.parent_event_id must never be None in continuation turns.

        Before the fix, continuation turns (no user_content) left parent_event_id=None
        because the user_query phase was skipped.
        """
        sid, uid = turn_session
        chain = str(uuid7())
        uq_eid = str(uuid7())

        # Initial turn with tool_call
        _run_persist(sid, uid, chain, uq_eid,
                     tool_calls=[{"id": "tc1", "function": {"name": "search", "arguments": "{}"}}],
                     full_text="Searching.")

        # Continuation: only tool_results, no user message
        _run_persist(sid, uid, chain, uq_eid,
                     messages=[],  # no user message
                     tool_results=[{"name": "search", "result": "found it", "tool_call_id": "tc1"}],
                     full_text="Found the result.")

        events = _fetch_events(db, sid)
        for e in events:
            if e["event_type"] in ("tool_result", "llm_response", "tool_call"):
                assert e["parent_event_id"] is not None, (
                    f"{e['event_type']} has parent_event_id=None — "
                    "continuation turn events must reference the original user_query"
                )
