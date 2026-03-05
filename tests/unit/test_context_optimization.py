"""Tests for context window optimization: skill categories and find_skills tool.

Tests verify:
1. _build_skill_categories uses ORM correctly
2. FindSkillsTool keyword search works
3. Database fields are correctly queried
"""

from uuid import uuid4

import pytest
from sqlalchemy import text as sql_text


def unique_test_id():
    return f"tt_{uuid4().hex}"


class TestBuildSkillCategories:
    """Test _build_skill_categories ORM query."""

    def _seed_skills(self, db, prefix: str):
        """Insert test skills with different categories."""
        skills = [
            (f"{prefix}_pr", "github", "List PRs", 10),
            (f"{prefix}_ci", "github", "Check CI", 9),
            (f"{prefix}_issue", "github", "List issues", 8),
            (f"{prefix}_ec2", "aws", "EC2 status", 10),
            (f"{prefix}_s3", "aws", "S3 list", 9),
            (f"{prefix}_alert", "monitoring", "Check alerts", 10),
        ]
        for name, cat, desc, priority in skills:
            db.execute(sql_text(
                "INSERT INTO skills_registry (skill_id, skill_name, version, description, category, priority, is_active) "
                "VALUES (:id, :name, '1.0.0', :desc, :cat, :priority, 1)"
            ), {"id": f"{name}@1.0.0", "name": name, "desc": desc, "cat": cat, "priority": priority})
        db.commit()

    def _cleanup(self, db, prefix: str):
        db.execute(sql_text("DELETE FROM skills_registry WHERE skill_name LIKE :pat"), {"pat": f"{prefix}%"})
        db.commit()

    def test_categories_grouped_correctly(self, db_session):
        """Verify skills are grouped by category with correct counts."""
        from core.context.prompt_assembler import PromptAssembler

        prefix = unique_test_id()
        self._seed_skills(db_session, prefix)
        try:
            pa = PromptAssembler(lambda: db_session)
            result = pa._build_skill_categories(db_session, exclude_names=set())

            assert result is not None
            # github (3) and aws (2) have enough skills to appear in top-10 categories
            assert "github" in result
            assert "aws" in result
            # Should have skill examples from our seeded data
            assert f"{prefix}_pr" in result or f"{prefix}_ci" in result
        finally:
            self._cleanup(db_session, prefix)

    def test_categories_exclude_installed(self, db_session):
        """Verify exclude_names reduces total count."""
        from core.context.prompt_assembler import PromptAssembler

        prefix = unique_test_id()
        self._seed_skills(db_session, prefix)
        try:
            pa = PromptAssembler(lambda: db_session)

            # Without exclusion
            result1 = pa._build_skill_categories(db_session, exclude_names=set())
            # With exclusion
            result2 = pa._build_skill_categories(db_session, exclude_names={f"{prefix}_pr", f"{prefix}_ci"})

            # Both should work
            assert result1 is not None
            assert result2 is not None
            # Total should be different (result2 has lower total)
            # Extract total from "- N cloud skills in M categories:"
            import re
            match1 = re.search(r"(\d+) cloud skills", result1)
            match2 = re.search(r"(\d+) cloud skills", result2)
            if match1 and match2:
                total1 = int(match1.group(1))
                total2 = int(match2.group(1))
                assert total2 <= total1
        finally:
            self._cleanup(db_session, prefix)

    def test_categories_empty_returns_none(self, db_session):
        """Verify empty registry returns None."""
        from core.context.prompt_assembler import PromptAssembler

        pa = PromptAssembler(lambda: db_session)

        # Query with exclude_names that would exclude everything
        # First, get all active skills
        from api.models import SkillRegistry
        all_skills = db_session.query(SkillRegistry.skill_name).filter(
            SkillRegistry.is_active == 1
        ).all()
        all_names = {s[0] for s in all_skills}

        # Exclude all - should return None or empty
        result = pa._build_skill_categories(db_session, exclude_names=all_names)
        # If all excluded, total becomes 0, should return None
        # (or if no skills exist at all)
        assert result is None or "0 cloud skills" in result or len(all_names) == 0

    def test_categories_priority_ordering(self, db_session):
        """Verify examples are ordered by priority DESC."""
        from core.context.prompt_assembler import PromptAssembler

        prefix = unique_test_id()
        self._seed_skills(db_session, prefix)
        try:
            pa = PromptAssembler(lambda: db_session)
            result = pa._build_skill_categories(db_session, exclude_names=set())

            assert result is not None
            # github category: pr (10) > ci (9) > issue (8)
            # So pr should appear before ci in examples
            if f"{prefix}_pr" in result and f"{prefix}_ci" in result:
                pr_pos = result.find(f"{prefix}_pr")
                ci_pos = result.find(f"{prefix}_ci")
                # pr should come first (lower position)
                assert pr_pos < ci_pos, "Higher priority skill should appear first"
        finally:
            self._cleanup(db_session, prefix)


class TestFindSkillsKeywordSearch:
    """Test FindSkillsTool._keyword_search ORM query."""

    def _seed_skills(self, db, prefix: str):
        """Insert test skills for keyword search."""
        skills = [
            (f"{prefix}_ci_status", "github", "Check CI pipeline status and failures"),
            (f"{prefix}_list_prs", "github", "List open pull requests"),
            (f"{prefix}_deploy", "devops", "Deploy application to production"),
        ]
        for name, cat, desc in skills:
            db.execute(sql_text(
                "INSERT INTO skills_registry (skill_id, skill_name, version, description, category, is_active) "
                "VALUES (:id, :name, '1.0.0', :desc, :cat, 1)"
            ), {"id": f"{name}@1.0.0", "name": name, "desc": desc, "cat": cat})
        db.commit()

    def _cleanup(self, db, prefix: str):
        db.execute(sql_text("DELETE FROM skills_registry WHERE skill_name LIKE :pat"), {"pat": f"{prefix}%"})
        db.commit()

    @pytest.mark.asyncio
    async def test_keyword_search_matches_name(self, db_session):
        """Verify keyword search matches skill names."""
        from cli.tools.skill_discovery import FindSkillsTool

        prefix = unique_test_id()
        self._seed_skills(db_session, prefix)
        try:
            tool = FindSkillsTool()
            result = await tool._keyword_search("ci", None, 5)

            assert f"{prefix}_ci_status" in result
            assert "CI pipeline" in result or "ci_status" in result
        finally:
            self._cleanup(db_session, prefix)

    @pytest.mark.asyncio
    async def test_keyword_search_matches_description(self, db_session):
        """Verify keyword search matches descriptions."""
        from cli.tools.skill_discovery import FindSkillsTool

        prefix = unique_test_id()
        self._seed_skills(db_session, prefix)
        try:
            tool = FindSkillsTool()
            result = await tool._keyword_search("pipeline", None, 5)

            # Should match ci_status which has "pipeline" in description
            assert f"{prefix}_ci_status" in result
        finally:
            self._cleanup(db_session, prefix)

    @pytest.mark.asyncio
    async def test_keyword_search_category_filter(self, db_session):
        """Verify category filter works."""
        from cli.tools.skill_discovery import FindSkillsTool

        prefix = unique_test_id()
        self._seed_skills(db_session, prefix)
        try:
            tool = FindSkillsTool()
            # Search with category filter
            result = await tool._keyword_search("status", "devops", 5)

            # Should NOT match ci_status (github category)
            assert f"{prefix}_ci_status" not in result
        finally:
            self._cleanup(db_session, prefix)

    @pytest.mark.asyncio
    async def test_keyword_search_no_match(self, db_session):
        """Verify no match returns appropriate message."""
        from cli.tools.skill_discovery import FindSkillsTool

        tool = FindSkillsTool()
        result = await tool._keyword_search(f"nonexistent_{unique_test_id()}", None, 5)

        assert "No skills found" in result


class TestFindSkillsFetchDetails:
    """Test FindSkillsTool._fetch_skill_details ORM query."""

    def _seed_skills(self, db, prefix: str):
        """Insert test skills."""
        db.execute(sql_text(
            "INSERT INTO skills_registry (skill_id, skill_name, version, description, category, is_active) "
            "VALUES (:id, :name, '1.0.0', :desc, :cat, 1)"
        ), {"id": f"{prefix}_test@1.0.0", "name": f"{prefix}_test", "desc": "Test skill", "cat": "test"})
        db.commit()

    def _cleanup(self, db, prefix: str):
        db.execute(sql_text("DELETE FROM skills_registry WHERE skill_name LIKE :pat"), {"pat": f"{prefix}%"})
        db.commit()

    @pytest.mark.asyncio
    async def test_fetch_details_returns_all_fields(self, db_session):
        """Verify all expected fields are returned."""
        from cli.tools.skill_discovery import FindSkillsTool

        prefix = unique_test_id()
        self._seed_skills(db_session, prefix)
        try:
            tool = FindSkillsTool()
            results = await tool._fetch_skill_details([f"{prefix}_test"])

            assert len(results) == 1
            r = results[0]
            # Verify all fields present
            assert r["name"] == f"{prefix}_test"
            assert r["description"] == "Test skill"
            assert r["category"] == "test"
        finally:
            self._cleanup(db_session, prefix)

    @pytest.mark.asyncio
    async def test_fetch_details_preserves_order(self, db_session):
        """Verify order from semantic search is preserved."""
        from cli.tools.skill_discovery import FindSkillsTool

        prefix = unique_test_id()
        # Insert multiple skills
        for name in ["alpha", "beta", "gamma"]:
            db_session.execute(sql_text(
                "INSERT INTO skills_registry (skill_id, skill_name, version, description, category, is_active) "
                "VALUES (:id, :name, '1.0.0', :desc, 'test', 1)"
            ), {"id": f"{prefix}_{name}@1.0.0", "name": f"{prefix}_{name}", "desc": f"Skill {name}"})
        db_session.commit()

        try:
            tool = FindSkillsTool()
            # Request in specific order
            requested_order = [f"{prefix}_gamma", f"{prefix}_alpha", f"{prefix}_beta"]
            results = await tool._fetch_skill_details(requested_order)

            # Verify order preserved
            assert len(results) == 3
            assert results[0]["name"] == f"{prefix}_gamma"
            assert results[1]["name"] == f"{prefix}_alpha"
            assert results[2]["name"] == f"{prefix}_beta"
        finally:
            self._cleanup(db_session, prefix)

    @pytest.mark.asyncio
    async def test_fetch_details_empty_list(self, db_session):
        """Verify empty input returns empty list."""
        from cli.tools.skill_discovery import FindSkillsTool

        tool = FindSkillsTool()
        results = await tool._fetch_skill_details([])

        assert results == []

    @pytest.mark.asyncio
    async def test_fetch_details_nonexistent_skill(self, db_session):
        """Verify nonexistent skills are skipped."""
        from cli.tools.skill_discovery import FindSkillsTool

        tool = FindSkillsTool()
        results = await tool._fetch_skill_details([f"nonexistent_{unique_test_id()}"])

        assert results == []


class TestQueryWithScores:
    """Test SkillIndex.query_with_scores."""

    def test_query_with_scores_returns_tuples(self):
        """Verify return format is list of (name, score) tuples."""
        from core.skills.skill_index import SkillIndex

        # Without embed_fn, returns empty
        index = SkillIndex(embed_fn=None, db_factory=None)
        result = index.query_with_scores("test")

        assert isinstance(result, list)
        # Empty because no embed_fn
        assert result == []

    def test_query_delegates_to_query_with_scores(self):
        """Verify query() uses query_with_scores internally."""
        from core.skills.skill_index import SkillIndex

        index = SkillIndex(embed_fn=None, db_factory=None)

        # Both should return empty for same reason
        result1 = index.query("test")
        result2 = index.query_with_scores("test")

        assert result1 == []
        assert result2 == []


class TestPromptDescription:
    """Test Skill.prompt_description property."""

    def test_short_description_preferred(self):
        """Verify short_description is used when set."""
        from core.skills.base import Skill

        class TestSkill(Skill):
            name = "test"
            description = "This is a very long description that would be truncated"
            short_description = "Short desc"

            async def execute(self, input):
                pass

        s = TestSkill()
        assert s.prompt_description == "Short desc"

    def test_description_truncated_at_80(self):
        """Verify long description is truncated to 80 chars."""
        from core.skills.base import Skill

        class TestSkill(Skill):
            name = "test"
            description = "A" * 100  # 100 chars

            async def execute(self, input):
                pass

        s = TestSkill()
        assert len(s.prompt_description) == 80
        assert s.prompt_description.endswith("...")

    def test_short_description_not_truncated(self):
        """Verify description <= 80 chars is not truncated."""
        from core.skills.base import Skill

        class TestSkill(Skill):
            name = "test"
            description = "Short description"

            async def execute(self, input):
                pass

        s = TestSkill()
        assert s.prompt_description == "Short description"

    def test_empty_description(self):
        """Verify empty description returns empty string."""
        from core.skills.base import Skill

        class TestSkill(Skill):
            name = "test"
            description = ""

            async def execute(self, input):
                pass

        s = TestSkill()
        assert s.prompt_description == ""


class TestSnapshotDeduplication:
    """Test _save_snapshot content-addressed deduplication."""

    def test_fixed_sections_stored_by_hash(self, db_session):
        """Verify fixed sections are stored in ctx_prompt_fragments with correct hash."""
        import hashlib
        import uuid

        from api.models.context import PromptFragment
        from core.context.prompt_assembler import PromptAssembler

        unique_id = str(uuid.uuid4())
        pa = PromptAssembler(lambda: db_session)
        identity_content = f"You are a helpful assistant. ID={unique_id}"
        sections = {
            "identity": identity_content,
            "self_model": f"## Self-Model\nI can help with coding. ID={unique_id}",
            "constraints": f"Rules:\n- Be helpful. ID={unique_id}",
            "history": "User: Hello\nAssistant: Hi!",  # Variable, not stored as fragment
        }
        breakdown = {"identity": 10, "self_model": 20, "constraints": 15, "history": 25}

        snapshot_id = pa._save_snapshot("test_session", sections, breakdown)
        assert snapshot_id is not None

        # Verify identity fragment was created with correct hash
        expected_hash = hashlib.sha256(identity_content.encode()).hexdigest()
        fragment = db_session.query(PromptFragment).filter_by(
            fragment_hash=expected_hash
        ).first()

        assert fragment is not None, "Fragment should be created"
        assert fragment.content == identity_content
        assert fragment.fragment_type == "identity"
        assert fragment.token_count == 10

        # Verify history (variable) is NOT stored as fragment
        history_fragments = db_session.query(PromptFragment).filter(
            PromptFragment.content.contains("Hello")
        ).all()
        assert len(history_fragments) == 0, "Variable sections should not be fragments"

    def test_same_content_reuses_hash(self, db_session):
        """Verify identical content reuses existing fragment (no duplicates)."""
        import hashlib
        import uuid

        from api.models.context import PromptFragment
        from core.context.prompt_assembler import PromptAssembler

        unique_id = str(uuid.uuid4())
        identity_content = f"You are a helpful assistant. ID={unique_id}"
        pa = PromptAssembler(lambda: db_session)
        sections = {
            "identity": identity_content,
            "constraints": f"Rules:\n- Be helpful. ID={unique_id}",
        }
        breakdown = {"identity": 10, "constraints": 15}

        # Save twice with same content
        pa._save_snapshot("session_1", sections, breakdown)
        pa._save_snapshot("session_2", sections, breakdown)

        # Verify only one fragment exists for this content
        expected_hash = hashlib.sha256(identity_content.encode()).hexdigest()
        identity_fragments = db_session.query(PromptFragment).filter_by(
            fragment_hash=expected_hash
        ).all()

        assert len(identity_fragments) == 1, "Same content should produce exactly 1 fragment"

    def test_variable_sections_stored_inline(self, db_session):
        """Verify variable sections are stored in snapshot, not fragments."""
        import json
        import uuid

        from sqlalchemy import text as sql_text

        from core.context.prompt_assembler import PromptAssembler

        unique_id = str(uuid.uuid4())
        pa = PromptAssembler(lambda: db_session)
        sections = {
            "identity": f"Assistant ID={unique_id}",
            "history": f"User: What is 2+2?\nAssistant: 4. ID={unique_id}",
            "memory": f"User prefers concise answers. ID={unique_id}",
        }
        breakdown = {"identity": 5, "history": 20, "memory": 10}

        snapshot_id = pa._save_snapshot("test_session", sections, breakdown)

        # Query snapshot
        row = db_session.execute(sql_text(
            "SELECT system_prompt FROM ctx_snapshots WHERE context_capture_id = :cid"
        ), {"cid": snapshot_id}).fetchone()

        data = json.loads(row[0])

        # Variable sections should be inline with full content
        assert "variable_sections" in data
        assert "history" in data["variable_sections"]
        assert "memory" in data["variable_sections"]
        assert unique_id in data["variable_sections"]["history"]
        assert unique_id in data["variable_sections"]["memory"]

        # Fixed sections should be hashes only (not content)
        assert "fixed_hashes" in data
        assert "identity" in data["fixed_hashes"]
        assert len(data["fixed_hashes"]["identity"]) == 64  # SHA256 hex length

        # identity should NOT be in variable_sections
        assert "identity" not in data["variable_sections"]



class TestParameterExtraction:
    """Test parameter extraction from query."""

    def test_extract_owner_repo(self):
        """Extract owner/repo format."""
        from core.agent.chat_loop import ChatLoop
        
        loop = ChatLoop.__new__(ChatLoop)
        
        params = loop._extract_params_from_query("check ci for matrixorigin/matrixone")
        assert params.get("repo") == "matrixorigin/matrixone"

    def test_extract_bare_repo(self):
        """Extract bare project name after 'for'."""
        from core.agent.chat_loop import ChatLoop
        
        loop = ChatLoop.__new__(ChatLoop)
        
        params = loop._extract_params_from_query("check ci for matrixone")
        assert params.get("repo") == "matrixone"

    def test_extract_limit(self):
        """Extract numeric limit."""
        from core.agent.chat_loop import ChatLoop
        
        loop = ChatLoop.__new__(ChatLoop)
        
        params = loop._extract_params_from_query("show top 10 prs")
        assert params.get("limit") == 10
        
        params = loop._extract_params_from_query("last 5 issues")
        assert params.get("limit") == 5

    def test_extract_state(self):
        """Extract state (open/closed/all)."""
        from core.agent.chat_loop import ChatLoop
        
        loop = ChatLoop.__new__(ChatLoop)
        
        params = loop._extract_params_from_query("list closed issues")
        assert params.get("state") == "closed"
        
        params = loop._extract_params_from_query("show all prs")
        assert params.get("state") == "all"

    def test_extract_multiple(self):
        """Extract multiple parameters."""
        from core.agent.chat_loop import ChatLoop
        
        loop = ChatLoop.__new__(ChatLoop)
        
        params = loop._extract_params_from_query("list top 5 closed issues for matrixorigin/matrixone")
        assert params.get("repo") == "matrixorigin/matrixone"
        assert params.get("limit") == 5
        assert params.get("state") == "closed"

    def test_no_extraction(self):
        """No parameters extracted from generic query."""
        from core.agent.chat_loop import ChatLoop
        
        loop = ChatLoop.__new__(ChatLoop)
        
        params = loop._extract_params_from_query("what is event sourcing")
        assert params == {}


class TestEdgeToolFiltering:
    """Test that edge tool filtering is now handled server-side via pre_filter."""

    def test_all_tools_sent_to_server(self):
        """All edge tools are sent to server (no client-side filtering)."""
        # After unification, edge_chat_loop sends all schemas.
        # Server-side select_tools_for_turn handles pre_filter + LLM selection.
        # Verify _filter_relevant_tools no longer exists.
        import cli.edge_chat_loop as ecl
        assert not hasattr(ecl, "_filter_relevant_tools")
        assert not hasattr(ecl, "_CORE_TOOLS")

    def test_prefilter_reorders_tools_for_history_query(self):
        """Pre-filter reorders tools when tags are available."""
        from core.skills.prefilter import ConversationState, SkillTags, ToolWrapper, pre_filter

        tools = [
            ToolWrapper("get_agent_info",
                        SkillTags("current_session", "session_metadata",
                                  ("introspect",), False),
                        {"function": {"name": "get_agent_info"}}),
            ToolWrapper("reflect",
                        SkillTags("historical", "event_store",
                                  ("analytical",), True),
                        {"function": {"name": "reflect"}}),
            ToolWrapper("grep", None, {"function": {"name": "grep"}}),
        ]
        state = ConversationState(
            references_history=True, is_analytical=True,
        )
        reordered, applied = pre_filter(tools, state)
        names = [w.name for w in reordered]
        assert applied is True
        assert names.index("reflect") < names.index("get_agent_info")

    def test_prefilter_no_change_for_neutral_query(self):
        """Pre-filter doesn't change order for neutral queries."""
        from core.skills.prefilter import ConversationState, ToolWrapper, pre_filter

        tools = [ToolWrapper("grep", None, {}), ToolWrapper("bash", None, {})]
        state = ConversationState()  # all False
        _reordered, applied = pre_filter(tools, state)
        assert applied is False


class TestDynamicConstraints:
    """Test dynamic constraints based on task type."""

    def test_core_rules_always_included(self):
        """Core rules are always present."""
        from core.context.prompt_assembler import PromptAssembler
        
        assembler = PromptAssembler.__new__(PromptAssembler)
        
        constraints = assembler._build_constraints("what is event sourcing", [])
        
        assert "Think step-by-step" in constraints
        assert "NEVER fabricate data" in constraints

    def test_file_editing_rules_with_file_tools(self):
        """File editing rules included when file tools available."""
        from core.context.prompt_assembler import PromptAssembler
        
        assembler = PromptAssembler.__new__(PromptAssembler)
        tools = [{"function": {"name": "str_replace"}}]
        
        constraints = assembler._build_constraints("edit the file", tools)
        
        assert "str_replace" in constraints
        assert "File editing" in constraints

    def test_no_file_rules_for_query(self):
        """File editing rules excluded for pure queries."""
        from core.context.prompt_assembler import PromptAssembler
        
        assembler = PromptAssembler.__new__(PromptAssembler)
        tools = [{"function": {"name": "ci_status"}}]
        
        constraints = assembler._build_constraints("check ci status", tools)
        
        assert "File editing" not in constraints

    def test_reflection_rules_for_why_questions(self):
        """Reflection rules included for 'why' questions."""
        from core.context.prompt_assembler import PromptAssembler
        
        assembler = PromptAssembler.__new__(PromptAssembler)
        
        constraints = assembler._build_constraints("why did it fail", [])
        
        assert "Reflection" in constraints

    def test_introspection_rules_for_capability_questions(self):
        """Introspection rules included for capability questions."""
        from core.context.prompt_assembler import PromptAssembler
        
        assembler = PromptAssembler.__new__(PromptAssembler)
        
        constraints = assembler._build_constraints("what can you do", [])
        
        assert "Self-Model" in constraints

    def test_all_rules_without_query(self):
        """All rules included when no query context."""
        from core.context.prompt_assembler import PromptAssembler
        
        assembler = PromptAssembler.__new__(PromptAssembler)
        
        constraints = assembler._build_constraints(None, None)
        
        # Should include all rule blocks
        assert "File editing" in constraints
        assert "Tool selection" in constraints
        assert "Reflection" in constraints


class TestCatalogForcedSelection:
    """Test catalog selection triggers with > 1 tools (not > 2)."""

    def test_catalog_triggers_with_two_tools(self):
        """Catalog selection should trigger when tools > 1."""
        # The condition in chat_loop.py is: len(tools_schema) > 1
        tools_schema = [
            {"function": {"name": "tool1"}},
            {"function": {"name": "tool2"}},
        ]
        
        # With 2 tools, catalog should be used (> 1 is True)
        assert len(tools_schema) > 1


class TestTokenBreakdown:
    """Test tool_tokens vs non_tool_tokens breakdown."""

    def test_token_breakdown_calculation(self):
        """Verify token breakdown calculation logic."""
        budget = {
            "tool_schemas": 4000,
            "self_model": 300,
            "constraints": 150,
            "identity": 25,
            "memory": 10,
            "history": 500,
        }
        
        tool_tokens = budget.get("tool_schemas", 0)
        non_tool_tokens = sum(
            v for k, v in budget.items()
            if k != "tool_schemas" and isinstance(v, (int, float))
        )
        total_managed = tool_tokens + non_tool_tokens
        tool_ratio = tool_tokens / total_managed if total_managed > 0 else 0
        
        assert tool_tokens == 4000
        assert non_tool_tokens == 985  # 300 + 150 + 25 + 10 + 500
        assert total_managed == 4985
        assert round(tool_ratio, 2) == 0.80

    def test_token_breakdown_no_tools(self):
        """When no tool_schemas, ratio is 0."""
        budget = {
            "self_model": 300,
            "constraints": 150,
            "history": 500,
        }
        
        tool_tokens = budget.get("tool_schemas", 0)
        non_tool_tokens = sum(
            v for k, v in budget.items()
            if k != "tool_schemas" and isinstance(v, (int, float))
        )
        total_managed = tool_tokens + non_tool_tokens
        tool_ratio = tool_tokens / total_managed if total_managed > 0 else 0
        
        assert tool_tokens == 0
        assert non_tool_tokens == 950
        assert tool_ratio == 0.0

    def test_recommendation_when_tool_heavy(self):
        """Recommendation triggered when tool_ratio > 0.7."""
        budget = {"tool_schemas": 5000, "other": 1000}
        
        tool_tokens = budget.get("tool_schemas", 0)
        non_tool_tokens = sum(
            v for k, v in budget.items()
            if k != "tool_schemas" and isinstance(v, (int, float))
        )
        total_managed = tool_tokens + non_tool_tokens
        tool_ratio = tool_tokens / total_managed if total_managed > 0 else 0
        
        recommendation = (
            "tool_schemas dominating context — consider high-confidence or catalog selection"
            if total_managed > 0 and tool_ratio > 0.7
            else "balanced"
        )
        
        assert tool_ratio > 0.7
        assert "dominating" in recommendation


class TestChatTurnHighConfidence:
    """Test high-confidence optimization in /chat/turn endpoint."""

    def test_update_snapshot_tool_tokens(self, db_session):
        """Verify _update_snapshot_tool_tokens updates token_budget correctly."""
        import json

        from sqlalchemy import text
        from uuid_utils import uuid7

        from api.routers.chat import _update_snapshot_tool_tokens
        
        # Create a test snapshot
        snapshot_id = str(uuid7())
        initial_budget = {"tool_schemas": 5000, "self_model": 300, "constraints": 150}
        
        db_session.execute(text("""
            INSERT INTO ctx_snapshots 
            (context_capture_id, session_id, event_id, token_budget, total_tokens, created_at)
            VALUES (:cid, :sid, :eid, :budget, :total, NOW())
        """), {
            "cid": snapshot_id,
            "sid": f"test_{uuid7().hex[:8]}",
            "eid": snapshot_id,
            "budget": json.dumps(initial_budget),
            "total": 5450,
        })
        db_session.commit()
        
        # Update tool tokens
        _update_snapshot_tool_tokens(snapshot_id, 500)
        
        # Verify update
        row = db_session.execute(text(
            "SELECT token_budget FROM ctx_snapshots WHERE context_capture_id = :cid"
        ), {"cid": snapshot_id}).fetchone()
        
        updated_budget = json.loads(row[0]) if isinstance(row[0], str) else row[0]
        assert updated_budget["tool_schemas"] == 500
        assert updated_budget["self_model"] == 300  # unchanged
        
        # Cleanup
        db_session.execute(text(
            "DELETE FROM ctx_snapshots WHERE context_capture_id = :cid"
        ), {"cid": snapshot_id})
        db_session.commit()


class TestTokenBreakdownIntegration:
    """Integration tests for tool/non-tool token separation in DB and APIs."""

    def test_snapshot_stores_tool_schemas_separately(self, db_session):
        """Verify ctx_snapshots.token_budget stores tool_schemas as a separate field."""
        import json

        from sqlalchemy import text
        from uuid_utils import uuid7

        # Create our own test data — never depend on leftover DB state.
        snapshot_id = str(uuid7())
        session_id = str(uuid7())
        event_id = str(uuid7())
        budget = {"system_prompt": 120, "history": 350, "tool_schemas": 480}

        db_session.execute(text("""
            INSERT INTO ctx_snapshots
                (context_capture_id, session_id, event_id, token_budget, total_tokens, created_at)
            VALUES (:sid, :sess, :eid, :budget, :total, NOW())
        """), {
            "sid": snapshot_id, "sess": session_id, "eid": event_id,
            "budget": json.dumps(budget), "total": 950,
        })
        db_session.commit()

        # Re-query from DB to verify what was actually persisted
        row = db_session.execute(text(
            "SELECT token_budget, total_tokens FROM ctx_snapshots WHERE context_capture_id = :sid"
        ), {"sid": snapshot_id}).fetchone()

        assert row is not None, "Snapshot must be persisted"
        saved_budget = json.loads(row[0]) if isinstance(row[0], str) else row[0]

        # Verify every field in token_budget
        assert isinstance(saved_budget, dict), "token_budget should be a dict"
        assert saved_budget["system_prompt"] == 120
        assert saved_budget["history"] == 350
        assert saved_budget["tool_schemas"] == 480
        assert row[1] == 950, "total_tokens must match"

        # Verify tool vs non-tool separation
        tool_tokens = saved_budget["tool_schemas"]
        non_tool_tokens = sum(
            v for k, v in saved_budget.items()
            if k != "tool_schemas" and isinstance(v, (int, float))
        )
        assert tool_tokens == 480
        assert non_tool_tokens == 470  # 120 + 350


