"""Tests for prompt constraints — ambiguity resolution and tool bypass warnings.

These tests verify that the constraints section in PromptAssembler includes:
1. Ambiguity resolution rules — guide agent to interpret "call chain" etc. as conversation context
2. Tool bypass warning — explicit prohibition of rm-then-write_file pattern

The constraints are critical for preventing the issues observed in session 019cb2d4:
- Agent misinterpreted "调用链合理吗" as code-level call chain instead of conversation tool calls
- Agent used `bash rm` to bypass write_file's "file already exists" protection
"""


from tests.integration.helpers import unique_test_id


class TestConstraintsAmbiguityResolution:
    """Verify constraints include ambiguity resolution rules."""

    def test_constraints_include_ambiguity_rules(self, db_session):
        """Constraints section contains ambiguity resolution guidance."""
        from core.context.prompt_assembler import PromptAssembler

        pa = PromptAssembler(lambda: db_session)
        result = pa.assemble(
            agent_id=None,
            user_query="test",
            session_id=unique_test_id(),
            user_id=unique_test_id(),
        )

        constraints = result.sections.get("constraints", "")

        # Must contain ambiguity resolution rules
        assert "Ambiguity resolution" in constraints, \
            "Constraints should include 'Ambiguity resolution' section"

        # Must mention key ambiguous terms
        assert "call chain" in constraints.lower(), \
            "Constraints should mention 'call chain' as an ambiguous term"

        # Must guide toward conversation context interpretation
        assert "YOUR recent actions" in constraints or "your recent actions" in constraints.lower(), \
            "Constraints should guide agent to interpret ambiguous terms as conversation context"

        # Must mention asking for clarification
        assert "clarification" in constraints.lower(), \
            "Constraints should mention asking for clarification when ambiguous"

    def test_ambiguity_rules_cover_common_terms(self, db_session):
        """Ambiguity rules cover the common terms that caused confusion."""
        from core.context.prompt_assembler import PromptAssembler

        pa = PromptAssembler(lambda: db_session)
        result = pa.assemble(
            agent_id=None,
            user_query="test",
            session_id=unique_test_id(),
            user_id=unique_test_id(),
        )

        constraints = result.sections.get("constraints", "")
        constraints_lower = constraints.lower()

        # These are the exact terms that caused confusion in session 019cb2d4
        ambiguous_terms = ["the process", "what happened", "the flow", "call chain", "decision process"]
        found_terms = [term for term in ambiguous_terms if term in constraints_lower]

        assert len(found_terms) >= 3, \
            f"Constraints should mention at least 3 ambiguous terms, found: {found_terms}"


class TestConstraintsToolBypassWarning:
    """Verify constraints explicitly prohibit rm-then-write_file bypass."""

    def test_constraints_prohibit_rm_bypass(self, db_session):
        """Constraints explicitly warn against using rm to bypass write_file."""
        from core.context.prompt_assembler import PromptAssembler

        pa = PromptAssembler(lambda: db_session)
        result = pa.assemble(
            agent_id=None,
            user_query="test",
            session_id=unique_test_id(),
            user_id=unique_test_id(),
        )

        constraints = result.sections.get("constraints", "")
        constraints_lower = constraints.lower()

        # Must mention rm bypass is blocked
        assert "rm" in constraints_lower, \
            "Constraints should mention 'rm' command"
        assert "bypass" in constraints_lower or "blocked" in constraints_lower, \
            "Constraints should mention that rm bypass is blocked"

    def test_file_editing_rules_are_explicit(self, db_session):
        """File editing rules clearly state write_file is for new files only."""
        from core.context.prompt_assembler import PromptAssembler

        pa = PromptAssembler(lambda: db_session)
        result = pa.assemble(
            agent_id=None,
            user_query="test",
            session_id=unique_test_id(),
            user_id=unique_test_id(),
        )

        constraints = result.sections.get("constraints", "")

        # Must have file editing rules section
        assert "File editing rules" in constraints, \
            "Constraints should have 'File editing rules' section"

        # Must state str_replace for existing files
        assert "str_replace" in constraints, \
            "Constraints should mention str_replace for editing"

        # Must state write_file for new files only
        assert "new files" in constraints.lower() or "don't exist" in constraints.lower(), \
            "Constraints should state write_file is for new files only"


class TestConstraintsCompleteness:
    """Verify all required constraint categories are present."""

    def test_all_constraint_categories_present(self, db_session):
        """Constraints include all required categories."""
        from core.context.prompt_assembler import PromptAssembler

        pa = PromptAssembler(lambda: db_session)
        result = pa.assemble(
            agent_id=None,
            user_query="test",
            session_id=unique_test_id(),
            user_id=unique_test_id(),
        )

        constraints = result.sections.get("constraints", "")

        required_categories = [
            "Rules:",
            "Ambiguity resolution",
            "Data integrity",
            "Tool selection",
            "Reflection",
            "File editing",
        ]

        for category in required_categories:
            assert category in constraints, \
                f"Constraints should include '{category}' category"

    def test_constraints_token_count_reasonable(self, db_session):
        """Constraints section doesn't exceed reasonable token budget."""
        from core.context.prompt_assembler import PromptAssembler

        pa = PromptAssembler(lambda: db_session)
        result = pa.assemble(
            agent_id=None,
            user_query="test",
            session_id=unique_test_id(),
            user_id=unique_test_id(),
        )

        constraints_tokens = result.token_breakdown.get("constraints", 0)

        # Constraints should be substantial but not excessive
        # ~400-800 tokens is reasonable for comprehensive rules
        assert 300 < constraints_tokens < 1000, \
            f"Constraints token count {constraints_tokens} outside reasonable range (300-1000)"


class TestConstraintsIntegration:
    """Integration tests for constraints in full prompt assembly."""

    def test_constraints_survive_compression(self, db_session):
        """Constraints are never dropped during compression."""
        from core.context.prompt_assembler import EdgeContext, PromptAssembler

        # Create edge context with large project rules to trigger compression
        large_rules = "Rule " * 2000  # ~8000 chars
        edge_ctx = EdgeContext(
            project_rules=large_rules,
            edge_tools=[],
            edge_profile={},
        )

        pa = PromptAssembler(lambda: db_session)
        result = pa.assemble(
            agent_id=None,
            user_query="test",
            session_id=unique_test_id(),
            user_id=unique_test_id(),
            edge_context=edge_ctx,
            max_tokens=2000,  # Force compression
        )

        # Constraints must survive compression
        assert "constraints" in result.sections, \
            "Constraints should never be dropped during compression"
        assert "File editing rules" in result.sections["constraints"], \
            "File editing rules should survive compression"

    def test_constraints_in_final_system_message(self, db_session):
        """Constraints appear in the final assembled system message."""
        from core.context.prompt_assembler import PromptAssembler

        pa = PromptAssembler(lambda: db_session)
        result = pa.assemble(
            agent_id=None,
            user_query="test",
            session_id=unique_test_id(),
            user_id=unique_test_id(),
        )

        # Key constraint rules must appear in final message
        assert "str_replace" in result.system_message, \
            "str_replace rule should appear in system_message"
        assert "Ambiguity" in result.system_message or "ambiguity" in result.system_message.lower(), \
            "Ambiguity rules should appear in system_message"
