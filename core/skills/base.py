"""Skill framework for mo-dev-agent.

Skills are first-class citizens with versioning, declarative requirements,
and full lifecycle management.
"""

from abc import ABC, abstractmethod
from typing import Any, Optional
from pydantic import BaseModel
from enum import Enum


class RepoType(str, Enum):
    """Repository types"""

    CODE = "code"
    CI = "ci"
    TESTER = "tester"
    DOCS = "docs"


class AccessScope(str, Enum):
    """Access scopes for repositories"""

    READ = "read"
    WRITE = "write"
    ADMIN = "admin"


class SkillRequirement(BaseModel):
    """What a skill needs to run"""

    repo_types: list[RepoType]  # e.g., ["code"] or ["code", "ci"]
    min_access: AccessScope  # e.g., READ or WRITE
    llm_required: bool = True  # Does this skill need LLM?


class SkillInput(BaseModel):
    """Base class for skill inputs"""

    repo_id: Optional[int] = None  # Resolved by framework
    user_id: str
    session_id: str


class SkillOutput(BaseModel):
    """Base class for skill outputs"""

    success: bool
    result: Any
    error: Optional[str] = None
    cost: float = 0.0  # LLM cost if applicable


class Skill(ABC):
    """Base class for all skills"""

    name: str
    version: str  # Semantic versioning (1.0.0)
    description: str
    requirements: SkillRequirement

    @abstractmethod
    def validate_input(self, input_data: dict) -> SkillInput:
        """Validate and parse input"""
        pass

    @abstractmethod
    async def execute(self, input: SkillInput) -> SkillOutput:
        """Execute the skill"""
        pass
