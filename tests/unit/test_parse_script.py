"""Unit tests for parse_script LLM output normalization."""

import pytest

from core.memory.programmer import InvalidScriptError, parse_script


class TestMarkdownCodeFenceStripping:
    """LLMs almost always wrap YAML in ```yaml ... ``` fences."""

    def test_strip_yaml_fence(self):
        raw = "```yaml\n- inject:\n    content: hello\n```"
        actions = parse_script(raw)
        assert actions[0]["inject"]["content"] == "hello"

    def test_strip_yml_fence(self):
        raw = "```yml\n- inject:\n    content: hello\n```"
        actions = parse_script(raw)
        assert actions[0]["inject"]["content"] == "hello"

    def test_strip_bare_fence(self):
        raw = "```\n- inject:\n    content: hello\n```"
        actions = parse_script(raw)
        assert actions[0]["inject"]["content"] == "hello"

    def test_plain_yaml_still_works(self):
        raw = "- inject:\n    content: hello"
        actions = parse_script(raw)
        assert actions[0]["inject"]["content"] == "hello"


class TestVersionFloatTolerance:
    """YAML parses `version: 1.0` as float; must accept it as version 1."""

    def test_version_float_1_0(self):
        raw = {"version": 1.0, "actions": [{"inject": {"content": "hi"}}]}
        actions = parse_script(raw)
        assert actions[0]["inject"]["content"] == "hi"

    def test_version_int_1(self):
        raw = {"version": 1, "actions": [{"inject": {"content": "hi"}}]}
        actions = parse_script(raw)
        assert len(actions) == 1

    def test_version_2_rejected(self):
        raw = {"version": 2.0, "actions": [{"inject": {"content": "hi"}}]}
        with pytest.raises(InvalidScriptError, match="Unsupported script version"):
            parse_script(raw)


class TestFlatActionNormalization:
    """LLMs sometimes return {action: "inject", content: ...} instead of {inject: {content: ...}}."""

    def test_flat_inject_normalized(self):
        raw = [{"action": "inject", "content": "hello", "memory_type": "semantic"}]
        actions = parse_script(raw)
        assert "inject" in actions[0]
        assert actions[0]["inject"]["content"] == "hello"
        assert actions[0]["inject"]["type"] == "semantic"
        assert "action" not in actions[0]["inject"]

    def test_flat_purge_normalized(self):
        raw = [{"action": "purge", "filter": {"memory_type": "working"}}]
        actions = parse_script(raw)
        assert actions[0]["purge"]["filter"]["type"] == "working"

    def test_nested_format_unchanged(self):
        """Already-correct nested format must not be altered."""
        raw = [{"inject": {"content": "hello"}}]
        actions = parse_script(raw)
        assert actions[0]["inject"]["content"] == "hello"

    def test_flat_from_yaml_string_with_fence(self):
        """Full round-trip: fenced YAML with flat action format."""
        raw = "```yaml\nversion: 1.0\nactions:\n  - action: inject\n    content: test\n```"
        actions = parse_script(raw)
        assert actions[0]["inject"]["content"] == "test"


class TestFieldNameNormalization:
    """LLMs output varying field names; parse_script normalizes to canonical."""

    def test_memory_type_to_type(self):
        raw = [{"inject": {"memory_type": "semantic", "content": "hi"}}]
        actions = parse_script(raw)
        assert actions[0]["inject"]["type"] == "semantic"
        assert "memory_type" not in actions[0]["inject"]

    def test_trust_tier_to_trust(self):
        raw = [{"inject": {"trust_tier": "T1", "content": "hi"}}]
        actions = parse_script(raw)
        assert actions[0]["inject"]["trust"] == "T1"
        assert "trust_tier" not in actions[0]["inject"]

    def test_text_to_content(self):
        raw = [{"inject": {"text": "hello"}}]
        actions = parse_script(raw)
        assert actions[0]["inject"]["content"] == "hello"

    def test_explicit_canonical_wins(self):
        """If both alias and canonical present, canonical wins."""
        raw = [{"inject": {"type": "procedural", "memory_type": "semantic", "content": "x"}}]
        actions = parse_script(raw)
        assert actions[0]["inject"]["type"] == "procedural"

    def test_strategy_key_to_strategy(self):
        raw = [{"tune": {"strategy_key": "recency", "user_id": "u1"}}]
        actions = parse_script(raw)
        assert actions[0]["tune"]["strategy"] == "recency"

    def test_non_action_fields_untouched(self):
        """Fields like user_id, filter that aren't aliases pass through."""
        raw = [{"inject": {"user_id": "alice", "content": "hi"}}]
        actions = parse_script(raw)
        assert actions[0]["inject"]["user_id"] == "alice"


class TestNlToScriptModelDefault:
    """nl_to_script should default to 'cheapest' model."""

    def test_default_model_is_cheapest(self):
        from unittest.mock import MagicMock

        from core.memory.programmer import nl_to_script

        mock_llm = MagicMock()
        mock_llm.chat.return_value = MagicMock(
            content="version: 1\nactions:\n  - inject:\n      content: test"
        )

        nl_to_script("remember something", "user1", mock_llm)

        call_kwargs = mock_llm.chat.call_args
        assert call_kwargs.kwargs["model"] == "cheapest"

    def test_explicit_model_overrides(self):
        from unittest.mock import MagicMock

        from core.memory.programmer import nl_to_script

        mock_llm = MagicMock()
        mock_llm.chat.return_value = MagicMock(
            content="version: 1\nactions:\n  - inject:\n      content: test"
        )

        nl_to_script("remember something", "user1", mock_llm, model="kimi-k2.5")

        call_kwargs = mock_llm.chat.call_args
        assert call_kwargs.kwargs["model"] == "kimi-k2.5"
