"""GitHub skill actions — registered as tools for the agent.

Each action wraps a GitHubSkillAPI method with typed input/output.
"""

from __future__ import annotations

from pydantic import BaseModel, field_validator

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
from skills.github.api import GitHubSkillAPI

# ------------------------------------------------------------------
# List PRs
# ------------------------------------------------------------------


class ListPRsInput(SkillInput):
    repo: str  # "owner/repo"
    state: str = "open"
    limit: int = 10


class ListPRsOutput(SkillOutput):
    prs: list[dict]


class ListPRsAction(Skill[ListPRsInput, ListPRsOutput]):
    name = "github_list_prs"
    version = "1.0.0"
    description = (
        "List pull requests in a GitHub repository. "
        "Use when user asks about PRs, recent changes, or what's being worked on. "
        "Requires repo in 'owner/repo' format. "
        "Use state='open' for active PRs, 'closed' for merged/closed, 'all' for both."
    )
    requirements = SkillRequirement(
        repo_types=[RepoType.CODE], min_access=AccessScope.READ, llm_required=False
    )
    side_effect_profile = SideEffectProfile(
        category=SideEffectCategory.READ, external_apis=["github"]
    )

    def __init__(self, api: GitHubSkillAPI):
        self._api = api

    async def execute(self, input: ListPRsInput) -> ListPRsOutput:
        prs = await self._api.list_prs(input.repo, input.state, input.limit)
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


# ------------------------------------------------------------------
# Get PR Checks
# ------------------------------------------------------------------


class GetPRChecksInput(SkillInput):
    repo: str  # "owner/repo"
    pr_number: int


class GetPRChecksOutput(SkillOutput):
    overall: str  # success / failure / pending
    check_runs: list[dict]


class GetPRChecksAction(Skill[GetPRChecksInput, GetPRChecksOutput]):
    name = "github_get_pr_checks"
    version = "1.0.0"
    description = (
        "Get CI/check run status for a specific PR — shows individual check results "
        "(pass/fail/pending) and overall status. Use when user asks about a PR's CI status "
        "or why a PR can't be merged. For repo-wide CI status, use ci_status instead."
    )
    requirements = SkillRequirement(
        repo_types=[RepoType.CODE, RepoType.CI], min_access=AccessScope.READ, llm_required=False
    )
    side_effect_profile = SideEffectProfile(
        category=SideEffectCategory.READ, external_apis=["github"]
    )

    def __init__(self, api: GitHubSkillAPI):
        self._api = api

    async def execute(self, input: GetPRChecksInput) -> GetPRChecksOutput:
        data = await self._api.get_pr_checks(input.repo, input.pr_number)
        return GetPRChecksOutput(
            success=True,
            result=data,
            overall=data["overall"],
            check_runs=data["check_runs"],
        )


# ------------------------------------------------------------------
# Summarize PR (requires LLM)
# ------------------------------------------------------------------


class SummarizePRInput(SkillInput):
    repo: str  # "owner/repo"
    pr_number: int
    include_diff: bool = True


class SummarizePROutput(SkillOutput):
    summary: str
    files_changed: int
    additions: int
    deletions: int


class SummarizePRAction(Skill[SummarizePRInput, SummarizePROutput]):
    name = "github_summarize_pr"
    version = "1.0.0"
    description = (
        "Summarize a specific GitHub PR using LLM analysis — includes diff review, "
        "change description, and impact assessment. Use when user asks to review, "
        "explain, or understand a specific PR. Requires repo ('owner/repo') and pr_number."
    )
    requirements = SkillRequirement(
        repo_types=[RepoType.CODE], min_access=AccessScope.READ, llm_required=True
    )
    side_effect_profile = SideEffectProfile(
        category=SideEffectCategory.READ, external_apis=["github", "llm"]
    )

    def __init__(self, api: GitHubSkillAPI, llm):
        self._api = api
        self._llm = llm

    async def execute(self, input: SummarizePRInput) -> SummarizePROutput:
        pr = await self._api.get_pr(input.repo, input.pr_number)
        prompt = (
            f"Summarize this PR concisely:\n\n"
            f"Title: {pr['title']}\n"
            f"Description: {pr['body']}\n"
            f"Files changed: {pr['files_changed']}\n"
        )
        if input.include_diff:
            diff = await self._api.get_pr_diff(input.repo, input.pr_number)
            prompt += f"\nDiff (first 5000 chars):\n{diff[:5000]}"

        response = await self._llm.chat(
            messages=[{"role": "user", "content": prompt}],
            user_id=input.user_id,
            session_id=input.session_id,
            metadata={"skill": self.name, "pr_number": input.pr_number},
        )
        return SummarizePROutput(
            success=True,
            result=response.content,
            summary=response.content,
            files_changed=pr["files_changed"],
            additions=pr["additions"],
            deletions=pr["deletions"],
            cost=response.cost_usd,
        )


# ------------------------------------------------------------------
# CI Status (workflow runs)
# ------------------------------------------------------------------


class CIStatusInput(SkillInput):
    repo: str  # "owner/repo"
    limit: int = 5
    detail: str = "brief"  # brief, normal, detailed, full


class WorkflowRun(BaseModel):
    """Fixed-schema workflow run — every field always present."""

    workflow: str
    conclusion: str  # success / failure / pending / skipped / cancelled / unknown
    branch: str | None = None
    pr_number: int | None = None
    actor: str | None = None
    triggered_at: str | None = None  # YYYY-MM-DD HH:MM
    url: str | None = None


class CIStatusOutput(SkillOutput):
    workflows: list[WorkflowRun]


# ------------------------------------------------------------------
# List Issues
# ------------------------------------------------------------------


class ListIssuesInput(SkillInput):
    repo: str  # "owner/repo"
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
    issues: list[dict]


class ListIssuesAction(Skill[ListIssuesInput, ListIssuesOutput]):
    name = "github_list_issues"
    version = "1.0.0"
    description = (
        "List issues in a GitHub repository (excludes pull requests). "
        "Use when user asks about bugs, feature requests, or open issues. "
        "Requires repo in 'owner/repo' format. "
        "Filters: state (open/closed/all), labels, assignee, creator, milestone, since (ISO datetime). "
        "Sort: created/updated/comments, direction: asc/desc. "
        "Detail: 'brief' for lists, 'normal' adds body/assignees, 'full' adds reactions/comments."
    )
    requirements = SkillRequirement(
        repo_types=[RepoType.CODE], min_access=AccessScope.READ, llm_required=False
    )
    side_effect_profile = SideEffectProfile(
        category=SideEffectCategory.READ, external_apis=["github"]
    )

    def __init__(self, api: GitHubSkillAPI):
        self._api = api

    async def execute(self, input: ListIssuesInput) -> ListIssuesOutput:
        issues = await self._api.list_issues(
            input.repo, input.state, input.labels, input.sort, input.direction,
            input.since, input.assignee, input.creator, input.milestone,
            input.limit, input.detail,
        )
        return ListIssuesOutput(success=True, result=issues, issues=issues)


# ------------------------------------------------------------------
# Get Issue
# ------------------------------------------------------------------


class GetIssueInput(SkillInput):
    repo: str  # "owner/repo"
    issue_number: int
    detail: str = "normal"  # brief, normal, full


class GetIssueOutput(SkillOutput):
    issue: dict


class GetIssueAction(Skill[GetIssueInput, GetIssueOutput]):
    name = "github_get_issue"
    version = "1.0.0"
    description = (
        "Get details of a specific GitHub issue by number. "
        "Detail: 'brief' for summary, 'normal' (default) adds body/assignees/milestone, "
        "'full' adds reactions, closed_by, and recent comments. "
        "Use when user asks about a specific issue."
    )
    requirements = SkillRequirement(
        repo_types=[RepoType.CODE], min_access=AccessScope.READ, llm_required=False
    )
    side_effect_profile = SideEffectProfile(
        category=SideEffectCategory.READ, external_apis=["github"]
    )

    def __init__(self, api: GitHubSkillAPI):
        self._api = api

    async def execute(self, input: GetIssueInput) -> GetIssueOutput:
        issue = await self._api.get_issue(input.repo, input.issue_number, input.detail)
        return GetIssueOutput(success=True, result=issue, issue=issue)


# ------------------------------------------------------------------
# Create Issue
# ------------------------------------------------------------------


class CreateIssueInput(SkillInput):
    repo: str  # "owner/repo"
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
    issue: dict


class CreateIssueAction(Skill[CreateIssueInput, CreateIssueOutput]):
    name = "github_create_issue"
    version = "1.0.0"
    description = (
        "Create a new GitHub issue. Use when user asks to file a bug report, "
        "feature request, or any new issue. Requires repo ('owner/repo') and title. "
        "Optionally set body, labels, and assignees."
    )
    requirements = SkillRequirement(
        repo_types=[RepoType.CODE], min_access=AccessScope.WRITE, llm_required=False
    )
    side_effect_profile = SideEffectProfile(
        category=SideEffectCategory.WRITE, external_apis=["github"]
    )

    def __init__(self, api: GitHubSkillAPI):
        self._api = api

    async def execute(self, input: CreateIssueInput) -> CreateIssueOutput:
        issue = await self._api.create_issue(
            input.repo, input.title, input.body, input.labels, input.assignees
        )
        return CreateIssueOutput(success=True, result=issue, issue=issue)


# ------------------------------------------------------------------
# CI Status (workflow runs)
# ------------------------------------------------------------------


class CIStatusAction(Skill[CIStatusInput, CIStatusOutput]):
    name = "github_ci_status"
    version = "1.0.0"
    description = (
        "Check CI/CD workflow run status in a GitHub repository — shows recent workflow runs "
        "with pass/fail/pending status. Use when user asks about build status or CI failures. "
        "repo can be 'owner/repo' or just a project name (auto-resolved by star count). "
        "detail: 'brief' (default) = workflow/conclusion/branch/actor/triggered_at; "
        "'normal' adds PR title + commit message + duration; "
        "'detailed' adds per-job status + failed job names; "
        "'full' adds failed step annotations. "
        "If resolved_by_search=True, tell user which repo was used. "
        "If result is an empty list, it means the repository has NO CI workflows configured — "
        "tell the user directly, do NOT retry with other tools. "
        "For checking CI on a specific PR, use get_pr_checks instead."
    )
    requirements = SkillRequirement(
        repo_types=[RepoType.CI, RepoType.CODE], min_access=AccessScope.READ, llm_required=False
    )
    side_effect_profile = SideEffectProfile(
        category=SideEffectCategory.READ, external_apis=["github"]
    )

    def __init__(self, api: GitHubSkillAPI):
        self._api = api

    async def execute(self, input: CIStatusInput) -> CIStatusOutput:
        runs = await self._api.list_wf_runs(input.repo, input.limit, input.detail)
        workflows = [
            WorkflowRun(
                workflow=r["workflow"],
                conclusion=r["conclusion"],
                branch=r.get("branch"),
                pr_number=r.get("pr_number"),
                actor=r.get("actor"),
                triggered_at=r.get("triggered_at"),
                url=r.get("url"),
            )
            for r in runs
        ]
        # Authoritative guidance: empty list means no CI configured.
        # Injected as system message so LLM cannot ignore it.
        guidance = None
        if not workflows:
            guidance = (
                "No CI workflows found for this repository. "
                "Do NOT retry with bash, curl, or any other tool."
            )
        return CIStatusOutput(
            success=True, result=workflows, workflows=workflows, guidance=guidance,
            user_message="No GitHub Actions CI workflows are configured for this repository." if not workflows else None,
        )
