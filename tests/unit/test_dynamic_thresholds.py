"""Unit tests for dynamic exploration thresholds.

Tests P0 SQL optimization with COALESCE fallback.
Design ref: context-window-management.md §3 Dynamic Exploration Thresholds
"""

import pytest
from unittest.mock import Mock, MagicMock
from core.context.exploration_guardrails import (
    get_dynamic_thresholds,
    EXPLORATION_THRESHOLDS,
)


class TestDynamicThresholds:
    """Test P0 dynamic thresholds with SQL optimization."""
    
    def test_default_thresholds_dev_agent(self):
        """Test default thresholds for dev-agent without DB."""
        thresholds = get_dynamic_thresholds("dev-agent", "session_123", db=None)
        
        assert thresholds["tier1"] == 4
        assert thresholds["tier2"] == 7
        assert thresholds["tier3"] == 12
    
    def test_default_thresholds_data_analyst(self):
        """Test default thresholds for data-analyst."""
        thresholds = get_dynamic_thresholds("data-analyst", "session_123", db=None)
        
        assert thresholds["tier1"] == 6
        assert thresholds["tier2"] == 10
        assert thresholds["tier3"] == 15
    
    def test_default_thresholds_chat_agent(self):
        """Test default thresholds for chat-agent."""
        thresholds = get_dynamic_thresholds("chat-agent", "session_123", db=None)
        
        assert thresholds["tier1"] == 2
        assert thresholds["tier2"] == 4
        assert thresholds["tier3"] == 6
    
    def test_unknown_agent_type_uses_dev_agent(self):
        """Test unknown agent type falls back to dev-agent."""
        thresholds = get_dynamic_thresholds("unknown-agent", "session_123", db=None)
        
        assert thresholds == EXPLORATION_THRESHOLDS["dev-agent"]
    
    def test_coalesce_fallback_no_data(self):
        """P0: COALESCE returns 0.7 when no historical data."""
        # Mock DB that returns None (no data)
        mock_db = Mock()
        mock_result = Mock()
        mock_result.__getitem__ = Mock(return_value=0.7)  # COALESCE default
        mock_db.execute = Mock(return_value=Mock(fetchone=Mock(return_value=mock_result)))
        
        thresholds = get_dynamic_thresholds("dev-agent", "session_123", db=mock_db)
        
        # Should use base thresholds (0.7 satisfaction = neutral)
        assert thresholds == {"tier1": 4, "tier2": 7, "tier3": 12}
    
    def test_low_satisfaction_lowers_thresholds(self):
        """Test thresholds lowered when satisfaction <0.6."""
        # Mock DB returning low satisfaction
        mock_db = Mock()
        mock_result = Mock()
        mock_result.__getitem__ = Mock(return_value=0.5)  # Low satisfaction
        mock_db.execute = Mock(return_value=Mock(fetchone=Mock(return_value=mock_result)))
        
        thresholds = get_dynamic_thresholds("dev-agent", "session_123", db=mock_db)
        
        # Should be lowered (4 * 0.7 = 2.8 → 2)
        assert thresholds["tier1"] <= 3  # Lowered from 4
        assert thresholds["tier2"] <= 5  # Lowered from 7
        assert thresholds["tier3"] <= 9  # Lowered from 12
    
    def test_high_satisfaction_raises_thresholds(self):
        """Test thresholds raised when satisfaction >0.8."""
        # Mock DB returning high satisfaction
        mock_db = Mock()
        mock_result = Mock()
        mock_result.__getitem__ = Mock(return_value=0.9)  # High satisfaction
        mock_db.execute = Mock(return_value=Mock(fetchone=Mock(return_value=mock_result)))
        
        thresholds = get_dynamic_thresholds("dev-agent", "session_123", db=mock_db)
        
        # Should be raised (4 * 1.3 = 5.2 → 5)
        assert thresholds["tier1"] >= 5  # Raised from 4
        assert thresholds["tier2"] >= 9  # Raised from 7
        assert thresholds["tier3"] >= 15  # Raised from 12
    
    def test_sql_query_exception_returns_base(self):
        """Test graceful fallback when SQL query fails."""
        # Mock DB that raises exception
        mock_db = Mock()
        mock_db.execute = Mock(side_effect=Exception("DB error"))
        
        thresholds = get_dynamic_thresholds("dev-agent", "session_123", db=mock_db)
        
        # Should return base thresholds (safe fallback)
        assert thresholds == {"tier1": 4, "tier2": 7, "tier3": 12}
    
    def test_minimum_threshold_is_2(self):
        """Test thresholds never go below 2."""
        # Mock very low satisfaction
        mock_db = Mock()
        mock_result = Mock()
        mock_result.__getitem__ = Mock(return_value=0.1)  # Very low
        mock_db.execute = Mock(return_value=Mock(fetchone=Mock(return_value=mock_result)))
        
        thresholds = get_dynamic_thresholds("chat-agent", "session_123", db=mock_db)
        
        # Even with 0.7 multiplier, should not go below 2
        assert all(v >= 2 for v in thresholds.values())
