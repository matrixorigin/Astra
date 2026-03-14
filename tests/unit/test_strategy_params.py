"""Unit tests for strategy params validation.

Tests Pydantic schemas for strategy parameters.
"""

import pytest
from core.memory.strategy.params import (
    ActivationV1Params,
    InvalidStrategyParamsError,
    VectorV1Params,
    get_default_params,
    validate_strategy_params,
)


class TestStrategyParamsValidation:
    def test_vector_v1_defaults(self):
        """VectorV1Params has correct defaults."""
        p = VectorV1Params()
        assert p.semantic_weight == 0.4
        assert p.temporal_weight == 0.3
        assert p.confidence_weight == 0.2
        assert p.importance_weight == 0.1

    def test_activation_v1_defaults(self):
        """ActivationV1Params has correct defaults."""
        p = ActivationV1Params()
        assert p.spreading_factor == 0.8
        assert p.num_iterations == 3
        assert p.inhibition_beta == 0.15
        assert p.sigmoid_theta == 0.1
        assert p.min_graph_nodes == 50

    def test_validate_valid_params(self):
        """Valid params pass validation and get defaults filled."""
        result = validate_strategy_params(
            "vector:v1",
            {"semantic_weight": 0.6},
        )
        assert result is not None
        assert result["semantic_weight"] == 0.6
        # Other fields get defaults
        assert result["temporal_weight"] == 0.3

    def test_validate_none_params(self):
        """None params are always valid."""
        assert validate_strategy_params("vector:v1", None) is None

    def test_validate_invalid_params_raises(self):
        """Out-of-range params raise InvalidStrategyParamsError."""
        with pytest.raises(InvalidStrategyParamsError):
            validate_strategy_params("vector:v1", {"semantic_weight": 2.0})

    def test_validate_invalid_type_raises(self):
        """Wrong type raises InvalidStrategyParamsError."""
        with pytest.raises(InvalidStrategyParamsError):
            validate_strategy_params("activation:v1", {"num_iterations": -1})

    def test_validate_unknown_strategy_passes_through(self):
        """Unknown strategy key passes params through without validation."""
        params = {"custom_param": 42}
        result = validate_strategy_params("custom:v1", params)
        assert result == params

    def test_get_default_params_known(self):
        """get_default_params returns defaults for known strategies."""
        defaults = get_default_params("vector:v1")
        assert defaults is not None
        assert defaults["semantic_weight"] == 0.4

    def test_get_default_params_unknown(self):
        """get_default_params returns None for unknown strategies."""
        assert get_default_params("unknown:v1") is None
