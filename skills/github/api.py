"""GitHub skill API — typed interface for data access.

Replaces the old GitHubClient. Key changes:
- repo identified by "owner/repo" string, not repo_id: int
- PR cache in platform DB (sk_github_ prefix tables)
- Credentials from platform credential manager
"""

from __future__ import annotations

import asyncio
import time
import uuid
from datetime import datetime, timezone

from github import Auth, Github, GithubException
from sqlalchemy.orm import Session

from core.exceptions import GitHubError, GitHubRateLimitError
from core.logging_config import get_logger
from skills.github.models import SkGithubPRCache, SkGithubRepo

logger = get_logger(__name__)

RATE_LIMIT_THRESHOLD = 10


class GitHubSkillAPI:
    """Typed API for GitHub skill. Users interact through this, not direct SQL."""

    def __init__(
        self,
        token: str | None = None,
        db: Session | None = None,
        base_url: str = "https://api.github.com",
    ):
        self._db = db
        self._base_url = base_url
        self._rl_checked_at: float = 0

        if token:
            self._client = Github(auth=Auth.Token(token=token), base_url=base_url)
        else:
            logger.warning("GitHub token not configured, using unauthenticated access")
            self._client = Github(base_url=base_url)

    # ------------------------------------------------------------------
    # Repo management
    # ------------------------------------------------------------------

    def add_repo(self, owner: str, name: str) -> dict:
        """Register a repository for tracking."""
        if not self._db:
            raise RuntimeError("DB session required for repo management")
        full_name = f"{owner}/{name}"
        existing = self._db.query(SkGithubRepo).filter_by(owner=owner, name=name).one_or_none()
        if existing:
            return {"repo_id": existing.repo_id, "full_name": full_name, "created": False}
        repo = SkGithubRepo(
            repo_id=str(uuid.uuid4()),
            owner=owner,
            name=name,
            full_name=full_name,
        )
        self._db.add(repo)
        self._db.flush()
        return {"repo_id": repo.repo_id, "full_name": full_name, "created": True}

    def list_repos(self) -> list[dict]:
        """List registered repositories."""
        if not self._db:
            return []
        rows = self._db.query(SkGithubRepo).all()
        return [
            {"repo_id": r.repo_id, "full_name": r.full_name, "default_branch": r.default_branch}
            for r in rows
        ]

    # ------------------------------------------------------------------
    # GitHub API helpers
    # ------------------------------------------------------------------

    def resolve_repo(self, repo: str) -> str:
        """Resolve a repo name to 'owner/repo' format.

        If repo already contains '/', return as-is.
        Otherwise search GitHub for the best-matching repository by star count.
        """
        if "/" in repo:
            return repo
        results = self._client.search_repositories(query=repo, sort="stars", order="desc")
        best = next(iter(results), None)
        if best is None:
            raise GitHubError(f"No GitHub repository found for '{repo}'", status_code=404)
        return best.full_name

    # Internal alias used by _get_repo and other methods that already have owner/repo.
    _resolve_repo = resolve_repo

    def _get_repo(self, repo: str):
        """Get PyGithub repo object by 'owner/repo' string."""
        try:
            return self._client.get_repo(repo)
        except GithubException as e:
            if e.status == 404:
                raise GitHubError(f"Repository {repo} not found on GitHub", status_code=404) from e
            elif e.status == 403:
                raise GitHubRateLimitError() from e
            raise GitHubError(
                f"GitHub API error: {e.data.get('message', str(e))}", status_code=e.status
            ) from e

    async def _check_rate_limit(self) -> None:
        """Pre-emptively wait if rate limit is nearly exhausted."""
        now = time.monotonic()
        if now - self._rl_checked_at < 60:
            return
        try:
            rl = self._client.get_rate_limit()
            self._rl_checked_at = now
            core = getattr(rl, "core", None)
            if core and core.remaining < RATE_LIMIT_THRESHOLD:
                reset_ts = core.reset.timestamp() if core.reset else 0
                wait = max(0, reset_ts - time.time()) + 1
                if wait > 0:
                    logger.warning(
                        f"GitHub rate limit low ({core.remaining}/{core.limit}), "
                        f"waiting {wait:.0f}s"
                    )
                    await asyncio.sleep(min(wait, 60))
        except Exception as exc:
            logger.debug(f"Rate limit check failed (non-fatal): {exc}")

    # ------------------------------------------------------------------
    # ------------------------------------------------------------------
    # Shared helpers (used by PR, issue, and workflow methods)
    # ------------------------------------------------------------------

    # Normalize GitHub conclusion values to a fixed vocabulary.
    # "neutral" means "check passed with warnings" — closer to success than skipped.
    # "timed_out" is a failure mode.
    # "stale" means the run was superseded — treat as cancelled.
    _CONCLUSION_MAP: dict[str | None, str] = {
        "success": "success",
        "failure": "failure",
        "cancelled": "cancelled",
        "skipped": "skipped",
        "timed_out": "failure",
        "neutral": "success",  # passed with warnings, not skipped
        "stale": "cancelled",
        None: "pending",
        "in_progress": "pending",
        "queued": "pending",
        "waiting": "pending",
        "requested": "pending",
        "action_required": "pending",
    }

    @staticmethod
    def _fmt_ts(dt: datetime | None) -> str | None:
        """Format datetime to 'YYYY-MM-DD HH:MM' UTC (LLM-friendly). Returns None if dt is None."""
        if dt is None:
            return None
        return dt.strftime("%Y-%m-%d %H:%M")

    @staticmethod
    def _trunc(text: str | None, limit: int) -> str | None:
        """Truncate text to limit chars, appending '[truncated]' if cut."""
        if not text:
            return text
        return text[:limit] + " [truncated]" if len(text) > limit else text

    # ------------------------------------------------------------------
    # PR operations
    # ------------------------------------------------------------------

    async def get_pr(self, repo: str, pr_number: int, detail: str = "normal") -> dict:
        """Fetch PR details.

        detail levels:
          brief    — number, title (80), author, state, created_at, ci_conclusion, changed_files
          normal   — brief + body (200), labels, reviewers, additions/deletions
          detailed — normal + key changed files (top 10), review_comments count, merge status
          full     — detailed + complete body (2000), all review comments (200 each)

        ci_conclusion uses GitHub Checks API (check_runs on the head commit), which aggregates
        all CI systems (Actions, CircleCI, etc.) at the PR level. This is intentionally different
        from ci_status which lists workflow runs at the repo level — they serve different purposes.
        """
        if detail not in self._VALID_DETAIL_LEVELS:
            raise ValueError(
                f"Invalid detail level {detail!r}, must be one of: {', '.join(sorted(self._VALID_DETAIL_LEVELS))}"
            )
        await self._check_rate_limit()
        try:
            gh_repo = self._get_repo(repo)
            pr = gh_repo.get_pull(pr_number)

            # Get CI conclusion from check runs on the PR head commit.
            # Only non-fatal errors are suppressed (e.g. no checks configured);
            # rate limit errors are re-raised.
            ci_conclusion: str = "pending"
            try:
                commit = gh_repo.get_commit(pr.head.sha)
                checks = list(commit.get_check_runs())
                if checks:
                    conclusions = {
                        self._CONCLUSION_MAP.get(c.conclusion, "unknown") for c in checks
                    }
                    if "failure" in conclusions:
                        ci_conclusion = "failure"
                    elif conclusions <= {"success", "skipped"}:
                        # All checks passed or were intentionally skipped
                        ci_conclusion = "success"
                    else:
                        ci_conclusion = "pending"
            except (GitHubError, GitHubRateLimitError):
                raise
            except Exception:
                pass  # No checks configured or insufficient permissions — leave as "pending"

            result: dict = {
                "number": pr.number,
                "title": self._trunc(pr.title, 80),
                "author": pr.user.login,
                "state": pr.state,
                "created_at": self._fmt_ts(pr.created_at),
                "ci_conclusion": ci_conclusion,
                "changed_files": pr.changed_files,
                "html_url": pr.html_url,
            }
            if detail == "brief":
                return result

            # normal+
            result.update(
                {
                    "body": self._trunc(pr.body or "", 200),
                    "labels": [lb.name for lb in pr.labels],
                    "reviewers": [r.login for r in pr.requested_reviewers],
                    "additions": pr.additions,
                    "deletions": pr.deletions,
                    "updated_at": self._fmt_ts(pr.updated_at),
                }
            )
            if detail == "normal":
                return result

            # detailed+
            files = list(pr.get_files())
            files_sorted = sorted(files, key=lambda f: f.changes, reverse=True)[:10]
            result.update(
                {
                    "key_files": [
                        {"filename": f.filename, "changes": f.changes, "status": f.status}
                        for f in files_sorted
                    ],
                    "review_comments": pr.review_comments,
                    "mergeable": pr.mergeable,
                    "merge_state": getattr(pr, "mergeable_state", None),
                }
            )
            if detail == "detailed":
                return result

            # full — cache here only: this is the most complete representation
            result["body"] = self._trunc(pr.body or "", 2000)
            try:
                reviews = []
                for rv in pr.get_reviews():
                    reviews.append(
                        {
                            "user": rv.user.login,
                            "state": rv.state,
                            "body": self._trunc(rv.body, 200),
                            "submitted_at": self._fmt_ts(rv.submitted_at),
                        }
                    )
                result["reviews"] = reviews
            except (GitHubError, GitHubRateLimitError):
                raise
            except Exception:
                result["reviews"] = []
            self._cache_pr(repo, result)
            return result
        except (GitHubError, GitHubRateLimitError):
            raise
        except GithubException as e:
            if e.status == 404:
                raise GitHubError(f"PR #{pr_number} not found", status_code=404) from e
            elif e.status == 403:
                raise GitHubRateLimitError() from e
            raise GitHubError(
                f"Failed to fetch PR: {e.data.get('message', str(e))}", status_code=e.status
            ) from e

    async def get_pr_diff(self, repo: str, pr_number: int) -> str:
        """Fetch PR diff."""
        await self._check_rate_limit()
        try:
            gh_repo = self._get_repo(repo)
            pr = gh_repo.get_pull(pr_number)
            parts = []
            for f in pr.get_files():
                if f.patch:
                    parts.append(f"diff --git a/{f.filename} b/{f.filename}")
                    parts.append(f.patch)
            return "\n".join(parts)
        except (GitHubError, GitHubRateLimitError):
            raise
        except GithubException as e:
            if e.status == 403:
                raise GitHubRateLimitError() from e
            raise GitHubError(
                f"Failed to fetch diff: {e.data.get('message', str(e))}", status_code=e.status
            ) from e

    _LIST_PRS_DETAIL_LEVELS = frozenset({"brief", "normal"})

    async def list_prs(
        self, repo: str, state: str = "open", limit: int = 10, detail: str = "brief"
    ) -> list[dict]:
        """List PRs in a repo.

        detail levels (list context — use get_pr for deeper detail on a single PR):
          brief  — number, title (80), author, state, created_at, html_url
          normal — brief + body (200), labels, reviewers, changed_files

        Note: ci_conclusion is intentionally omitted from list_prs — fetching it requires
        one extra API call per PR (get_check_runs), which is too expensive for a list.
        Use get_pr(detail='brief') for a single PR with ci_conclusion.
        """
        if detail not in self._LIST_PRS_DETAIL_LEVELS:
            raise ValueError(
                f"Invalid detail level {detail!r} for list_prs, must be one of: brief, normal"
            )
        await self._check_rate_limit()
        try:
            gh_repo = self._get_repo(repo)
            prs = gh_repo.get_pulls(state=state)
            result = []
            for i, pr in enumerate(prs):
                if i >= limit:
                    break
                item: dict = {
                    "number": pr.number,
                    "title": self._trunc(pr.title, 80),
                    "author": pr.user.login,
                    "state": pr.state,
                    "created_at": self._fmt_ts(pr.created_at),
                    "html_url": pr.html_url,
                }
                if detail != "brief":
                    item.update(
                        {
                            "body": self._trunc(pr.body or "", 200),
                            "labels": [lb.name for lb in pr.labels],
                            "reviewers": [r.login for r in pr.requested_reviewers],
                            "changed_files": pr.changed_files,
                        }
                    )
                result.append(item)
            return result
        except (GitHubError, GitHubRateLimitError):
            raise
        except GithubException as e:
            if e.status == 403:
                raise GitHubRateLimitError() from e
            raise GitHubError(
                f"Failed to list PRs: {e.data.get('message', str(e))}", status_code=e.status
            ) from e

    async def get_pr_checks(self, repo: str, pr_number: int) -> dict:
        """Get CI/check run status for a specific PR."""
        await self._check_rate_limit()
        try:
            gh_repo = self._get_repo(repo)
            pr = gh_repo.get_pull(pr_number)
            commit = gh_repo.get_commit(pr.head.sha)
            check_runs = commit.get_check_runs()
            runs = []
            for cr in check_runs:
                runs.append(
                    {
                        "name": cr.name,
                        "status": cr.status,
                        "conclusion": cr.conclusion,
                        "html_url": cr.html_url,
                        "started_at": cr.started_at.isoformat() if cr.started_at else None,
                        "completed_at": cr.completed_at.isoformat() if cr.completed_at else None,
                    }
                )
            # Overall status
            overall = "success"
            for r in runs:
                if r["conclusion"] == "failure":
                    overall = "failure"
                    break
                if r["status"] != "completed":
                    overall = "pending"
            return {"pr_number": pr_number, "overall": overall, "check_runs": runs}
        except (GitHubError, GitHubRateLimitError):
            raise
        except GithubException as e:
            if e.status == 404:
                raise GitHubError(f"PR #{pr_number} not found", status_code=404) from e
            elif e.status == 403:
                raise GitHubRateLimitError() from e
            raise GitHubError(
                f"Failed to get check runs: {e.data.get('message', str(e))}", status_code=e.status
            ) from e

    # ------------------------------------------------------------------
    # Issue operations
    # ------------------------------------------------------------------

    _VALID_DETAIL_LEVELS = frozenset({"brief", "normal", "detailed", "full"})

    def _format_issue(self, issue, detail: str = "normal") -> dict:
        """Format an issue object at the requested detail level.

        detail levels:
          brief    — number, title (80), state, user, labels, created_at, html_url
          normal   — brief + body (200), assignees, comment_count, milestone
          detailed — normal + recent 3 comments (200 each), linked PRs
          full     — detailed + complete body (2000), all comments (200 each, up to 20)
        """
        if detail not in GitHubSkillAPI._VALID_DETAIL_LEVELS:
            raise ValueError(
                f"Invalid detail level {detail!r}, must be one of: brief, normal, detailed, full"
            )
        d: dict = {
            "number": issue.number,
            "title": self._trunc(issue.title, 80),
            "state": issue.state,
            "user": issue.user.login,
            "labels": [lb.name for lb in issue.labels],
            "created_at": self._fmt_ts(issue.created_at),
            "html_url": issue.html_url,
        }
        if detail == "brief":
            return d

        # normal+
        d.update(
            {
                "body": self._trunc(issue.body or "", 200),
                "assignees": [a.login for a in issue.assignees],
                "comment_count": issue.comments,
                "milestone": issue.milestone.title if issue.milestone else None,
                "updated_at": self._fmt_ts(issue.updated_at),
            }
        )
        if detail == "normal":
            return d

        # detailed+ — fetch comments once; limit depends on detail level
        # (full reuses the same fetch, just with a higher cap)
        comment_limit = 3 if detail == "detailed" else 20
        try:
            comments = []
            for c in issue.get_comments():
                if len(comments) >= comment_limit:
                    break
                comments.append(
                    {
                        "user": c.user.login,
                        "created_at": self._fmt_ts(c.created_at),
                        "body": self._trunc(c.body, 200),
                    }
                )
            d["recent_comments"] = comments
        except Exception as exc:
            logger.warning("Failed to fetch comments for issue #%s: %s", issue.number, exc)
            d["recent_comments"] = []
        if detail == "detailed":
            return d

        # full — expand body and add metadata
        d["body"] = self._trunc(issue.body or "", 2000)
        d.update(
            {
                "reactions": issue.reactions if isinstance(issue.reactions, dict) else {},
                "closed_at": self._fmt_ts(issue.closed_at),
                "closed_by": issue.closed_by.login if issue.closed_by else None,
                "locked": issue.locked,
            }
        )
        return d

    async def list_issues(
        self,
        repo: str,
        state: str = "open",
        labels: list[str] | None = None,
        sort: str = "created",
        direction: str = "desc",
        since: str | None = None,
        assignee: str | None = None,
        creator: str | None = None,
        milestone: str | None = None,
        limit: int = 10,
        detail: str = "brief",
    ) -> list[dict]:
        """List issues in a repo (excludes pull requests).

        Args:
            state: open, closed, all
            labels: filter by label names
            sort: created, updated, comments
            direction: asc, desc
            since: ISO datetime — only issues updated after this time
            assignee: filter by assignee login, or 'none'/'*'
            creator: filter by creator login
            milestone: filter by milestone title, or 'none'/'*'
            limit: max results
            detail: brief, normal, full
        """
        await self._check_rate_limit()
        try:
            gh_repo = self._get_repo(repo)
            kwargs: dict = {"state": state, "sort": sort, "direction": direction}
            if labels:
                kwargs["labels"] = labels
            if since:
                from datetime import datetime as dt

                try:
                    kwargs["since"] = dt.fromisoformat(since)
                except ValueError as exc:
                    raise GitHubError(
                        f"Invalid ISO datetime for 'since': {since!r}", status_code=422
                    ) from exc
            if assignee:
                kwargs["assignee"] = assignee
            if creator:
                kwargs["creator"] = creator
            if milestone:
                kwargs["milestone"] = milestone
            # Remove None values from optional filters (state/sort/direction are always
            # non-None from defaults, but this keeps the pattern uniform if defaults change).
            kwargs = {k: v for k, v in kwargs.items() if v is not None}
            issues = gh_repo.get_issues(**kwargs)
            result = []
            for issue in issues:
                if issue.pull_request is not None:
                    continue  # GitHub API returns PRs as issues — no server-side filter
                result.append(self._format_issue(issue, detail))
                if len(result) >= limit:
                    break
            return result
        except (GitHubError, GitHubRateLimitError):
            raise
        except GithubException as e:
            if e.status == 403:
                raise GitHubRateLimitError() from e
            raise GitHubError(
                f"Failed to list issues: {e.data.get('message', str(e))}", status_code=e.status
            ) from e

    async def get_issue(self, repo: str, issue_number: int, detail: str = "normal") -> dict:
        """Fetch a single issue by number.

        Args:
            detail: brief, normal, full (full includes reactions + recent comments)
        """
        await self._check_rate_limit()
        try:
            gh_repo = self._get_repo(repo)
            issue = gh_repo.get_issue(issue_number)
            return self._format_issue(issue, detail)
        except (GitHubError, GitHubRateLimitError):
            raise
        except GithubException as e:
            if e.status == 404:
                raise GitHubError(f"Issue #{issue_number} not found", status_code=404) from e
            elif e.status == 403:
                raise GitHubRateLimitError() from e
            raise GitHubError(
                f"Failed to fetch issue: {e.data.get('message', str(e))}", status_code=e.status
            ) from e

    async def create_issue(
        self,
        repo: str,
        title: str,
        body: str = "",
        labels: list[str] | None = None,
        assignees: list[str] | None = None,
    ) -> dict:
        """Create a new issue."""
        await self._check_rate_limit()
        try:
            gh_repo = self._get_repo(repo)
            kwargs: dict = {"title": title}
            if body:
                kwargs["body"] = body
            if labels:
                kwargs["labels"] = labels
            if assignees:
                kwargs["assignees"] = assignees
            issue = gh_repo.create_issue(**kwargs)
            return self._format_issue(issue, "normal")
        except (GitHubError, GitHubRateLimitError):
            raise
        except GithubException as e:
            if e.status == 403:
                raise GitHubRateLimitError() from e
            raise GitHubError(
                f"Failed to create issue: {e.data.get('message', str(e))}", status_code=e.status
            ) from e

    # ------------------------------------------------------------------
    # Workflow runs
    # ------------------------------------------------------------------

    async def list_wf_runs(self, repo: str, limit: int = 5, detail: str = "brief") -> list[dict]:
        """List workflow runs with PR/branch context.

        detail levels:
          brief    — workflow, conclusion, branch/PR, actor, created_at, url
          normal   — brief + PR title, commit message, duration_seconds
          detailed — normal + per-job status list, failed job names
          full     — detailed + failed job annotations/step errors (extra API calls)
        """
        await self._check_rate_limit()
        try:
            gh_repo = self._get_repo(repo)
            runs = gh_repo.get_workflow_runs()
            result = []
            for i, run in enumerate(runs):
                if i >= limit:
                    break

                # Resolve PR context — a run may be triggered by a PR
                pr_number: int | None = None
                pr_title: str | None = None
                if run.pull_requests:
                    pr = run.pull_requests[0]
                    pr_number = pr.number
                    if detail in ("normal", "detailed", "full"):
                        try:
                            pr_obj = gh_repo.get_pull(pr.number)
                            pr_title = pr_obj.title
                        except Exception:
                            pass

                item: dict = {
                    "workflow": run.name or "Unnamed",
                    "conclusion": self._CONCLUSION_MAP.get(run.conclusion, "unknown"),
                    "branch": run.head_branch,
                    "pr_number": pr_number,
                    "actor": run.actor.login if run.actor else None,
                    "triggered_at": self._fmt_ts(run.created_at),
                    "url": run.html_url,
                }

                if detail in ("normal", "detailed", "full"):
                    item["pr_title"] = pr_title
                    item["commit_message"] = self._trunc(
                        run.head_commit.message.split("\n")[0] if run.head_commit else None, 80
                    )
                    if run.updated_at and run.created_at:
                        item["duration_seconds"] = int(
                            (run.updated_at - run.created_at).total_seconds()
                        )

                if detail in ("detailed", "full"):
                    try:
                        jobs = list(run.jobs())
                        job_list = []
                        failed_jobs = []
                        for job in jobs:
                            job_conclusion = self._CONCLUSION_MAP.get(job.conclusion, "unknown")
                            job_list.append(
                                {
                                    "name": job.name,
                                    "conclusion": job_conclusion,
                                }
                            )
                            if job_conclusion == "failure":
                                failed_jobs.append(job.name)
                        item["jobs"] = job_list
                        item["failed_jobs"] = failed_jobs

                        if detail == "full":
                            failed_steps = []
                            for job in jobs:
                                if job.conclusion != "failure":
                                    continue
                                for step in job.steps:
                                    if step.conclusion == "failure":
                                        failed_steps.append(
                                            {
                                                "job": job.name,
                                                "step": step.name,
                                                "number": step.number,
                                            }
                                        )
                            item["failed_steps"] = failed_steps
                    except (GitHubError, GitHubRateLimitError):
                        raise  # propagate rate-limit / auth errors
                    except Exception as e:
                        # Non-fatal: job details unavailable (e.g. permissions).
                        # Surface the error so the LLM can report it honestly.
                        item["jobs_error"] = str(e)

                result.append(item)
            return result
        except (GitHubError, GitHubRateLimitError):
            raise
        except GithubException as e:
            if e.status == 403:
                raise GitHubRateLimitError() from e
            raise GitHubError(
                f"Failed to list workflows: {e.data.get('message', str(e))}", status_code=e.status
            ) from e

    # ------------------------------------------------------------------
    # Rate limit
    # ------------------------------------------------------------------

    def get_rate_limit(self) -> dict:
        """Get current rate limit status."""
        try:
            rl = self._client.get_rate_limit()
            core = getattr(rl, "core", None)
            if not core:
                return {"limit": 0, "remaining": 0, "reset": None}
            return {
                "limit": core.limit,
                "remaining": core.remaining,
                "reset": core.reset.isoformat() if core.reset else None,
            }
        except Exception as e:
            logger.error(f"Failed to get rate limit: {e}")
            return {"limit": 0, "remaining": 0, "reset": None}

    # ------------------------------------------------------------------
    # Cache helpers
    # ------------------------------------------------------------------

    def _cache_pr(self, repo: str, pr_data: dict) -> None:
        """Upsert PR data into cache (if DB session available)."""
        if not self._db:
            return
        try:
            existing = (
                self._db.query(SkGithubPRCache)
                .filter_by(repo_full_name=repo, pr_number=pr_data["number"])
                .one_or_none()
            )
            now = datetime.now(timezone.utc)
            if existing:
                existing.title = pr_data.get("title")
                existing.state = pr_data.get("state")
                existing.author = pr_data.get("user")
                existing.data = pr_data
                existing.fetched_at = now
            else:
                self._db.add(
                    SkGithubPRCache(
                        cache_id=str(uuid.uuid4()),
                        repo_full_name=repo,
                        pr_number=pr_data["number"],
                        title=pr_data.get("title"),
                        state=pr_data.get("state"),
                        author=pr_data.get("user"),
                        data=pr_data,
                        fetched_at=now,
                    )
                )
            self._db.flush()
        except Exception as e:
            logger.debug(f"PR cache upsert failed (non-fatal): {e}")
