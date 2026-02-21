"""GitHub skill tables — platform DB with sk_github_ prefix."""

from sqlalchemy import Column, DateTime, Index, Integer, JSON, String, Text, UniqueConstraint
from sqlalchemy.sql import func

from api.models import Base


class SkGithubRepo(Base):
    """Registered GitHub repositories for a user."""

    __tablename__ = "sk_github_repos"
    __table_args__ = (
        UniqueConstraint("owner", "name", name="uq_sk_github_repo_owner_name"),
    )

    repo_id = Column(String(36), primary_key=True)
    owner = Column(String(100), nullable=False)
    name = Column(String(100), nullable=False)
    full_name = Column(String(200), nullable=False)
    default_branch = Column(String(100), default="main")
    created_at = Column(DateTime, default=func.now(), nullable=False)


class SkGithubPRCache(Base):
    """Cached PR data — fetched from GitHub API, stored locally."""

    __tablename__ = "sk_github_pr_cache"
    __table_args__ = (
        UniqueConstraint("repo_full_name", "pr_number", name="uq_sk_github_pr_repo_pr"),
        Index("ix_sk_github_pr_cache_repo_state", "repo_full_name", "state"),
    )

    cache_id = Column(String(36), primary_key=True)
    repo_full_name = Column(String(200), nullable=False)
    pr_number = Column(Integer, nullable=False)
    title = Column(String(500))
    state = Column(String(20))  # open / closed / merged
    author = Column(String(100))
    ci_status = Column(String(20))  # success / failure / pending / None
    ci_conclusion = Column(String(20))
    data = Column(JSON)  # full PR payload
    fetched_at = Column(DateTime, default=func.now(), nullable=False)
