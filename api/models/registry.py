"""Model group registry — controls which ORM models are loaded per runtime mode."""

from __future__ import annotations

from enum import Enum
from typing import Sequence


class ModelGroup(Enum):
    FOUNDATION = "foundation"  # Config, DistributedLock, SandboxMetadata
    AUTH = "auth"  # User, Role, Token, AuditLog, ...
    EVENTS = "events"  # Agent, Session, Event, RunEvent, AgentScratchpad
    MEMORY = "memory"  # MemoryRecord, MemoryEditLog, MemoryExperiment, Graph*
    SKILLS = "skills"  # SkillRegistry, SkillInstallation, ...
    EVALUATION = "evaluation"  # QualityAssessment, TrainingData, ...
    CONTEXT = "context"  # ContextSnapshot, DecisionAudit, EventEmbedding, ...
    VERIFICATION = "verification"  # ClaimEvidence, HallucinationCheck
    WORKFLOW = "workflow"  # WorkflowDefinition, WorkflowRun, Trigger
    KNOWLEDGE = "knowledge"  # KnowledgeEntry, KnowledgeEntrySource, ...


# Import paths per group — each entry is (module, names_to_import).
_GROUP_IMPORTS: dict[ModelGroup, list[tuple[str, list[str]]]] = {
    ModelGroup.FOUNDATION: [
        ("api.models.infra", ["Config", "DistributedLock", "LLMModel", "Repo", "SandboxMetadata"]),
    ],
    ModelGroup.AUTH: [
        ("api.models.auth", ["AuditLog", "RefreshToken", "Role", "Token", "User", "UserRole"]),
    ],
    ModelGroup.EVENTS: [
        ("api.models.agent", ["Agent", "AgentScratchpad", "Event", "RunEvent", "Session"]),
    ],
    ModelGroup.MEMORY: [
        ("core.memory.models.memory", ["MemoryRecord"]),
        ("core.memory.models.memory_config", ["MemoryUserConfig"]),
        ("core.memory.models.memory_edit_log", ["MemoryEditLog"]),
        ("core.memory.models.memory_experiment", ["MemoryExperiment"]),
        ("core.memory.models.graph", ["GraphEdge", "GraphNode"]),
    ],
    ModelGroup.SKILLS: [
        ("api.models.skill", [
            "SkillExecutionMetric", "SkillInstallation", "SkillPermission",
            "SkillRegistry", "SkillResourceBinding", "SkillSelectionEvent",
            "SkillSelectionLearning", "SkillSetting", "SkillUserCredential",
        ]),
    ],
    ModelGroup.EVALUATION: [
        ("api.models.evaluation", [
            "GateResult", "LLMCallLog", "LLMFeedback",
            "QualityAssessment", "TrainingData", "UserFeedback",
        ]),
    ],
    ModelGroup.CONTEXT: [
        ("api.models.context", [
            "ContextSnapshot", "DecisionAudit", "EventEmbedding",
            "PromptTemplate", "PromptVariant",
        ]),
    ],
    ModelGroup.VERIFICATION: [
        ("api.models.verification", ["ClaimEvidence", "HallucinationCheck"]),
    ],
    ModelGroup.WORKFLOW: [
        ("api.models.workflow", ["Trigger", "WorkflowDefinition", "WorkflowRun"]),
    ],
    ModelGroup.KNOWLEDGE: [
        ("skills.knowledge.models", [
            "SkKnowledgeEntry", "SkKnowledgeEntrySource", "SkKnowledgeRelation",
        ]),
    ],
}

# Memory service loads only these groups.
MEMORY_SERVICE_GROUPS: frozenset[ModelGroup] = frozenset({
    ModelGroup.FOUNDATION,
    ModelGroup.AUTH,
    ModelGroup.EVENTS,
    ModelGroup.MEMORY,
})

# Full runtime loads everything.
FULL_RUNTIME_GROUPS: frozenset[ModelGroup] = frozenset(ModelGroup)


def get_groups_for_mode(mode: str) -> frozenset[ModelGroup]:
    """Return model groups to load for the given runtime mode."""
    if mode == "memory":
        return MEMORY_SERVICE_GROUPS
    return FULL_RUNTIME_GROUPS


def import_models_for_groups(groups: Sequence[ModelGroup] | frozenset[ModelGroup]) -> None:
    """Import ORM model modules for the given groups.

    Importing the modules registers them with SQLAlchemy's Base.metadata,
    which is required before create_all().
    """
    import importlib

    for group in groups:
        for module_path, _names in _GROUP_IMPORTS.get(group, []):
            importlib.import_module(module_path)
