"""Built-in skills for mo-agent-engine."""

import logging

from pydantic import field_validator
from sqlalchemy.orm import Session

from core.exceptions import GitHubError, GitHubRateLimitError
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

logger = logging.getLogger(__name__)


# ============================================================================
# Cloud Skill Design Rules
# ============================================================================
#
# PRINCIPLE: The skill controls information density. The LLM must NOT do
# secondary trimming — it receives exactly what it needs, no more, no less.
#
# DETAIL LEVELS (all data-fetching skills must support `detail` parameter):
#
#   brief    — default. Minimum viable for a decision. Used for lists/overviews.
#   normal   — user asks "what is this" / "tell me about X". Key context added.
#   detailed — user asks "why did it fail" / "show me details". Diagnostic info.
#   full     — user explicitly asks for "everything". Near-raw, noise stripped.
#
# Each level is a strict superset of the level below it.
# `full` still truncates at a token budget — it is NOT "no truncation".
#
# TRUNCATION LIMITS (apply consistently):
#
#   Title/name:        80 chars across all levels (full: unlimited)
#   Body/description:  omit | 200 | 500 | 2000 chars
#   Log/diff content:  omit | omit | 500 | 2000 chars
#   List items:        10   | 10   | 20  | 50
#   Total output:      ~500 | ~1500 | ~4000 | ~8000 chars
#
#   Always append "[truncated]" when content is cut. Never silently drop.
#
# STRUCTURED OUTPUT:
#   - Fixed field set per level — LLM never checks `if field in result`
#   - Normalize API values (e.g. GitHub conclusion → success/failure/pending/
#     skipped/cancelled). Never pass through raw API strings.
#   - Timestamps: always "YYYY-MM-DD HH:MM", never ISO 8601.
#
# REPO AUTO-RESOLUTION (all GitHub skills):
#   - Accept bare project name (e.g. "milvus") OR "owner/repo"
#   - Standard pattern:
#       resolved_by_search = isinstance(repo, str) and "/" not in repo
#       resolved = self.github.resolve_repo(repo) if resolved_by_search else repo
#   - Output must include: resolved_repo: str | None, resolved_by_search: bool
#   - LLM description must say: "If resolved_by_search=True, show results first,
#     then add a note: 'Auto-resolved to {resolved_repo} — let me know if you
#     meant a different repo.' Do NOT ask for confirmation before showing results."
#
# SKILL DESCRIPTION must cover (in order):
#   1. What it does (one sentence)
#   2. When to use it (trigger conditions)
#   3. repo format — auto-resolved from bare name
#   4. detail levels — what each level adds
#   5. resolved_by_search — confirmation instruction
#   6. "Do NOT call proactively" (if applicable)
#
# CI SKILL field requirements by level:
#   brief:    workflow name, conclusion, branch, triggered_at
#   normal:   + pr_number, pr_title, commit message (80 chars), duration
#   detailed: + failed job names, first failed step + error (200 chars each)
#   full:     + all job statuses, failed step log snippets (500 chars each)
#
# PR SKILL field requirements by level:
#   brief:    number, title (80), author, state, created_at, ci_conclusion, changed_files
#             (ci_conclusion only in get_pr, not list_prs — requires extra API call per PR)
#   normal:   + body (200), labels, reviewers, changed_files count
#   detailed: + additions/deletions, key changed files (top 10), review count
#   full:     + complete body (2000), per-file diff (500/file), all reviews
#
# ISSUE SKILL field requirements by level:
#   brief:    number, title (80), state, labels, created_at
#   normal:   + body (200), assignee, milestone, comment_count
#   detailed: + recent 3 comments (200 each), linked PRs
#   full:     + complete body (2000), all comments (200 each, up to 20)
#
# See also: .kiro/steering/cloud-skill-design.md
# ============================================================================

# Shared description fragments — injected into every GitHub skill description
# to avoid repeating ~350 chars of boilerplate per skill.
_GITHUB_BOILERPLATE = (
    "repo: 'owner/repo' or bare name (auto-resolved by stars). "
    "On 'owner/repo' error, report not found — do NOT retry bare. "
    "If resolved_by_search=True, show results then note which repo was used. "
    "On success=False, report error — no bash/curl fallback."
)


class SummarizePRInput(SkillInput):
    """Input for summarize_pr skill"""

    repo: str = ""  # "owner/repo", e.g. "matrixorigin/matrixone"
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
    config_namespace = "github"
    version = "1.0.0"
    description = (
        "Summarize a specific GitHub PR using LLM analysis — includes diff review, "
        "change description, and impact assessment. Use this when user asks to review, "
        "explain, or understand a specific PR. Requires repo ('owner/repo') and pr_number."
    )
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
        # 1. Fetch PR from GitHub — use full detail for LLM analysis
        repo = input.repo or input.repo_id
        pr = await self.github.get_pr(repo, input.pr_number, detail="full")

        # 2. Build prompt
        prompt = f"""Summarize this PR concisely:

Title: {pr["title"]}
Description: {pr["body"]}
Files changed: {pr["changed_files"]}
"""
        if input.include_diff:
            diff = await self.github.get_pr_diff(repo, input.pr_number)
            prompt += f"\nDiff (first 5000 chars):\n{diff[:5000]}"

        # 3. Call LLM
        response = self.llm.chat(
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
            files_changed=pr["changed_files"],
            additions=pr["additions"],
            deletions=pr["deletions"],
            cost=response.cost_usd,
        )


# ============================================================================
# List PRs Skill
# ============================================================================


class ListPRsInput(SkillInput):
    """Input for list_prs skill"""

    repo: str = ""  # "owner/repo", e.g. "matrixorigin/matrixone"
    state: str = "open"  # open, closed, all
    limit: int = 10
    detail: str = "brief"  # brief | normal (ci_conclusion requires per-PR API call; use get_pr for that)


class ListPRsOutput(SkillOutput):
    """Output for list_prs skill"""

    prs: list[dict]
    resolved_repo: str | None = None
    resolved_by_search: bool = False


class ListPRsSkill(Skill[ListPRsInput, ListPRsOutput]):
    """List PRs in a repository"""

    name = "list_prs"
    config_namespace = "github"
    version = "1.0.0"
    description = (
        "List pull requests. "
        "detail: brief=number/title/author/state/date; normal adds body+labels+reviewers+files. "
        + _GITHUB_BOILERPLATE
    )
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
        repo = input.repo or input.repo_id
        if not repo:
            return ListPRsOutput(success=False, result="repo is required (e.g. 'matrixorigin/matrixone')", prs=[])
        resolved_by_search = isinstance(repo, str) and "/" not in repo
        resolved = self.github.resolve_repo(repo) if resolved_by_search else repo
        try:
            prs = await self.github.list_prs(resolved, input.state, input.limit, input.detail)
        except GitHubError as e:
            return ListPRsOutput(success=False, result=str(e), prs=[])
        except GitHubRateLimitError:
            return ListPRsOutput(success=False, result="GitHub rate limit exceeded, please try again later.", prs=[])
        return ListPRsOutput(
            success=True, result=prs, prs=prs,
            resolved_repo=resolved if resolved_by_search else None,
            resolved_by_search=resolved_by_search,
        )


# ============================================================================
# CI Status Skill
# ============================================================================


class CIStatusInput(SkillInput):
    """Input for ci_status skill"""

    repo: str = ""  # "owner/repo", e.g. "matrixorigin/matrixone"
    limit: int = 5
    detail: str = "brief"  # brief | normal | detailed | full


class CIStatusOutput(SkillOutput):
    """Output for ci_status skill"""

    workflows: list[dict]
    resolved_repo: str | None = None  # set when repo name was resolved via GitHub search
    resolved_by_search: bool = False  # True = LLM should confirm repo with user if result looks wrong


class CIStatusSkill(Skill[CIStatusInput, CIStatusOutput]):
    """Check CI workflow status"""

    name = "ci_status"
    config_namespace = "github"
    version = "1.0.0"
    description = (
        "Check CI/CD workflow run status — recent runs with pass/fail/pending. "
        "detail: brief=workflow/conclusion/branch/date; normal +PR+commit+duration; "
        "detailed +per-job+failed jobs; full +failed steps. "
        + _GITHUB_BOILERPLATE
    )
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
        repo = input.repo or input.repo_id
        resolved_by_search = isinstance(repo, str) and bool(repo) and "/" not in repo
        resolved = self.github.resolve_repo(repo) if resolved_by_search else repo
        try:
            runs = await self.github.list_wf_runs(resolved, input.limit, detail=input.detail)
        except GitHubError as e:
            return CIStatusOutput(success=False, result=str(e), workflows=[])
        except GitHubRateLimitError:
            return CIStatusOutput(success=False, result="GitHub rate limit exceeded, please try again later.", workflows=[])
        return CIStatusOutput(
            success=True,
            result=runs,
            workflows=runs,
            resolved_repo=resolved if resolved_by_search else None,
            resolved_by_search=resolved_by_search,
        )


# ============================================================================
# List Issues Skill
# ============================================================================


class ListIssuesInput(SkillInput):
    """Input for list_issues skill"""

    repo: str = ""  # "owner/repo"
    state: str = "open"  # open, closed, all
    labels: list[str] | None = None
    sort: str = "created"  # created, updated, comments
    direction: str = "desc"  # asc, desc
    since: str | None = None  # ISO datetime
    assignee: str | None = None  # login, 'none', '*'
    creator: str | None = None  # login
    milestone: str | None = None  # title, 'none', '*'
    limit: int = 10
    detail: str = "brief"  # brief, normal, full


class ListIssuesOutput(SkillOutput):
    """Output for list_issues skill"""

    issues: list[dict]
    resolved_repo: str | None = None
    resolved_by_search: bool = False


class ListIssuesSkill(Skill[ListIssuesInput, ListIssuesOutput]):
    """List issues in a repository (excludes PRs)"""

    name = "list_issues"
    config_namespace = "github"
    version = "1.0.0"
    description = (
        "List issues (excludes PRs). "
        "detail: brief=title/state/labels; normal +body/assignees; full +reactions/comments. "
        + _GITHUB_BOILERPLATE
    )
    requirements = SkillRequirement(
        repo_types=[RepoType.CODE], min_access=AccessScope.READ, llm_required=False
    )
    side_effect_profile = SideEffectProfile(
        category=SideEffectCategory.READ, external_apis=["github"]
    )

    def __init__(self, github: GitHubClient, session: Session | None = None):
        self.github = github

    async def execute(self, input: ListIssuesInput) -> ListIssuesOutput:
        repo = input.repo or input.repo_id
        if not repo:
            return ListIssuesOutput(success=False, result="repo is required (e.g. 'matrixorigin/matrixone')", issues=[])
        resolved_by_search = isinstance(repo, str) and "/" not in repo
        resolved = self.github.resolve_repo(repo) if resolved_by_search else repo
        try:
            issues = await self.github.list_issues(
                resolved, input.state, input.labels, input.sort, input.direction,
                input.since, input.assignee, input.creator, input.milestone,
                input.limit, input.detail,
            )
        except GitHubError as e:
            return ListIssuesOutput(success=False, result=str(e), issues=[])
        except GitHubRateLimitError:
            return ListIssuesOutput(success=False, result="GitHub rate limit exceeded, please try again later.", issues=[])
        return ListIssuesOutput(
            success=True, result=issues, issues=issues,
            resolved_repo=resolved if resolved_by_search else None,
            resolved_by_search=resolved_by_search,
        )


# ============================================================================
# Get Issue Skill
# ============================================================================


class GetIssueInput(SkillInput):
    """Input for get_issue skill"""

    repo: str = ""  # "owner/repo"
    issue_number: int
    detail: str = "normal"  # brief, normal, full


class GetIssueOutput(SkillOutput):
    """Output for get_issue skill"""

    issue: dict
    resolved_repo: str | None = None
    resolved_by_search: bool = False


class GetIssueSkill(Skill[GetIssueInput, GetIssueOutput]):
    """Get details of a specific issue"""

    name = "get_issue"
    config_namespace = "github"
    version = "1.0.0"
    description = (
        "Get a specific issue by number. "
        "detail: brief=summary; normal +body/assignees/milestone; full +reactions/comments. "
        + _GITHUB_BOILERPLATE
    )
    requirements = SkillRequirement(
        repo_types=[RepoType.CODE], min_access=AccessScope.READ, llm_required=False
    )
    side_effect_profile = SideEffectProfile(
        category=SideEffectCategory.READ, external_apis=["github"]
    )

    def __init__(self, github: GitHubClient, session: Session | None = None):
        self.github = github

    async def execute(self, input: GetIssueInput) -> GetIssueOutput:
        repo = input.repo or input.repo_id
        if not repo:
            return GetIssueOutput(success=False, result="repo is required", issue={})
        resolved_by_search = isinstance(repo, str) and "/" not in repo
        resolved = self.github.resolve_repo(repo) if resolved_by_search else repo
        try:
            issue = await self.github.get_issue(resolved, input.issue_number, input.detail)
        except GitHubError as e:
            return GetIssueOutput(success=False, result=str(e), issue={})
        except GitHubRateLimitError:
            return GetIssueOutput(success=False, result="GitHub rate limit exceeded, please try again later.", issue={})
        return GetIssueOutput(
            success=True, result=issue, issue=issue,
            resolved_repo=resolved if resolved_by_search else None,
            resolved_by_search=resolved_by_search,
        )


# ============================================================================
# Create Issue Skill
# ============================================================================


class CreateIssueInput(SkillInput):
    """Input for create_issue skill"""

    repo: str = ""  # "owner/repo"
    title: str  # non-empty enforced by Pydantic validator
    body: str = ""
    labels: list[str] | None = None
    assignees: list[str] | None = None

    @field_validator("title")
    @classmethod
    def title_must_not_be_empty(cls, v: str) -> str:
        if not v.strip():
            raise ValueError("title must not be empty")
        return v


class CreateIssueOutput(SkillOutput):
    """Output for create_issue skill"""

    issue: dict
    resolved_repo: str | None = None
    resolved_by_search: bool = False


class CreateIssueSkill(Skill[CreateIssueInput, CreateIssueOutput]):
    """Create a new GitHub issue"""

    name = "create_issue"
    config_namespace = "github"
    version = "1.0.0"
    description = (
        "Create a new GitHub issue. Requires title. Optional: body, labels, assignees. "
        + _GITHUB_BOILERPLATE
    )
    requirements = SkillRequirement(
        repo_types=[RepoType.CODE], min_access=AccessScope.WRITE, llm_required=False
    )
    side_effect_profile = SideEffectProfile(
        category=SideEffectCategory.WRITE, external_apis=["github"]
    )

    def __init__(self, github: GitHubClient, session: Session | None = None):
        self.github = github

    async def execute(self, input: CreateIssueInput) -> CreateIssueOutput:
        repo = input.repo or input.repo_id
        if not repo:
            return CreateIssueOutput(success=False, result="repo is required", issue={})
        resolved_by_search = isinstance(repo, str) and "/" not in repo
        resolved = self.github.resolve_repo(repo) if resolved_by_search else repo
        try:
            issue = await self.github.create_issue(
                resolved, input.title, input.body, input.labels, input.assignees
            )
        except GitHubError as e:
            return CreateIssueOutput(success=False, result=str(e), issue={})
        except GitHubRateLimitError:
            return CreateIssueOutput(success=False, result="GitHub rate limit exceeded, please try again later.", issue={})
        return CreateIssueOutput(
            success=True, result=issue, issue=issue,
            resolved_repo=resolved if resolved_by_search else None,
            resolved_by_search=resolved_by_search,
        )


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
    description = (
        "Execute Python code in isolated environment with optional database access. "
        "Use when user asks to run code, query data, or perform calculations. "
        "Set data_access='read' and source_db to query a database."
    )
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
    registry, db_factory, llm=None, github=None, agent_registry=None, chat_loop_factory=None,
    code_executor=None,
):
    """Register all built-in skills.

    Args:
        registry: SkillRegistry instance
        db_factory: Callable that returns a new Session
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
        github = GitHubClient()
    if llm is None:
        llm = LLMClient(db_factory)

    # Load github manifest once — all github skills share it.
    # Stored in skills_registry.manifest so SkillConfigCenter can find it
    # without needing the skills/ directory at runtime.
    import yaml
    from pathlib import Path
    _manifest_path = Path(__file__).parent.parent.parent / "skills" / "github" / "manifest.yaml"
    _github_manifest: dict | None = None
    if _manifest_path.exists():
        try:
            _github_manifest = yaml.safe_load(_manifest_path.read_text())
        except Exception as e:
            logger.warning("Failed to load github manifest: %s", e)

    # Register skills with metadata
    skills = [
        (
            SummarizePRSkill(llm, github),
            "github",
            "pr_management",
            ["summarize", "summary", "pr", "pull request"],
            [],
            8,
            "medium",
            {"scope": "external", "data_source": "external_api", "intent_type": ["analytical", "fetch"], "requires_history": False},
        ),
        (
            ListPRsSkill(github),
            "github",
            "pr_management",
            ["list", "show", "prs", "pull requests"],
            [],
            5,
            "low",
            {"scope": "external", "data_source": "external_api", "intent_type": ["fetch"], "requires_history": False},
        ),
        (
            CIStatusSkill(github),
            "github",
            "ci_cd",
            ["ci", "build", "workflow", "status"],
            [],
            7,
            "low",
            {"scope": "external", "data_source": "external_api", "intent_type": ["fetch"], "requires_history": False},
        ),
        (
            ListIssuesSkill(github),
            "github",
            "issue_management",
            ["issues", "bugs", "feature requests", "open issues"],
            [],
            5,
            "low",
            {"scope": "external", "data_source": "external_api", "intent_type": ["fetch"], "requires_history": False},
        ),
        (
            GetIssueSkill(github),
            "github",
            "issue_management",
            ["issue", "bug", "issue details"],
            [],
            5,
            "low",
            {"scope": "external", "data_source": "external_api", "intent_type": ["fetch"], "requires_history": False},
        ),
        (
            CreateIssueSkill(github),
            "github",
            "issue_management",
            ["create issue", "file bug", "report issue", "new issue"],
            [],
            7,
            "low",
            {"scope": "external", "data_source": "external_api", "intent_type": ["mutate"], "requires_history": False},
        ),
    ]

    for skill, category, subcategory, triggers, dependencies, priority, cost, tags in skills:
        try:
            # Pass github manifest so SkillConfigCenter can find required secrets
            # without needing the skills/ directory on the deployment server.
            skill_manifest = _github_manifest if category == "github" or subcategory in ("pr_management", "ci_cd", "issue_management") else None
            registry.register(
                skill=skill,
                is_active=True,
                category=category,
                subcategory=subcategory,
                triggers=triggers,
                dependencies=dependencies,
                priority=priority,
                cost_estimate=cost,
                manifest=skill_manifest,
                tags=tags,
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
                tags={"scope": "current_session", "data_source": "session_metadata", "intent_type": ["mutate"], "requires_history": False},
            )
            logger.info(f"Registered {exec_skill.name}@{exec_skill.version}")
        except Exception as e:
            logger.warning(f"Failed to register execute_code skill: {e}")

    # Register introspection skill (zero LLM cost, answers agent self-queries)
    try:
        from skills.introspection.skill import IntrospectionSkill
        introspection_skill = IntrospectionSkill(db_factory=db_factory)
        registry.register(
            skill=introspection_skill,
            is_active=True,
            category="system",
            subcategory="introspection",
            triggers=[
                # Chinese: agent self-queries
                "上下文", "多大", "多少轮", "能力", "状态",
                # English: multi-word triggers to avoid false positives
                # (e.g. "token" alone would match "session token in web app")
                "context size", "context window", "token usage",
                "how many turns", "what model", "agent status",
                "my capabilities",
            ],
            dependencies=[],
            priority=8,
            cost_estimate="low",
            tags={"scope": "current_session", "data_source": "session_metadata", "intent_type": ["introspect"], "requires_history": False},
        )
        logger.info(f"Registered {introspection_skill.name}@{introspection_skill.version}")
    except Exception as e:
        logger.warning(f"Failed to register introspection skill: {e}")

    # Register skill config wizard (guided configuration via conversation)
    try:
        from skills.skill_config_wizard.skill import SkillConfigWizardSkill
        wizard_skill = SkillConfigWizardSkill(db_factory=db_factory)
        # config_center injected lazily at execution time via executor
        registry.register(
            skill=wizard_skill,
            is_active=True,
            category="system",
            subcategory="configuration",
            triggers=[
                "configure skill", "set up skill", "skill config",
                "配置", "设置 skill",
            ],
            dependencies=[],
            priority=7,
            cost_estimate="low",
            tags={"scope": "current_session", "data_source": "session_metadata", "intent_type": ["introspect"], "requires_history": False},
        )
        logger.info(f"Registered {wizard_skill.name}@{wizard_skill.version}")
    except Exception as e:
        logger.warning(f"Failed to register skill_config_wizard: {e}")

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
                tags={"scope": "current_session", "data_source": "session_metadata", "intent_type": ["mutate"], "requires_history": False},
            )
            logger.info(f"Registered {delegation_skill.name}@{delegation_skill.version}")
        except Exception as e:
            logger.warning(f"Failed to register delegation skill: {e}")


    # ── Register edge tool metadata ──────────────────────────────
    # Edge tools execute on CLI side but need tags in skills_registry
    # so the unified selector pipeline can pre-filter and rank them.
    # No Skill instance needed — just metadata + tags.
    _edge_tool_metadata = [
        # (name, version, description, category, subcategory, triggers, tags)
        ("reflect", "1.0.0",
         "Diagnose PAST behavior: event trails, skill selection audit, "
         "decision chain evaluation, cross-session history.",
         "diagnostics", "event_analysis",
         ["分析", "决策链", "前一个", "上一个", "之前",
          "analyze", "previous", "decision", "why", "diagnose"],
         {"scope": "historical", "data_source": "event_store",
          "intent_type": ["analytical"], "requires_history": True}),
        ("get_agent_info", "1.0.0",
         "Query CURRENT runtime state: token counts, context window, "
         "session info, available tools.",
         "system", "introspection",
         ["token", "上下文", "context", "session", "tools",
          "model", "状态"],
         {"scope": "current_session", "data_source": "session_metadata",
          "intent_type": ["introspect"], "requires_history": False}),
        ("read_file", "1.0.0",
         "Read file contents from local filesystem.",
         "file_ops", "read",
         ["read", "file", "show", "cat", "view"],
         {"scope": "local", "data_source": "local_filesystem",
          "intent_type": ["fetch"], "requires_history": False}),
        ("write_file", "1.0.0",
         "Create or overwrite a file on local filesystem.",
         "file_ops", "write",
         ["write", "create", "save", "file"],
         {"scope": "local", "data_source": "local_filesystem",
          "intent_type": ["mutate"], "requires_history": False}),
        ("str_replace", "1.0.0",
         "Replace text in an existing file.",
         "file_ops", "write",
         ["replace", "edit", "modify", "change"],
         {"scope": "local", "data_source": "local_filesystem",
          "intent_type": ["mutate"], "requires_history": False}),
        ("list_dir", "1.0.0",
         "List directory contents.",
         "file_ops", "read",
         ["list", "ls", "dir", "directory", "tree"],
         {"scope": "local", "data_source": "local_filesystem",
          "intent_type": ["fetch"], "requires_history": False}),
        ("grep", "1.0.0",
         "Search for text patterns in files.",
         "search", "text",
         ["grep", "search", "find", "pattern"],
         {"scope": "local", "data_source": "local_filesystem",
          "intent_type": ["fetch"], "requires_history": False}),
        ("glob", "1.0.0",
         "Find files matching a glob pattern.",
         "search", "files",
         ["glob", "find", "files", "pattern"],
         {"scope": "local", "data_source": "local_filesystem",
          "intent_type": ["fetch"], "requires_history": False}),
        ("bash", "1.0.0",
         "Execute a shell command.",
         "shell", "execution",
         ["bash", "shell", "run", "execute", "command"],
         {"scope": "local", "data_source": "local_runtime",
          "intent_type": ["mutate"], "requires_history": False}),
        ("git_status", "1.0.0",
         "Show git working tree status.",
         "vcs", "git",
         ["git", "status", "changes", "modified"],
         {"scope": "local", "data_source": "local_vcs",
          "intent_type": ["fetch"], "requires_history": False}),
        ("git_diff", "1.0.0",
         "Show git diff of changes.",
         "vcs", "git",
         ["git", "diff", "changes"],
         {"scope": "local", "data_source": "local_vcs",
          "intent_type": ["fetch"], "requires_history": False}),
        ("git_log", "1.0.0",
         "Show git commit history.",
         "vcs", "git",
         ["git", "log", "history", "commits"],
         {"scope": "cross_session", "data_source": "local_vcs",
          "intent_type": ["analytical", "fetch"],
          "requires_history": False}),
        ("find_skills", "1.0.0",
         "Discover available skills and their capabilities.",
         "system", "discovery",
         ["skills", "capabilities", "what can"],
         {"scope": "current_session", "data_source": "session_metadata",
          "intent_type": ["fetch"], "requires_history": False}),
        ("set_skill_setting", "1.0.0",
         "Configure a skill setting.",
         "system", "configuration",
         ["configure", "setting", "config"],
         {"scope": "current_session", "data_source": "session_metadata",
          "intent_type": ["mutate"], "requires_history": False}),
        ("bind_skill_resource", "1.0.0",
         "Bind a resource to a skill.",
         "system", "configuration",
         ["bind", "resource", "connect"],
         {"scope": "current_session", "data_source": "session_metadata",
          "intent_type": ["mutate"], "requires_history": False}),
        ("validate_skill_config", "1.0.0",
         "Validate skill configuration.",
         "system", "configuration",
         ["validate", "check", "config"],
         {"scope": "current_session", "data_source": "session_metadata",
          "intent_type": ["fetch"], "requires_history": False}),
    ]

    for name, ver, desc, cat, subcat, triggers, tags in _edge_tool_metadata:
        try:
            registry.register_edge_metadata(
                name=name,
                version=ver,
                description=desc,
                category=cat,
                subcategory=subcat,
                triggers=triggers,
                tags=tags,
            )
            logger.info("Registered edge tool metadata: %s", name)
        except Exception as e:
            logger.warning("Failed to register edge tool %s: %s", name, e)
