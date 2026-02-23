"""Prompt experiment workflow — P2 Data Versioning.

Sandbox-isolated prompt variants with A/B testing, statistical significance testing.
"""

from __future__ import annotations

import json
import logging
from dataclasses import dataclass, field
from datetime import datetime
from enum import Enum
from typing import TYPE_CHECKING, Optional

from sqlalchemy import text
from sqlalchemy.orm import Session

from core.sandbox import Sandbox, Branch

if TYPE_CHECKING:
    from core.evaluation.regression_gate import RegressionGate

logger = logging.getLogger(__name__)


class ExperimentStatus(str, Enum):
    """Experiment lifecycle status."""
    DRAFT = "draft"
    RUNNING = "running"
    COMPLETED = "completed"
    ARCHIVED = "archived"


@dataclass
class PromptVariant:
    """Single prompt variant in experiment."""
    variant_id: str
    name: str
    system_prompt: str
    temperature: float = 0.7
    max_tokens: int = 2048
    metadata: dict = field(default_factory=dict)
    
    def to_dict(self) -> dict:
        """Serialize to dict."""
        return {
            "variant_id": self.variant_id,
            "name": self.name,
            "system_prompt": self.system_prompt,
            "temperature": self.temperature,
            "max_tokens": self.max_tokens,
            "metadata": self.metadata,
        }


@dataclass
class ExperimentConfig:
    """Experiment configuration."""
    experiment_id: str
    name: str
    description: str
    skill_name: str
    baseline_variant: PromptVariant
    test_variants: list[PromptVariant]
    sample_size: int = 100
    confidence_threshold: float = 0.95
    min_effect_size: float = 0.05  # Minimum detectable effect
    metadata: dict = field(default_factory=dict)
    
    def to_dict(self) -> dict:
        """Serialize to dict."""
        return {
            "experiment_id": self.experiment_id,
            "name": self.name,
            "description": self.description,
            "skill_name": self.skill_name,
            "baseline_variant": self.baseline_variant.to_dict(),
            "test_variants": [v.to_dict() for v in self.test_variants],
            "sample_size": self.sample_size,
            "confidence_threshold": self.confidence_threshold,
            "min_effect_size": self.min_effect_size,
            "metadata": self.metadata,
        }


@dataclass
class ExperimentResult:
    """Experiment result with statistical significance."""
    experiment_id: str
    variant_id: str
    accuracy: float
    latency_ms: float
    cost_usd: float
    satisfaction: float
    sample_count: int
    confidence_interval: tuple[float, float]
    p_value: float  # Statistical significance
    is_winner: bool = False
    effect_size: float = 0.0  # Cohen's d or similar


class PromptExperiment:
    """Manage prompt experiments in isolated sandboxes with statistical rigor."""
    
    def __init__(self, db: Session, account: str = "sys", source_db: str = "dev_agent"):
        """Initialize experiment manager.
        
        Args:
            db: Database session
            account: Account for sandbox isolation
            source_db: Source database for branching
        """
        self.db = db
        self.sandbox = Sandbox(db=db, source_db=source_db, account=account)
        self.branch = Branch(database=source_db, db=db)
        self.source_db = source_db
    
    def create_experiment(self, config: ExperimentConfig) -> str:
        """Create new experiment with sandbox and branched conversation_events.
        
        Args:
            config: Experiment configuration
            
        Returns:
            Experiment ID
        """
        exp_id = config.experiment_id
        
        # 1. Create sandbox for experiment
        self.sandbox.create(
            name=exp_id,
            description=f"Experiment: {config.name}",
            created_by="system",
            tables=["conversation_events"],  # Branch conversation_events
        )
        
        # 2. Create experiment_config table in sandbox
        self.db.execute(text(f"""
            CREATE TABLE {exp_id}.experiment_config (
                experiment_id VARCHAR(255) PRIMARY KEY,
                name VARCHAR(255) NOT NULL,
                description TEXT,
                skill_name VARCHAR(255) NOT NULL,
                baseline_variant JSON NOT NULL,
                test_variants JSON NOT NULL,
                sample_size INT DEFAULT 100,
                confidence_threshold DECIMAL(3,2) DEFAULT 0.95,
                min_effect_size DECIMAL(3,2) DEFAULT 0.05,
                status VARCHAR(50) DEFAULT 'draft',
                created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
                started_at TIMESTAMP NULL,
                completed_at TIMESTAMP NULL,
                archived_at TIMESTAMP NULL,
                winner_variant_id VARCHAR(255) NULL,
                metadata JSON
            )
        """))
        
        # 3. Create variant_results table in sandbox
        self.db.execute(text(f"""
            CREATE TABLE {exp_id}.variant_results (
                result_id INT AUTO_INCREMENT PRIMARY KEY,
                variant_id VARCHAR(255) NOT NULL,
                session_id VARCHAR(255) NOT NULL,
                event_id VARCHAR(255) NOT NULL,
                accuracy DECIMAL(3,2),
                latency_ms INT,
                cost_usd DECIMAL(10,6),
                satisfaction DECIMAL(3,2),
                recorded_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
                INDEX idx_variant (variant_id),
                INDEX idx_session (session_id)
            )
        """))
        
        # 4. Store config
        self.db.execute(text(f"""
            INSERT INTO {exp_id}.experiment_config 
            (experiment_id, name, description, skill_name, baseline_variant, test_variants, 
             sample_size, confidence_threshold, min_effect_size, status, metadata)
            VALUES (:exp_id, :name, :desc, :skill, :baseline, :test_variants, 
                    :sample_size, :conf_threshold, :min_effect, :status, :metadata)
        """), {
            "exp_id": exp_id,
            "name": config.name,
            "desc": config.description,
            "skill": config.skill_name,
            "baseline": json.dumps(config.baseline_variant.to_dict()),
            "test_variants": json.dumps([v.to_dict() for v in config.test_variants]),
            "sample_size": config.sample_size,
            "conf_threshold": config.confidence_threshold,
            "min_effect": config.min_effect_size,
            "status": ExperimentStatus.DRAFT.value,
            "metadata": json.dumps(config.metadata),
        })
        self.db.commit()
        
        return exp_id
    
    def start_experiment(self, experiment_id: str) -> None:
        """Start experiment — route traffic to variants.
        
        Args:
            experiment_id: Experiment ID
        """
        self.db.execute(text(f"""
            UPDATE {experiment_id}.experiment_config
            SET status = :status, started_at = CURRENT_TIMESTAMP
            WHERE experiment_id = :exp_id
        """), {"status": ExperimentStatus.RUNNING.value, "exp_id": experiment_id})
        self.db.commit()
    
    def record_variant_result(
        self,
        experiment_id: str,
        variant_id: str,
        session_id: str,
        event_id: str,
        accuracy: float,
        latency_ms: int,
        cost_usd: float,
        satisfaction: float,
    ) -> None:
        """Record single result for variant. Commits immediately.

        For high-volume recording, prefer record_variant_results_batch().
        """
        self.record_variant_results_batch(experiment_id, [{
            "variant_id": variant_id,
            "session_id": session_id,
            "event_id": event_id,
            "accuracy": accuracy,
            "latency_ms": latency_ms,
            "cost_usd": cost_usd,
            "satisfaction": satisfaction,
        }])

    def record_variant_results_batch(
        self,
        experiment_id: str,
        results: list[dict],
    ) -> int:
        """Batch-insert variant results in a single transaction.

        Chunks into statements of at most BATCH_LIMIT rows to avoid
        oversized SQL packets.  All chunks share one commit.

        Args:
            experiment_id: Experiment ID (sandbox database name)
            results: List of dicts with keys: variant_id, session_id, event_id,
                     accuracy, latency_ms, cost_usd, satisfaction

        Returns:
            Number of rows inserted
        """
        if not results:
            return 0
        BATCH_LIMIT = 100
        for chunk_start in range(0, len(results), BATCH_LIMIT):
            chunk = results[chunk_start:chunk_start + BATCH_LIMIT]
            placeholders = []
            params: dict = {}
            for i, r in enumerate(chunk):
                placeholders.append(
                    f"(:v{i}, :s{i}, :e{i}, :a{i}, :l{i}, :c{i}, :sat{i})"
                )
                params[f"v{i}"] = r["variant_id"]
                params[f"s{i}"] = r["session_id"]
                params[f"e{i}"] = r["event_id"]
                params[f"a{i}"] = r["accuracy"]
                params[f"l{i}"] = r["latency_ms"]
                params[f"c{i}"] = r["cost_usd"]
                params[f"sat{i}"] = r["satisfaction"]
            sql = (
                f"INSERT INTO {experiment_id}.variant_results "
                f"(variant_id, session_id, event_id, accuracy, latency_ms, cost_usd, satisfaction) "
                f"VALUES {', '.join(placeholders)}"
            )
            self.db.execute(text(sql), params)
        self.db.commit()
        return len(results)
    
    def get_experiment_results(self, experiment_id: str) -> dict[str, ExperimentResult]:
        """Get aggregated results with statistical significance testing.
        
        Args:
            experiment_id: Experiment ID
            
        Returns:
            Dict mapping variant_id to ExperimentResult
        """
        # Fetch all results
        rows = self.db.execute(text(f"""
            SELECT 
                variant_id,
                accuracy,
                latency_ms,
                cost_usd,
                satisfaction
            FROM {experiment_id}.variant_results
            ORDER BY variant_id
        """)).fetchall()
        
        # Group by variant
        variant_data = {}
        for row in rows:
            variant_id, accuracy, latency, cost, satisfaction = row
            if variant_id not in variant_data:
                variant_data[variant_id] = {
                    "accuracies": [],
                    "latencies": [],
                    "costs": [],
                    "satisfactions": [],
                }
            variant_data[variant_id]["accuracies"].append(float(accuracy or 0))
            variant_data[variant_id]["latencies"].append(int(latency or 0))
            variant_data[variant_id]["costs"].append(float(cost or 0))
            variant_data[variant_id]["satisfactions"].append(float(satisfaction or 0))
        
        # Get baseline for comparison
        baseline_id = None
        baseline_accuracies = None
        try:
            config_row = self.db.execute(text(f"""
                SELECT baseline_variant FROM {experiment_id}.experiment_config
                WHERE experiment_id = :exp_id
            """), {"exp_id": experiment_id}).fetchone()
            if config_row:
                baseline_config = json.loads(config_row[0])
                baseline_id = baseline_config["variant_id"]
                if baseline_id in variant_data:
                    baseline_accuracies = variant_data[baseline_id]["accuracies"]
        except Exception:
            pass
        
        # Compute statistics with t-test
        results = {}
        for variant_id, data in variant_data.items():
            accuracies = data["accuracies"]
            n = len(accuracies)
            
            if n == 0:
                continue
            
            mean_accuracy = sum(accuracies) / n
            
            # Compute variance (Bessel's correction for sample variance)
            if n > 1:
                variance = sum((x - mean_accuracy) ** 2 for x in accuracies) / (n - 1)
            else:
                variance = 0.0
            
            stddev = variance ** 0.5
            
            # 95% confidence interval
            if n > 1:
                ci_margin = 1.96 * stddev / (n ** 0.5)
            else:
                ci_margin = 0.0
            
            # Welch's t-test vs baseline (if not baseline itself)
            p_value = 1.0
            effect_size = 0.0
            
            if baseline_id and variant_id != baseline_id and baseline_accuracies:
                p_value, effect_size = self._welch_ttest(
                    baseline_accuracies, accuracies
                )
            
            results[variant_id] = ExperimentResult(
                experiment_id=experiment_id,
                variant_id=variant_id,
                accuracy=mean_accuracy,
                latency_ms=sum(data["latencies"]) / n if data["latencies"] else 0,
                cost_usd=sum(data["costs"]) / n if data["costs"] else 0,
                satisfaction=sum(data["satisfactions"]) / n if data["satisfactions"] else 0,
                sample_count=n,
                confidence_interval=(mean_accuracy - ci_margin, mean_accuracy + ci_margin),
                p_value=p_value,
                effect_size=effect_size,
            )
        
        return results
    
    def _welch_ttest(self, group1: list[float], group2: list[float]) -> tuple[float, float]:
        """Welch's t-test (unequal variance t-test).
        
        Args:
            group1: First group (baseline)
            group2: Second group (variant)
            
        Returns:
            (p_value, effect_size)
        """
        n1 = len(group1)
        n2 = len(group2)
        
        if n1 < 2 or n2 < 2:
            return 1.0, 0.0
        
        mean1 = sum(group1) / n1
        mean2 = sum(group2) / n2
        
        var1 = sum((x - mean1) ** 2 for x in group1) / (n1 - 1)
        var2 = sum((x - mean2) ** 2 for x in group2) / (n2 - 1)
        
        # Avoid division by zero
        if var1 == 0 and var2 == 0:
            return 1.0, 0.0
        
        # Welch's t-statistic
        se = (var1 / n1 + var2 / n2) ** 0.5
        if se == 0:
            return 1.0, 0.0
        
        t_stat = (mean2 - mean1) / se
        
        # Welch-Satterthwaite degrees of freedom
        numerator = (var1 / n1 + var2 / n2) ** 2
        denominator = (var1 / n1) ** 2 / (n1 - 1) + (var2 / n2) ** 2 / (n2 - 1)
        
        if denominator == 0:
            df = n1 + n2 - 2
        else:
            df = numerator / denominator
        
        # Approximate p-value using t-distribution (two-tailed)
        # For simplicity, use normal approximation for large df
        if df > 30:
            # Normal approximation
            p_value = 2 * (1 - self._normal_cdf(abs(t_stat)))
        else:
            # Conservative estimate for small df
            p_value = self._t_distribution_pvalue(t_stat, df)
        
        # Cohen's d effect size
        pooled_std = ((var1 + var2) / 2) ** 0.5
        if pooled_std == 0:
            effect_size = 0.0
        else:
            effect_size = (mean2 - mean1) / pooled_std
        
        return min(p_value, 1.0), effect_size
    
    def _normal_cdf(self, x: float) -> float:
        """Approximate normal CDF using error function."""
        # Approximation: Φ(x) ≈ 0.5 * (1 + erf(x / sqrt(2)))
        # Using simpler approximation for production
        if x < -6:
            return 0.0
        if x > 6:
            return 1.0
        
        # Abramowitz and Stegun approximation
        a1 = 0.254829592
        a2 = -0.284496736
        a3 = 1.421413741
        a4 = -1.453152027
        a5 = 1.061405429
        p = 0.3275911
        
        sign = 1 if x >= 0 else -1
        x = abs(x) / (2 ** 0.5)
        
        t = 1.0 / (1.0 + p * x)
        y = 1.0 - (((((a5 * t + a4) * t) + a3) * t + a2) * t + a1) * t * (2.718281828 ** (-x * x))
        
        return 0.5 * (1.0 + sign * y)
    
    def _t_distribution_pvalue(self, t_stat: float, df: float) -> float:
        """Approximate t-distribution p-value (two-tailed)."""
        # For small df, use conservative approximation
        # This is a simplified approximation
        abs_t = abs(t_stat)
        
        if df >= 30:
            return 2 * (1 - self._normal_cdf(abs_t))
        
        # Conservative: use normal approximation with adjustment
        p_normal = 2 * (1 - self._normal_cdf(abs_t))
        
        # Adjustment factor for small df (t-distribution has heavier tails)
        adjustment = 1.0 + 1.0 / (4 * df)
        
        return min(p_normal * adjustment, 1.0)
    
    def determine_winner(self, experiment_id: str) -> Optional[str]:
        """Determine statistical winner using multi-armed bandit logic.
        
        Args:
            experiment_id: Experiment ID
            
        Returns:
            Winner variant_id or None if no clear winner
        """
        results = self.get_experiment_results(experiment_id)
        
        if not results:
            return None
        
        # Get baseline
        config_row = self.db.execute(text(f"""
            SELECT baseline_variant FROM {experiment_id}.experiment_config
            WHERE experiment_id = :exp_id
        """), {"exp_id": experiment_id}).fetchone()
        
        if not config_row:
            return None
        
        baseline_config = json.loads(config_row[0])
        baseline_id = baseline_config["variant_id"]
        baseline_result = results.get(baseline_id)
        
        if not baseline_result:
            return None
        
        # Find variant with highest accuracy and statistically significant improvement
        best_variant = baseline_id
        best_accuracy = baseline_result.accuracy
        
        for variant_id, result in results.items():
            if variant_id == baseline_id:
                continue
            
            # Check if improvement is significant
            improvement = result.accuracy - baseline_result.accuracy
            if improvement > 0.05 and result.p_value < 0.05:  # 5% improvement, p < 0.05
                if result.accuracy > best_accuracy:
                    best_variant = variant_id
                    best_accuracy = result.accuracy
        
        return best_variant
    
    def complete_experiment(
        self,
        experiment_id: str,
        regression_gate: Optional[RegressionGate] = None,
    ) -> str:
        """Complete experiment, optionally gating the winner through regression validation.

        Args:
            experiment_id: Experiment ID
            regression_gate: If provided, winner must pass gate before promotion.

        Returns:
            Winner variant_id, or "gate_failed" / "unknown".
        """
        winner_id = self.determine_winner(experiment_id)

        if not winner_id:
            return "unknown"

        # Gate: validate winner against golden sessions before promoting
        if regression_gate is not None:
            from core.evaluation.regression_gate import ChangeType

            # Fetch winner prompt content from config
            config_row = self.db.execute(text(f"""
                SELECT baseline_variant, test_variants
                FROM {experiment_id}.experiment_config
                WHERE experiment_id = :exp_id
            """), {"exp_id": experiment_id}).fetchone()

            prompt_content = ""
            if config_row:
                baseline = json.loads(config_row[0])
                if baseline["variant_id"] == winner_id:
                    prompt_content = baseline.get("system_prompt", "")
                else:
                    for v in json.loads(config_row[1]):
                        if v["variant_id"] == winner_id:
                            prompt_content = v.get("system_prompt", "")
                            break

            gate_result = regression_gate.validate_change(
                change_type=ChangeType.PROMPT,
                change_id=f"{experiment_id}:{winner_id}",
                change_content={"content": prompt_content, "template_id": experiment_id},
            )

            if gate_result["verdict"] != "pass":
                logger.warning(
                    "Experiment %s winner %s failed regression gate: %s",
                    experiment_id, winner_id, gate_result["reason"],
                )
                self.db.execute(text(f"""
                    UPDATE {experiment_id}.experiment_config
                    SET status = 'gate_failed'
                    WHERE experiment_id = :exp_id
                """), {"exp_id": experiment_id})
                self.db.commit()
                return "gate_failed"

        self.db.execute(text(f"""
            UPDATE {experiment_id}.experiment_config
            SET status = :status, completed_at = CURRENT_TIMESTAMP, winner_variant_id = :winner
            WHERE experiment_id = :exp_id
        """), {
            "status": ExperimentStatus.COMPLETED.value,
            "winner": winner_id,
            "exp_id": experiment_id,
        })

        self.db.commit()
        return winner_id
    
    def cleanup_experiment(self, experiment_id: str) -> None:
        """Archive experiment and cleanup sandbox.
        
        Args:
            experiment_id: Experiment ID
        """
        self.db.execute(text(f"""
            UPDATE {experiment_id}.experiment_config
            SET status = :status, archived_at = CURRENT_TIMESTAMP
            WHERE experiment_id = :exp_id
        """), {"status": ExperimentStatus.ARCHIVED.value, "exp_id": experiment_id})
        self.db.commit()
        
        # Delete sandbox (includes branched conversation_events)
        self.sandbox.delete(experiment_id)
