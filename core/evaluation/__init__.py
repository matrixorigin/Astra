"""Evaluation and quality measurement module.

Provides:
- Unified regression gate for prompt/skill/config changes
- Golden session selection
- Quality metrics computation
- CI/CD integration
"""

from core.evaluation.regression_gate import RegressionGate, ChangeType

__all__ = ["RegressionGate", "ChangeType"]
