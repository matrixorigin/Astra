"""Regression tests for Bug 15: prompt_assembler verbose stats crash on retrieval dict."""

from __future__ import annotations
from unittest.mock import MagicMock, patch


class TestPromptAssemblerMemoryStats:
    def _make_loader(self, l1_content="- [semantic] some memory"):
        loader = MagicMock()
        from core.context.tiered_loader import TieredLoaderStats
        stats = TieredLoaderStats(
            l0_loaded=True, l0_tokens=10, l0_ms=1.0,
            l1_loaded=True, l1_count=1, l1_tokens=20, l1_ms=2.0,
            retrieval={"final_count": 1, "source": "memoria"},
            total_ms=3.0,
        )
        loader.build_section.return_value = ("some profile\n\nRelevant Memories:\n- [semantic] some memory", stats)
        loader.load_l0.return_value = "some profile"
        loader.load_l1.return_value = (f"Relevant Memories:\n{l1_content}", None)
        return loader

    def test_verbose_stats_no_crash_with_dict_retrieval(self):
        """Bug 15: tiered_stats.retrieval is dict, not dataclass — asdict() and .final_count crash."""
        from core.context.tiered_loader import TieredMemoryLoader

        loader = self._make_loader()
        with patch("core.context.tiered_loader.TieredMemoryLoader", return_value=loader):
            from core.context.prompt_assembler import PromptAssembler
            pa = PromptAssembler.__new__(PromptAssembler)
            pa._memory_service = None
            pa._embed_fn = None

            # Must not raise AttributeError
            section, stats = pa._build_memory(
                user_id="u1", session_id="s1", query="test",
                explain=True, verbose=True,
            )

        assert section is not None
        assert stats is not None
        assert "l0" in stats
        assert "l1" in stats

    def test_verbose_stats_retrieval_stored_as_dict(self):
        """retrieval stats must be stored as dict in output stats."""
        from core.context.tiered_loader import TieredMemoryLoader

        loader = self._make_loader()
        with patch("core.context.tiered_loader.TieredMemoryLoader", return_value=loader):
            from core.context.prompt_assembler import PromptAssembler
            pa = PromptAssembler.__new__(PromptAssembler)
            pa._memory_service = None
            pa._embed_fn = None

            _, stats = pa._build_memory(
                user_id="u1", session_id="s1", query="test",
                explain=True, verbose=False,
            )

        assert isinstance(stats.get("retrieval"), dict)
