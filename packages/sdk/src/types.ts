// ─── SSE Event Types ────────────────────────────────────────────────
// Stream event types matching the Rust backend's SSE/WebSocket protocol.

export type StreamEventType =
  | "session_info"
  | "run_started"
  | "run_paused"
  | "run_resumed"
  | "run_finished"
  | "run_cancelled"
  | "run_waiting"
  | "run_error"
  | "run_interrupted"
  | "run_input_queued"
  | "text_delta"
  | "text_done"
  | "reasoning_delta"
  | "reasoning_done"
  | "thinking_delta"
  | "thinking_done"
  | "reasoning_message_content"
  | "tool_call"
  | "tool_call_start"
  | "tool_call_end"
  | "usage"
  | "turn_complete"
  | "error"
  | "warning"
  | "explain"
  | "plan_created"
  | "plan_revised"
  | "plan_step_start"
  | "plan_step_done"
  | "workspace_bound"
  | "executor_bound"
  | "executor_status_changed"
  | "tool_routing_decision"
  | "tool_transport_started"
  | "tool_transport_completed"
  | "tool_transport_failed"
  | "run_blocked"
  | "agent_delegated"
  | "agent_spawned"
  | "agent_live_event"
  | "agent_live_gap"
  | "stream_gap"
  | "agent_waiting"
  | "agent_progress"
  | "agent_completed"
  | "agent_failed"
  | "agent_cancelled"
  | "agent_interrupted"
  | "task_board_snapshot"
  | "tool_approval_request"
  | "ping"
  | "device_revoked"
  | "device_lease_expired"
  | "tool_execution_started"
  | "tool_output_delta"
  | "tool_execution_completed";

export type SessionInfoEvent = {
  type: "session_info";
  session_id: string;
  run_id?: string;
};

export type RunStartedEvent = {
  type: "run_started";
  run_id?: string;
  session_id?: string;
} & ExecutionBindingFields;

export type RunPausedEvent = {
  type: "run_paused";
  run_id?: string;
  waiting_for?: string | null;
};

export type RunResumedEvent = {
  type: "run_resumed";
  run_id?: string;
};

export type RunFinishedEvent = {
  type: "run_finished";
  run_id?: string;
  status?: string;
  error?: string | null;
  interrupted?: boolean;
  resumable?: boolean;
  waiting_for?: string | null;
};

export type RunCancelledEvent = {
  type: "run_cancelled";
  run_id: string;
};

export type RunWaitingEvent = {
  type: "run_waiting";
  run_id?: string;
  reason?: string;
  waiting_for?: string | null;
  timestamp?: number;
} & ExecutionBindingFields;

export type RunErrorEvent = {
  type: "run_error";
  run_id?: string;
  message?: string;
  error?: string;
  code?: string;
  error_kind?: string;
};

export type RunInterruptedEvent = {
  type: "run_interrupted";
  run_id?: string;
  kind?: string;
  message?: string;
  waiting_for?: string | null;
  resumable?: boolean;
  task_board?: RunInterruptedTaskBoardSummary;
};

export type RunInterruptedTaskBoardSummary = {
  summary?: string;
  tracked_count?: number;
  pending_count?: number;
  in_progress_count?: number;
  paused_count?: number;
  blocked_count?: number;
  terminal_non_success_count?: number;
  active_tasks?: string[];
};

export type TextDeltaEvent = {
  type: "text_delta";
  content: string;
};

export type TextDoneEvent = {
  type: "text_done";
  full_text: string;
};

export type ThinkingDeltaEvent = {
  type: "reasoning_delta";
  content: string;
};

export type ThinkingDoneEvent = {
  type: "reasoning_done";
};

export type WorkspaceBinding = {
  kind:
    | "server_sandbox"
    | "edge_workspace"
    | "uploaded_snapshot"
    | "git_checkout"
    | "none"
    | "unknown"
    | string;
  display_name?: string;
  cwd?: string | null;
  authority?: "read_only" | "read_write" | string;
  fallback_policy?: "disabled";
};

export type ExecutorBinding = {
  kind:
    | "server_local"
    | "edge_agent"
    | "orchestrator_managed"
    | "thin_client"
    | "mcp"
    | "unknown";
  executor_id?: string;
  display_name?: string;
  transport?:
    | "server_local"
    | "edge_ws"
    | "edge_ledger"
    | "gateway_relay"
    | "sandbox_resident_agent"
    | "mcp_http"
    | "unknown";
  status?: "online" | "offline" | "degraded" | "unknown" | string;
};

export type ExecutionBindingFields = {
  workspace?: WorkspaceBinding;
  executor?: ExecutorBinding;
  transport?: string;
  fallback_policy?: string;
  route?: string;
};

type ExecutionRouteFields = Pick<
  ExecutionBindingFields,
  "transport" | "fallback_policy" | "route"
>;

export type ToolCallStartEvent = {
  type: "tool_call_start";
  tool: string;
  call_id: string;
  arguments?: unknown;
} & ExecutionBindingFields;

export type ToolCallEvent = {
  type: "tool_call";
  tool_call: {
    id?: string;
    call_id?: string;
    name?: string;
    tool?: string;
    arguments?: unknown;
    args?: unknown;
    function?: {
      name?: string;
      arguments?: unknown;
      id?: string;
      call_id?: string;
    };
  };
} & ExecutionBindingFields;

export type ToolCallEndEvent = {
  type: "tool_call_end";
  call_id: string;
  result?: string;
  success?: boolean;
  duration_ms?: number;
  error_kind?: string;
  blocked?: boolean;
} & ExecutionBindingFields;

export type UsageEvent = {
  type: "usage";
  prompt_tokens: number;
  completion_tokens: number;
  cache_creation_tokens?: number;
  cache_read_tokens?: number;
  // Also accept alternative field names from backend
  cache_creation_input_tokens?: number;
  cache_read_input_tokens?: number;
};

export type TurnCompleteEvent = {
  type: "turn_complete";
  /** Some runtime paths include final visible assistant text on turn completion. */
  assistant_text?: string;
  followup_suggestion?: string;
};

export type StreamErrorEvent = {
  type: "error";
  code?: string;
  error_code?: string;
  message: string;
  retryable?: boolean;
  retry_after_ms?: number;
};

export type WarningEvent = {
  type: "warning";
  message: string;
  claims_failed?: number;
};

export type ExplainEvent = {
  type: "explain";
  content: string;
};

export type PlanCreatedEvent = {
  type: "plan_created";
  plan: {
    plan_id?: string;
    title?: string;
    subtasks: Array<{ id: string; title: string; status?: string }>;
  };
};

export type PlanRevisedEvent = {
  type: "plan_revised";
  plan: {
    plan_id?: string;
    title?: string;
    subtasks: Array<{ id: string; title: string; status?: string }>;
  };
};

export type PlanStepStartEvent = {
  type: "plan_step_start";
  step: string;
  subtask_id?: string;
};

export type PlanStepDoneEvent = {
  type: "plan_step_done";
  step: string;
  subtask_id?: string;
  result?: string;
};

export type AgentDelegatedEvent = {
  type: "agent_delegated";
  agent_id: string;
  task: string;
} & ExecutionBindingFields;

export type AgentSpawnedEvent = {
  type: "agent_spawned";
  agent_id: string;
  run_id: string;
  parent_run_id: string;
  agent_type: string;
  description: string;
  timestamp?: number;
} & ExecutionBindingFields;

export type AgentWaitingEvent = {
  type: "agent_waiting";
  agent_id: string;
  run_id?: string;
  parent_run_id?: string;
  status?: "waiting" | string;
  reason?: string;
  waiting_for?: string | null;
  timestamp?: number;
} & ExecutionBindingFields;

export type AgentProgressEvent = {
  type: "agent_progress";
  agent_id: string;
  status: string;
  description?: string;
  tool_name?: string;
  turn?: number;
  max_turns?: number;
  total_prompt_tokens?: number;
  total_completion_tokens?: number;
  total_tool_calls?: number;
  timestamp?: number;
};

export type AgentLiveEvent = {
  type: "agent_live_event";
  agent_id: string;
  run_id?: string;
  event_kind:
    | "output_delta"
    | "thinking_delta"
    | "status"
    | "signal"
    | "tool_started"
    | "tool_completed"
    | "agent_terminated";
  content?: string;
  signal?: Record<string, unknown> | string;
  name?: string;
  description?: string;
  tool_use_id?: string;
  status?: string;
  duration_ms?: number;
  output_summary?: string | null;
  output?: string | null;
  termination?: "completed" | "delegated" | "failed" | "interrupted" | "cancelled";
  reason?: string | null;
  timestamp?: number;
} & ExecutionBindingFields;

/**
 * A bounded live lane dropped one or more events. This is transport-integrity
 * evidence, never agent output. Consumers repair from durable run truth.
 */
export type AgentLiveGapEvent = {
  type: "agent_live_gap";
  run_id: string;
  agent_id: string;
  dropped_event_count: number;
  repair: "refresh_run_snapshot";
};

/** The bounded run stream dropped one or more coalescible events. */
export type StreamGapEvent = {
  type: "stream_gap";
  run_id: string;
  dropped_event_count: number;
  repair: "refresh_run_snapshot";
};

export type AgentCompletedEvent = {
  type: "agent_completed";
  agent_id: string;
  status: "completed" | "failed" | "cancelled";
  result_summary?: string;
  error?: string;
  reason?: string;
  total_tool_calls?: number;
  total_tokens?: { prompt: number; completion: number };
  duration_ms?: number;
  timestamp?: number;
};

export type AgentFailedEvent = {
  type: "agent_failed";
  agent_id: string;
  status?: "failed";
  error?: string;
  timestamp?: number;
};

export type AgentCancelledEvent = {
  type: "agent_cancelled";
  agent_id: string;
  status?: "cancelled";
  reason?: string;
  timestamp?: number;
};

export type AgentInterruptedEvent = {
  type: "agent_interrupted";
  agent_id: string;
  status?: "interrupted";
  reason?: string;
  partial_summary?: string;
  total_tool_calls?: number;
  total_tokens?: { prompt: number; completion: number };
  duration_ms?: number;
  timestamp?: number;
};

export type SessionSubtask = {
  id: string;
  title: string;
  description?: string | null;
  status: string;
  owner?: string | null;
  depends_on?: string[];
};

export type SessionTask = {
  id: string;
  title: string;
  description?: string | null;
  active_form?: string | null;
  status: string;
  owner?: string | null;
  metadata?: Record<string, unknown> | null;
  blocks?: string[];
  blocked_by?: string[];
  subtasks?: SessionSubtask[];
  created_at: string;
  updated_at: string;
  archived_at?: string | null;
};

export type AgentActivity = {
  agentId: string;
  runId?: string;
  parentRunId?: string;
  agentType?: string;
  description?: string;
  task?: string;
  status: string;
  reason?: string;
  error?: string;
  resultSummary?: string;
  toolName?: string;
  turn?: number;
  maxTurns?: number;
  totalPromptTokens?: number;
  totalCompletionTokens?: number;
  totalToolCalls?: number;
  durationMs?: number;
  updatedAt: number;
};

export type TaskBoardSnapshotEvent = {
  type: "task_board_snapshot";
  session_id: string;
  reason?: string;
  tasks: SessionTask[];
};

export type WorkspaceBoundEvent = {
  type: "workspace_bound";
  session_id?: string;
  workspace: WorkspaceBinding;
  executor?: ExecutorBinding;
} & ExecutionRouteFields;

export type ExecutorBoundEvent = {
  type: "executor_bound" | "executor_status_changed";
  session_id?: string;
  executor: ExecutorBinding;
  workspace?: WorkspaceBinding;
} & ExecutionRouteFields;

export type ToolRoutingDecisionEvent = {
  type: "tool_routing_decision";
  call_id: string;
  tool?: string;
} & ExecutionBindingFields;

export type ToolTransportStartedEvent = {
  type: "tool_transport_started";
  call_id: string;
  tool: string;
  arguments?: unknown;
} & ExecutionBindingFields;

export type ToolTransportCompletedEvent = {
  type: "tool_transport_completed";
  call_id: string;
  tool?: string;
  result?: unknown;
  success?: boolean;
  duration_ms?: number;
} & ExecutionBindingFields;

export type ToolTransportFailedEvent = {
  type: "tool_transport_failed";
  call_id: string;
  tool?: string;
  error?: string;
  error_kind?: string;
  blocked?: boolean;
  success?: false;
  duration_ms?: number;
} & ExecutionBindingFields;

export type RunInputQueuedEvent = {
  type: "run_input_queued";
  run_id?: string;
  session_id?: string;
};

export type ThinkingAliasDeltaEvent = {
  type: "thinking_delta";
  content: string;
};

export type ThinkingAliasDoneEvent = {
  type: "thinking_done";
};

export type ReasoningMessageContentEvent = {
  type: "reasoning_message_content";
  content: string;
};

type RunBlockedBaseEvent = {
  run_id?: string;
  session_id?: string;
  reason?: string;
  message?: string;
  call_id?: string;
  tool?: string;
  timestamp?: number;
} & ExecutionBindingFields;

export type RunBlockedEvent = RunBlockedBaseEvent & {
  type: "run_blocked";
  reason:
    | "executor_offline"
    | "transport_disconnected"
    | "fallback_disabled"
    | "workspace_executor_unavailable";
};

export type ToolApprovalRequestEvent = {
  type: "tool_approval_request";
  request_id: string;
  tool: string;
  args: Record<string, unknown>;
};

export type PingEvent = {
  type: "ping";
  run_id?: string;
  heartbeat_interval_ms?: number;
};

export type DeviceLeaseEndedEvent = {
  type: "device_revoked" | "device_lease_expired";
  lease_id: string;
  session_id: string;
  device_id: string;
  device_fingerprint: string;
  reason: string;
  ended_at_server: string;
};

export type ToolExecutionStartedEvent = {
  type: "tool_execution_started";
  call_id: string;
  tool: string;
};

export type ToolOutputDeltaEvent = {
  type: "tool_output_delta";
  call_id: string;
  content: string;
};

export type ToolExecutionCompletedEvent = {
  type: "tool_execution_completed";
  call_id: string;
  success: boolean;
};

export type StreamEvent = (
  | SessionInfoEvent
  | RunStartedEvent
  | RunPausedEvent
  | RunResumedEvent
  | RunFinishedEvent
  | RunCancelledEvent
  | RunWaitingEvent
  | RunErrorEvent
  | RunInterruptedEvent
  | RunInputQueuedEvent
  | TextDeltaEvent
  | TextDoneEvent
  | ThinkingDeltaEvent
  | ThinkingDoneEvent
  | ThinkingAliasDeltaEvent
  | ThinkingAliasDoneEvent
  | ReasoningMessageContentEvent
  | ToolCallEvent
  | ToolCallStartEvent
  | ToolCallEndEvent
  | UsageEvent
  | TurnCompleteEvent
  | StreamErrorEvent
  | WarningEvent
  | ExplainEvent
  | PlanCreatedEvent
  | PlanRevisedEvent
  | PlanStepStartEvent
  | PlanStepDoneEvent
  | AgentDelegatedEvent
  | AgentSpawnedEvent
  | AgentLiveEvent
  | AgentLiveGapEvent
  | StreamGapEvent
  | AgentWaitingEvent
  | AgentProgressEvent
  | AgentCompletedEvent
  | AgentFailedEvent
  | AgentCancelledEvent
  | AgentInterruptedEvent
  | TaskBoardSnapshotEvent
  | WorkspaceBoundEvent
  | ExecutorBoundEvent
  | ToolRoutingDecisionEvent
  | ToolTransportStartedEvent
  | ToolTransportCompletedEvent
  | ToolTransportFailedEvent
  | RunBlockedEvent
  | ToolApprovalRequestEvent
  | PingEvent
  | DeviceLeaseEndedEvent
  | ToolExecutionStartedEvent
  | ToolOutputDeltaEvent
  | ToolExecutionCompletedEvent
) & { index?: number };

export type ConnectionState =
  "disconnected" | "connecting" | "connected" | "error";

// ─── Chat / Workspace Types ────────────────────────────────────────

export type ChatRole = "user" | "assistant" | "system";

export type ToolStatus = "running" | "done" | "error" | "cancelled" | "skipped";

export type ToolCall = {
  callId: string;
  tool: string;
  arguments?: string;
  result?: string;
  status: ToolStatus;
  errorKind?: string;
  blocked?: boolean;
  workspace?: WorkspaceBinding;
  executor?: ExecutorBinding;
  transport?: string;
  fallbackPolicy?: string;
  route?: string;
  durationMs?: number;
  startedAt: number;
  finishedAt?: number;
};

export type ThinkingBlock = {
  content: string;
  done: boolean;
};

export type PlanSubtask = {
  id: string;
  title: string;
  status: "pending" | "running" | "done" | "error";
};

export type PlanState = {
  planId?: string;
  title?: string;
  subtasks: PlanSubtask[];
  activeStepId?: string;
};

export type TokenUsage = {
  promptTokens: number;
  completionTokens: number;
  totalTokens: number;
  cacheCreationTokens: number;
  cacheReadTokens: number;
};

export type ChatMessage = {
  id: string;
  role: ChatRole;
  content: string;
  toolCalls?: ToolCall[];
  thinking?: ThinkingBlock;
  timestamp: number;
  /** Whether this message is still being streamed. */
  streaming?: boolean;
};

export type WorkspaceState = {
  sessionId: string | null;
  runId: string | null;
  runStatus: string | null;
  waitingFor: string | null;
  workspace?: WorkspaceBinding;
  executor?: ExecutorBinding;
  transport: string | null;
  fallbackPolicy: string | null;
  messages: ChatMessage[];
  toolCalls: ToolCall[];
  followupSuggestion: string | null;
  isStreaming: boolean;
  error: string | null;
  plan: PlanState | null;
  usage: TokenUsage;
  agentEvents: StreamEvent[];
};

/** Matches `astra_core::SkillSearchSettings` (JSON uses snake_case on the wire). */
export type SkillSearchSettings = {
  dynamicSurface: boolean;
  minCatalogSize: number;
  surfaceCap: number;
};

export type AgentBindingSelection = {
  id: string;
  capabilityServerRefs: {
    mcp: string;
    skills: string;
  };
};

export type RuntimeProfile =
  | "agent_binding_registry"
  | "request_scoped_runtime_mcp";

export type ExecutionBudget = {
  initialTurns?: number;
  hardTurnLimit?: number;
};

export type ChatConfig = {
  /** Optional runtime URL used by tests and direct clients; the Web UI proxies requests. */
  apiUrl?: string;
  sessionId?: string;
  agentId?: string;
  /** Effective Offering selected from Astra's model-access projection. */
  offeringId?: string;
  /** Durable binding for external MCP and skill capability servers. */
  agentBinding?: AgentBindingSelection;
  /** Runtime capability resolution mode used for this chat surface. */
  runtimeProfile?: RuntimeProfile;
  /** Explicit bounded execution budget for autonomous work. */
  executionBudget?: ExecutionBudget;
  /** Runtime capabilities requested by the embedding application. */
  capabilities?: string[];
  /** Include structured decision evidence in supported runtime paths. */
  explain?: boolean;
  /** Application-owned structured context; never flatten this into user text. */
  context?: Record<string, unknown>;
  /** When set and non-empty, sent as `allow_skills` on chat requests. */
  allowSkills?: string[];
  /** When set and non-empty, sent as `allow_tools` on chat requests. */
  allowTools?: string[];
  /** Optional external tools explicitly enabled by the embedding product. */
  enabledTools?: string[];
  /** Catalog surfacing — sent as `skill_search` (snake_case fields on the wire). */
  skillSearch?: SkillSearchSettings;
  /** Optional explicit workspace boundary for direct SDK integrations. */
  workspaceBinding?: WorkspaceBinding;
  /** Optional explicit executor boundary for direct SDK integrations. */
  executorBinding?: ExecutorBinding;
};

// ─── Client Configuration ──────────────────────────────────────────

export type AstraClientConfig = {
  baseUrl: string;
  /**
   * Optional path prefix before thin-client routes (e.g. `/api` when a gateway mounts
   * the runtime at `https://host/api/...`). Default: empty — paths match `astra-thin-client` /
   * `astra-server` (`/auth/login`, `/chat/stream`, …).
   */
  pathPrefix?: string;
  accessToken?: string;
  refreshToken?: string;
  onTokenRefresh?: (tokens: {
    accessToken: string;
    refreshToken: string;
  }) => void | Promise<void>;
  headers?: Record<string, string>;
};

export type SSEClientOptions = {
  url: string;
  token?: string;
  headers?: Record<string, string>;
  onEvent: (event: StreamEvent) => void;
  onStateChange?: (state: ConnectionState) => void;
  onRawLine?: (line: string) => void;
  maxRetries?: number;
  retryDelayMs?: number;
  /** Abort the stream when no SSE event arrives within this window. Defaults to disabled. */
  heartbeatTimeoutMs?: number;
  signal?: AbortSignal;
  /** HTTP method. Defaults to 'GET'. Use 'POST' for streaming chat endpoints. */
  method?: "GET" | "POST";
  /** Request body for POST requests. */
  body?: string;
};

// ─── API Types ─────────────────────────────────────────────────────

export type ChatRequest = {
  message: string;
  parts?: unknown[];
  attachments?: unknown[];
  sessionId?: string;
  agentId?: string;
  modelSelection: {
    offeringId: string;
  };
  agentBinding?: AgentBindingSelection;
  runtimeAuth?: {
    authorization: string;
  };
  runtimeProfile?: RuntimeProfile;
  executionBudget?: ExecutionBudget;
  context?: Record<string, unknown>;
  explain?: boolean;
  planSubtaskId?: string;
  isPlanSubtask?: boolean;
  /** §5.5 — edge executor that will receive `tool_request` callbacks. */
  edgeExecutorId?: string;
  capabilities?: string[];
  /** When set and non-empty, sent as `allow_skills`. */
  allowSkills?: string[];
  /** When set and non-empty, sent as `allow_tools`. */
  allowTools?: string[];
  /** Optional external tools explicitly enabled for this request. An empty
   * array intentionally disables all user-selectable optional tools. */
  enabledTools?: string[];
  skillSearch?: SkillSearchSettings;
  workspaceBinding?: WorkspaceBinding;
  executorBinding?: ExecutorBinding;
};

export type CapabilityServerType = "mcp" | "skill";
export type CapabilityServerTransport = "streamable_http";
export type AgentBindingStatus = "active" | "disabled" | "invalid";
export type AgentBindingToolMode = "mcp_gateway";

export type CapabilityServerEndpoint = {
  id: string;
  type: CapabilityServerType;
  transport: CapabilityServerTransport;
  endpoint_url: string;
};

export type AgentBindingRuntimePolicy = {
  max_steps?: number | null;
  tool_mode: AgentBindingToolMode;
};

export type AgentBindingPayload = {
  binding_name: string;
  agent_md: string;
  capability_servers: CapabilityServerEndpoint[];
  runtime_policy: AgentBindingRuntimePolicy;
  metadata?: Record<string, unknown> | null;
  binding_schema_version: string;
};

export type AgentBindingCreateRequest = {
  idempotency_key: string;
  binding: AgentBindingPayload;
};

export type AgentBindingCreateResponse = {
  agent_binding_id: string;
  binding_name: string;
  status: AgentBindingStatus;
};

export type AgentBindingRecord = {
  agent_binding_id: string;
  binding_name: string;
  status: AgentBindingStatus;
  agent_md: string;
  capability_servers: CapabilityServerEndpoint[];
  runtime_policy: AgentBindingRuntimePolicy;
  metadata?: Record<string, unknown> | null;
  binding_schema_version: string;
  created_at: string;
  disabled_at?: string | null;
};

export type ModelProtocol = "openai_chat_completions";
export type ModelGatewayStatus = "active" | "disabled" | "invalid";

export type ModelGatewayCreateRequest = {
  id: string;
  resolve_url: string;
  model_protocol: ModelProtocol;
  metadata?: Record<string, unknown> | null;
};

export type ModelGatewayCreateResponse = {
  id: string;
  status: ModelGatewayStatus;
};

export type ModelGatewayRecord = {
  id: string;
  resolve_url: string;
  model_protocol: ModelProtocol;
  status: ModelGatewayStatus;
  metadata?: Record<string, unknown> | null;
  created_at: string;
  updated_at: string;
  disabled_at?: string | null;
};

export type RunStatus = {
  runId: string;
  sessionId: string;
  /** Durable run-tree identity. `null` means this is a root conversation run. */
  parentRunId: string | null;
  rootRunId: string;
  depth: number;
  status:
    | "running"
    | "input-queued"
    | "completed"
    | "failed"
    | "cancelled"
    | "paused"
    | "waiting"
    | "blocked"
    | string;
  eventsCount: number;
  waitingFor?: string | null;
  workspace?: WorkspaceBinding;
  executor?: ExecutorBinding;
  transport?: string | null;
  fallbackPolicy?: string | null;
};

export type RunProjectionResponse = {
  run_id: string;
  session_id: string;
  status: string;
  waiting_for?: string | null;
  error_message?: string | null;
  workspace?: WorkspaceBinding;
  executor?: ExecutorBinding;
  transport?: string | null;
  fallback_policy?: string | null;
  run_event_high_watermark: number;
  projection_event_idx: number;
  projection_updated_at: string;
  projection_hash: string;
  latest_event_type?: string | null;
  total_prompt_tokens: number;
  total_completion_tokens: number;
  total_tool_calls: number;
  latest_checkpoint?: {
    checkpoint_id: string;
    checkpoint_kind: string;
    checkpoint_version: string;
    node_seq: number;
    created_at: string;
  } | null;
  observability: {
    has_durable_projection: boolean;
    observability_available: boolean;
    projection_lag_events: number;
    prompt_request_count: number;
    latest_prompt_request?: {
      request_id: string;
      request_hash: string;
      message_count: number;
      tool_count: number;
      delta_counts: unknown;
    } | null;
  };
  recent_events: Array<StreamEvent | Record<string, unknown>>;
};

export type RunProjectionRepairResponse = {
  repaired: boolean;
  projection: RunProjectionResponse;
};

export type RunInputRequestBody = {
  idempotencyKey: string;
  input?: unknown;
};

export type RunInputResponse = {
  runId: string;
  accepted: boolean;
  duplicate: boolean;
};

export type SessionInfo = {
  sessionId: string;
  createdAt: string;
  lastActive: string;
  title?: string;
  status?: string;
  userId?: string;
  agentId?: string | null;
};

// ─── Runtime wire response DTOs ───────────────────────────────────

export type RuntimeChatResponse = {
  session_id: string;
  run_id: string;
  status: string;
};

export type RuntimeSessionResponse = {
  session_id: string;
  user_id?: string;
  agent_id?: string | null;
  title?: string | null;
  metadata?: Record<string, unknown>;
  status?: string;
  event_count?: number;
  created_at: string;
  updated_at?: string | null;
  ended_at?: string | null;
};

export type RuntimeSessionListResponse = {
  sessions: RuntimeSessionResponse[];
  total?: number | null;
  limit?: number;
  next_cursor?: RuntimeSessionListCursor | null;
};

export type RuntimeSessionListCursor = {
  updated_at: string;
  session_id: string;
};

export type RuntimeSessionCreateBody = {
  agent_id?: string | null;
  title?: string | null;
  metadata?: Record<string, unknown>;
};

export type RuntimeSessionUpdateBody = {
  title?: string | null;
  metadata?: Record<string, unknown>;
  status?: string;
};

export type RuntimeSessionListParams = {
  limit?: number;
  cursor?: RuntimeSessionListCursor;
};

export type RuntimeTranscriptItemResponse = {
  session_id: string;
  item_seq: number;
  run_id?: string | null;
  role: string;
  content: string;
  reasoning?: string | null;
  reasoning_status?: string | null;
  created_at?: string;
};

export type RuntimeTranscriptResponse = {
  session_id: string;
  items: RuntimeTranscriptItemResponse[];
  next_before_seq?: number | null;
  has_more?: boolean;
};

export type RuntimeTranscriptParams = {
  before_seq?: number;
  limit?: number;
};

export type RuntimeModelAccessKind =
  | "astra_cloud"
  | "workspace"
  | "this_device"
  | "self_hosted";

export type RuntimeModelExecutionPlacement = "server" | "edge";

export type RuntimeModelListItem = {
  offering_id: string;
  access_id: string;
  access_kind: RuntimeModelAccessKind;
  access_label: string;
  execution_placement: RuntimeModelExecutionPlacement;
  name: string;
  provider: string;
  description: string | null;
  is_active: boolean;
  context_window: number;
  max_completion_tokens: number | null;
  architecture: unknown | null;
  thinking_capability: "both" | "effort_only" | "native_only" | "none" | null;
};

export type RuntimeModelListResponse = RuntimeModelListItem[];

export type RuntimeModelAccessView = {
  id: string;
  kind: RuntimeModelAccessKind;
  label: string;
  execution_placement: RuntimeModelExecutionPlacement;
  status: "ready" | "unavailable";
  available_model_count: number;
  actions: Array<"contact_administrator" | "reconnect_device">;
};

export type RuntimeModelAccessProjection = {
  accesses: RuntimeModelAccessView[];
  offerings: RuntimeModelListItem[];
  observed_at: string;
};

export type RuntimeArtifactResponse = {
  artifact_id?: string;
  artifact_kind?: string;
  source?: string | null;
  content?: unknown;
  metadata?: Record<string, unknown> | null;
  created_at?: string | null;
};

export type RuntimeArtifactListResponse = {
  artifacts?: RuntimeArtifactResponse[];
};

export type RuntimeArtifactListParams = {
  limit?: number;
  offset?: number;
};

// ─── Auth Types ────────────────────────────────────────────────────

/** Login / refresh token payload (`AuthTokenResponse` on the server). */
export type AuthResult = {
  access_token: string;
  refresh_token: string;
  token_type?: string;
  expires_in?: number;
  /** Set on `register` (`AuthRegisterResponse`). */
  user_id?: string;
  username?: string;
  email?: string;
  display_name?: string | null;
};

export type UserInfo = {
  user_id: string;
  username: string;
  email: string;
  display_name?: string | null;
};

// ─── Memory Types ──────────────────────────────────────────────────

export type MemoryEntry = {
  content: string;
  memory_type?: "semantic" | "episodic" | "procedural";
  session_id?: string;
  trust_tier?: string;
};

export type MemorySearchResult = {
  id: string;
  content: string;
  score: number;
  memory_type?: string;
  created_at?: string;
};

// ─── Skill Types ───────────────────────────────────────────────────

export type SkillInfo = {
  id: string;
  name: string;
  description: string;
  status: string;
};

export type RuntimeSkillListItem = {
  skill_id?: string;
  skill_name?: string;
  version?: string;
  description?: string | null;
  source?: string | null;
  category?: string | null;
  status?: string | null;
};

export type RuntimeSkillListResponse = {
  skills?: RuntimeSkillListItem[];
  total?: number;
  limit?: number;
  next_cursor?: RuntimeSkillListCursor | null;
};

export type RuntimeSkillListCursor = {
  skill_name: string;
  version: string;
  skill_id: string;
};

export type RuntimeSkillListParams = {
  limit?: number;
  cursor?: RuntimeSkillListCursor | null;
};

/** JSON body for `POST /skills` — matches services `RegisterSkillRequest`. */
export type RegisterSkillBody = {
  skill_id?: string;
  skill_name: string;
  skill_version: string;
  skill_code?: string;
  skill_type?: string;
  remote_url?: string | null;
  description?: string | null;
  metadata?: unknown;
};

/** JSON body for `POST /skills/publish` — matches services `PublishSkillRequest`. */
export type PublishSkillBody = {
  name: string;
  version: string;
  description: string;
  dependencies?: string[];
  manifest?: unknown;
  skill_type?: string;
  remote_url?: string | null;
  category?: string;
  priority?: number;
};

/** `GET /skills/{id}` response — matches services `SkillRecord`. */
export type SkillRecord = {
  skill_id: string;
  skill_name: string;
  version: string;
  description?: string | null;
  metadata?: unknown;
  created_at?: string | null;
};

// ─── Audit Types ───────────────────────────────────────────────────

export type SessionActivity = {
  timestamp: string;
  event_type: string;
  details: Record<string, unknown>;
};

/** `GET /sessions/{id}/audit/summary` — matches `SessionAuditSummary` in the services crate. */
export type SessionAuditSummary = {
  session_id: string;
  status: string;
  turn_count: number;
  tokens_in: number;
  tokens_out: number;
  tool_calls_total: number;
  tool_calls_failed: number;
  error_count: number;
  stall_count: number;
  checkpoint_count: number;
  compact_count: number;
  execution_boundary_opened_count: number;
  execution_boundary_committed_count: number;
  execution_boundary_aborted_count: number;
  approval_required_count: number;
  approval_decision_count: number;
  approval_timeout_count: number;
  models_used: string[];
  duration_secs: number;
  created_at: string;
  ended_at?: string | null;
};

// ─── Session lifecycle (HTTP) ─────────────────────────────────────

export type SessionUpdateBody = {
  title?: string;
  metadata?: Record<string, unknown>;
  status?: string;
};

/** `GET /sessions/{id}/activity` — matches `SessionActivityResponse`. */
export type SessionActivityEntryResponse = {
  log_id: string;
  action: string;
  details: Record<string, unknown>;
  created_at: string;
};

export type SessionActivityCursor = {
  created_at: string;
  log_id: string;
};

export type SessionActivityResponse = {
  session_id: string;
  activities: SessionActivityEntryResponse[];
  total: number;
  limit: number;
  next_cursor?: SessionActivityCursor | null;
};

// ─── Run list ─────────────────────────────────────────────────────

export type RunListCursor = {
  updatedAt: string;
  runId: string;
};

export type RunListParams = {
  limit?: number;
  cursor?: RunListCursor;
};

export type RunListResponse = {
  runs: RunStatus[];
  total: number | null;
  limit: number;
  nextCursor?: RunListCursor | null;
};

// ─── Delegation (multi-agent) ─────────────────────────────────────

/** `POST /chat/runs/{run_id}/delegate` body — matches `coordination::DelegationRequest`. */
export type DelegationRequestBody = {
  delegation_id: string;
  parent_run_id: string;
  task: string;
  /** `CoordinationPattern` — use structured JSON per server schema. */
  pattern: unknown;
  user_id: string;
  depth: number;
  context?: Record<string, unknown>;
};

export type DelegationAgentResultResponse = {
  agent_id: string;
  status: string;
  output?: string | null;
  error?: string | null;
};

export type DelegationResponse = {
  delegation_id: string;
  status: string;
  agent_results: DelegationAgentResultResponse[];
  aggregated_output?: string | null;
  total_prompt_tokens: number;
  total_completion_tokens: number;
  total_tool_calls: number;
};

export type DelegationListResponse = {
  parent_run_id: string;
  sub_run_ids: string[];
};

export type DelegationMutationResponse = {
  parent_run_id: string;
  affected: number;
};

// ─── Reflect / decision trace ───────────────────────────────────

export type ReflectQueryParams = {
  focus?: string;
  last_n?: number;
  question?: string;
};

/** Subset of `ReflectReport` — extended fields parsed as JSON. */
export type ReflectReport = {
  session_id: string;
  focus: string;
  overview: Record<string, unknown>;
  diagnoses: unknown[];
  insights: unknown[];
  recommendations: string[];
  reflection_context?: unknown;
  prompt_preview?: string | null;
  evidence_graph?: unknown;
};

// ─── Events (pipeline / audit trail) ──────────────────────────────

export type EventResponse = {
  event_id: string;
  user_id: string;
  session_id: string;
  event_type: string;
  content: string;
  agent_id?: string | null;
  agent_version?: string | null;
  parent_event_id?: string | null;
  parent_event_ids?: string[];
  causal_chain_id: string;
  metadata: Record<string, unknown>;
  created_at: string;
};

export type EventListResponse = {
  events: EventResponse[];
  total: number | null;
  limit: number;
  next_cursor?: EventListCursor | null;
};

export type EventListCursor = {
  created_at: string;
  event_id: string;
};

export type EventListFilters = {
  sessionId?: string;
  eventType?: string;
  agentId?: string;
  causalChainId?: string;
  limit?: number;
  cursor?: EventListCursor | null;
};

// ─── Edge status ───────────────────────────────────────────────────

export type EdgeInfo = {
  edge_agent_id: string;
  hostname?: string | null;
  workspace_dir?: string | null;
  connected_secs: number;
};

export type EdgeStatusResponse = {
  edges: EdgeInfo[];
};

// ─── §5.5 Thin protocol request bodies ─────────────────────────────

export type ToolResultRequestBody = {
  request_id: string;
  status: string;
  output?: string;
  duration_ms?: number;
};

export type ApprovalDecision = "allow" | "deny" | "allow_session";

export type ApprovalKind = "standard" | "explicit";

export type ApprovalRespondRequestBody = {
  request_id: string;
  decision: ApprovalDecision;
  reason?: string;
  session_id: string;
  run_id: string;
  tool_name?: string;
  approval_kind?: ApprovalKind;
};

export type EdgeRegisterRequestBody = {
  edge_agent_id: string;
  hostname?: string;
  worktree_path?: string;
  capabilities?: Record<string, unknown>;
};

export type EdgeHeartbeatRequestBody = {
  edge_agent_id: string;
};

export type TaskLeaseMutationRequestBody = {
  edge_agent_id: string;
  ttl_sec?: number;
};
