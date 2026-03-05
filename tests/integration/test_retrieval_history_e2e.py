"""Integration tests for retrieval-based history management.

Verifies:
1. Compaction triggers at lowered threshold (16K tokens)
2. EmbeddingWorker embeds tool_result events
3. _build_retrieval_view trims history on Turn 3+
4. Prompt tokens stay bounded regardless of turn count
5. Fallback to full history when retrieval unavailable
"""

import json
import uuid

import pytest

from core.context.compaction import (
    _COMPACT_HISTORY_THRESHOLD,
    compact_history_messages,
    estimate_tokens,
)


# ── Helpers ──────────────────────────────────────────────────────────

def _make_turn(turn_num: int, tool_result_chars: int = 500) -> list[dict]:
    """Generate one complete turn: user + assistant(tc) + tool + assistant."""
    return [
        {"role": "user", "content": f"Question about topic {turn_num}"},
        {"role": "assistant", "content": "", "tool_calls": [
            {"id": f"tc-{turn_num}", "type": "function",
             "function": {"name": f"tool_{turn_num}", "arguments": "{}"}}
        ]},
        {"role": "tool", "tool_call_id": f"tc-{turn_num}",
         "content": f"Result for topic {turn_num}: " + "x" * tool_result_chars},
        {"role": "assistant", "content": f"Here is the answer for topic {turn_num}. " + "y" * 200},
    ]


def _build_history(num_turns: int, tool_result_chars: int = 500) -> list[dict]:
    """Build a multi-turn history with system prompt."""
    history = [{"role": "system", "content": "You are a helpful assistant. " + "z" * 500}]
    for i in range(num_turns):
        history.extend(_make_turn(i, tool_result_chars))
    return history


# ── Step 1: Compaction threshold tests ───────────────────────────────

class TestCompactionThreshold:
    """Verify compaction triggers at the lowered 16K char threshold."""

    def test_threshold_is_16k(self):
        assert _COMPACT_HISTORY_THRESHOLD == 16000

    def test_small_history_not_compacted(self):
        """2-turn history (~3K chars) should pass through unchanged."""
        history = _build_history(2, tool_result_chars=300)
        total_chars = sum(len(m.get("content") or "") for m in history)
        assert total_chars < _COMPACT_HISTORY_THRESHOLD

        result = compact_history_messages(history)
        # Should be identical (no compaction)
        assert len(result) == len(history)
        for orig, comp in zip(history, result):
            assert orig.get("content") == comp.get("content")

    def test_large_history_compacted_at_16k(self):
        """6-turn history with large tool results should trigger compaction."""
        history = _build_history(6, tool_result_chars=3000)
        total_chars = sum(len(m.get("content") or "") for m in history)
        assert total_chars > _COMPACT_HISTORY_THRESHOLD, \
            f"Test setup: {total_chars} chars should exceed {_COMPACT_HISTORY_THRESHOLD}"

        result = compact_history_messages(history)
        result_chars = sum(len(m.get("content") or "") for m in result)
        assert result_chars < total_chars, \
            f"Compaction should reduce: {result_chars} >= {total_chars}"

    def test_recent_2_turns_preserved_after_compaction(self):
        """Last 2 user turns must be kept in full fidelity."""
        history = _build_history(6, tool_result_chars=3000)
        result = compact_history_messages(history)

        # Find last 2 user messages in original
        user_indices = [i for i, m in enumerate(history) if m.get("role") == "user"]
        last_2_start = user_indices[-2]
        original_recent = history[last_2_start:]

        # Find corresponding messages in result
        result_recent = result[-len(original_recent):]
        for orig, comp in zip(original_recent, result_recent):
            assert orig.get("content") == comp.get("content"), \
                f"Recent message modified: {orig.get('content')[:50]} != {comp.get('content')[:50]}"

    def test_old_tool_results_truncated(self):
        """Tool results from old turns should be truncated."""
        history = _build_history(6, tool_result_chars=3000)
        result = compact_history_messages(history)

        # First tool result (oldest) should be truncated
        first_tool = next(m for m in result if m.get("role") == "tool")
        assert len(first_tool["content"]) < 3000, \
            f"Old tool result not truncated: {len(first_tool['content'])} chars"
        assert "truncated" in first_tool["content"].lower() or len(first_tool["content"]) <= 500


# ── Step 2: EmbeddingWorker type coverage ────────────────────────────

class TestEmbeddingWorkerTypes:
    """Verify tool_result is in EMBED_EVENT_TYPES."""

    def test_tool_result_in_embed_types(self):
        from core.events.embedding_worker import EMBED_EVENT_TYPES
        assert "tool_result" in EMBED_EVENT_TYPES
        assert "user_query" in EMBED_EVENT_TYPES
        assert "llm_response" in EMBED_EVENT_TYPES


# ── Step 3: Retrieval view construction ──────────────────────────────

class TestRetrievalView:
    """Verify _build_retrieval_view trims history correctly."""

    def test_short_history_unchanged(self):
        """History with < 3 turns should be returned as-is."""
        from api.routers.chat import _build_retrieval_view, _MIN_HISTORY_FOR_RETRIEVAL

        history = _build_history(2)
        assert len(history) < _MIN_HISTORY_FOR_RETRIEVAL

        result, _scores = _build_retrieval_view(history, "test-session", [], None)
        assert result is history  # Same object, no transformation

    def test_long_history_trimmed(self, db_factory):
        """History with 6+ turns should be trimmed to recent + retrieved."""
        from api.routers.chat import (
            _build_retrieval_view,
            _MIN_HISTORY_FOR_RETRIEVAL,
            _RECENT_MESSAGES_KEEP,
        )

        history = _build_history(8, tool_result_chars=500)
        assert len(history) >= _MIN_HISTORY_FOR_RETRIEVAL

        current_messages = [{"role": "user", "content": "What about topic 2?"}]

        with db_factory() as db:
            result, _scores = _build_retrieval_view(history, "test-session", current_messages, db)

        # Result should be smaller than full history
        assert len(result) < len(history), \
            f"Retrieval view should be smaller: {len(result)} >= {len(history)}"

        # System message should be first
        assert result[0].get("role") == "system"

        # Recent messages should be at the end
        # The last _RECENT_MESSAGES_KEEP messages from history should appear
        recent_from_history = history[-_RECENT_MESSAGES_KEEP:]
        for orig in recent_from_history:
            content = orig.get("content")
            if content:
                found = any(m.get("content") == content for m in result)
                assert found, f"Recent message missing from view: {content[:50]}"

    def test_token_count_bounded(self, db_factory):
        """Retrieval view tokens should not grow with turn count."""
        from api.routers.chat import _build_retrieval_view

        current_messages = [{"role": "user", "content": "Tell me about topic 3"}]

        token_counts = []
        for num_turns in [4, 8, 12, 16]:
            history = _build_history(num_turns, tool_result_chars=500)
            with db_factory() as db:
                result, _scores = _build_retrieval_view(history, f"test-{num_turns}", current_messages, db)
            tokens = estimate_tokens(result)
            token_counts.append(tokens)

        # Token count should NOT grow linearly with turns
        # Allow some variance but 16-turn should not be 4x of 4-turn
        assert token_counts[-1] < token_counts[0] * 2.5, \
            f"Tokens growing too fast: {token_counts} (4→16 turns)"

    def test_system_message_always_present(self, db_factory):
        """System message must always be in the result."""
        from api.routers.chat import _build_retrieval_view

        history = _build_history(6)
        current_messages = [{"role": "user", "content": "hello"}]

        with db_factory() as db:
            result, _scores = _build_retrieval_view(history, "test-sys", current_messages, db)

        assert result[0]["role"] == "system"
        assert "helpful assistant" in result[0]["content"]


# ── Step 4: Fallback behavior ────────────────────────────────────────

class TestRetrievalFallback:
    """Verify fallback to rule-based extraction when embeddings unavailable."""

    def test_rule_based_finds_keyword_matches(self):
        from api.routers.chat import _rule_based_extraction

        history = _build_history(6)
        # Inject a message with specific keyword
        history.insert(5, {"role": "assistant", "content": "The database migration completed successfully"})

        recent = history[-8:]
        result = _rule_based_extraction(history, recent, "database migration")

        assert result is not None
        assert "database" in result.lower() or "migration" in result.lower()

    def test_rule_based_returns_none_for_no_match(self):
        from api.routers.chat import _rule_based_extraction

        history = _build_history(6)
        recent = history[-8:]
        result = _rule_based_extraction(history, recent, "xyzzy_nonexistent_term")

        assert result is None

    def test_rule_based_respects_budget(self):
        from api.routers.chat import _RETRIEVAL_BUDGET_CHARS, _rule_based_extraction

        # Build history with many matching messages
        history = [{"role": "system", "content": "system"}]
        for i in range(20):
            history.append({"role": "user", "content": f"database query {i}"})
            history.append({"role": "assistant", "content": f"database result {i} " + "x" * 2000})

        recent = history[-8:]
        result = _rule_based_extraction(history, recent, "database query")

        assert result is not None
        assert len(result) <= _RETRIEVAL_BUDGET_CHARS + 200  # small overhead for header


# ── Step 5: _build_turn_messages integration ─────────────────────────

class TestBuildTurnMessagesRetrieval:
    """Verify _build_turn_messages uses retrieval view on Turn 3+."""

    def test_turn3_uses_retrieval_view(self, db_factory):
        """After 3+ turns, returned messages should be trimmed."""
        from api.routers.chat import _build_turn_messages, _session_cache

        session_id = f"test-retrieval-{uuid.uuid4().hex[:8]}"
        history = _build_history(6, tool_result_chars=1000)

        _session_cache[session_id] = {"history": history, "sections": {"identity": "test"}}

        try:
            with db_factory() as db:
                result, _, _ = _build_turn_messages(
                    db=db, user_id="test-user", session_id=session_id,
                    messages=[{"role": "user", "content": "What about topic 2?"}],
                    tool_results=None, project_rules=None,
                )

            # Result should be smaller than full history
            result_tokens = estimate_tokens(result)
            full_tokens = estimate_tokens(history)
            assert result_tokens < full_tokens, \
                f"Retrieval view should be smaller: {result_tokens} >= {full_tokens}"

            # Full history should still be in cache (unchanged)
            cached = _session_cache[session_id]["history"]
            assert len(cached) >= len(history), \
                "Cache should retain full history"
        finally:
            _session_cache.pop(session_id, None)

    def test_full_history_in_cache_after_retrieval(self, db_factory):
        """_session_cache must retain full history even when LLM gets trimmed view."""
        from api.routers.chat import _build_turn_messages, _session_cache

        session_id = f"test-cache-{uuid.uuid4().hex[:8]}"
        history = _build_history(6, tool_result_chars=500)
        original_len = len(history)

        _session_cache[session_id] = {"history": history, "sections": {"identity": "test"}}

        try:
            with db_factory() as db:
                _build_turn_messages(
                    db=db, user_id="test-user", session_id=session_id,
                    messages=[{"role": "user", "content": "hello"}],
                    tool_results=None, project_rules=None,
                )

            cached = _session_cache[session_id]["history"]
            # Cache should have original + new user message appended
            assert len(cached) >= original_len
        finally:
            _session_cache.pop(session_id, None)
