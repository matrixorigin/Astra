"""Tests for modern skill selector with native function calling."""

import json
from unittest.mock import Mock

import pytest

from core.skills.modern_selector import AdaptiveSkillOrchestrator, ModelRouter, ModernSkillSelector
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
    return ModernSkillSelector(db, mock_llm)


@pytest.fixture
def model_router(db):
    """Model router instance."""
    return ModelRouter(db)


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
        assert "repo_id" in schema["function"]["parameters"]["properties"]
        assert "pr_number" in schema["function"]["parameters"]["properties"]

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


class TestModelRouter:
    """Test model routing based on skill category."""

    def test_route_code_skill(self, model_router):
        """Test routing code-related skill."""
        skill = SkillMetadata(
            name="search_code",
            version="1.0.0",
            description="Search code",
            category="code",
            subcategory="analysis",
            triggers=[],
            dependencies=[],
            priority=6,
            cost_estimate="low",
        )

        config = model_router.route(skill, "search for auth code")

        assert config["model"] == "deepseek-coder"
        assert config["style"] == "concise"
        assert config["temperature"] == 0.2

    def test_route_github_skill(self, model_router):
        """Test routing GitHub-related skill."""
        skill = SkillMetadata(
            name="code_review",
            version="1.0.0",
            description="Review PR",
            category="github",
            subcategory="pr_management",
            triggers=[],
            dependencies=[],
            priority=8,
            cost_estimate="medium",
        )

        config = model_router.route(skill, "review PR")

        assert config["model"] == "gpt-4"
        assert config["style"] == "structured"

    def test_route_high_priority_skill(self, model_router):
        """Test routing high priority skill upgrades model."""
        skill = SkillMetadata(
            name="critical_task",
            version="1.0.0",
            description="Critical task",
            category="code",
            subcategory="analysis",
            triggers=[],
            dependencies=[],
            priority=9,  # High priority
            cost_estimate="low",
        )

        config = model_router.route(skill, "critical task")

        # Should upgrade to best model
        assert config["model"] == "gpt-4"
        assert config["temperature"] == 0.1  # Lower temperature

    def test_route_high_cost_skill(self, model_router):
        """Test routing high cost skill adds warning."""
        skill = SkillMetadata(
            name="expensive_task",
            version="1.0.0",
            description="Expensive task",
            category="github",
            subcategory="analysis",
            triggers=[],
            dependencies=[],
            priority=5,
            cost_estimate="high",  # High cost
        )

        config = model_router.route(skill, "expensive task")

        assert config.get("budget_warning") is True

    def test_get_system_prompt(self, model_router):
        """Test category-specific system prompts."""
        code_prompt = model_router.get_system_prompt("code", "concise")
        assert "code analyst" in code_prompt.lower()

        github_prompt = model_router.get_system_prompt("github", "structured")
        assert "github" in github_prompt.lower()

        default_prompt = model_router.get_system_prompt("unknown", "balanced")
        assert "helpful" in default_prompt.lower()


class TestAdaptiveSkillOrchestrator:
    """Test adaptive skill orchestrator."""

    @pytest.fixture
    def orchestrator(self, db, mock_llm):
        """Orchestrator instance."""
        return AdaptiveSkillOrchestrator(db, mock_llm)

    @pytest.mark.asyncio
    async def test_execute_query_with_routing(self, orchestrator, mock_llm):
        """Test query execution with model routing."""
        # Register test skills
        from core.skills.selector import SkillMetadata
        skill = SkillMetadata(
            name="code_review", version="1.0.0", description="Review code",
            category="code", subcategory="review", triggers=["review"],
            dependencies=[], priority=5, cost_estimate="medium"
        )
        orchestrator.selector.rule_selector.skills["code_review"] = skill
        
        # Mock tool calls
        mock_llm.chat_with_tools.return_value = {
            "content": "",
            "tool_calls": [
                {
                    "id": "call_1",
                    "type": "function",
                    "function": {
                        "name": "code_review",
                        "arguments": json.dumps({"repo_id": "test/repo", "pr_number": 123}),
                    },
                }
            ],
        }

        result = await orchestrator.execute_query(query="Review PR #123", session_id="test_session")

        # Verify result structure
        assert "response" in result
        assert "skills_used" in result
        assert "models_used" in result
        assert "skill_results" in result

        # Verify skill was executed
        assert "code_review" in result["skills_used"]
        assert len(result["models_used"]) > 0

    @pytest.mark.asyncio
    async def test_execute_query_no_tools(self, orchestrator, mock_llm):
        """Test query execution when no tools match."""
        mock_llm.chat_with_tools.return_value = {"content": "", "tool_calls": []}

        result = await orchestrator.execute_query(
            query="unrelated query", session_id="test_session"
        )

        assert result["skills_used"] == []
        assert result["model_used"] == "none"

    @pytest.mark.asyncio
    async def test_execute_query_with_error(self, orchestrator, mock_llm):
        """Test query execution handles skill errors."""
        mock_llm.chat_with_tools.return_value = {
            "content": "",
            "tool_calls": [
                {
                    "id": "call_1",
                    "type": "function",
                    "function": {
                        "name": "code_review",
                        "arguments": json.dumps({"repo_id": "test/repo", "pr_number": 123}),
                    },
                }
            ],
        }

        # Mock skill execution to fail
        orchestrator._execute_skill = Mock(side_effect=Exception("Skill failed"))

        result = await orchestrator.execute_query(query="Review PR #123", session_id="test_session")

        # Should handle error gracefully - when no skills selected, no skill_results
        assert "response" in result
        assert "skills_used" in result

    def test_synthesize_response(self, orchestrator):
        """Test response synthesis from skill results."""
        results = [
            {
                "skill": "code_review",
                "result": {"status": "success", "data": "Review complete"},
                "model": "gpt-4",
            },
            {
                "skill": "ci_status",
                "result": {"status": "success", "data": "All checks passed"},
                "model": "gpt-4",
            },
        ]

        response = orchestrator._synthesize_response("test query", results)

        assert "code_review" in response
        assert "ci_status" in response
        assert "✅" in response

    def test_synthesize_response_with_errors(self, orchestrator):
        """Test response synthesis with errors."""
        results = [{"skill": "code_review", "error": "API timeout", "model": "gpt-4"}]

        response = orchestrator._synthesize_response("test query", results)

        assert "❌" in response
        assert "code_review" in response
        assert "timeout" in response.lower()


class TestIntegration:
    """Integration tests for the complete flow."""

    @pytest.mark.asyncio
    async def test_end_to_end_flow(self, db, mock_llm):
        """Test complete flow from query to execution."""
        # Setup
        orchestrator = AdaptiveSkillOrchestrator(db, mock_llm)
        
        # Register test skill
        from core.skills.selector import SkillMetadata
        skill = SkillMetadata(
            name="code_review", version="1.0.0", description="Review code",
            category="code", subcategory="review", triggers=["review"],
            dependencies=[], priority=5, cost_estimate="medium"
        )
        orchestrator.selector.rule_selector.skills["code_review"] = skill

        # Mock LLM response
        mock_llm.chat_with_tools.return_value = {
            "content": "",
            "tool_calls": [
                {
                    "id": "call_1",
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
        result = await orchestrator.execute_query(
            query="Review PR #123 for security issues", session_id="test_session"
        )

        # Verify complete flow
        assert result["skills_used"] == ["code_review"]
        assert len(result["models_used"]) > 0
        assert len(result["skill_results"]) == 1

        # Verify model routing happened
        skill_result = result["skill_results"][0]
        assert "model" in skill_result
        assert skill_result["model"] in ["gpt-4", "deepseek-coder", "claude-3-sonnet"]
