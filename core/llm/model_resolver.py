"""Unified model resolution.

Priority chain (highest → lowest):
  1. Explicit request model (user chose a model for this request)
  2. Agent config model (agent-level default)
  3. SLO escalation (auto-upgrade after quality issues)
  4. Global default from LLM config
"""


def resolve_model(
    request_model: str | None = None,
    agent_config_model: str | None = None,
    slo_escalation_model: str | None = None,
    default_model: str = "gpt-4o",
) -> str:
    """Return the model to use, following the priority chain."""
    return request_model or agent_config_model or slo_escalation_model or default_model
