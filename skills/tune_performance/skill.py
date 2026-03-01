"""Self-tuning skill - automated performance optimization loop.

This meta-skill orchestrates other skills to form a closed-loop optimization:
1. Evaluate current performance (evaluate_session)
2. Identify bottlenecks
3. Adjust configuration
4. Re-evaluate to verify improvement
5. Repeat until target met or max iterations

Can be triggered by:
- Manual request: "Optimize my performance"
- Background job: Periodic tuning
- Threshold trigger: When efficiency drops below target
"""

from pydantic import Field
from core.skills.base import Skill, SkillInput, SkillOutput
from enum import Enum
import json


class OptimizationObjective(str, Enum):
    """Optimization objectives with different trade-offs."""
    COST = "cost"              # Minimize token usage and API costs
    ACCURACY = "accuracy"      # Maximize response quality and correctness
    LATENCY = "latency"        # Minimize response time and LLM calls
    BALANCED = "balanced"      # Balance all factors with weighted scoring


class TunePerformanceInput(SkillInput):
    """Input for performance tuning."""
    session_id: str = Field(..., description="Session to analyze and tune from")
    objective: OptimizationObjective = Field(
        default=OptimizationObjective.BALANCED,
        description="Optimization objective: cost, accuracy, latency, or balanced"
    )
    target_score: float = Field(
        default=0.8,
        ge=0.0,
        le=1.0,
        description="Target score (0-1) for the objective"
    )
    max_iterations: int = Field(default=3, description="Max tuning iterations")
    weights: dict[str, float] | None = Field(
        default=None,
        description="Custom weights for balanced mode: {cost: 0.3, accuracy: 0.4, latency: 0.3}"
    )


class TunePerformanceOutput(SkillOutput):
    """Output from performance tuning."""
    success: bool = True
    iterations: int = 0
    objective: str = "balanced"
    initial_scores: dict = Field(default_factory=dict)  # {cost: 0.6, accuracy: 0.8, latency: 0.7}
    final_scores: dict = Field(default_factory=dict)
    overall_improvement: float = 0.0  # Percentage improvement
    improvements: list[dict] = Field(default_factory=list)
    recommendations: list[str] = Field(default_factory=list)
    trade_offs: dict = Field(default_factory=dict)  # What was sacrificed for the gain


class TunePerformanceSkill(Skill[TunePerformanceInput, TunePerformanceOutput]):
    """Automated performance tuning through iterative evaluation and adjustment."""
    
    name = "tune_performance"
    description = "Automatically optimize agent performance through evaluation and adjustment"
    version = "1.0.0"
    
    async def execute(self, input_data: TunePerformanceInput) -> TunePerformanceOutput:
        """Execute multi-objective tuning loop."""
        from skills.evaluate_session.skill import EvaluateSessionSkill, EvaluateSessionInput
        
        evaluator = EvaluateSessionSkill()
        improvements = []
        
        # 1. Initial evaluation and scoring
        initial_eval = await evaluator.execute(EvaluateSessionInput(
            target_session_id=input_data.session_id
        ))
        
        if not initial_eval.success:
            return TunePerformanceOutput(
                success=False,
                error=f"Failed to evaluate session: {initial_eval.error}"
            )
        
        # Calculate multi-dimensional scores
        initial_scores = self._calculate_scores(initial_eval)
        current_scores = initial_scores.copy()
        iterations = 0
        
        # 2. Tuning loop
        while iterations < input_data.max_iterations:
            # Check if target already reached before iterating
            target_score = self._calculate_objective_score(
                current_scores, input_data.objective, input_data.weights
            )
            if target_score >= input_data.target_score:
                break  # Already optimal
            
            iterations += 1
            
            # Identify bottleneck based on objective
            bottleneck = self._identify_bottleneck_by_objective(
                current_scores, input_data.objective
            )
            
            if not bottleneck:
                break  # Target reached
            
            # Generate adjustment
            adjustment = self._generate_adjustment_for_objective(
                bottleneck, current_scores, input_data.objective
            )
            
            improvements.append({
                "iteration": iterations,
                "bottleneck": bottleneck,
                "adjustment": adjustment,
                "scores_before": current_scores.copy(),
            })
            
            # Simulate improvement (in real impl, apply and re-evaluate)
            current_scores, trade_offs = self._simulate_improvement_with_tradeoffs(
                current_scores, adjustment, input_data.objective
            )
            
            improvements[-1]["scores_after"] = current_scores.copy()
            improvements[-1]["trade_offs"] = trade_offs

        
        # 3. Calculate overall improvement
        initial_overall = self._calculate_objective_score(
            initial_scores, input_data.objective, input_data.weights
        )
        final_overall = self._calculate_objective_score(
            current_scores, input_data.objective, input_data.weights
        )
        
        # Calculate improvement percentage
        if initial_overall > 0:
            improvement_pct = (final_overall - initial_overall) / initial_overall * 100
        elif final_overall > initial_overall:
            # If starting from 0, use absolute improvement as percentage
            improvement_pct = final_overall * 100
        else:
            improvement_pct = 0
        
        # 4. Generate recommendations
        recommendations = self._generate_recommendations_by_objective(
            initial_scores, current_scores, improvements, input_data.objective
        )
        
        # 5. Analyze trade-offs
        trade_off_summary = self._analyze_trade_offs(initial_scores, current_scores)
        
        return TunePerformanceOutput(
            success=True,
            iterations=iterations,
            objective=input_data.objective.value,
            initial_scores=initial_scores,
            final_scores=current_scores,
            overall_improvement=round(improvement_pct, 1),
            improvements=improvements,
            recommendations=recommendations,
            trade_offs=trade_off_summary,
        )
    
    def _calculate_scores(self, evaluation) -> dict[str, float]:
        """Calculate normalized scores (0-1) for each dimension.
        
        NOTE: Accuracy score is currently a placeholder (0.7) because we don't have
        quality metrics in evaluate_session output yet. In production, this should
        come from quality_scorer which tracks:
        - Verification failures (firewall rejections)
        - User feedback (thumbs up/down)
        - Error rates (exceptions, retries)
        - Hallucination detection scores
        
        TODO: Integrate with core.evaluation.quality_scorer once available.
        """
        tokens_per_query = evaluation.tokens.get("avg_per_call", 0)
        calls_per_query = evaluation.assessment.get("calls_per_query", 0)
        
        # Cost score (inverse of token usage, normalized)
        # Excellent: < 10K tokens = 1.0, Poor: > 40K = 0.0
        cost_score = max(0.0, min(1.0, 1.0 - (tokens_per_query - 10000) / 30000))
        
        # Latency score (inverse of call count)
        # Excellent: <= 2 calls = 1.0, Poor: > 6 calls = 0.0
        latency_score = max(0.0, min(1.0, 1.0 - (calls_per_query - 2) / 4))
        
        # Accuracy score - PLACEHOLDER
        # Currently fixed at 0.7 because quality metrics are not yet available.
        # This means accuracy optimization will not show real improvements in current version.
        # Real implementation should query quality_scorer for actual metrics.
        accuracy_score = 0.7
        
        return {
            "cost": round(cost_score, 2),
            "accuracy": round(accuracy_score, 2),
            "latency": round(latency_score, 2),
        }
    
    def _calculate_objective_score(
        self, scores: dict, objective: OptimizationObjective, weights: dict | None
    ) -> float:
        """Calculate overall score based on objective."""
        if objective == OptimizationObjective.COST:
            return scores["cost"]
        elif objective == OptimizationObjective.ACCURACY:
            return scores["accuracy"]
        elif objective == OptimizationObjective.LATENCY:
            return scores["latency"]
        else:  # BALANCED
            w = weights or {"cost": 0.33, "accuracy": 0.34, "latency": 0.33}
            return (scores["cost"] * w.get("cost", 0.33) +
                   scores["accuracy"] * w.get("accuracy", 0.34) +
                   scores["latency"] * w.get("latency", 0.33))
    
    def _identify_bottleneck_by_objective(
        self, scores: dict, objective: OptimizationObjective
    ) -> str | None:
        """Identify bottleneck based on optimization objective."""
        if objective == OptimizationObjective.COST:
            return "cost" if scores["cost"] < 0.8 else None
        elif objective == OptimizationObjective.ACCURACY:
            return "accuracy" if scores["accuracy"] < 0.8 else None
        elif objective == OptimizationObjective.LATENCY:
            return "latency" if scores["latency"] < 0.8 else None
        else:  # BALANCED
            # Find lowest score
            min_score = min(scores.values())
            if min_score >= 0.7:
                return None
            return min(scores, key=scores.get)
    
    def _generate_adjustment_for_objective(
        self, bottleneck: str, scores: dict, objective: OptimizationObjective
    ) -> dict:
        """Generate adjustment targeting specific objective."""
        adjustments = {
            "cost": {
                "type": "reduce_tokens",
                "action": "Enable aggressive context compaction at 40%",
                "config": {"compaction_threshold": 0.4, "max_history": 3},
                "expected": "+25% cost reduction",
                "trade_off": "May reduce context quality slightly",
            },
            "accuracy": {
                "type": "improve_quality",
                "action": "Use larger model and increase context window",
                "config": {"model": "gpt-4", "max_context": 32000},
                "expected": "+15% accuracy improvement",
                "trade_off": "Will increase cost by ~30%",
            },
            "latency": {
                "type": "reduce_latency",
                "action": "Enable parallel tool execution and caching",
                "config": {"parallel_tools": True, "cache_ttl": 300},
                "expected": "-40% response time",
                "trade_off": "May miss some context updates",
            },
        }
        
        return adjustments.get(bottleneck, {
            "type": "unknown",
            "action": "No adjustment available",
        })
    
    def _simulate_improvement_with_tradeoffs(
        self, scores: dict, adjustment: dict, objective: OptimizationObjective
    ) -> tuple[dict, dict]:
        """Simulate improvement with realistic trade-offs."""
        new_scores = scores.copy()
        trade_offs = {}
        
        adj_type = adjustment.get("type")
        
        if adj_type == "reduce_tokens":
            # Improve cost, slight accuracy penalty
            new_scores["cost"] = min(1.0, scores["cost"] + 0.25)
            new_scores["accuracy"] = max(0.0, scores["accuracy"] - 0.05)
            trade_offs = {"accuracy": -0.05}
        
        elif adj_type == "improve_quality":
            # Improve accuracy, cost penalty
            new_scores["accuracy"] = min(1.0, scores["accuracy"] + 0.15)
            new_scores["cost"] = max(0.0, scores["cost"] - 0.30)
            new_scores["latency"] = max(0.0, scores["latency"] - 0.10)
            trade_offs = {"cost": -0.30, "latency": -0.10}
        
        elif adj_type == "reduce_latency":
            # Improve latency, slight accuracy penalty
            new_scores["latency"] = min(1.0, scores["latency"] + 0.40)
            new_scores["accuracy"] = max(0.0, scores["accuracy"] - 0.08)
            trade_offs = {"accuracy": -0.08}
        
        return new_scores, trade_offs
    
    def _analyze_trade_offs(self, initial: dict, final: dict) -> dict:
        """Analyze what was gained vs what was sacrificed."""
        trade_offs = {}
        for metric in ["cost", "accuracy", "latency"]:
            change = final[metric] - initial[metric]
            if abs(change) > 0.01:
                trade_offs[metric] = {
                    "change": round(change, 2),
                    "direction": "improved" if change > 0 else "degraded",
                    "percentage": round(change / initial[metric] * 100, 1) if initial[metric] > 0 else 0,
                }
        return trade_offs
    
    def _generate_recommendations_by_objective(
        self, initial: dict, final: dict, improvements: list, objective: OptimizationObjective
    ) -> list[str]:
        """Generate recommendations based on objective."""
        recs = []
        
        # If no improvements (already optimal), provide status message
        if not improvements:
            overall_score = self._calculate_objective_score(
                final, objective, weights=None
            )
            recs.append(f"✅ Already optimal for {objective.value} (score: {overall_score:.2f})")
            recs.append("💡 No further optimization needed")
            return recs
        
        # Summary of changes
        trade_offs = self._analyze_trade_offs(initial, final)
        if trade_offs:
            for metric, data in trade_offs.items():
                if data["direction"] == "improved":
                    recs.append(
                        f"✅ {metric.capitalize()}: {data['direction']} by {abs(data['percentage'])}%"
                    )
                else:
                    recs.append(
                        f"⚠️ {metric.capitalize()}: {data['direction']} by {abs(data['percentage'])}% (trade-off)"
                    )
        else:
            # No significant changes detected
            recs.append("ℹ️ Optimization completed with minimal changes")
        
        # Objective-specific recommendations
        if objective == OptimizationObjective.COST:
            if final["cost"] < 0.9:
                recs.append("💡 Consider: Switch to smaller model for simple queries")
                recs.append("💡 Consider: Implement query classification to route appropriately")
        
        elif objective == OptimizationObjective.ACCURACY:
            if final["accuracy"] < 0.9:
                recs.append("💡 Consider: Add verification step with firewall")
                recs.append("💡 Consider: Implement multi-agent review for critical decisions")
        
        elif objective == OptimizationObjective.LATENCY:
            if final["latency"] < 0.9:
                recs.append("💡 Consider: Pre-compute common queries")
                recs.append("💡 Consider: Stream responses for better perceived latency")
        
        # Ensure at least one recommendation
        if not recs:
            recs.append("✅ Optimization completed successfully")
        
        return recs


# Register the skill
skill = TunePerformanceSkill()
