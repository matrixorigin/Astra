"""Skill framework for mo-agent-engine.

All tools are skills. Execution location is determined by runtime_requirements,
not by skill classification. The executor inspects requirements and routes
to local (filesystem), remote (database/sandbox), or MCP runtime automatically.
"""

from abc import ABC, abstractmethod
from enum import Enum
from typing import Any, ClassVar, Generic, TypeVar, get_args

from pydantic import BaseModel

InputT = TypeVar("InputT", bound="SkillInput")
OutputT = TypeVar("OutputT", bound="SkillOutput")


# ---------------------------------------------------------------------------
# Runtime requirements — executor uses these to route execution
# ---------------------------------------------------------------------------

class RuntimeRequirement(str, Enum):
    """What runtime capabilities a skill needs. Executor routes based on these."""

    FILESYSTEM = "filesystem"  # Needs local filesystem access → local runtime
    DATABASE = "database"      # Needs platform DB → remote runtime
    NETWORK = "network"        # Needs network/API access → either runtime
    SANDBOX = "sandbox"        # Needs isolated sandbox → remote runtime
    GPU = "gpu"                # Needs GPU → heavyweight backend
    NONE = "none"              # Pure computation, runs anywhere


# ---------------------------------------------------------------------------
# Enums
# ---------------------------------------------------------------------------

class RepoType(str, Enum):
    CODE = "code"
    CI = "ci"
    TESTER = "tester"
    DOCS = "docs"


class AccessScope(str, Enum):
    READ = "read"
    WRITE = "write"
    ADMIN = "admin"


class SideEffectCategory(str, Enum):
    """Side-effect categories for replay safety and permission checking."""

    READ = "read"
    WRITE = "write"
    EXECUTE = "execute"        # Shell/subprocess execution
    DESTRUCTIVE = "destructive"


# ---------------------------------------------------------------------------
# Profiles and requirements
# ---------------------------------------------------------------------------

class SideEffectProfile(BaseModel):
    category: SideEffectCategory
    external_apis: list[str] = []
    mock_strategy: str = "recorded"


class SkillRequirement(BaseModel):
    """What a skill needs to run."""

    runtime: list[RuntimeRequirement] = [RuntimeRequirement.NONE]
    llm_required: bool = False
    timeout_seconds: int = 60
    min_memory_gb: float = 0.5
    gpu_required: bool = False
    conda_env: str | None = None
    async_execution: bool = False
    # Legacy fields — used by cloud skills (builtin, extended, delegation)
    repo_types: list[RepoType] = []
    min_access: AccessScope = AccessScope.READ


# ---------------------------------------------------------------------------
# Input / Output
# ---------------------------------------------------------------------------

class SkillInput(BaseModel):
    """Base class for skill inputs."""

    repo_id: int | None = None
    user_id: str | None = None
    session_id: str | None = None

    _FRAMEWORK_FIELDS: ClassVar[set[str]] = {"repo_id", "user_id", "session_id"}


class SkillOutput(BaseModel):
    """Base class for skill outputs."""

    success: bool
    result: Any = None
    error: str | None = None
    cost: float = 0.0
    data_source: str = ""       # Origin of data, e.g. "alpha_vantage_api". Empty string means not set.
    data_timestamp: str = ""    # ISO 8601 when data was fetched. Empty string means not set.


# ---------------------------------------------------------------------------
# Skill base class
# ---------------------------------------------------------------------------

class Skill(ABC, Generic[InputT, OutputT]):
    """Base class for ALL tools/skills in the system.

    Every tool — file ops, shell, git, grep, GitHub, code execution,
    delegation — inherits from this. Execution location is orthogonal:
    the executor reads ``requirements.runtime`` and routes accordingly.
    """

    name: str
    version: str = "1.0.0"
    description: str = ""
    short_description: str = ""  # <=80 chars for system prompt; auto-truncates if empty
    requirements: SkillRequirement = SkillRequirement()

    @property
    def prompt_description(self) -> str:
        """Short description for system prompt (<=80 chars).

        Used in category summaries and skill listings where token budget is tight.
        Falls back to truncated description if short_description not set.
        """
        if self.short_description:
            return self.short_description
        if not self.description:
            return ""
        if len(self.description) <= 80:
            return self.description
        return self.description[:77] + "..."
    side_effect_profile: SideEffectProfile = SideEffectProfile(
        category=SideEffectCategory.READ,
    )
    # Skills that share credentials declare a config namespace.
    # e.g. summarize_pr, list_prs, ci_status all set config_namespace = "github"
    # so SkillConfigCenter looks up tokens under "github" not the individual skill name.
    config_namespace: ClassVar[str | None] = None

    _input_cls: ClassVar[type["SkillInput"] | None] = None
    _output_cls: ClassVar[type["SkillOutput"] | None] = None

    def __init_subclass__(cls, **kwargs: Any) -> None:
        super().__init_subclass__(**kwargs)
        for base in getattr(cls, "__orig_bases__", ()):
            args = get_args(base)
            if len(args) == 2 and isinstance(args[0], type) and issubclass(args[0], SkillInput):
                cls._input_cls = args[0]
                cls._output_cls = args[1]
                break

    def validate_input(self, input_data: dict) -> InputT:
        if self._input_cls is None:
            raise TypeError(f"{type(self).__name__} has no _input_cls; "
                            "specify Generic type args or override validate_input()")
        return self._input_cls(**input_data)  # type: ignore[return-value]

    @abstractmethod
    async def execute(self, input: InputT) -> OutputT:
        """Execute the skill."""
        ...

    def to_openai_schema(self) -> dict[str, Any]:
        """Return OpenAI function calling tool schema.

        Derives parameters from ``_input_cls`` Pydantic schema, excluding
        framework-injected fields. Skills can override for custom schemas.
        """
        if self._input_cls is not None:
            schema = self._input_cls.model_json_schema()
            props = {k: v for k, v in schema.get("properties", {}).items()
                     if k not in SkillInput._FRAMEWORK_FIELDS}
            required = [r for r in schema.get("required", [])
                        if r not in SkillInput._FRAMEWORK_FIELDS]
            params = {"type": "object", "properties": props}
            if required:
                params["required"] = required
        else:
            params = {"type": "object", "properties": {}}
        return {
            "type": "function",
            "function": {
                "name": self.name,
                "description": self.description,
                "parameters": params,
            },
        }
