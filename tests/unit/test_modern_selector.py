"""Tests for modern skill selector with native function calling."""

import json
from unittest.mock import Mock

import pytest

from core.skills.modern_selector import ModernSkillSelector
from core.skills.selector import SkillMetadata




@pytest.fixture
def mock_llm():
    """Mock LLM client."""
    llm = Mock()
    llm.chat_with_tools = Mock()
    return llm


@pytest.fixture
def modern_selector(db, mock_llm):
    """Modern skill selector instance."""
    return ModernSkillSelector(lambda: db, mock_llm)


class TestModernSkillSelector:
    """Test modern skill selector with function calling."""

    def test_select_and_execute_with_function_calling(self, modern_selector, mock_llm):
        """Test skill selection with native function calling."""
        # Register a test skill
        from core.skills.selector import SkillMetadata
        skill = SkillMetadata(
            name="code_review", version="1.0.0", description="Review code",
            category="code", subcategory="review", triggers=["review", "pr"],
            dependencies=[], priority=5, cost_estimate="medium"
        )
        modern_selector.rule_selector.skills["code_review"] = skill
        
        # Mock LLM response with tool calls
        mock_llm.chat_with_tools.return_value = {
            "content": "",
            "tool_calls": [
                {
                    "id": "call_123",
                    "type": "function",
                    "function": {
                        "name": "code_review",
                        "arguments": json.dumps(
                            {"repo_id": "test/repo", "pr_number": 123, "focus": "security"}
                        ),
                    },
                }
            ],
        }

        # Execute
        result = modern_selector.select_and_execute("Review PR #123 for security issues")

        # Verify
        assert len(result) == 1
        assert result[0]["function"]["name"] == "code_review"

        args = json.loads(result[0]["function"]["arguments"])
        assert args["pr_number"] == 123
        assert args["focus"] == "security"

        # Verify LLM was called with tools
        mock_llm.chat_with_tools.assert_called_once()
        call_args = mock_llm.chat_with_tools.call_args
        assert "tools" in call_args[1]
        assert len(call_args[1]["tools"]) > 0

    def test_skill_to_tool_schema(self, modern_selector):
        """Test conversion of skill to OpenAI tool schema."""
        skill = SkillMetadata(
            name="code_review",
            version="1.0.0",
            description="Review code changes",
            category="github",
            subcategory="pr_management",
            triggers=["review"],
            dependencies=[],
            priority=8,
            cost_estimate="medium",
        )

        schema = modern_selector._skill_to_tool_schema(skill)

        # Verify schema structure
        assert schema["type"] == "function"
        assert schema["function"]["name"] == "code_review"
        assert schema["function"]["description"] == "Review code changes"
        assert "parameters" in schema["function"]
        assert schema["function"]["parameters"]["type"] == "object"

    def test_fallback_to_rules(self, modern_selector, mock_llm):
        """Test fallback when function calling fails."""
        # Mock LLM failure
        mock_llm.chat_with_tools.side_effect = Exception("API error")

        # Execute
        result = modern_selector.select_and_execute("Review PR #123")

        # Should fallback to rule-based
        assert isinstance(result, list)
        # May be empty or have fallback result

    def test_no_candidates(self, modern_selector):
        """Test when no skills match query."""
        result = modern_selector.select_and_execute("completely unrelated query xyz")

        assert result == []

    def test_multiple_tool_calls(self, modern_selector, mock_llm):
        """Test handling multiple tool calls."""
        # Register test skills
        from core.skills.selector import SkillMetadata
        skills = [
            SkillMetadata(name="search_code", version="1.0.0", description="Search", category="code", subcategory="search", triggers=["search"], dependencies=[], priority=5, cost_estimate="low"),
            SkillMetadata(name="analyze_bug", version="1.0.0", description="Analyze", category="debug", subcategory="analyze", triggers=["bug"], dependencies=[], priority=5, cost_estimate="medium")
        ]
        for skill in skills:
            modern_selector.rule_selector.skills[skill.name] = skill
            
        mock_llm.chat_with_tools.return_value = {
            "content": "",
            "tool_calls": [
                {
                    "id": "call_1",
                    "type": "function",
                    "function": {
                        "name": "search_code",
                        "arguments": json.dumps({"repo_id": "test/repo", "query": "auth"}),
                    },
                },
                {
                    "id": "call_2",
                    "type": "function",
                    "function": {
                        "name": "analyze_bug",
                        "arguments": json.dumps({"repo_id": "test/repo", "issue_number": 456}),
                    },
                },
            ],
        }

        result = modern_selector.select_and_execute("Find auth bugs in issue #456")

        assert len(result) == 2
        assert result[0]["function"]["name"] == "search_code"
        assert result[1]["function"]["name"] == "analyze_bug"
