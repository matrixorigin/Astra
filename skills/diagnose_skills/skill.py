"""Skill health diagnosis — detect and report skill issues.

Analyzes skill registry for:
- Orphaned skills (DB record exists, local file missing)
- Load errors (local file exists but fails to load)
- Version mismatches (DB version != loaded version)
"""

from __future__ import annotations

from enum import Enum
from pathlib import Path
from typing import TYPE_CHECKING

from pydantic import Field

from core.logging_config import get_logger
from core.skills.base import (
    RuntimeRequirement,
    SideEffectCategory,
    SideEffectProfile,
    Skill,
    SkillInput,
    SkillOutput,
    SkillRequirement,
)

if TYPE_CHECKING:
    from sqlalchemy.orm import Session

logger = get_logger(__name__)

# Max skill names to show in summary mode per category
SUMMARY_NAMES_LIMIT = 5


class DiagnosisLevel(str, Enum):
    """Level of detail in diagnosis report."""

    SUMMARY = "summary"  # Counts + up to 5 names per category
    DETAILED = "detailed"  # Full diagnosis of ONE issue


class DiagnoseSkillsInput(SkillInput):
    """Input for skill diagnosis."""

    level: DiagnosisLevel = Field(
        default=DiagnosisLevel.SUMMARY,
        description="'summary': counts + 5 names per category; 'detailed': full diagnosis of first issue",
    )
    source: str = Field(
        default="all",
        description="Skill source: 'user', 'builtin', 'marketplace', or 'all'",
    )
    skill_name: str | None = Field(
        default=None,
        description="(detailed mode) Specific skill to diagnose. If None, diagnoses first problematic skill.",
    )


class DiagnoseSkillsOutput(SkillOutput):
    """Output from skill diagnosis."""

    # Counts (always returned)
    total_skills: int = 0
    healthy: int = 0
    orphaned: int = 0
    load_errors: int = 0
    version_mismatches: int = 0

    # Health status
    health_status: str = "healthy"  # healthy, warning, critical

    # Summary: up to 5 names per category
    orphaned_names: list[str] = Field(default_factory=list)
    load_error_names: list[str] = Field(default_factory=list)
    mismatch_names: list[str] = Field(default_factory=list)
    more_issues: bool = False  # True if more than 5 in any category

    # Detailed: ONE issue with full diagnosis
    diagnosis: dict | None = None

    # Suggestions
    suggestions: list[str] = Field(default_factory=list)


class DiagnoseSkillsSkill(Skill[DiagnoseSkillsInput, DiagnoseSkillsOutput]):
    """Diagnose skill health and report issues."""

    name = "diagnose_skills"
    version = "1.1.0"
    description = "Check skill health: find broken, misconfigured, or orphaned skills. Use when skills fail unexpectedly."
    requirements = SkillRequirement(
        runtime=[RuntimeRequirement.DATABASE],
        llm_required=False,
        timeout_seconds=60,
    )
    side_effect_profile = SideEffectProfile(
        category=SideEffectCategory.READ,
        external_apis=[],
    )

    def __init__(self, db: Session | None = None) -> None:
        self._db = db

    async def execute(self, input_data: DiagnoseSkillsInput) -> DiagnoseSkillsOutput:
        """Run skill health diagnosis."""
        from api.database import get_db_context
        from api.models import SkillRegistry as SkillModel
        from core.skills.loader import SkillLoader

        if self._db:
            db = self._db
            should_close = False
        else:
            db = get_db_context().__enter__()
            should_close = True

        try:
            output = DiagnoseSkillsOutput(success=True)

            # Sources to check
            sources = (
                ["user", "builtin", "marketplace"]
                if input_data.source == "all"
                else [input_data.source]
            )

            # Load local skills once
            project_root = Path(__file__).resolve().parent.parent.parent
            local_skills = SkillLoader.discover([project_root / "skills"])
            loadable = {s.skill.name: s.skill for s in local_skills if s.skill}

            # Collect all issues
            orphaned: list[tuple[str, str]] = []  # (name, skill_id)
            load_errors: list[tuple[str, str, str]] = []  # (name, skill_id, error)
            mismatches: list[tuple[str, str, str, str]] = []  # (name, skill_id, db_ver, local_ver)

            db_skills = (
                db.query(SkillModel)
                .filter(
                    SkillModel.is_active == 1,
                    SkillModel.source.in_(sources),
                )
                .all()
            )

            output.total_skills = len(db_skills)

            for skill in db_skills:
                name, sid = skill.skill_name, skill.skill_id

                if name not in loadable:
                    skill_dir = project_root / "skills" / name
                    if not skill_dir.exists():
                        orphaned.append((name, sid))
                    else:
                        err = self._diagnose_load_error(skill_dir)
                        load_errors.append((name, sid, err))
                elif loadable[name].version != skill.version:
                    mismatches.append((name, sid, skill.version, loadable[name].version))
                else:
                    output.healthy += 1

            output.orphaned = len(orphaned)
            output.load_errors = len(load_errors)
            output.version_mismatches = len(mismatches)

            # Health status
            if orphaned or load_errors:
                output.health_status = "critical"
            elif mismatches:
                output.health_status = "warning"

            # Summary mode: show up to 5 names
            output.orphaned_names = [n for n, _ in orphaned[:SUMMARY_NAMES_LIMIT]]
            output.load_error_names = [n for n, _, _ in load_errors[:SUMMARY_NAMES_LIMIT]]
            output.mismatch_names = [n for n, _, _, _ in mismatches[:SUMMARY_NAMES_LIMIT]]
            output.more_issues = (
                len(orphaned) > SUMMARY_NAMES_LIMIT
                or len(load_errors) > SUMMARY_NAMES_LIMIT
                or len(mismatches) > SUMMARY_NAMES_LIMIT
            )

            # Detailed mode: diagnose ONE skill
            if input_data.level == DiagnosisLevel.DETAILED:
                target = input_data.skill_name
                if target:
                    # User specified a skill
                    output.diagnosis = self._detailed_diagnosis(
                        target, project_root, loadable, db_skills
                    )
                else:
                    # Pick first problematic skill
                    if orphaned:
                        name, sid = orphaned[0]
                        output.diagnosis = {
                            "skill_name": name,
                            "skill_id": sid,
                            "issue_type": "orphaned",
                            "message": f"Directory not found: skills/{name}/",
                            "suggestion": f"Restore files to skills/{name}/ or cleanup DB record",
                            "details": {"expected_path": str(project_root / "skills" / name)},
                        }
                    elif load_errors:
                        name, sid, err = load_errors[0]
                        output.diagnosis = {
                            "skill_name": name,
                            "skill_id": sid,
                            "issue_type": "load_error",
                            "message": err,
                            "suggestion": "Fix the error in skill.py",
                            "details": {
                                "skill_py_path": str(project_root / "skills" / name / "skill.py")
                            },
                        }
                    elif mismatches:
                        name, sid, db_ver, local_ver = mismatches[0]
                        output.diagnosis = {
                            "skill_name": name,
                            "skill_id": sid,
                            "issue_type": "version_mismatch",
                            "message": f"DB has v{db_ver}, local has v{local_ver}",
                            "suggestion": "Re-register skill to sync versions",
                            "details": {"db_version": db_ver, "local_version": local_ver},
                        }

            # Suggestions
            total_issues = output.orphaned + output.load_errors + output.version_mismatches
            if total_issues == 0:
                output.suggestions.append("All skills healthy!")
            else:
                if output.orphaned:
                    output.suggestions.append(
                        f"{output.orphaned} orphaned - restore files or cleanup DB"
                    )
                if output.load_errors:
                    output.suggestions.append(f"{output.load_errors} load errors - fix skill.py")
                if output.version_mismatches:
                    output.suggestions.append(
                        f"{output.version_mismatches} version mismatches - re-register"
                    )
                if output.more_issues:
                    output.suggestions.append(
                        "Use detailed mode with skill_name to inspect specific skill"
                    )

            return output

        finally:
            if should_close:
                db.close()

    def _detailed_diagnosis(self, name: str, root: Path, loadable: dict, db_skills: list) -> dict:
        """Full diagnosis for a specific skill."""
        skill_dir = root / "skills" / name
        db_skill = next((s for s in db_skills if s.skill_name == name), None)

        if not db_skill:
            return {
                "skill_name": name,
                "issue_type": "not_in_db",
                "message": "Skill not found in database (not registered or inactive)",
                "suggestion": "Register the skill first",
            }

        if name not in loadable:
            if not skill_dir.exists():
                return {
                    "skill_name": name,
                    "skill_id": db_skill.skill_id,
                    "issue_type": "orphaned",
                    "message": f"Directory not found: {skill_dir}",
                    "suggestion": "Restore files or cleanup DB record",
                    "details": {"expected_path": str(skill_dir)},
                }
            else:
                err = self._diagnose_load_error(skill_dir)
                return {
                    "skill_name": name,
                    "skill_id": db_skill.skill_id,
                    "issue_type": "load_error",
                    "message": err,
                    "suggestion": "Fix the error in skill.py",
                    "details": {"skill_py_path": str(skill_dir / "skill.py")},
                }

        local = loadable[name]
        if local.version != db_skill.version:
            return {
                "skill_name": name,
                "skill_id": db_skill.skill_id,
                "issue_type": "version_mismatch",
                "message": f"DB v{db_skill.version} != local v{local.version}",
                "suggestion": "Re-register to sync",
                "details": {"db_version": db_skill.version, "local_version": local.version},
            }

        return {
            "skill_name": name,
            "skill_id": db_skill.skill_id,
            "issue_type": "healthy",
            "message": "Skill is healthy",
            "details": {"version": local.version, "description": local.description},
        }

    def _diagnose_load_error(self, skill_dir: Path) -> str:
        """Try to load skill and capture error."""
        skill_py = skill_dir / "skill.py"
        if not skill_py.exists():
            return "skill.py not found"

        try:
            import importlib.util

            spec = importlib.util.spec_from_file_location("_diag", skill_py)
            if spec is None or spec.loader is None:
                return "Failed to create module spec"
            mod = importlib.util.module_from_spec(spec)
            spec.loader.exec_module(mod)
            return "Module loaded but no Skill subclass found"
        except SyntaxError as e:
            return f"SyntaxError line {e.lineno}: {e.msg}"
        except ImportError as e:
            return f"ImportError: {e}"
        except Exception as e:
            return f"{type(e).__name__}: {e}"
