"""Event models and types.

Defines the data structures for conversation events following the event-centric design.
"""

from datetime import datetime, timezone
from enum import Enum
from typing import Any

from pydantic import BaseModel, Field


class EventType(str, Enum):
    """Event types in the conversation flow."""

    USER_QUERY = "user_query"
    LLM_REQUEST = "llm_request"
    LLM_RESPONSE = "llm_response"
    TOOL_CALL = "tool_call"
    TOOL_RESULT = "tool_result"
    SYSTEM_MESSAGE = "system_message"
    SESSION_HISTORY_SNAPSHOT = "session_history_snapshot"
    MULTI_AGENT_MESSAGE = "multi_agent_message"

    # Planning events
    PLAN_CREATED = "plan_created"
    PLAN_STEP_START = "plan_step_start"
    PLAN_STEP_DONE = "plan_step_done"
    PLAN_REVISED = "plan_revised"
    PLAN_REFLECTION = "plan_reflection"
    PLAN_COMPLETED = "plan_completed"
    PLAN_FAILED = "plan_failed"

    # Memory governance events
    KNOWLEDGE_EXTRACTED = "knowledge_extracted"

    # Tool quality assessment
    TOOL_RESULT_QUALITY = "tool_result_quality"

    # Agent run lifecycle
    RUN_STARTED = "run_started"
    RUN_WAITING = "run_waiting"
    RUN_RESUMED = "run_resumed"
    RUN_COMPLETED = "run_completed"
    RUN_FAILED = "run_failed"
    RUN_CANCELLED = "run_cancelled"
    CHILD_RUN_CREATED = "child_run_created"
    MODEL_SELECTED = "model_selected"

    # Streaming events
    STREAM_RUN_STARTED = "stream_run_started"
    STREAM_RUN_FINISHED = "stream_run_finished"
    STREAM_RUN_ERROR = "stream_run_error"
    STREAM_TEXT_DELTA = "stream_text_delta"
    STREAM_TEXT_DONE = "stream_text_done"
    STREAM_THINKING_DELTA = "stream_thinking_delta"
    STREAM_THINKING_DONE = "stream_thinking_done"
    STREAM_TOOL_CALL_START = "stream_tool_call_start"
    STREAM_TOOL_CALL_ARGS = "stream_tool_call_args"
    STREAM_TOOL_CALL_END = "stream_tool_call_end"
    STREAM_TOOL_RESULT = "stream_tool_result"
    STREAM_PLAN_CREATED = "stream_plan_created"
    STREAM_PLAN_STEP_START = "stream_plan_step_start"
    STREAM_PLAN_STEP_DONE = "stream_plan_step_done"
    STREAM_PLAN_REVISED = "stream_plan_revised"
    STREAM_AGENT_DELEGATED = "stream_agent_delegated"
    STREAM_AGENT_PROGRESS = "stream_agent_progress"
    STREAM_AGENT_COMPLETED = "stream_agent_completed"


class StreamEventType(str, Enum):
    """Stream event types for real-time output.

    AG-UI protocol compliant with backward-compatible aliases.
    See: https://docs.ag-ui.com/concepts/events
    """

    # Lifecycle
    RUN_STARTED = "run_started"
    RUN_FINISHED = "run_finished"
    RUN_ERROR = "run_error"
    STEP_STARTED = "step_started"
    STEP_FINISHED = "step_finished"

    # Text messages (AG-UI standard: Start → Content → End)
    TEXT_MESSAGE_START = "text_message_start"
    TEXT_MESSAGE_CONTENT = "text_message_content"
    TEXT_MESSAGE_END = "text_message_end"

    # Text generation (backward-compatible aliases)
    TEXT_DELTA = "text_delta"
    TEXT_DONE = "text_done"

    # Reasoning (AG-UI standard, replaces deprecated THINKING_*)
    REASONING_START = "reasoning_start"
    REASONING_MESSAGE_START = "reasoning_message_start"
    REASONING_MESSAGE_CONTENT = "reasoning_message_content"
    REASONING_MESSAGE_END = "reasoning_message_end"
    REASONING_END = "reasoning_end"

    # Thinking (deprecated → use REASONING_*)
    THINKING_DELTA = "thinking_delta"
    THINKING_DONE = "thinking_done"

    # Tool use
    TOOL_CALL_START = "tool_call_start"
    TOOL_CALL_ARGS = "tool_call_args"
    TOOL_CALL_END = "tool_call_end"
    TOOL_RESULT = "tool_result"

    # State management (AG-UI standard)
    STATE_SNAPSHOT = "state_snapshot"
    STATE_DELTA = "state_delta"
    MESSAGES_SNAPSHOT = "messages_snapshot"

    # Planning (mo-agent-engine extension)
    PLAN_CREATED = "plan_created"
    PLAN_STEP_START = "plan_step_start"
    PLAN_STEP_DONE = "plan_step_done"
    PLAN_REVISED = "plan_revised"

    # Multi-agent (mo-agent-engine extension)
    AGENT_DELEGATED = "agent_delegated"
    AGENT_PROGRESS = "agent_progress"
    AGENT_COMPLETED = "agent_completed"

    # Special (AG-UI standard)
    CUSTOM = "custom"
    RAW = "raw"


class StreamEvent(BaseModel):
    """Stream event for real-time output.

    AG-UI protocol compliant. Each streamed chunk is also a logged event
    for auditability. Includes agent_id for multi-agent stream multiplexing.
    """

    event_type: StreamEventType
    data: dict[str, Any] = Field(default_factory=dict)
    event_id: str | None = None
    causal_chain_id: str | None = None
    agent_id: str | None = None  # For multi-agent stream multiplexing

    # AG-UI base properties
    timestamp: str | None = None
    thread_id: str | None = None
    run_id: str | None = None
    message_id: str | None = None
    tool_call_id: str | None = None


class TokenUsage(BaseModel):
    """Token usage statistics."""

    prompt: int = Field(description="Prompt tokens")
    completion: int = Field(description="Completion tokens")
    total: int = Field(description="Total tokens")


class ContextSnapshot(BaseModel):
    """Context capture for reproducibility.

    Captures the state needed to reproduce an LLM call.
    This is a business-level capture, not a MatrixOne database snapshot.
    """

    context_capture_id: str | None = Field(
        default=None, description="Reference to context_captures table"
    )
    prompt_template_id: str | None = Field(
        default=None, description="Prompt template ID and version"
    )
    skills_used: list[str] | None = Field(
        default=None, description="Skills used in this interaction"
    )
    history_events: list[str] | None = Field(
        default=None, description="Event IDs included in context"
    )
    retrieved_chunks: list[str] | None = Field(default=None, description="RAG chunk IDs retrieved")


class ConversationEvent(BaseModel):
    """Conversation event model.

    Represents a single atomic event in the conversation flow.
    Based on the event-centric design in context-memory-session-and-tables.md §4.1
    """

    event_id: str = Field(description="ULID, globally unique and sortable")
    user_id: str = Field(description="User identifier")
    session_id: str = Field(description="Session identifier")
    agent_id: str = Field(description="Agent type (e.g., dev-agent)")
    agent_version: str = Field(description="Agent code/config version")
    event_type: EventType = Field(description="Event type")
    content: str = Field(description="Original content")
    desensitized_content: str | None = Field(
        default=None, description="Desensitized version for compliance"
    )
    metadata: dict[str, Any] | None = Field(
        default=None, description="Namespace convention: dev.*, chat.*, etc."
    )
    context_snapshot: ContextSnapshot | None = Field(
        default=None, description="Reproducibility snapshot"
    )
    token_usage: TokenUsage | None = Field(default=None, description="Token usage statistics")
    embedding_ref: str | None = Field(default=None, description="External vector store chunk ID")
    created_at: datetime = Field(default_factory=lambda: datetime.now(timezone.utc))
    prompt_template_id: str | None = Field(default=None, description="References ctx_prompt_templates")
    skills_snapshot: list[dict[str, Any]] | None = Field(
        default=None, description="Skills used with versions"
    )
    quality_score: float | None = Field(default=None, description="System pre-score (0-5)")
    is_flagged: bool = Field(default=False, description="Flagged for review")
    training_eligible: bool = Field(default=False, description="Eligible for training pipeline")
    parent_event_id: str | None = Field(
        default=None, description="Immediate prior event in causal chain"
    )
    causal_chain_id: str | None = Field(
        default=None, description="Groups one user query + full LLM/tool chain"
    )
    llm_model_used: str | None = Field(
        default=None, description="Model identifier at inference time"
    )
    llm_params: dict[str, Any] | None = Field(
        default=None, description="LLM parameters (temperature, max_tokens, etc.)"
    )
    skill_name: str | None = Field(
        default=None, description="Skill/tool name for tool_call/tool_result events"
    )
    skill_version: str | None = Field(
        default=None, description="Skill version at invocation time"
    )
    skill_result: Any | None = Field(
        default=None, description="Skill execution result"
    )

    model_config = {"use_enum_values": True}
