"""ORM models — split by domain, re-exported here."""

from api.base import Base  # noqa: F401

# auth
from api.models.auth import AuditLog, RefreshToken, Role, Token, User, UserRole  # noqa: F401

# agent / session / event / run
from api.models.agent import (  # noqa: F401
    Agent,
    AgentScratchpad,
    Event,
    RunEvent,
    Session,
)

# memory
from api.models.memory import MemoryRecord  # noqa: F401
from api.models.graph import GraphEdge, GraphNode  # noqa: F401

# skills
from api.models.skill import (  # noqa: F401
    SkillExecutionMetric,
    SkillInstallation,
    SkillPermission,
    SkillRegistry,
    SkillResourceBinding,
    SkillSelectionEvent,
    SkillSelectionLearning,
    SkillSetting,
    SkillUserCredential,
)

# Keep backward-compat alias
UserCredential = SkillUserCredential

# evaluation / quality / feedback / training
from api.models.evaluation import (  # noqa: F401
    GateResult,
    LLMCallLog,
    LLMFeedback,
    QualityAssessment,
    TrainingData,
    UserFeedback,
)

# context / decision / prompt
from api.models.context import (  # noqa: F401
    ContextSnapshot,
    DecisionAudit,
    EventEmbedding,
    PromptTemplate,
    PromptVariant,
)

# verification
from api.models.verification import ClaimEvidence, HallucinationCheck  # noqa: F401

# workflow / trigger
from api.models.workflow import Trigger, WorkflowDefinition, WorkflowRun  # noqa: F401

# infra
from api.models.infra import (  # noqa: F401
    Config,
    DistributedLock,
    LLMModel,
    Repo,
    SandboxMetadata,
)

# Re-exports from skill knowledge models
from skills.knowledge.models import SkKnowledgeEntry as KnowledgeEntry  # noqa: F401
from skills.knowledge.models import SkKnowledgeEntrySource as KnowledgeEntrySource  # noqa: F401
from skills.knowledge.models import SkKnowledgeRelation as KnowledgeRelation  # noqa: F401

# MatrixOne SDK's VectorType lacks cache_ok, causing SQLAlchemy to raise a
# warning-as-error when ORM queries touch any embedding column.  Patch once
# at import time so every consumer benefits.
from matrixone import VectorType as _VectorType  # noqa: E402

if not getattr(_VectorType, "cache_ok", False):
    _VectorType.cache_ok = True
