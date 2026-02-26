"""Tests for ContextBudgetManager."""

import pytest
from core.context.budget_manager import (
    ContextBudgetManager,
    ConversationStage,
    BudgetAllocation,
    classify_stage,
)


class TestBudgetAllocation:
    """Tests for budget allocation."""

    def test_default_allocation(self):
        """Default allocation uses query stage."""
        mgr = ContextBudgetManager(max_context_tokens=128000)
        alloc = mgr.allocate()
        
        assert alloc.total > 0
        assert alloc.total <= mgr.available_tokens

    def test_stage_affects_allocation(self):
        """Different stages have different allocations."""
        mgr = ContextBudgetManager(max_context_tokens=128000)
        
        query_alloc = mgr.allocate(ConversationStage.QUERY)
        analysis_alloc = mgr.allocate(ConversationStage.ANALYSIS)
        
        # Analysis stage should have more tool_output budget
        assert analysis_alloc.tool_output > query_alloc.tool_output

    def test_generation_has_more_code_context(self):
        """Generation stage allocates more to code context."""
        mgr = ContextBudgetManager(max_context_tokens=128000)
        
        query_alloc = mgr.allocate(ConversationStage.QUERY)
        gen_alloc = mgr.allocate(ConversationStage.GENERATION)
        
        assert gen_alloc.code_context > query_alloc.code_context

    def test_string_stage_works(self):
        """Can pass stage as string."""
        mgr = ContextBudgetManager(max_context_tokens=128000)
        alloc = mgr.allocate("analysis")
        assert alloc.tool_output > 0


class TestUsageTracking:
    """Tests for usage tracking."""

    def test_record_usage(self):
        """Can record and retrieve usage."""
        mgr = ContextBudgetManager(max_context_tokens=128000)
        
        mgr.record_usage("history", 1000)
        mgr.record_usage("tool_output", 500)
        
        assert mgr.get_usage("history") == 1000
        assert mgr.get_usage("tool_output") == 500
        assert mgr.used_tokens == 1500

    def test_remaining_tokens(self):
        """Remaining tokens decreases with usage."""
        mgr = ContextBudgetManager(max_context_tokens=128000, reserve_for_output=4000)
        initial = mgr.remaining_tokens
        
        mgr.record_usage("history", 10000)
        
        assert mgr.remaining_tokens == initial - 10000

    def test_allocate_remaining(self):
        """allocate_remaining uses remaining budget."""
        mgr = ContextBudgetManager(max_context_tokens=128000)
        
        # Use half the budget
        mgr.record_usage("history", mgr.available_tokens // 2)
        
        alloc = mgr.allocate_remaining()
        assert alloc.total <= mgr.remaining_tokens

    def test_reset(self):
        """Reset clears all usage."""
        mgr = ContextBudgetManager(max_context_tokens=128000)
        mgr.record_usage("history", 1000)
        
        mgr.reset()
        
        assert mgr.used_tokens == 0


class TestToolOutputBudget:
    """Tests for tool output budget helper."""

    def test_get_tool_output_budget_bytes(self):
        """get_tool_output_budget returns bytes."""
        mgr = ContextBudgetManager(max_context_tokens=128000)
        budget = mgr.get_tool_output_budget()
        
        # Should be reasonable size (tokens * 4)
        assert budget > 10000
        assert budget < 200000

    def test_budget_decreases_with_usage(self):
        """Tool output budget decreases as context fills."""
        mgr = ContextBudgetManager(max_context_tokens=128000)
        
        initial = mgr.get_tool_output_budget()
        mgr.record_usage("history", 50000)
        after = mgr.get_tool_output_budget()
        
        assert after < initial


class TestStageClassification:
    """Tests for automatic stage classification."""

    def test_planning_keywords(self):
        """Planning keywords detected."""
        assert classify_stage("How to implement this feature?") == ConversationStage.PLANNING
        assert classify_stage("Plan the migration step by step") == ConversationStage.PLANNING

    def test_generation_keywords(self):
        """Generation keywords detected."""
        assert classify_stage("Write a function to parse JSON") == ConversationStage.GENERATION
        assert classify_stage("Create a new test file") == ConversationStage.GENERATION

    def test_analysis_keywords(self):
        """Analysis keywords detected."""
        assert classify_stage("Why is this failing?") == ConversationStage.ANALYSIS
        assert classify_stage("Debug this error") == ConversationStage.ANALYSIS

    def test_many_tool_calls_triggers_analysis(self):
        """Many tool calls triggers analysis stage."""
        assert classify_stage("What's in this file?", tool_calls_count=5) == ConversationStage.ANALYSIS

    def test_default_is_query(self):
        """Default stage is query."""
        assert classify_stage("Hello") == ConversationStage.QUERY
        assert classify_stage("What is X?") == ConversationStage.QUERY
