"""Built-in skills for mo-agent-engine."""

import logging

from core.llm import LLMClient
from core.skills.base import (
    AccessScope,
    RepoType,
    SideEffectCategory,
    SideEffectProfile,
    Skill,
    SkillInput,
    SkillOutput,
    SkillRequirement,
)
from core.skills.github_client import GitHubClient
from sqlalchemy.orm import Session
from api.database import get_db_session

logger = logging.getLogger(__name__)


# ============================================================================
# Summarize PR Skill
# ============================================================================


class SummarizePRInput(SkillInput):
    """Input for summarize_pr skill"""

    pr_number: int
    include_diff: bool = True


class SummarizePROutput(SkillOutput):
    """Output for summarize_pr skill"""

    summary: str
    files_changed: int
    additions: int
    deletions: int


class SummarizePRSkill(Skill[SummarizePRInput, SummarizePROutput]):
    """Summarize a GitHub PR with LLM"""

    name = "summarize_pr"
    version = "1.0.0"
    description = "Summarize a GitHub PR with LLM analysis"
    requirements = SkillRequirement(
        repo_types=[RepoType.CODE], min_access=AccessScope.READ, llm_required=True
    )
    side_effect_profile = SideEffectProfile(
        category=SideEffectCategory.READ,
        external_apis=["github", "llm"],
    )

    def __init__(self, llm: LLMClient, github: GitHubClient, session: Session | None = None):
        self._session = session
        self.llm = llm
        self.github = github

    async def execute(self, input: SummarizePRInput) -> SummarizePROutput:
        """Execute the skill"""
        # 1. Fetch PR from GitHub
        pr = await self.github.get_pr(input.repo_id, input.pr_number)

        # 2. Build prompt
        prompt = f"""Summarize this PR concisely:

Title: {pr["title"]}
Description: {pr["body"]}
Files changed: {pr["files_changed"]}
"""
        if input.include_diff:
            diff = await self.github.get_pr_diff(input.repo_id, input.pr_number)
            prompt += f"\nDiff (first 5000 chars):\n{diff[:5000]}"

        # 3. Call LLM
        response = await self.llm.chat(
            messages=[{"role": "user", "content": prompt}],
            user_id=input.user_id,
            session_id=input.session_id,
            metadata={
                "skill": self.name,
                "skill_version": self.version,
                "pr_number": input.pr_number,
                "input": input.model_dump(),  # Store input for replay
            },
        )

        # 4. Return result
        return SummarizePROutput(
            success=True,
            result=response.content,
            summary=response.content,
            files_changed=pr["files_changed"],
            additions=pr["additions"],
            deletions=pr["deletions"],
            cost=response.cost_usd,
        )


# ============================================================================
# List PRs Skill
# ============================================================================


class ListPRsInput(SkillInput):
    """Input for list_prs skill"""

    state: str = "open"  # open, closed, all
    limit: int = 10


class ListPRsOutput(SkillOutput):
    """Output for list_prs skill"""

    prs: list[dict]


class ListPRsSkill(Skill[ListPRsInput, ListPRsOutput]):
    """List PRs in a repository"""

    name = "list_prs"
    version = "1.0.0"
    description = "List open/closed PRs in a repository"
    requirements = SkillRequirement(
        repo_types=[RepoType.CODE], min_access=AccessScope.READ, llm_required=False
    )
    side_effect_profile = SideEffectProfile(
        category=SideEffectCategory.READ,
        external_apis=["github"],
    )

    def __init__(self, github: GitHubClient, session: Session | None = None):
        self.github = github

    async def execute(self, input: ListPRsInput) -> ListPRsOutput:
        """Execute the skill"""
        prs = await self.github.list_prs(input.repo_id, input.state, input.limit)

        result = [
            {
                "number": pr["number"],
                "title": pr["title"],
                "author": pr["user"],
                "created_at": pr["created_at"],
                "url": pr["html_url"],
            }
            for pr in prs
        ]

        return ListPRsOutput(success=True, result=result, prs=result)


# ============================================================================
# CI Status Skill
# ============================================================================


class CIStatusInput(SkillInput):
    """Input for ci_status skill"""

    limit: int = 5


class CIStatusOutput(SkillOutput):
    """Output for ci_status skill"""

    workflows: list[dict]


class CIStatusSkill(Skill[CIStatusInput, CIStatusOutput]):
    """Check CI workflow status"""

    name = "ci_status"
    version = "1.0.0"
    description = "Check CI workflow status in a repository"
    requirements = SkillRequirement(
        repo_types=[RepoType.CI, RepoType.CODE],
        min_access=AccessScope.READ,
        llm_required=False,
    )
    side_effect_profile = SideEffectProfile(
        category=SideEffectCategory.READ,
        external_apis=["github"],
    )

    def __init__(self, github: GitHubClient, session: Session | None = None):
        self.github = github

    async def execute(self, input: CIStatusInput) -> CIStatusOutput:
        """Execute the skill"""
        runs = await self.github.list_workflow_runs(input.repo_id, input.limit)

        workflows = [
            {
                "workflow": run["name"],
                "status": run["status"],
                "conclusion": run["conclusion"],
                "url": run["html_url"],
                "created_at": run["created_at"],
            }
            for run in runs
        ]

        return CIStatusOutput(success=True, result=workflows, workflows=workflows)


# ============================================================================
# Execute Code Skill
# ============================================================================


class ExecuteCodeInput(SkillInput):
    """Input for execute_code skill"""
    code: str
    language: str = "python"
    data_access: str = "none"  # "none", "read", "write"
    source_db: str | None = None          # Required for read/write
    tables: list[str] | None = None       # Required for write
    session_id: str | None = None
    allowed_imports: list[str] | None = None


class ExecuteCodeOutput(SkillOutput):
    """Output for execute_code skill"""
    success: bool = False
    result: str = ""
    error: str | None = None
    execution_time_ms: float = 0
    data_diff: list[dict] | None = None
    time_travel: dict | None = None


class ExecuteCodeSkill(Skill[ExecuteCodeInput, ExecuteCodeOutput]):
    """Execute Python code in isolated environment with optional database access."""

    name = "execute_code"
    version = "1.0.0"
    description = "Execute Python code in isolated environment with optional database access"
    requirements = SkillRequirement(
        repo_types=[],
        min_access=AccessScope.WRITE,
        llm_required=False,
    )
    side_effect_profile = SideEffectProfile(
        category=SideEffectCategory.WRITE,
        mock_strategy="recorded",
    )

    def __init__(self, code_executor):
        self.code_executor = code_executor

    async def execute(self, input: ExecuteCodeInput) -> ExecuteCodeOutput:
        from core.code_executor import CodeExecutionRequest
        from core.code_executor.data_context import DataAccessLevel

        result = self.code_executor.execute(CodeExecutionRequest(
            code=input.code,
            language=input.language,
            data_access=DataAccessLevel(input.data_access),
            source_db=input.source_db,
            tables=input.tables,
            session_id=input.session_id,
            allowed_imports=input.allowed_imports,
        ))

        time_travel_dict = None
        if result.time_travel:
            tt = result.time_travel
            time_travel_dict = {
                "started_at": tt.started_at.isoformat(),
                "source_db": tt.source_db,
                "sandbox_db": tt.sandbox_db,
            }

        return ExecuteCodeOutput(
            success=result.execution.exit_code == 0,
            result=result.execution.stdout,
            error=result.execution.stderr if result.execution.exit_code != 0 else None,
            execution_time_ms=result.execution.execution_time_ms,
            data_diff=[
                {"table": d.table, "rows": d.rows}
                for d in result.data_diff
            ] if result.data_diff else None,
            time_travel=time_travel_dict,
        )


def register_builtin_skills(
    registry, db, llm=None, github=None, agent_registry=None, chat_loop_factory=None,
    code_executor=None,
):
    """Register all built-in skills.

    Args:
        registry: SkillRegistry instance
        session: Session | None = None instance
        llm: Optional LLMClient instance
        github: Optional GitHubClient instance
        agent_registry: Optional AgentRegistry for multi-agent delegation
        chat_loop_factory: Optional factory for creating ChatLoop instances
    """
    from core.llm import LLMClient
    from core.skills.delegation import DelegateTaskSkill
    from core.skills.github_client import GitHubClient

    # Initialize clients if not provided
    if github is None:
        github = GitHubClient(db)
    if llm is None:
        llm = LLMClient(db)

    # Register skills with metadata
    skills = [
        (
            SummarizePRSkill(db, llm, github),
            "github",
            "pr_management",
            ["summarize", "summary", "pr", "pull request"],
            [],
            8,
            "medium",
        ),
        (
            ListPRsSkill(db, github),
            "github",
            "pr_management",
            ["list", "show", "prs", "pull requests"],
            [],
            5,
            "low",
        ),
        (
            CIStatusSkill(db, github),
            "github",
            "ci_cd",
            ["ci", "build", "workflow", "status"],
            [],
            7,
            "low",
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
            logger.info(f"Registered {skill.name}@{skill.version}")
        except Exception as e:
            logger.warning(f"Failed to register {skill.name}: {e}")

    # Register code execution skill
    if code_executor:
        try:
            exec_skill = ExecuteCodeSkill(code_executor)
            registry.register(
                skill=exec_skill,
                is_active=True,
                category="code_execution",
                subcategory="sandbox",
                triggers=["execute", "run", "code", "python", "compute", "calculate"],
                dependencies=[],
                priority=9,
                cost_estimate="medium",
            )
            logger.info(f"Registered {exec_skill.name}@{exec_skill.version}")
        except Exception as e:
            logger.warning(f"Failed to register execute_code skill: {e}")

    # Register delegation skill for multi-agent collaboration
    if agent_registry and chat_loop_factory:
        try:
            delegation_skill = DelegateTaskSkill(
                agent_registry=agent_registry, chat_loop_factory=chat_loop_factory
            )
            registry.register(
                skill=delegation_skill,
                is_active=True,
                category="multi_agent",
                subcategory="coordination",
                triggers=["delegate", "assign", "task", "agent"],
                dependencies=[],
                priority=10,
                cost_estimate="low",
            )
            logger.info(f"Registered {delegation_skill.name}@{delegation_skill.version}")
        except Exception as e:
            logger.warning(f"Failed to register delegation skill: {e}")
