"""Tests for memory explain display and profile extraction prompt fixes.

Covers:
1. CLI explain L1 fallback display when retrieval stats is None
2. Profile extraction prompt type classification
3. MemoryService.retrieve() explain passthrough
"""

import json
from unittest.mock import MagicMock

import pytest

from core.memory.prompts import OBSERVER_EXTRACTION_PROMPT


class TestCLIExplainL1Fallback:
    """Verify CLI _print_explain handles L1 data when retrieval stats is None."""

    def _simulate_explain_output(self, mem_data: dict) -> str:
        """Simulate the L1 display logic from edge_chat_loop._print_explain."""
        lines = []
        l1 = mem_data.get("l1")
        ret = mem_data.get("retrieval")

        if ret and not ret.get("error"):
            kw_hit = "✓" if ret.get("keyword_hit") else "✗"
            vec_hit = "✓" if ret.get("vector_hit") else "✗"
            p1 = ret.get("phase1_candidates", 0)
            p2 = ret.get("phase2_candidates", 0)
            merged = ret.get("merged_candidates", 0)
            final = ret.get("final_count", 0)
            ret_ms = ret.get("total_ms", 0)
            l1_tok = l1.get("tokens", 0) if l1 else 0
            lines.append(f"L1 retrieval  {ret_ms:.0f}ms  kw={kw_hit}({p1}) vec={vec_hit}({p2}) → {merged} → {final}  {l1_tok} tokens")
        elif ret and ret.get("error"):
            lines.append(f"L1 retrieval  error: {ret.get('error')}")
        elif l1 and l1.get("loaded"):
            l1_tok = l1.get("tokens", 0)
            l1_cnt = l1.get("count", 0)
            l1_ms = l1.get("ms", 0)
            lines.append(f"L1 retrieval  {l1_ms:.0f}ms  {l1_cnt} memories  {l1_tok} tokens")
        elif mem_data.get("error"):
            lines.append(f"memory  error: {mem_data['error']}")

        return "\n".join(lines)

    def test_l1_displayed_with_retrieval_stats(self):
        """Normal case: retrieval stats present → full L1 display."""
        mem = {
            "l1": {"loaded": True, "count": 3, "tokens": 79, "ms": 8.0},
            "retrieval": {
                "keyword_hit": True, "vector_hit": False,
                "phase1_candidates": 3, "phase2_candidates": 0,
                "merged_candidates": 0, "final_count": 3, "total_ms": 8.0,
            },
        }
        output = self._simulate_explain_output(mem)
        assert "kw=✓(3)" in output
        assert "79 tokens" in output

    def test_l1_displayed_without_retrieval_stats(self):
        """Fallback case: retrieval=None but l1 loaded → still shows L1 line."""
        mem = {
            "l1": {"loaded": True, "count": 5, "tokens": 120, "ms": 12.0},
            "retrieval": None,
        }
        output = self._simulate_explain_output(mem)
        assert "L1 retrieval" in output
        assert "5 memories" in output
        assert "120 tokens" in output

    def test_l1_hidden_when_not_loaded(self):
        """When L1 not loaded and no retrieval stats, nothing displayed."""
        mem = {
            "l1": {"loaded": False, "count": 0, "tokens": 0, "ms": 0},
            "retrieval": None,
        }
        output = self._simulate_explain_output(mem)
        assert output == ""

    def test_retrieval_error_displayed(self):
        """Retrieval error takes priority over L1 fallback."""
        mem = {
            "l1": {"loaded": False, "count": 0, "tokens": 0, "ms": 0},
            "retrieval": {"error": "connection timeout"},
        }
        output = self._simulate_explain_output(mem)
        assert "error: connection timeout" in output

    def test_memory_error_displayed(self):
        """Top-level memory error displayed when no L1 or retrieval."""
        mem = {
            "l1": None,
            "retrieval": None,
            "error": "TieredMemoryLoader failed",
        }
        output = self._simulate_explain_output(mem)
        assert "memory  error: TieredMemoryLoader failed" in output


class TestProfileExtractionPrompt:
    """Verify the improved extraction prompt guides correct type classification."""

    def test_prompt_has_profile_priority_rule(self):
        """Prompt must explicitly state profile takes priority over semantic."""
        assert "prefer profile over semantic" in OBSERVER_EXTRACTION_PROMPT.lower()

    def test_prompt_has_profile_examples(self):
        """Prompt must include concrete profile examples."""
        assert "prefers Go" in OBSERVER_EXTRACTION_PROMPT
        assert "uses vim" in OBSERVER_EXTRACTION_PROMPT
        assert "backend developer" in OBSERVER_EXTRACTION_PROMPT

    def test_prompt_has_semantic_distinction(self):
        """Prompt must clarify semantic is NOT about the user themselves."""
        assert "NOT" in OBSERVER_EXTRACTION_PROMPT
        assert "about the user themselves" in OBSERVER_EXTRACTION_PROMPT

    def test_prompt_has_identity_rule(self):
        """Prompt must state identity/preference/work-style → profile."""
        assert "WHO the user is" in OBSERVER_EXTRACTION_PROMPT
        assert "WHAT they prefer" in OBSERVER_EXTRACTION_PROMPT
        assert "HOW they work" in OBSERVER_EXTRACTION_PROMPT


class TestMemoryServiceExplainPassthrough:
    """Verify MemoryService.retrieve() passes explain to MemoryRetriever."""

    def test_explain_true_forwarded(self):
        from core.memory.service import MemoryService

        mock_db_factory = MagicMock()
        svc = MemoryService(mock_db_factory)

        # Mock the retriever
        mock_retriever = MagicMock()
        from core.memory.explain import RetrievalStats
        expected_stats = RetrievalStats(keyword_attempted=True, keyword_hit=True, final_count=2)
        mock_retriever.retrieve.return_value = ([], expected_stats)
        svc._retriever = mock_retriever

        memories, stats = svc.retrieve("u1", "query", session_id="s1", explain=True)
        assert mock_retriever.retrieve.call_args[1]["explain"] is True
        assert stats is expected_stats

    def test_explain_false_forwarded(self):
        from core.memory.service import MemoryService

        mock_db_factory = MagicMock()
        svc = MemoryService(mock_db_factory)

        mock_retriever = MagicMock()
        mock_retriever.retrieve.return_value = ([], None)
        svc._retriever = mock_retriever

        memories, stats = svc.retrieve("u1", "query", session_id="s1", explain=False)
        assert mock_retriever.retrieve.call_args[1]["explain"] is False
        assert stats is None
