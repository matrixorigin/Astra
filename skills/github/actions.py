"""GitHub skill actions — registered as tools for the agent.

Each action wraps a GitHubSkillAPI method with typed input/output.
"""

from __future__ import annotations

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
    description = "List open/closed PRs in a GitHub repository"
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
    description = "Get CI/check run status for a specific PR"
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
    description = "Summarize a GitHub PR with LLM analysis"
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


class CIStatusOutput(SkillOutput):
    workflows: list[dict]


class CIStatusAction(Skill[CIStatusInput, CIStatusOutput]):
    name = "github_ci_status"
    version = "1.0.0"
    description = "Check CI workflow status in a GitHub repository"
    requirements = SkillRequirement(
        repo_types=[RepoType.CI, RepoType.CODE], min_access=AccessScope.READ, llm_required=False
    )
    side_effect_profile = SideEffectProfile(
        category=SideEffectCategory.READ, external_apis=["github"]
    )

    def __init__(self, api: GitHubSkillAPI):
        self._api = api

    async def execute(self, input: CIStatusInput) -> CIStatusOutput:
        runs = await self._api.list_wf_runs(input.repo, input.limit)
        workflows = [
            {
                "workflow": r["name"],
                "status": r["status"],
                "conclusion": r["conclusion"],
                "url": r["html_url"],
                "created_at": r["created_at"],
            }
            for r in runs
        ]
        return CIStatusOutput(success=True, result=workflows, workflows=workflows)
