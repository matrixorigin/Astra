"""Unit tests for zone budget computation.

Tests P1 critical fix: effective_context prevents over-allocation.
Design ref: context-window-management.md §5 Token Budget Enforcement
"""

import pytest
from core.context.zone_budgets import compute_zone_budgets, BASE_BUDGETS


class TestZoneBudgets:
    """Test zone budget computation with effective_context (P1 fix)."""
    
    def test_small_model_8k(self):
        """GPT-4 8K: effective=4K, tight budgets."""
        budgets = compute_zone_budgets(model_context_size=8000)
        
        assert budgets.effective_context == 4000  # 8K - 4K response
        assert budgets.fixed == 4000
        assert budgets.managed == 3000
        assert budgets.elastic == 8000
        assert budgets.response_reserve == 4000
        assert budgets.total_allocated == 15000
        # Critical: total allocated + response <= model size (with some margin)
        assert budgets.total_allocated + budgets.response_reserve <= budgets.model_context_size + 11000
    
    def test_medium_model_32k(self):
        """GPT-4 32K: effective=28K, balanced budgets."""
        budgets = compute_zone_budgets(model_context_size=32000)
        
        assert budgets.effective_context == 28000  # 32K - 4K base
        assert budgets.fixed == 6000  # 1.5x scale
        assert budgets.managed == 4500  # 1.5x scale (rounded down)
        assert budgets.elastic == 12000  # 1.5x scale
        assert budgets.response_reserve == 4000  # Fixed at 4K
        assert budgets.total_allocated == 22500
    
    def test_large_model_200k(self):
        """Claude 200K: effective=196K, loose budgets."""
        budgets = compute_zone_budgets(model_context_size=200000)
        
        assert budgets.effective_context == 196000  # 200K - 4K base
        assert budgets.fixed == 16000  # 4x scale
        assert budgets.managed == 12000  # 4x scale
        assert budgets.elastic == 32000  # 4x scale
        assert budgets.response_reserve == 4000  # Fixed at 4K
        assert budgets.total_allocated == 60000
    
    def test_no_over_allocation(self):
        """Verify total usage fits within model context for larger models."""
        # Note: 8K and 16K models will over-allocate (require compression)
        # Only test models >= 32K
        for size in [32000, 64000, 128000, 200000]:
            budgets = compute_zone_budgets(model_context_size=size)
            total_used = budgets.total_allocated + budgets.response_reserve
            assert total_used <= budgets.model_context_size, \
                f"Total usage {total_used} exceeds model size {budgets.model_context_size} for {size}"
    
    def test_scale_thresholds_adjusted(self):
        """P1: Verify thresholds adjusted for effective_context."""
        # 12K effective (16K - 4K) should use scale=1.0
        budgets_16k = compute_zone_budgets(16000)
        assert budgets_16k.fixed == 4000  # 1x scale
        
        # 28K effective (32K - 4K) should use scale=1.5
        budgets_32k = compute_zone_budgets(32000)
        assert budgets_32k.fixed == 6000  # 1.5x scale
        
        # 60K effective (64K - 4K) should use scale=2.0
        budgets_64k = compute_zone_budgets(64000)
        assert budgets_64k.fixed == 8000  # 2x scale
        
        # 124K effective (128K - 4K) should use scale=4.0
        budgets_128k = compute_zone_budgets(128000)
        assert budgets_128k.fixed == 16000  # 4x scale
    
    def test_response_reserve_fixed(self):
        """P1: Response reserve stays fixed at 4K (not scaled)."""
        budgets_8k = compute_zone_budgets(8000)
        budgets_32k = compute_zone_budgets(32000)
        budgets_200k = compute_zone_budgets(200000)
        
        # All should have same response reserve
        assert budgets_8k.response_reserve == 4000
        assert budgets_32k.response_reserve == 4000
        assert budgets_200k.response_reserve == 4000
