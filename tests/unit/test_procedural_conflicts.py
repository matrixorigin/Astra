"""Unit tests for procedural memory conflict resolution.

Tests P0 zero-cost helpers for detecting user intent conflicts.
Design ref: context-window-management.md §1 Priority Resolution Mechanism
"""

import pytest
from core.context.procedural_hints import (
    extract_parameter_values,
    extract_user_specified_value,
    contradicts_user_intent,
)


class TestConflictResolution:
    """Test P0 conflict resolution with zero-cost helpers."""
    
    def test_extract_parameter_values_single(self):
        """Test regex extraction of single parameter=value pair."""
        pattern = "use analysis_type='overview'"
        params = extract_parameter_values(pattern)
        assert params == {"analysis_type": "overview"}
    
    def test_extract_parameter_values_multiple(self):
        """Test extraction of multiple parameters."""
        pattern = "use analysis_type='overview' and period='3mo'"
        params = extract_parameter_values(pattern)
        assert params == {"analysis_type": "overview", "period": "3mo"}
    
    def test_extract_parameter_values_no_quotes(self):
        """Test extraction without quotes."""
        pattern = "set type=stock and period=6mo"
        params = extract_parameter_values(pattern)
        assert params == {"type": "stock", "period": "6mo"}
    
    def test_extract_parameter_values_empty(self):
        """Test extraction from pattern with no parameters."""
        pattern = "always use comprehensive analysis"
        params = extract_parameter_values(pattern)
        assert params == {}
    
    def test_extract_user_value_explicit_analysis_type(self):
        """Test extraction when user explicitly specifies analysis type."""
        message = "give me advice analysis for this stock"
        value = extract_user_specified_value(message, "analysis_type")
        assert value == "advice"
    
    def test_extract_user_value_explicit_period(self):
        """Test extraction of time period."""
        message = "show me data for last 6 months"
        value = extract_user_specified_value(message, "period")
        assert value == "6mo"
    
    def test_extract_user_value_none_when_not_specified(self):
        """Test no extraction when user doesn't specify."""
        message = "analyze this stock"
        value = extract_user_specified_value(message, "analysis_type")
        assert value is None
    
    def test_extract_user_value_case_insensitive(self):
        """Test case-insensitive matching."""
        message = "Give me ADVICE analysis"
        value = extract_user_specified_value(message, "analysis_type")
        assert value.lower() == "advice"
    
    def test_contradicts_user_intent_true(self):
        """P0 Critical: Detect explicit contradiction."""
        pattern = "use analysis_type='overview'"
        user_message = "give me advice analysis"
        assert contradicts_user_intent(pattern, user_message) is True
    
    def test_contradicts_user_intent_false_no_specification(self):
        """P0 Critical: No contradiction when user doesn't specify."""
        pattern = "use analysis_type='overview'"
        user_message = "analyze this stock"
        assert contradicts_user_intent(pattern, user_message) is False
    
    def test_contradicts_user_intent_false_same_value(self):
        """No contradiction when user specifies same value."""
        pattern = "use analysis_type='overview'"
        user_message = "give me overview analysis"
        assert contradicts_user_intent(pattern, user_message) is False
    
    def test_contradicts_user_intent_multiple_params(self):
        """Test contradiction detection with multiple parameters."""
        pattern = "use analysis_type='overview' and period='3mo'"
        
        # Contradicts first param
        assert contradicts_user_intent(pattern, "give me advice analysis") is True
        
        # Contradicts second param
        assert contradicts_user_intent(pattern, "show 6 months data") is True
        
        # No contradiction
        assert contradicts_user_intent(pattern, "analyze the stock") is False
    
    def test_no_false_positives(self):
        """Critical: Don't suppress when no actual contradiction."""
        pattern = "prefer comprehensive analysis"
        user_message = "quick summary please"
        # Pattern has no extractable parameters, should not contradict
        assert contradicts_user_intent(pattern, user_message) is False
