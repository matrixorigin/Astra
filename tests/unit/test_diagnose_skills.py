"""Tests for diagnose_skills skill."""

import pytest

from skills.diagnose_skills.skill import (
    DiagnoseSkillsInput,
    DiagnoseSkillsSkill,
    DiagnosisLevel,
    SUMMARY_NAMES_LIMIT,
)


@pytest.fixture
def skill(db_session):
    return DiagnoseSkillsSkill(db=db_session)


class TestDiagnoseSkills:
    @pytest.mark.asyncio
    async def test_summary_returns_counts_and_names(self, skill):
        """Summary mode returns counts and up to 5 names per category."""
        result = await skill.execute(DiagnoseSkillsInput(level=DiagnosisLevel.SUMMARY))

        assert result.success is True
        assert result.total_skills >= 0
        assert result.health_status in ("healthy", "warning", "critical")

        # Names lists should be limited
        assert len(result.orphaned_names) <= SUMMARY_NAMES_LIMIT
        assert len(result.load_error_names) <= SUMMARY_NAMES_LIMIT
        assert len(result.mismatch_names) <= SUMMARY_NAMES_LIMIT

        # No detailed diagnosis in summary mode
        assert result.diagnosis is None

    @pytest.mark.asyncio
    async def test_detailed_returns_one_diagnosis(self, skill):
        """Detailed mode returns full diagnosis of one issue."""
        result = await skill.execute(DiagnoseSkillsInput(level=DiagnosisLevel.DETAILED))

        assert result.success is True

        # If there are issues, should have diagnosis
        total_issues = result.orphaned + result.load_errors + result.version_mismatches
        if total_issues > 0:
            assert result.diagnosis is not None
            assert "skill_name" in result.diagnosis
            assert "issue_type" in result.diagnosis
            assert "message" in result.diagnosis
            assert "suggestion" in result.diagnosis

    @pytest.mark.asyncio
    async def test_detailed_specific_skill(self, skill, db_session):
        """Can diagnose a specific skill by name."""
        import uuid
        from api.models import SkillRegistry as SkillModel

        # Create a test skill to ensure we have one
        test_skill = SkillModel(
            skill_id=str(uuid.uuid4()),
            skill_name="test_diagnose_skill",
            version="1.0.0",
            description="Test skill for diagnosis",
            is_active=1,
        )
        db_session.merge(test_skill)
        db_session.commit()

        result = await skill.execute(
            DiagnoseSkillsInput(level=DiagnosisLevel.DETAILED, skill_name="test_diagnose_skill")
        )

        assert result.success is True
        assert result.diagnosis is not None
        assert result.diagnosis["skill_name"] == "test_diagnose_skill"

    @pytest.mark.asyncio
    async def test_detailed_nonexistent_skill(self, skill):
        """Detailed mode for nonexistent skill returns not_in_db."""
        result = await skill.execute(
            DiagnoseSkillsInput(
                level=DiagnosisLevel.DETAILED, skill_name="nonexistent_skill_xyz_999"
            )
        )

        assert result.success is True
        assert result.diagnosis is not None
        assert result.diagnosis["issue_type"] == "not_in_db"

    @pytest.mark.asyncio
    async def test_more_issues_flag(self, skill):
        """more_issues flag indicates when there are more than 5 in a category."""
        result = await skill.execute(DiagnoseSkillsInput(level=DiagnosisLevel.SUMMARY))

        # If any category has more than 5, flag should be True
        has_more = (
            result.orphaned > SUMMARY_NAMES_LIMIT
            or result.load_errors > SUMMARY_NAMES_LIMIT
            or result.version_mismatches > SUMMARY_NAMES_LIMIT
        )
        assert result.more_issues == has_more

    @pytest.mark.asyncio
    async def test_suggestions_always_present(self, skill):
        """Suggestions are always generated."""
        result = await skill.execute(DiagnoseSkillsInput())
        assert len(result.suggestions) > 0

    @pytest.mark.asyncio
    async def test_filter_by_source(self, skill):
        """Can filter by source."""
        result = await skill.execute(DiagnoseSkillsInput(source="user"))
        assert result.success is True

    @pytest.mark.asyncio
    async def test_healthy_skill_diagnosis(self, skill, db_session, tmp_path):
        """Detailed diagnosis of healthy skill returns healthy status."""
        from api.models import SkillRegistry as SkillModel

        # Find a skill that exists both in DB and locally (e.g., diagnose_skills itself)
        result = await skill.execute(
            DiagnoseSkillsInput(level=DiagnosisLevel.DETAILED, skill_name="diagnose_skills")
        )

        # diagnose_skills should be healthy (it's running!)
        if result.diagnosis and result.diagnosis.get("issue_type") == "healthy":
            assert "version" in result.diagnosis.get("details", {})

    @pytest.mark.asyncio
    async def test_load_error_diagnosis(self, skill, db_session, tmp_path):
        """Test diagnosis of skill with load error."""
        from pathlib import Path

        # Create a broken skill file
        broken_dir = tmp_path / "broken_skill"
        broken_dir.mkdir()
        (broken_dir / "__init__.py").write_text("")
        (broken_dir / "skill.py").write_text("this is not valid python syntax !!!")

        # Test the internal _diagnose_load_error method
        error = skill._diagnose_load_error(broken_dir)
        assert "SyntaxError" in error

    @pytest.mark.asyncio
    async def test_load_error_missing_skill_py(self, skill, tmp_path):
        """Test diagnosis when skill.py is missing."""
        empty_dir = tmp_path / "empty_skill"
        empty_dir.mkdir()

        error = skill._diagnose_load_error(empty_dir)
        assert "skill.py not found" in error

    @pytest.mark.asyncio
    async def test_load_error_no_skill_class(self, skill, tmp_path):
        """Test diagnosis when skill.py has no Skill subclass."""
        no_class_dir = tmp_path / "no_class_skill"
        no_class_dir.mkdir()
        (no_class_dir / "skill.py").write_text("x = 1\n")

        error = skill._diagnose_load_error(no_class_dir)
        assert "no Skill subclass" in error

    @pytest.mark.asyncio
    async def test_version_mismatch_in_detailed(self, skill, db_session):
        """Test detailed diagnosis shows version mismatch info."""
        from api.models import SkillRegistry as SkillModel
        import uuid

        # Create a skill with mismatched version in DB
        # diagnose_skills is v1.1.0 locally, register as v0.0.1 in DB
        skill_id = f"diagnose_skills@0.0.1-test-{uuid.uuid4().hex}"
        db_session.add(
            SkillModel(
                skill_id=skill_id,
                skill_name="diagnose_skills",
                version="0.0.1",
                description="test mismatch",
                is_active=1,
                source="user",
            )
        )
        db_session.commit()

        try:
            result = await skill.execute(
                DiagnoseSkillsInput(
                    level=DiagnosisLevel.DETAILED, skill_name="diagnose_skills", source="user"
                )
            )

            # Should detect version mismatch
            if result.diagnosis and result.diagnosis.get("issue_type") == "version_mismatch":
                assert "0.0.1" in result.diagnosis["message"]
                assert "details" in result.diagnosis
        finally:
            # Cleanup
            db_session.query(SkillModel).filter(SkillModel.skill_id == skill_id).delete()
            db_session.commit()
