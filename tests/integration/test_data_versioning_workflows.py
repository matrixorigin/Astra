"""Integration tests for P2 Data Versioning — Prompt Experiments, Knowledge Regression, Training Data."""

import json
import pytest
from datetime import datetime, timedelta
from sqlalchemy import text
from sqlalchemy.orm import Session

from core.data_versioning import (
    PromptExperiment,
    PromptVariant,
    ExperimentConfig,
    ExperimentStatus,
    KnowledgeRegression,
    RegressionType,
    TrainingDataPipeline,
    DatasetConfig,
    DatasetStatus,
)
from api.database import get_db_session


@pytest.fixture
def db():
    """Get database session."""
    return next(get_db_session())


class TestPromptExperiment:
    """Test prompt experiment workflow."""
    
    def test_prompt_variant_serialization(self, db: Session):
        """Test PromptVariant serialization."""
        variant = PromptVariant(
            variant_id="v1",
            name="Test Variant",
            system_prompt="Test prompt",
            temperature=0.8,
            max_tokens=1024,
        )
        
        variant_dict = variant.to_dict()
        assert variant_dict["variant_id"] == "v1"
        assert variant_dict["name"] == "Test Variant"
        assert variant_dict["temperature"] == 0.8
    
    def test_experiment_config_serialization(self, db: Session):
        """Test ExperimentConfig serialization."""
        config = ExperimentConfig(
            experiment_id="exp_001",
            name="Test Experiment",
            description="Test",
            skill_name="test_skill",
            baseline_variant=PromptVariant(
                variant_id="baseline",
                name="Baseline",
                system_prompt="Baseline prompt",
            ),
            test_variants=[
                PromptVariant(
                    variant_id="v1",
                    name="Variant 1",
                    system_prompt="Variant 1 prompt",
                ),
            ],
        )
        
        config_dict = config.to_dict()
        assert config_dict["experiment_id"] == "exp_001"
        assert config_dict["skill_name"] == "test_skill"
        assert len(config_dict["test_variants"]) == 1
    
    def test_experiment_result_creation(self, db: Session):
        """Test ExperimentResult creation."""
        from core.data_versioning import ExperimentResult
        
        result = ExperimentResult(
            experiment_id="exp_001",
            variant_id="v1",
            accuracy=0.95,
            latency_ms=150,
            cost_usd=0.001,
            satisfaction=0.9,
            sample_count=100,
            confidence_interval=(0.93, 0.97),
            p_value=0.01,
            effect_size=0.2,
        )
        
        assert result.accuracy == 0.95
        assert result.sample_count == 100
        assert result.p_value == 0.01
    
    def test_welch_ttest_significant_difference(self, db: Session):
        """Test Welch's t-test with significant difference."""
        exp = PromptExperiment(lambda: db)
        
        # Group 1: baseline (mean=0.8)
        group1 = [0.78, 0.79, 0.80, 0.81, 0.82] * 10  # 50 samples
        
        # Group 2: variant (mean=0.90, clearly different)
        group2 = [0.88, 0.89, 0.90, 0.91, 0.92] * 10  # 50 samples
        
        p_value, effect_size = exp._welch_ttest(group1, group2)
        
        # Should be highly significant (p < 0.05)
        assert p_value < 0.05, f"Expected p < 0.05, got {p_value}"
        # Effect size should be large (Cohen's d > 0.8)
        assert effect_size > 0.5, f"Expected effect_size > 0.5, got {effect_size}"
    
    def test_welch_ttest_no_difference(self, db: Session):
        """Test Welch's t-test with no significant difference."""
        exp = PromptExperiment(lambda: db)
        
        # Both groups have same mean
        group1 = [0.80, 0.81, 0.79, 0.80, 0.81] * 10
        group2 = [0.80, 0.81, 0.79, 0.80, 0.81] * 10
        
        p_value, effect_size = exp._welch_ttest(group1, group2)
        
        # Should NOT be significant (p > 0.05)
        assert p_value > 0.05, f"Expected p > 0.05, got {p_value}"
        # Effect size should be near zero
        assert abs(effect_size) < 0.1, f"Expected effect_size ≈ 0, got {effect_size}"
    
    def test_welch_ttest_small_sample(self, db: Session):
        """Test Welch's t-test with small sample size."""
        exp = PromptExperiment(lambda: db)
        
        # Too small samples
        group1 = [0.8]
        group2 = [0.9]
        
        p_value, effect_size = exp._welch_ttest(group1, group2)
        
        # Should return default values
        assert p_value == 1.0
        assert effect_size == 0.0
    
    def test_normal_cdf_values(self, db: Session):
        """Test normal CDF approximation."""
        exp = PromptExperiment(lambda: db)
        
        # Test known values
        cdf_0 = exp._normal_cdf(0.0)
        assert 0.49 < cdf_0 < 0.51, f"CDF(0) should be ≈0.5, got {cdf_0}"
        
        cdf_pos = exp._normal_cdf(1.96)
        assert 0.97 < cdf_pos < 0.98, f"CDF(1.96) should be ≈0.975, got {cdf_pos}"
        
        cdf_neg = exp._normal_cdf(-1.96)
        assert 0.02 < cdf_neg < 0.03, f"CDF(-1.96) should be ≈0.025, got {cdf_neg}"



class TestKnowledgeRegression:
    """Test knowledge regression detection."""
    
    def test_regression_signal_creation(self, db: Session):
        """Test RegressionSignal creation."""
        from core.data_versioning import RegressionSignal
        
        signal = RegressionSignal(
            signal_id="test_signal",
            regression_type=RegressionType.SKILL_DEPRECATED,
            affected_skill="old_skill",
            affected_sessions=10,
            affected_decisions=15,
            confidence=0.95,
            detected_at=datetime.utcnow(),
            metadata={"reason": "deprecated"},
        )
        
        assert signal.signal_id == "test_signal"
        assert signal.regression_type == RegressionType.SKILL_DEPRECATED
        assert signal.affected_sessions == 10
        assert signal.confidence == 0.95
    
    def test_regression_report_creation(self, db: Session):
        """Test RegressionReport creation."""
        from core.data_versioning import RegressionReport, RegressionSignal
        
        signals = [
            RegressionSignal(
                signal_id="sig1",
                regression_type=RegressionType.SKILL_DEPRECATED,
                affected_skill="skill1",
                affected_sessions=5,
                affected_decisions=5,
                confidence=0.9,
                detected_at=datetime.utcnow(),
            ),
        ]
        
        report = RegressionReport(
            report_id="report_001",
            signals=signals,
            total_affected_sessions=5,
            total_affected_decisions=5,
            generated_at=datetime.utcnow(),
        )
        
        assert report.report_id == "report_001"
        assert len(report.signals) == 1
        assert report.total_affected_sessions == 5


class TestTrainingDataPipeline:
    """Test training data extraction pipeline."""
    
    def test_dataset_config_creation(self, db: Session):
        """Test DatasetConfig creation."""
        config = DatasetConfig(
            dataset_id="dataset_001",
            name="SQL Generator Training",
            description="Training data for SQL generator",
            filters={"skill_name": "sql_generator"},
            quality_threshold=0.75,
        )
        
        assert config.dataset_id == "dataset_001"
        assert config.name == "SQL Generator Training"
        assert config.quality_threshold == 0.75
    
    def test_training_example_creation(self, db: Session):
        """Test TrainingExample creation."""
        from core.data_versioning import TrainingExample, DatasetLineage
        
        lineage = DatasetLineage(
            dataset_id="dataset_001",
            source_branch="dataset_dataset_001",
            source_table="conversation_events",
            extraction_query="SELECT * FROM ...",
            extracted_at=datetime.utcnow(),
            row_count=100,
        )
        
        example = TrainingExample(
            example_id="ex_001",
            session_id="sess_001",
            event_id="evt_001",
            input_text="What is SQL?",
            output_text="SQL is...",
            skill_name="sql_generator",
            quality_score=0.95,
            lineage=lineage,
        )
        
        assert example.example_id == "ex_001"
        assert example.quality_score == 0.95
        assert example.lineage.row_count == 100


class TestDataVersioningIntegration:
    """Integration tests across all P2 components."""
    
    def test_experiment_config_json_serialization(self, db: Session):
        """Test experiment config can be serialized to JSON."""
        config = ExperimentConfig(
            experiment_id="exp_001",
            name="Test",
            description="Test",
            skill_name="test_skill",
            baseline_variant=PromptVariant(
                variant_id="baseline",
                name="Baseline",
                system_prompt="Test",
            ),
            test_variants=[
                PromptVariant(
                    variant_id="v1",
                    name="Variant 1",
                    system_prompt="Variant 1",
                ),
            ],
        )
        
        config_dict = config.to_dict()
        config_json = json.dumps(config_dict)
        
        assert "exp_001" in config_json
        assert "baseline" in config_json
        assert "v1" in config_json
    
    def test_regression_signal_json_serialization(self, db: Session):
        """Test regression signal can be serialized to JSON."""
        from core.data_versioning import RegressionSignal
        
        signal = RegressionSignal(
            signal_id="sig_001",
            regression_type=RegressionType.SKILL_DEPRECATED,
            affected_skill="old_skill",
            affected_sessions=10,
            affected_decisions=15,
            confidence=0.95,
            detected_at=datetime.utcnow(),
            metadata={"reason": "deprecated"},
        )
        
        signal_dict = {
            "signal_id": signal.signal_id,
            "regression_type": signal.regression_type.value,
            "affected_skill": signal.affected_skill,
            "affected_sessions": signal.affected_sessions,
            "confidence": signal.confidence,
        }
        
        signal_json = json.dumps(signal_dict)
        assert "sig_001" in signal_json
        assert "skill_deprecated" in signal_json
    
    def test_dataset_lineage_tracking(self, db: Session):
        """Test dataset lineage can be tracked."""
        from core.data_versioning import DatasetLineage
        
        lineage = DatasetLineage(
            dataset_id="dataset_001",
            source_branch="dataset_dataset_001",
            source_table="conversation_events",
            extraction_query="SELECT * FROM conversation_events WHERE quality_score > 0.75",
            extracted_at=datetime.utcnow(),
            row_count=1000,
            metadata={
                "filter": "quality_score > 0.75",
                "extraction_method": "streaming",
            },
        )
        
        assert lineage.dataset_id == "dataset_001"
        assert lineage.row_count == 1000
        assert lineage.metadata["extraction_method"] == "streaming"
