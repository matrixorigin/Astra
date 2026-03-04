"""Tests for prompt constraints — dynamic rule loading based on task type.

These tests verify that the constraints section in PromptAssembler:
1. Always includes core rules (think step-by-step, no fabrication)
2. Dynamically loads task-specific rules based on query and available tools
3. File editing rules only included when file tools are available
"""

from tests.integration.helpers import unique_test_id


class TestConstraintsCoreRules:
    """Verify core rules are always present."""

    def test_core_rules_always_included(self, db_session):
        """Core rules are present regardless of query."""
        from core.context.prompt_assembler import PromptAssembler

        pa = PromptAssembler(lambda: db_session)
        result = pa.assemble(
            agent_id=None,
            user_query="what is event sourcing",
            session_id=unique_test_id(),
            user_id=unique_test_id(),
        )

        constraints = result.sections.get("constraints", "")

        assert "Think step-by-step" in constraints
        assert "NEVER fabricate data" in constraints

    def test_constraints_token_count_reasonable(self, db_session):
        """Constraints token count is within reasonable range."""
        from core.context.prompt_assembler import PromptAssembler

        pa = PromptAssembler(lambda: db_session)
        result = pa.assemble(
            agent_id=None,
            user_query="test query",
            session_id=unique_test_id(),
            user_id=unique_test_id(),
        )

        token_count = result.token_breakdown.get("constraints", 0)
        # Dynamic constraints: core rules ~35 tokens, with task-specific ~100-300
        assert 30 <= token_count <= 500, \
            f"Constraints token count {token_count} outside reasonable range (30-500)"


class TestConstraintsDynamicLoading:
    """Verify task-specific rules are loaded dynamically."""

    def test_file_editing_rules_with_file_query(self, db_session):
        """File editing rules included when query mentions editing."""
        from core.context.prompt_assembler import PromptAssembler, EdgeContext

        pa = PromptAssembler(lambda: db_session)
        edge_ctx = EdgeContext(
            edge_tools=[{"function": {"name": "str_replace"}}],
        )
        result = pa.assemble(
            agent_id=None,
            user_query="edit the config file",
            session_id=unique_test_id(),
            user_id=unique_test_id(),
            edge_context=edge_ctx,
        )

        constraints = result.sections.get("constraints", "")
        assert "File editing" in constraints
        assert "str_replace" in constraints

    def test_no_file_rules_for_query_task(self, db_session):
        """File editing rules excluded for pure query tasks."""
        from core.context.prompt_assembler import PromptAssembler, EdgeContext

        pa = PromptAssembler(lambda: db_session)
        edge_ctx = EdgeContext(
            edge_tools=[{"function": {"name": "ci_status"}}],
        )
        result = pa.assemble(
            agent_id=None,
            user_query="check ci status",
            session_id=unique_test_id(),
            user_id=unique_test_id(),
            edge_context=edge_ctx,
        )

        constraints = result.sections.get("constraints", "")
        # File editing rules should NOT be included
        assert "File editing" not in constraints

    def test_reflection_rules_for_why_questions(self, db_session):
        """Reflection rules included for 'why' questions."""
        from core.context.prompt_assembler import PromptAssembler

        pa = PromptAssembler(lambda: db_session)
        result = pa.assemble(
            agent_id=None,
            user_query="why did the build fail",
            session_id=unique_test_id(),
            user_id=unique_test_id(),
        )

        constraints = result.sections.get("constraints", "")
        assert "Reflection" in constraints

    def test_tool_selection_rules_with_multiple_tools(self, db_session):
        """Tool selection rules included when multiple tools available."""
        from core.context.prompt_assembler import PromptAssembler, EdgeContext

        pa = PromptAssembler(lambda: db_session)
        edge_ctx = EdgeContext(
            edge_tools=[
                {"function": {"name": "grep"}},
                {"function": {"name": "bash"}},
                {"function": {"name": "ci_status"}},
            ],
        )
        result = pa.assemble(
            agent_id=None,
            user_query="search for errors",
            session_id=unique_test_id(),
            user_id=unique_test_id(),
            edge_context=edge_ctx,
        )

        constraints = result.sections.get("constraints", "")
        assert "Tool selection" in constraints


class TestConstraintsIntegration:
    """Verify constraints appear in final system message."""

    def test_constraints_in_final_system_message(self, db_session):
        """Constraints section appears in assembled system message."""
        from core.context.prompt_assembler import PromptAssembler

        pa = PromptAssembler(lambda: db_session)
        result = pa.assemble(
            agent_id=None,
            user_query="test",
            session_id=unique_test_id(),
            user_id=unique_test_id(),
        )

        # Core rules should appear in system_message
        assert "Think step-by-step" in result.system_message
        assert "NEVER fabricate data" in result.system_message

    def test_constraints_survive_compression(self, db_session):
        """Constraints survive compression (they're in FIXED_SECTIONS)."""
        from core.context.prompt_assembler import PromptAssembler

        pa = PromptAssembler(lambda: db_session)
        # Use a very small max_tokens to trigger compression
        result = pa.assemble(
            agent_id=None,
            user_query="test",
            session_id=unique_test_id(),
            user_id=unique_test_id(),
            max_tokens=500,
        )

        # Core rules should still be present after compression
        constraints = result.sections.get("constraints", "")
        assert "Think step-by-step" in constraints, \
            "Core rules should survive compression"
