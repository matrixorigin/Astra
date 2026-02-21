"""Extended skills - properly inheriting from Skill base class."""

from typing import Any

from core.skills.base import AccessScope, RepoType, Skill, SkillInput, SkillOutput, SkillRequirement
from core.skills.github_client import GitHubClient
from sqlalchemy.orm import Session

# ============================================================================
# Code Review Skill
# ============================================================================


class CodeReviewInput(SkillInput):
    pr_number: int
    focus: str = "all"


class CodeReviewOutput(SkillOutput):
    review: dict[str, Any]


class CodeReviewSkill(Skill):
    name = "code_review"
    version = "1.0.0"
    description = "Review code changes in a pull request"
    requirements = SkillRequirement(
        repo_types=[RepoType.CODE], min_access=AccessScope.READ, llm_required=False
    )
    input_schema = CodeReviewInput
    output_schema = CodeReviewOutput

    def __init__(self, github: GitHubClient, session: Session | None = None):
        self.db = session
        self.github = github

    async def execute(self, input: CodeReviewInput) -> CodeReviewOutput:  # type: ignore[override]
        pr = await self.github.get_pr(input.repo_id, input.pr_number)
        review = {
            "pr_number": input.pr_number,
            "files_changed": pr.get("changed_files", 0),
            "additions": pr.get("additions", 0),
            "deletions": pr.get("deletions", 0),
            "focus": input.focus,
            "suggestions": [],
        }
        if review["additions"] > 500:
            review["suggestions"].append("Large PR - consider splitting")
        return CodeReviewOutput(success=True, result=review, review=review)


# ============================================================================
# Search Code Skill
# ============================================================================


class SearchCodeInput(SkillInput):
    query: str
    file_pattern: str = "*"


class SearchCodeOutput(SkillOutput):
    results: list[dict[str, Any]]


class SearchCodeSkill(Skill):
    name = "search_code"
    version = "1.0.0"
    description = "Search code in repository"
    requirements = SkillRequirement(
        repo_types=[RepoType.CODE], min_access=AccessScope.READ, llm_required=False
    )
    input_schema = SearchCodeInput
    output_schema = SearchCodeOutput

    def __init__(self, github: GitHubClient, session: Session | None = None):
        self.db = session
        self.github = github

    async def execute(self, input: SearchCodeInput) -> SearchCodeOutput:
        results = [{"file": "example.py", "line": 42, "content": f"Match for: {input.query}"}]
        return SearchCodeOutput(success=True, result=results, results=results)


# ============================================================================
# Generate Tests Skill
# ============================================================================


class GenerateTestsInput(SkillInput):
    file_path: str
    function_name: str


class GenerateTestsOutput(SkillOutput):
    test_code: str


class GenerateTestsSkill(Skill):
    name = "generate_tests"
    version = "1.0.0"
    description = "Generate unit tests for code"
    requirements = SkillRequirement(
        repo_types=[RepoType.CODE], min_access=AccessScope.READ, llm_required=True
    )
    input_schema = GenerateTestsInput
    output_schema = GenerateTestsOutput

    def __init__(self, github: GitHubClient, session: Session | None = None):
        self.db = session
        self.github = github

    async def execute(self, input: GenerateTestsInput) -> GenerateTestsOutput:
        test_code = f"def test_{input.function_name}():\n    pass"
        return GenerateTestsOutput(success=True, result=test_code, test_code=test_code)


# ============================================================================
# Analyze Bug Skill
# ============================================================================


class AnalyzeBugInput(SkillInput):
    issue_number: int


class AnalyzeBugOutput(SkillOutput):
    analysis: dict[str, Any]


class AnalyzeBugSkill(Skill):
    name = "analyze_bug"
    version = "1.0.0"
    description = "Analyze bug reports and suggest fixes"
    requirements = SkillRequirement(
        repo_types=[RepoType.CODE], min_access=AccessScope.READ, llm_required=True
    )
    input_schema = AnalyzeBugInput
    output_schema = AnalyzeBugOutput

    def __init__(self, github: GitHubClient, session: Session | None = None):
        self.db = session
        self.github = github

    async def execute(self, input: AnalyzeBugInput) -> AnalyzeBugOutput:
        analysis = {
            "issue_number": input.issue_number,
            "severity": "medium",
            "suggested_fix": "Investigate further",
        }
        return AnalyzeBugOutput(success=True, result=analysis, analysis=analysis)


# ============================================================================
# Registration Helper
# ============================================================================


def register_extended_skills(registry, db, github=None):
    """Register all extended skills."""
    from core.skills.github_client import GitHubClient

    if github is None:
        github = GitHubClient(db)

    skills = [
        (
            CodeReviewSkill(github, db),
            "github",
            "pr_management",
            ["review", "code review"],
            [],
            8,
            "medium",
        ),
        (SearchCodeSkill(github, db), "code", "analysis", ["search", "find", "code"], [], 6, "low"),
        (
            GenerateTestsSkill(github, db),
            "code",
            "testing",
            ["test", "generate", "unit test"],
            ["search_code"],
            7,
            "high",
        ),
        (
            AnalyzeBugSkill(github, db),
            "github",
            "issue_management",
            ["bug", "issue", "analyze"],
            ["search_code"],
            7,
            "high",
        ),
    ]

    for skill, category, subcategory, triggers, dependencies, priority, cost in skills:
        try:
            registry.register(
                skill=skill,
                is_active=True,
                category=category,
                subcategory=subcategory,
                triggers=triggers,
                dependencies=dependencies,
                priority=priority,
                cost_estimate=cost,
            )
        except Exception:
            pass  # Already registered
