"""Tests for model resolution priority chain."""

from core.llm.model_resolver import resolve_model


class TestResolveModel:
    """Test resolve_model priority chain: request > agent_config > slo > default."""

    def test_request_model_highest_priority(self):
        """Explicit request model should override all others."""
        result = resolve_model(
            request_model="gpt-4-turbo",
            agent_config_model="gpt-3.5-turbo",
            slo_escalation_model="gpt-4o",
            default_model="gpt-4o-mini",
        )
        assert result == "gpt-4-turbo"

    def test_agent_config_when_no_request(self):
        """Agent config model used when no request model."""
        result = resolve_model(
            request_model=None,
            agent_config_model="gpt-3.5-turbo",
            slo_escalation_model="gpt-4o",
        )
        assert result == "gpt-3.5-turbo"

    def test_slo_escalation_when_no_request_or_agent(self):
        """SLO escalation model used when no request or agent config."""
        result = resolve_model(
            request_model=None,
            agent_config_model=None,
            slo_escalation_model="gpt-4o",
        )
        assert result == "gpt-4o"

    def test_default_when_all_none(self):
        """Default model used when all others are None."""
        result = resolve_model()
        assert result == "gpt-4o"

    def test_custom_default(self):
        """Custom default model can be specified."""
        result = resolve_model(default_model="claude-3-sonnet")
        assert result == "claude-3-sonnet"

    def test_empty_string_treated_as_falsy(self):
        """Empty string should fall through to next priority."""
        result = resolve_model(
            request_model="",
            agent_config_model="gpt-3.5-turbo",
        )
        assert result == "gpt-3.5-turbo"
