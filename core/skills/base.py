"""Skill framework for mo-agent-engine.

Skills are first-class citizens with versioning, declarative requirements,
and full lifecycle management.
"""

from abc import ABC, abstractmethod
from enum import Enum
from typing import Any, ClassVar, Generic, TypeVar

from pydantic import BaseModel

InputT = TypeVar("InputT", bound="SkillInput")
OutputT = TypeVar("OutputT", bound="SkillOutput")


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


class SideEffectCategory(str, Enum):
    """Side-effect categories for replay safety"""

    READ = "read"  # Safe to replay, no external changes
    WRITE = "write"  # Has side-effects, must use recorded results
    DESTRUCTIVE = "destructive"  # Dangerous, blocked in replay mode


class SideEffectProfile(BaseModel):
    """Side-effect profile for a skill"""

    category: SideEffectCategory
    external_apis: list[str] = []  # e.g., ["github", "llm"]
    mock_strategy: str = "recorded"  # How to mock in replay mode


class SkillRequirement(BaseModel):
    """What a skill needs to run"""

    repo_types: list[RepoType]  # e.g., ["code"] or ["code", "ci"]
    min_access: AccessScope  # e.g., READ or WRITE
    llm_required: bool = True  # Does this skill need LLM?


class SkillInput(BaseModel):
    """Base class for skill inputs"""

    repo_id: int | None = None  # Resolved by framework
    user_id: str | None = None  # Injected by executor
    session_id: str | None = None  # Injected by executor

    # Fields auto-injected by framework, excluded from LLM tool schema
    _FRAMEWORK_FIELDS: ClassVar[set[str]] = {"repo_id", "user_id", "session_id"}


class SkillOutput(BaseModel):
    """Base class for skill outputs"""

    success: bool
    result: Any
    error: str | None = None
    cost: float = 0.0  # LLM cost if applicable


class Skill(ABC, Generic[InputT, OutputT]):
    """Base class for all skills"""

    name: str
    version: str  # Semantic versioning (1.0.0)
    description: str
    requirements: SkillRequirement
    side_effect_profile: SideEffectProfile = SideEffectProfile(
        category=SideEffectCategory.READ
    )  # Default to READ (safe)

    @abstractmethod
    def validate_input(self, input_data: dict) -> InputT:
        """Validate and parse input"""
        pass

    @abstractmethod
    async def execute(self, input: InputT) -> OutputT:
        """Execute the skill"""
        pass
