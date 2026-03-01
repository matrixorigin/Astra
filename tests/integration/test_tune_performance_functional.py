"""Comprehensive functional tests for tune_performance skill.

High coverage, realistic scenarios, no real LLM required.
Tests the complete optimization loop with mock data that simulates real behavior.
"""

import pytest
from skills.tune_performance.skill import (
    TunePerformanceSkill,
    TunePerformanceInput,
    OptimizationObjective,
)
from skills.evaluate_session.skill import EvaluateSessionSkill, EvaluateSessionInput
from sqlalchemy import text
from api.database import SessionLocal


@pytest.mark.asyncio
class TestTunePerformanceFunctional:
    """Functional tests with realistic mock data."""
    
    def setup_method(self):
        """Setup test data before each test."""
        self.db = SessionLocal()
        self.skill = TunePerformanceSkill()
    
    def teardown_method(self):
        """Cleanup after each test."""
        self.db.execute(text("DELETE FROM agent_events WHERE session_id LIKE 'test-tune-%'"))
        self.db.commit()
        self.db.close()
    
    def _create_session_with_metrics(
        self, session_id: str, queries: int, tokens_per_call: int, calls_per_query: int
    ):
        """Create realistic session data."""
        for q in range(queries):
            # User query
            self.db.execute(text("""
                INSERT INTO agent_events (
                    event_id, session_id, user_id, agent_id, agent_version,
                    event_type, content, causal_chain_id, created_at
                )
                VALUES (
                    :eid, :sid, 'test-user', 'test-agent', '1.0',
                    'user_query', :content, :chain, NOW()
                )
            """), {
                "eid": f"evt-q{q}",
                "sid": session_id,
                "content": f"Query {q}",
                "chain": f"chain-{q}"
            })
            
            # LLM responses
            for c in range(calls_per_query):
                self.db.execute(text("""
                    INSERT INTO agent_events (
                        event_id, session_id, user_id, agent_id, agent_version,
                        event_type, content, token_usage, llm_model_used,
                        causal_chain_id, created_at
                    )
                    VALUES (
                        :eid, :sid, 'test-user', 'test-agent', '1.0',
                        'llm_response', 'Response', :usage, 'test-model',
                        :chain, NOW()
                    )
                """), {
                    "eid": f"evt-r{q}-{c}",
                    "sid": session_id,
                    "usage": f'{{"prompt": {tokens_per_call}, "completion": 100, "total": {tokens_per_call + 100}}}',
                    "chain": f"chain-{q}"
                })
        
        self.db.commit()
    
    # ========== Cost Optimization Tests ==========
    
    async def test_cost_optimization_high_token_usage(self):
        """Test cost optimization for session with high token usage."""
        session_id = "test-tune-cost-001"
        
        # Scenario: 3 queries, 50K tokens per call, 2 calls per query
        # Total: 300K tokens, 100K per query → needs_improvement
        self._create_session_with_metrics(
            session_id, queries=3, tokens_per_call=50000, calls_per_query=2
        )
        
        # Execute cost optimization
        result = await self.skill.execute(TunePerformanceInput(
            session_id=session_id,
            objective=OptimizationObjective.COST,
            target_score=0.8,
            max_iterations=3
        ))
        
        # Verify
        assert result.success is True
        assert result.objective == "cost"
        assert result.iterations > 0
        
        # Initial should be poor (100K tokens/query)
        assert result.initial_scores["cost"] < 0.5
        
        # Final should improve
        assert result.final_scores["cost"] > result.initial_scores["cost"]
        
        # Should show improvement
        assert result.overall_improvement > 0
        
        # Should have applied cost reduction adjustments
        assert any("reduce_tokens" in imp["adjustment"]["type"] 
                  for imp in result.improvements)
        
        # Should show trade-offs
        assert "accuracy" in result.trade_offs or "latency" in result.trade_offs
    
    async def test_cost_optimization_already_optimal(self):
        """Test cost optimization when already efficient.
        
        When performance is already optimal (cost score >= 0.8), the skill should:
        1. Recognize this in the first iteration
        2. Not apply any adjustments
        3. Return with 0 iterations (no optimization needed)
        """
        session_id = "test-tune-cost-002"
        
        # Scenario: Already excellent (5K tokens per call)
        self._create_session_with_metrics(
            session_id, queries=2, tokens_per_call=5000, calls_per_query=1
        )
        
        result = await self.skill.execute(TunePerformanceInput(
            session_id=session_id,
            objective=OptimizationObjective.COST,
            target_score=0.9
        ))
        
        # Should recognize already optimal
        assert result.success is True
        assert result.initial_scores["cost"] >= 0.8
        
        # Should not iterate when already optimal
        # The skill checks target before entering loop, so iterations should be 0
        # If this fails, it means the skill is doing unnecessary work
        assert result.iterations == 0, (
            f"Expected 0 iterations for already-optimal session, got {result.iterations}. "
            f"Initial cost score was {result.initial_scores['cost']}, target was 0.9"
        )
    
    # ========== Latency Optimization Tests ==========
    
    async def test_latency_optimization_many_calls(self):
        """Test latency optimization for session with many LLM calls."""
        session_id = "test-tune-latency-001"
        
        # Scenario: 2 queries, 8 calls per query → needs_improvement
        self._create_session_with_metrics(
            session_id, queries=2, tokens_per_call=10000, calls_per_query=8
        )
        
        result = await self.skill.execute(TunePerformanceInput(
            session_id=session_id,
            objective=OptimizationObjective.LATENCY,
            target_score=0.8
        ))
        
        assert result.success is True
        assert result.objective == "latency"
        
        # Initial should be poor (8 calls/query)
        assert result.initial_scores["latency"] < 0.5
        
        # Should improve
        assert result.final_scores["latency"] > result.initial_scores["latency"]
        
        # Should apply latency reduction
        assert any("reduce_latency" in imp["adjustment"]["type"]
                  for imp in result.improvements)
    
    # ========== Balanced Optimization Tests ==========
    
    async def test_balanced_optimization_default_weights(self):
        """Test balanced optimization with default weights."""
        session_id = "test-tune-balanced-001"
        
        # Scenario: Mixed performance (moderate tokens, many calls)
        self._create_session_with_metrics(
            session_id, queries=3, tokens_per_call=25000, calls_per_query=5
        )
        
        result = await self.skill.execute(TunePerformanceInput(
            session_id=session_id,
            objective=OptimizationObjective.BALANCED,
            target_score=0.75
        ))
        
        assert result.success is True
        assert result.objective == "balanced"
        
        # Should optimize worst dimension first
        assert result.iterations > 0
        assert result.overall_improvement > 0
        
        # Should have multiple improvements
        assert len(result.improvements) > 0
    
    async def test_balanced_optimization_custom_weights(self):
        """Test balanced optimization with custom weights."""
        session_id = "test-tune-balanced-002"
        
        self._create_session_with_metrics(
            session_id, queries=2, tokens_per_call=30000, calls_per_query=4
        )
        
        # Prioritize cost over latency
        result = await self.skill.execute(TunePerformanceInput(
            session_id=session_id,
            objective=OptimizationObjective.BALANCED,
            target_score=0.7,
            weights={"cost": 0.6, "accuracy": 0.2, "latency": 0.2}
        ))
        
        assert result.success is True
        
        # Should have attempted optimization
        assert result.iterations > 0
        
        # Overall score should improve (even if individual metrics have trade-offs)
        assert result.overall_improvement != 0  # Some change occurred
    
    # ========== Edge Cases ==========
    
    async def test_nonexistent_session(self):
        """Test with non-existent session."""
        result = await self.skill.execute(TunePerformanceInput(
            session_id="nonexistent-session",
            objective=OptimizationObjective.COST
        ))
        
        assert result.success is False
        assert result.error is not None
        assert "Failed to evaluate" in result.error
    
    async def test_max_iterations_limit(self):
        """Test that max iterations is respected."""
        session_id = "test-tune-maxiter-001"
        
        # Create very poor performance
        self._create_session_with_metrics(
            session_id, queries=5, tokens_per_call=60000, calls_per_query=10
        )
        
        result = await self.skill.execute(TunePerformanceInput(
            session_id=session_id,
            objective=OptimizationObjective.BALANCED,
            target_score=0.95,  # Unrealistic target
            max_iterations=2
        ))
        
        assert result.success is True
        assert result.iterations <= 2  # Should stop at max
    
    async def test_target_score_reached_early(self):
        """Test early termination when target is reached."""
        session_id = "test-tune-early-001"
        
        # Moderate performance
        self._create_session_with_metrics(
            session_id, queries=2, tokens_per_call=15000, calls_per_query=3
        )
        
        result = await self.skill.execute(TunePerformanceInput(
            session_id=session_id,
            objective=OptimizationObjective.COST,
            target_score=0.6,  # Low target, easy to reach
            max_iterations=5
        ))
        
        assert result.success is True
        assert result.iterations < 5  # Should stop early
    
    # ========== Score Calculation Tests ==========
    
    async def test_score_calculation_excellent(self):
        """Test score calculation for excellent performance."""
        session_id = "test-tune-score-001"
        
        # Excellent: < 10K tokens, <= 2 calls
        self._create_session_with_metrics(
            session_id, queries=2, tokens_per_call=4000, calls_per_query=1
        )
        
        # Evaluate to get scores
        evaluator = EvaluateSessionSkill()
        eval_result = await evaluator.execute(EvaluateSessionInput(
            target_session_id=session_id
        ))
        
        scores = self.skill._calculate_scores(eval_result)
        
        assert scores["cost"] >= 0.9  # Excellent cost
        assert scores["latency"] >= 0.9  # Excellent latency
    
    async def test_score_calculation_poor(self):
        """Test score calculation for poor performance."""
        session_id = "test-tune-score-002"
        
        # Poor: > 40K tokens, > 6 calls
        self._create_session_with_metrics(
            session_id, queries=1, tokens_per_call=50000, calls_per_query=8
        )
        
        evaluator = EvaluateSessionSkill()
        eval_result = await evaluator.execute(EvaluateSessionInput(
            target_session_id=session_id
        ))
        
        scores = self.skill._calculate_scores(eval_result)
        
        assert scores["cost"] <= 0.3  # Poor cost
        assert scores["latency"] <= 0.3  # Poor latency
    
    # ========== Trade-off Analysis Tests ==========
    
    async def test_trade_off_analysis(self):
        """Test trade-off analysis between dimensions."""
        initial = {"cost": 0.6, "accuracy": 0.8, "latency": 0.7}
        final = {"cost": 0.85, "accuracy": 0.75, "latency": 0.68}
        
        trade_offs = self.skill._analyze_trade_offs(initial, final)
        
        # Cost improved
        assert trade_offs["cost"]["direction"] == "improved"
        assert trade_offs["cost"]["change"] > 0
        
        # Accuracy degraded (trade-off)
        assert trade_offs["accuracy"]["direction"] == "degraded"
        assert trade_offs["accuracy"]["change"] < 0
    
    # ========== Recommendation Tests ==========
    
    async def test_recommendations_generated(self):
        """Test that recommendations are generated."""
        session_id = "test-tune-rec-001"
        
        self._create_session_with_metrics(
            session_id, queries=2, tokens_per_call=30000, calls_per_query=5
        )
        
        result = await self.skill.execute(TunePerformanceInput(
            session_id=session_id,
            objective=OptimizationObjective.COST
        ))
        
        assert len(result.recommendations) > 0
        
        # Should have improvement summary
        assert any("✅" in rec or "⚠️" in rec for rec in result.recommendations)
        
        # Should have future suggestions
        assert any("💡" in rec for rec in result.recommendations)
    
    # ========== Integration Tests ==========
    
    async def test_full_optimization_cycle(self):
        """Test complete optimization cycle: evaluate → tune → verify."""
        session_id = "test-tune-cycle-001"
        
        # 1. Create session with poor performance
        self._create_session_with_metrics(
            session_id, queries=3, tokens_per_call=35000, calls_per_query=6
        )
        
        # 2. Initial evaluation
        evaluator = EvaluateSessionSkill()
        initial_eval = await evaluator.execute(EvaluateSessionInput(
            target_session_id=session_id
        ))
        
        assert initial_eval.success is True
        assert initial_eval.assessment["token_efficiency"] in ("moderate", "needs_improvement")
        
        # 3. Run tuning
        tune_result = await self.skill.execute(TunePerformanceInput(
            session_id=session_id,
            objective=OptimizationObjective.BALANCED,
            target_score=0.75
        ))
        
        assert tune_result.success is True
        assert tune_result.iterations > 0
        
        # 4. Verify improvement
        assert tune_result.overall_improvement > 0
        assert tune_result.final_scores["cost"] > tune_result.initial_scores["cost"]
        
        # 5. Check recommendations
        assert len(tune_result.recommendations) > 0
    
    async def test_multiple_objectives_same_session(self):
        """Test optimizing same session for different objectives."""
        session_id = "test-tune-multi-001"
        
        self._create_session_with_metrics(
            session_id, queries=2, tokens_per_call=25000, calls_per_query=5
        )
        
        # Optimize for cost
        cost_result = await self.skill.execute(TunePerformanceInput(
            session_id=session_id,
            objective=OptimizationObjective.COST
        ))
        
        # Optimize for latency
        latency_result = await self.skill.execute(TunePerformanceInput(
            session_id=session_id,
            objective=OptimizationObjective.LATENCY
        ))
        
        # Both should succeed
        assert cost_result.success is True
        assert latency_result.success is True
        
        # Should have different improvements
        assert cost_result.improvements[0]["bottleneck"] != latency_result.improvements[0]["bottleneck"]
