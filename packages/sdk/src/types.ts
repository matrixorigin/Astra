// ─── SSE Event Types ────────────────────────────────────────────────
// Stream event types matching the Rust backend's SSE/WebSocket protocol.

export type StreamEventType =
  | "session_info"
  | "work_turn_started"
  | "work_task_graph_changed"
  | "context_meta"
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

export type WorkTurnStartedEventV1 = {
  type: "work_turn_started";
  schema_version: 1;
  work_id: string;
  branch_id: string;
  run_id: string;
};

/** Canonical Task Graph invalidation emitted only after a graph revision has
 * committed. Consumers re-read that revision instead of applying tool prose. */
export type WorkTaskGraphChangedEventV1 = {
  type: "work_task_graph_changed";
  schema_version: 1;
  graph_revision: number;
  branch_revision: number;
};

/** Runtime context accounting. Detailed values are versioned trace payloads;
 * consumers may retain them as evidence but must not infer lifecycle state. */
export type ContextMetaEvent = {
  type: "context_meta";
  system_prompt_tokens?: number;
  system_prompt_breakdown?: unknown;
  context_manifest_trace?: unknown;
  compactions?: unknown[];
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
  tool?: string;
  arguments?: unknown;
  result?: unknown;
  status?: string;
  success?: boolean;
  duration_ms?: number;
  error_kind?: string;
  blocked?: boolean;
  artifacts?: unknown[];
} & ExecutionBindingFields;

export type UsageEvent = {
  type: "usage";
  prompt_tokens: number;
  completion_tokens: number;
  /** `run_total` is an authoritative terminal aggregate and replaces live deltas. */
  usage_scope?: "request" | "run_total" | string;
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
  http_status?: number;
  category?: string;
  action_hints?: string[];
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
  | WorkTurnStartedEventV1
  | WorkTaskGraphChangedEventV1
  | ContextMetaEvent
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

/** Events emitted by the Work-first continuation stream. Structural session
 * identity is deliberately unrepresentable on this public surface. */
export type WorkTurnStreamEvent =
  | WorkTurnStartedEventV1
  | WorkTaskGraphChangedEventV1
  | (StreamEvent & { session_id?: never });

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
  /** Optional typed decoder for non-2xx responses before an SSE stream starts. */
  decodeHttpError?: (response: Response) => Promise<StreamEvent>;
  maxRetries?: number;
  retryDelayMs?: number;
  /** Abort the stream when no SSE event arrives within this window. Defaults to disabled. */
  heartbeatTimeoutMs?: number;
  /** Require a protocol terminal before EOF is treated as a completed stream. */
  requireTerminalEvent?: boolean;
  signal?: AbortSignal;
  /** HTTP method. Defaults to 'GET'. Use 'POST' for streaming chat endpoints. */
  method?: "GET" | "POST";
  /** Request body for POST requests. */
  body?: string;
};

// ─── API Types ─────────────────────────────────────────────────────

export type WorkBinding = {
  /** Canonical user-visible Work identity. */
  workId: string;
  /** Exact Work branch owned by the current session. */
  branchId: string;
};

export type ChatRequest = {
  message: string;
  parts?: unknown[];
  attachments?: unknown[];
  sessionId?: string;
  /** Enables typed Work planning only after the server validates owner/session/branch coherence. */
  workBinding?: WorkBinding;
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
  metadata_patch?: Record<string, unknown>;
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
  artifacts?: unknown[];
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

export type RuntimeModelCatalogCursor = {
  provider: string;
  model_name: string;
  model_id: string;
};

export type RuntimeModelListPageResponse = {
  items: RuntimeModelListItem[];
  next_cursor: RuntimeModelCatalogCursor | null;
  limit: number;
  total: number;
  catalog_revision: string;
};

export type RuntimeModelAccessStatus =
  | "setting_up"
  | "ready"
  | "degraded"
  | "action_required"
  | "unavailable"
  | "disabled";

export type RuntimeModelAccessReason =
  | "provisioning"
  | "no_eligible_offerings"
  | "reauthentication_required"
  | "billing_action_required"
  | "connection_degraded"
  | "connection_unavailable"
  | "device_offline"
  | "policy_disabled";

export type RuntimeModelAccessAction =
  | "contact_administrator"
  | "reconnect_device"
  | "configure_device_models"
  | "reauthenticate"
  | "manage_billing"
  | "retry";

export type RuntimeModelAccessView = {
  id: string;
  kind: RuntimeModelAccessKind;
  label: string;
  execution_placement: RuntimeModelExecutionPlacement;
  status: RuntimeModelAccessStatus;
  reason: RuntimeModelAccessReason | null;
  usable: boolean;
  retry_after_seconds: number | null;
  available_model_count: number;
  actions: RuntimeModelAccessAction[];
};

export type RuntimeModelDefaultSource = "astra" | "external_provider";

export type RuntimeModelDefaultScope = "effective_catalog";

export type RuntimeModelDefaultResolution =
  | {
      state: "selected";
      offering_id: string;
      source: RuntimeModelDefaultSource;
      scope: RuntimeModelDefaultScope;
    }
  | { state: "missing" }
  | {
      state: "invalid";
      reason: "invalid_offering_id" | "not_effective_offering";
    };

export type RuntimeModelAccessProjection = {
  accesses: RuntimeModelAccessView[];
  offerings: RuntimeModelListItem[];
  default_offering_id: string | null;
  default_resolution?: RuntimeModelDefaultResolution;
  next_cursor: RuntimeModelCatalogCursor | null;
  limit: number;
  total: number;
  catalog_revision: string;
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

export type ReauthenticationPurpose =
  | "device_trust"
  | "device_reenroll"
  | "session_forced_takeover";

export type ReauthenticationProof = {
  proof: string;
  purpose: ReauthenticationPurpose;
  expires_in: number;
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
  metadataPatch?: Record<string, unknown>;
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

// ─── Work-first public contract ────────────────────────────────────

export type WorkCreateCriterion =
  | {
      criterionId: string;
      kind: "command_check";
      statement: string;
      command: string;
    }
  | {
      criterionId: string;
      kind: "test_check";
      statement: string;
      command: string;
    }
  | {
      criterionId: string;
      kind: "human_review";
      statement: string;
    };

export type WorkCreateInput = {
  /** Stable identity for one logical Start Work action and its exact retries. */
  requestId: string;
  goal: string;
  /** Explicit Done-when criteria; use [] when the user has not accepted any. */
  criteria: readonly WorkCreateCriterion[];
};

export type WorkTurnInput = {
  /** Stable identity for one logical branch continuation and exact retries. */
  requestId: string;
  /** Durable read attachment that may claim idle conversation control. */
  attachmentId: string;
  message: string;
};

export type WorkContentHash = `sha256:${string}`;
export type WorkRetentionState = "active" | "archived";
export type WorkRevisionAlignment = "current" | "behind";
export type WorkObservationScope = "declared_work";
export type WorkObservationSourceKind =
  | "work"
  | "goal"
  | "criterion_set"
  | "delivery_branch"
  | "graph"
  | "work_events";

export type WorkObservationCursorV1 = {
  work_revision: number;
  goal_revision: number;
  criteria_set_revision: number;
  delivery_branch_revision: number;
  graph_revision: number;
  event_head: number;
};

export type WorkObservationSourceRevisionV1 =
  | { source: "work"; revision: number }
  | { source: "goal"; revision: number }
  | {
      source: "criterion_set";
      revision: number;
      content_hash: WorkContentHash;
    }
  | { source: "delivery_branch"; revision: number }
  | {
      source: "graph";
      revision: number;
      content_hash: WorkContentHash;
    }
  | { source: "work_events"; event_head: number };

export type WorkObservationCoverageGapV1 = {
  source: WorkObservationSourceKind;
  reason: "source_unavailable_at_causal_cut";
};

export type WorkObservationCoherenceV1 = "coherent";

export type WorkObservationFactCodeV1 =
  | "criteria_not_accepted"
  | "branch_basis_out_of_date"
  | "subject_unavailable"
  | "verification_required"
  | "ready_for_review";

export type WorkObservationCauseCodeV1 =
  | "accepted_criteria_empty"
  | "branch_basis_stale"
  | "current_subject_missing"
  | "current_evidence_incomplete"
  | "current_evidence_complete";

export type WorkObservationFindingV1 = {
  fact_code: WorkObservationFactCodeV1;
  cause_code: WorkObservationCauseCodeV1;
};

export type WorkObservationSatisfactionEvidenceRefV1 =
  | {
      kind: "check_run";
      criterion: { criterion_id: string; revision: number };
      check_run_id: string;
      payload_hash: WorkContentHash;
    }
  | {
      kind: "acceptance_decision";
      criterion: { criterion_id: string; revision: number };
      decision_id: string;
      payload_hash: WorkContentHash;
    };

export type WorkGoalOverviewV1 = {
  revision: number;
  goal: string;
};

export type WorkCriteriaSummaryV1 = {
  revision: number;
  member_count: number;
  manifest_hash: WorkContentHash;
};

export type WorkBranchOverviewV1 = {
  work_id: string;
  branch_id: string;
  branch_revision: number;
  origin_branch_id: string | null;
  fork_cursor: string | null;
  goal_revision_ref: number;
  goal_alignment: WorkRevisionAlignment;
  criteria_set_revision_ref: number;
  criteria_alignment: WorkRevisionAlignment;
  basis_graph_revision: number;
  current_graph_revision: number;
  retention_state: WorkRetentionState;
  created_at: string;
  archived_at: string | null;
};

export type WorkGraphSummaryV1 = {
  revision: number;
  item_count: number;
  edge_count: number;
  manifest_hash: WorkContentHash;
};

export type WorkDeliveryStatusV1 =
  | "criteria_not_accepted"
  | "branch_basis_out_of_date"
  | "subject_unavailable"
  | "verification_required"
  | "ready_for_review";

export type WorkDeliverySummaryV1 = {
  status: WorkDeliveryStatusV1;
  required_criterion_count: number;
  satisfied_criterion_count: number;
  fresh_check_count: number;
  accepted_gap_count: number;
  remaining_criterion_count: number;
  subject_revision: WorkContentHash | null;
  freshness_valid_until: string | null;
};

export type WorkOverviewV1 = {
  work_id: string;
  work_revision: number;
  project_id: string | null;
  original_intent_ref: string;
  goal: WorkGoalOverviewV1;
  criteria: WorkCriteriaSummaryV1;
  delivery_branch: WorkBranchOverviewV1;
  graph: WorkGraphSummaryV1;
  delivery: WorkDeliverySummaryV1;
  event_head: number;
  retention_state: WorkRetentionState;
  created_at: string;
  archived_at: string | null;
};

export type WorkObservationReportV1 = {
  schema_version: 1;
  report_id: `work-observation:${string}`;
  content_hash: WorkContentHash;
  scope: WorkObservationScope;
  as_of: WorkObservationCursorV1;
  source_revisions: WorkObservationSourceRevisionV1[];
  coherence: WorkObservationCoherenceV1;
  coverage_gaps: WorkObservationCoverageGapV1[];
  finding: WorkObservationFindingV1;
  satisfaction_evidence_refs: WorkObservationSatisfactionEvidenceRefV1[];
  overview: WorkOverviewV1;
};

export type WorkCatalogAttentionV1 = "needs_review" | "updated" | "none";
export type WorkBranchActivityV1 = "working" | "waiting" | "paused" | "idle";

export type WorkCatalogCursorV1 = {
  created_at: string;
  work_id: string;
};

export type WorkCatalogEntryV1 = {
  work_id: string;
  goal: string;
  work_revision: number;
  delivery_branch_id: string;
  delivery_branch_revision: number;
  graph_revision: number;
  graph_item_count: number;
  pending_decision_count: number;
  event_head: number;
  seen_through_event_seq: number | null;
  unseen_event_count: number;
  attention: WorkCatalogAttentionV1;
  delivery_branch_activity: WorkBranchActivityV1;
  created_at: string;
  last_activity_at: string;
};

export type WorkCatalogPageV1 = {
  schema_version: 1;
  entries: WorkCatalogEntryV1[];
  next_cursor: WorkCatalogCursorV1 | null;
};

export type WorkConversationHeadV1 = {
  completed_turn: number;
  journal_event_seq: number;
  conversation_seq: number;
  canonical_root_hash: string;
  projection_schema: number;
  compaction_generation: number;
  config_version_id: string | null;
};

export type WorkBranchSyncStateV1 =
  | "current"
  | "projection_stale"
  | "degraded"
  | "corrupt"
  | "offline";

export type WorkTranscriptItemV1 = {
  item_seq: number;
  committed_turn: number;
  role: string;
  content: string;
  content_truncated: boolean;
  payload: unknown | null;
  payload_omitted: boolean;
  content_hash: string;
  created_at: string;
};

export type WorkTranscriptPageV1 = {
  schema_version: 1;
  work_id: string;
  branch_id: string;
  sync: WorkBranchSyncStateV1;
  canonical_head: WorkConversationHeadV1 | null;
  transcript_cursor: WorkConversationHeadV1 | null;
  items: WorkTranscriptItemV1[];
  next_before_item_seq: number | null;
  has_more: boolean;
};

export type WorkBranchAttachmentV1 = {
  schema_version: 1;
  work_id: string;
  branch_id: string;
  attachment_id: string;
  attachment_epoch: number;
  branch_revision: number;
  mode: "read_only" | "controller";
  sync: "current";
  control_basis: WorkBranchControlBasisV1;
  head: WorkConversationHeadV1 | null;
  attached_at: string;
  expires_at: string;
};

export type WorkBranchControlBasisV1 = {
  writer_epoch: number;
  canonical_root_hash: string | null;
};

export type WorkBranchControlCommand =
  | { kind: "acquire_branch_control"; attachmentId: string }
  | { kind: "force_takeover"; attachmentId: string; reauthenticationProof: string }
  | { kind: "release_branch_control"; attachmentId: string };

export type WorkBranchControlOperationV2 = {
  schema_version: 2;
  operation_id: string;
  work_id: string;
  branch_id: string;
  attachment_id: string;
  kind: "acquire_branch_control" | "force_takeover" | "release_branch_control";
  state: "pending" | "aborted" | "succeeded" | "conflict";
  outcome:
    | "pending"
    | "aborted"
    | "acquired"
    | "already_controlled"
    | "taken_over"
    | "released"
    | "already_released"
    | "writer_conflict"
    | "branch_revision_conflict"
    | "head_conflict";
  branch_revision: number;
  control_basis: WorkBranchControlBasisV1 | null;
  progress?: {
    phase: "awaiting_reauthentication" | "preparing" | "fencing" | "sealing_effects" | "activating";
    abortable: boolean;
  };
  created_at: string;
  completed_at: string | null;
};

export type WorkBranchCreationInputV1 = {
  requestId: string;
  expectedBranchRevision: number;
  committedCursor: WorkConversationHeadV1;
};

export type WorkBranchCreationOperationV1 = {
  schema_version: 1;
  operation_id: string;
  work_id: string;
  origin_branch_id: string;
  child_branch_id: string;
  fork_cursor: WorkContentHash;
  state: "pending" | "aborted" | "succeeded" | "conflict";
  outcome:
    | "pending"
    | "aborted"
    | "created"
    | "branch_revision_conflict"
    | "cursor_conflict"
    | "capacity_exceeded";
  origin_branch_revision: number;
  created_at: string;
  completed_at: string | null;
};

export type WorkBranchDeletionInputV1 = {
  requestId: string;
  expectedWorkRevision: number;
  expectedBranchRevision: number;
};

export type WorkBranchDeletionOperationV1 = {
  schema_version: 1;
  operation_id: string;
  work_id: string;
  branch_id: string;
  state: "pending" | "succeeded" | "conflict";
  phase: "fence" | "session_cleanup" | "lineage_gc" | "branch_cleanup" | "complete";
  outcome:
    | "pending"
    | "deleted"
    | "delivery_branch_protected"
    | "work_revision_conflict"
    | "branch_revision_conflict";
  work_revision: number;
  branch_revision: number;
  created_at: string;
  completed_at: string | null;
};

export type WorkBranchCatalogEntryV1 = {
  branch_id: string;
  branch_revision: number;
  is_delivery: boolean;
  origin_branch_id: string | null;
  fork_cursor: WorkContentHash | null;
  goal_revision_ref: number;
  criteria_set_revision_ref: number;
  basis_graph_revision: number;
  current_graph_revision: number;
  materialization: WorkBranchDimensionSummaryV1[] | null;
  created_at: string;
};

export type WorkBranchDimensionV1 =
  | "conversation"
  | "goal"
  | "criteria"
  | "task_graph"
  | "checkpoint"
  | "workspace"
  | "artifacts"
  | "transient_authority";

export type WorkBranchDimensionDispositionV1 =
  | "shared"
  | "copied"
  | "rebased"
  | "excluded"
  | "gap";

export type WorkBranchDimensionSummaryV1 = {
  dimension: WorkBranchDimensionV1;
  disposition: WorkBranchDimensionDispositionV1;
};

export type WorkBranchCatalogV1 = {
  schema_version: 1;
  work_id: string;
  work_revision: number;
  delivery_branch_id: string;
  branches: WorkBranchCatalogEntryV1[];
};

export type WorkArchivedBranchCursorV1 = {
  archived_at: string;
  branch_id: string;
};

export type WorkArchivedBranchEntryV1 = {
  branch_id: string;
  branch_revision: number;
  origin_branch_id: string | null;
  archived_at: string;
  created_at: string;
};

export type WorkArchivedBranchPageV1 = {
  schema_version: 1;
  work_id: string;
  work_revision: number;
  branches: WorkArchivedBranchEntryV1[];
  next_cursor: WorkArchivedBranchCursorV1 | null;
};

export type WorkArchivedBranchListParamsV1 = {
  before?: WorkArchivedBranchCursorV1;
  limit?: number;
};

export type WorkBranchComparisonRelationV2 = "same" | "different" | "unavailable";
export type WorkBranchComparisonBlockerV2 =
  | "goal_revision_differs"
  | "criteria_revision_differs";
export type WorkBranchComparisonCoverageGapV2 =
  | "change_details"
  | "risks"
  | "time_cost";

export type WorkBranchComparisonSideV2 = {
  branch_id: string;
  branch_revision: number;
  is_delivery: boolean;
  goal_revision_ref: number;
  criteria: { revision: number; manifest_hash: WorkContentHash; member_count: number };
  graph: {
    basis_revision: number;
    current_revision: number;
    manifest_hash: WorkContentHash;
    item_count: number;
    edge_count: number;
  };
  subject: {
    subject_ref: string;
    subject_revision: WorkContentHash;
    graph_revision: number;
  } | null;
};

export type WorkBranchComparisonEvidenceV2 = {
  manifest_hash: WorkContentHash;
  required_count: number;
  fresh_check_count: number;
  accepted_gap_count: number;
};

export type WorkBranchComparisonReportV2 = {
  schema_version: 2;
  work_id: string;
  work_revision: number;
  directly_comparable: boolean;
  blockers: WorkBranchComparisonBlockerV2[];
  graph_relation: WorkBranchComparisonRelationV2;
  subject_relation: WorkBranchComparisonRelationV2;
  evidence_relation: WorkBranchComparisonRelationV2;
  left: WorkBranchComparisonSideV2;
  right: WorkBranchComparisonSideV2;
  left_evidence: WorkBranchComparisonEvidenceV2;
  right_evidence: WorkBranchComparisonEvidenceV2;
  coverage_gaps: WorkBranchComparisonCoverageGapV2[];
};

export type WorkPatchArtifactV1 = {
  schema_version: 1;
  work_id: string;
  branch_id: string;
  patch_artifact_id: string;
  source_branch_revision: number;
  source_graph_revision: number;
  base_subject_revision: WorkContentHash;
  result_subject_revision: WorkContentHash;
  payload_hash: WorkContentHash;
  payload_bytes: number;
  format: "unified_diff_v1";
  provider_invocation_ref: string;
  source_ref: string;
  created_at: string;
};

export type WorkPatchArtifactExportInputV1 = {
  requestId: string;
  expectedBranchRevision: number;
  expectedGraphRevision: number;
};

export type WorkPatchArtifactContent = {
  data: string;
  hash: WorkContentHash;
  bytes: number;
};

export type WorkPatchArtifactCursorV1 = {
  created_at: string;
  patch_artifact_id: string;
};

export type WorkPatchArtifactPageV1 = {
  schema_version: 1;
  work_id: string;
  branch_id: string;
  artifacts: WorkPatchArtifactV1[];
  next_cursor: WorkPatchArtifactCursorV1 | null;
};

export type WorkPatchArtifactListParamsV1 = {
  before?: WorkPatchArtifactCursorV1;
  limit?: number;
};

export type WorkPatchMaterializationInputV1 = {
  requestId: string;
  patchArtifactId: string;
  expectedTargetBranchRevision: number;
  expectedTargetGraphRevision: number;
};

export type WorkPatchMaterializationCursorV1 = {
  created_at: string;
  operation_id: string;
};

export type WorkPatchMaterializationListParamsV1 = {
  sourceBranchId: string;
  before?: WorkPatchMaterializationCursorV1;
  limit?: number;
};

export type WorkPatchMaterializationOperationV2 = {
  schema_version: 2;
  operation_id: string;
  work_id: string;
  request_id: string;
  patch_artifact_id: string;
  source_branch_id: string;
  target_branch_id: string;
  target_branch_revision: number;
  target_graph_revision: number;
  base_subject_revision: WorkContentHash;
  result_subject_revision: WorkContentHash;
  payload_hash: WorkContentHash;
  provider_ref: string;
  policy_decision_ref: string;
  state: "pending" | "aborted" | "succeeded" | "conflict" | "failed";
  phase: "awaiting_dispatch" | "applying" | "reconciling" | "verifying" | "complete";
  apply_invocation_ref: string | null;
  observed_subject_revision: WorkContentHash | null;
  apply_outcome:
    | "applied"
    | "not_applied"
    | "result_mismatch"
    | "target_changed"
    | null;
  failure_code:
    | "provider_unavailable"
    | "authorization_denied"
    | "workspace_unavailable"
    | "patch_rejected"
    | "invocation_cancelled"
    | "provider_internal"
    | null;
  verification_evidence_hash: WorkContentHash | null;
  verification_outcome: "passed" | "target_changed" | null;
  created_at: string;
  completed_at: string | null;
};

export type WorkPatchMaterializationPageV2 = {
  schema_version: 2;
  work_id: string;
  target_branch_id: string;
  source_branch_id: string;
  operations: WorkPatchMaterializationOperationV2[];
  next_cursor: WorkPatchMaterializationCursorV1 | null;
};

export type WorkPatchCommitInputV1 = {
  requestId: string;
  patchArtifactId: string;
  expectedTargetBranchRevision: number;
  expectedTargetGraphRevision: number;
  message: string;
};

export type WorkPatchCommitCursorV1 = {
  created_at: string;
  operation_id: string;
};

export type WorkPatchCommitListParamsV1 = {
  before?: WorkPatchCommitCursorV1;
  limit?: number;
};

export type WorkPatchCommitOperationV1 = {
  schema_version: 1;
  operation_id: string;
  work_id: string;
  request_id: string;
  patch_artifact_id: string;
  source_branch_id: string;
  target_branch_id: string;
  target_branch_revision: number;
  target_graph_revision: number;
  base_subject_revision: WorkContentHash;
  result_subject_revision: WorkContentHash;
  payload_hash: WorkContentHash;
  message: string;
  provider_ref: string;
  policy_decision_ref: string;
  state: "pending" | "aborted" | "succeeded" | "conflict" | "failed";
  phase: "awaiting_dispatch" | "committing" | "reconciling" | "complete";
  commit_invocation_ref: string | null;
  commit_sha: string | null;
  observed_subject_revision: WorkContentHash | null;
  index_reconciled: boolean | null;
  failure_code:
    | "authorization_denied"
    | "workspace_unavailable"
    | "provider_unavailable"
    | "invalid_metadata"
    | "base_changed"
    | "result_changed"
    | "patch_rejected"
    | "commit_rejected"
    | "ref_conflict"
    | null;
  created_at: string;
  completed_at: string | null;
};

export type WorkPatchCommitPageV1 = {
  schema_version: 1;
  work_id: string;
  target_branch_id: string;
  operations: WorkPatchCommitOperationV1[];
  next_cursor: WorkPatchCommitCursorV1 | null;
};

export type WorkDeliverySelectionSubjectV1 = {
  graphRevision: number;
  subjectRef: string;
  subjectRevision: WorkContentHash;
};

export type WorkDeliverySelectionInputV1 = {
  requestId: string;
  branchId: string;
  expectedWorkRevision: number;
  expectedBranchRevision: number;
  expectedGoalRevision: number;
  expectedCriteriaSetRevision: number;
  expectedGraphRevision: number;
  expectedSubject: WorkDeliverySelectionSubjectV1 | null;
  expectedEvidenceManifestHash: WorkContentHash;
};

export type WorkDeliverySelectionReceiptV1 = {
  schema_version: 1;
  work_id: string;
  request_id: string;
  delivery_branch_id: string;
  work_revision: number;
  branch_revision: number;
  graph_revision: number;
  evidence_manifest_hash: WorkContentHash;
  outcome: "selected" | "already_selected";
};

export type WorkBranchRetentionInputV1 = {
  requestId: string;
  expectedWorkRevision: number;
  expectedBranchRevision: number;
};

export type WorkBranchRetentionReceiptV1 = {
  schema_version: 1;
  work_id: string;
  branch_id: string;
  request_id: string;
  kind: "archive" | "restore";
  work_revision: number;
  branch_revision: number;
  outcome: "applied" | "already_in_state";
};

export type WorkReadCursorReceiptV1 = {
  schema_version: 1;
  work_id: string;
  through_event_seq: number;
  receipt_revision: number;
  receipt_hash: WorkContentHash;
  updated_at: string;
};

export type WorkEventKind =
  | "work_created"
  | "goal_revised"
  | "criteria_accepted"
  | "branch_basis_adopted"
  | "graph_replaced"
  | "delivery_branch_selected"
  | "branch_archived"
  | "branch_restored"
  | "subject_changed"
  | "patch_artifact_exported"
  | "plan_proposed"
  | "criteria_proposed"
  | "proposal_rejected"
  | "check_recorded"
  | "gaps_accepted"
  | "run_completed"
  | "run_delegated"
  | "run_failed"
  | "run_cancelled"
  | "runtime_events_expired";

export type WorkEventRecordV1 = {
  event_seq: number;
  branch_id: string | null;
  kind: WorkEventKind;
  work_revision: number | null;
  goal_revision: number | null;
  criterion_set_revision: number | null;
  branch_revision: number | null;
  graph_revision: number | null;
  source_ref: string;
  created_at: string;
};

export type WorkEventPageV1 = {
  schema_version: 1;
  work_id: string;
  requested_after_event_seq: number | null;
  next_after_event_seq: number | null;
  event_head: number;
  retained_from_event_seq: number;
  seen_through_event_seq: number | null;
  coverage: "complete" | "expired";
  has_more: boolean;
  events: WorkEventRecordV1[];
};

export type WorkCriteriaBasisV1 = {
  work_id: string;
  work_revision: number;
  criteria_set_revision: number;
  manifest_hash: WorkContentHash;
  member_count: number;
};

export type WorkCriterionV1 = {
  criterion_id: string;
  revision: number;
  definition_hash: WorkContentHash;
} & (
  | { kind: "command_check"; statement: string; command: string }
  | { kind: "test_check"; statement: string; command: string }
  | { kind: "human_review"; statement: string }
);

export type WorkCriteriaCursorV1 = {
  criteria_set_revision: number;
  offset: number;
};

export type WorkCriteriaPageV1 = {
  schema_version: 1;
  basis: WorkCriteriaBasisV1;
  cursor: WorkCriteriaCursorV1;
  next_cursor: WorkCriteriaCursorV1 | null;
  criteria: {
    offset: number;
    limit: number;
    total: number;
    entries: WorkCriterionV1[];
  };
};

export type WorkProposalSourceKind = "model" | "reflection";
export type WorkProposalStatus =
  | "pending"
  | "accepted"
  | "rejected"
  | "stale"
  | "superseded"
  | "expired";

export type WorkCriteriaProposalBasisV1 = {
  work_revision: number;
  goal_revision: number;
  criteria_set_revision: number;
  branch_revision: number;
  graph_revision: number;
};

export type WorkCriteriaProposalMemberV1 =
  | {
      member_kind: "existing";
      criterion_id: string;
      revision: number;
    }
  | {
      member_kind: "new";
      criterion_id: string;
      definition:
        | { kind: "command_check"; statement: string; command: string }
        | { kind: "test_check"; statement: string; command: string }
        | { kind: "human_review"; statement: string };
    };

export type WorkCriteriaProposalSummaryV1 = {
  work_id: string;
  branch_id: string;
  proposal_id: string;
  proposal_seq: number;
  payload_hash: WorkContentHash;
  status: WorkProposalStatus;
  basis: WorkCriteriaProposalBasisV1;
  member_count: number;
  source_kind: WorkProposalSourceKind;
  proposed_at: string;
  expires_at: string;
};

export type WorkCriteriaProposalResolutionV1 = {
  resolution_ref: string;
  resolved_at: string;
  result_work_revision: number | null;
  result_criteria_set_revision: number | null;
};

export type WorkCriteriaProposalDetailV1 = {
  schema_version: 1;
  proposal: WorkCriteriaProposalSummaryV1;
  members: WorkCriteriaProposalMemberV1[];
  resolution: WorkCriteriaProposalResolutionV1 | null;
};

export type WorkCriteriaProposalListV1 = {
  schema_version: 1;
  work_id: string;
  branch_id: string;
  proposals: WorkCriteriaProposalSummaryV1[];
};

export type WorkCriteriaProposalDecisionInput = {
  /** Stable identity for one exact accept or reject action and its retries. */
  requestId: string;
  decision: "accept" | "reject";
};

export type WorkTaskGraphBasisV1 = {
  work_id: string;
  work_revision: number;
  goal_revision: number;
  goal: string;
  criteria_set_revision: number;
  criteria_member_count: number;
  criteria_manifest_hash: WorkContentHash;
  branch_id: string;
  branch_revision: number;
  branch_goal_revision: number;
  branch_criteria_set_revision: number;
  branch_basis_graph_revision: number;
  graph_revision: number;
  graph_item_count: number;
  graph_edge_count: number;
  graph_manifest_hash: WorkContentHash;
};

/** Constant-size bootstrap from an already-known session to public Work identity. */
export type WorkSessionBindingV1 = {
  schema_version: 1;
  work_id: string;
  branch_id: string;
  graph_revision: number;
};

export type WorkTaskGraphItemV2 = {
  item_id: string;
  revision: number;
  kind: "milestone" | "task";
  objective: string;
  expected_result: string;
  declaration_state: "active" | "superseded" | "cancelled";
  execution: WorkItemExecutionV1;
  delivery: WorkItemDeliveryV1;
  verification: WorkItemVerificationV1;
};

export type WorkItemDeliveryStatusV1 =
  | "unreported"
  | "delivered"
  | "blocked"
  | "failed";

export type WorkItemDeliveryBlockerKindV1 =
  | "capability_unavailable"
  | "dependency_blocked"
  | "policy_blocked"
  | "external_unavailable";

/** Typed result of the exact run attempt. This is independent of both the
 * terminal execution state and verification evidence. */
export type WorkItemDeliveryV1 = {
  status: WorkItemDeliveryStatusV1;
  summary: string | null;
  blocker_kind: WorkItemDeliveryBlockerKindV1 | null;
  unavailable_capabilities: string[];
};

export type WorkItemExecutionStatusV1 =
  | "not_started"
  | "running"
  | "waiting"
  | "paused"
  | "completed"
  | "delegated"
  | "failed"
  | "cancelled";

export type WorkItemExecutionRunRefV1 = {
  run_id: string;
  attempt_id: string;
  graph_revision: number;
  run_generation: number;
  last_event_idx: number;
  updated_at: string;
};

export type WorkItemExecutionV1 =
  | {
      status: "not_started";
      terminal: false;
      run: null;
    }
  | {
      status: "running" | "waiting" | "paused";
      terminal: false;
      run: WorkItemExecutionRunRefV1;
    }
  | {
      status: "completed" | "delegated" | "failed" | "cancelled";
      terminal: true;
      run: WorkItemExecutionRunRefV1;
    };

export type WorkCheckFreshnessV1 =
  | "current"
  | "criteria_changed"
  | "graph_changed"
  | "subject_unavailable"
  | "subject_changed"
  | "expired";

export type WorkItemVerificationStatusV1 =
  | "unknown"
  | "evidence_available"
  | "stale_evidence";

export type WorkItemCheckFactV1 = {
  check_run_id: string;
  criterion: { criterion_id: string; revision: number };
  criterion_set_revision: number;
  graph_revision: number;
  verifier_kind: "command" | "test";
  outcome: "passed" | "failed" | "error" | "cancelled";
  coverage: "complete" | "partial" | "unavailable";
  subject_revision: WorkContentHash;
  evidence_ref_count: number;
  produced_at: string;
  expires_at: string | null;
  freshness: WorkCheckFreshnessV1;
};

export type WorkItemVerificationV1 = {
  status: WorkItemVerificationStatusV1;
  latest_check: WorkItemCheckFactV1 | null;
};

export type WorkTaskGraphDependencyV1 = {
  predecessor_item_id: string;
  successor_item_id: string;
  kind: "dependency";
};

export type WorkTaskGraphCursorV1 = {
  graph_revision: number;
  item_offset: number;
  dependency_offset: number;
};

export type WorkTaskGraphPageV2 = {
  schema_version: 2;
  scope: "declared_work";
  basis: WorkTaskGraphBasisV1;
  cursor: WorkTaskGraphCursorV1;
  next_cursor: WorkTaskGraphCursorV1 | null;
  items: {
    offset: number;
    limit: number;
    total: number;
    entries: WorkTaskGraphItemV2[];
  };
  dependencies: {
    offset: number;
    limit: number;
    total: number;
    entries: WorkTaskGraphDependencyV1[];
  };
};

export type WorkApiErrorV1 = {
  code:
    | "unsupported_client_version"
    | "authentication_required"
    | "authentication_rejected"
    | "authentication_context_invalid"
    | "invalid_work_create_request"
    | "invalid_work_goal"
    | "invalid_work_criteria"
    | "invalid_work_id"
    | "invalid_work_branch_id"
    | "invalid_work_turn_request"
    | "invalid_work_session_binding"
    | "invalid_work_task_graph_query"
    | "invalid_work_task_graph_cursor"
    | "invalid_work_criteria_query"
    | "invalid_work_criteria_cursor"
    | "invalid_work_proposal_id"
    | "invalid_work_proposal_decision"
    | "invalid_work_read_cursor_request"
    | "invalid_work_catalog_query"
    | "invalid_work_catalog_cursor"
    | "invalid_work_catalog_limit"
    | "invalid_work_attachment_request"
    | "invalid_work_control_request"
    | "invalid_work_event_cursor"
    | "invalid_work_event_query"
    | "invalid_work_event_limit"
    | "work_not_found"
    | "branch_not_found"
    | "work_session_binding_not_found"
    | "work_read_unavailable"
    | "work_write_unavailable"
    | "work_event_cursor_ahead"
    | "work_graph_revision_conflict"
    | "work_criteria_revision_conflict"
    | "work_criteria_proposal_not_found"
    | "work_proposal_basis_conflict"
    | "work_proposal_already_resolved"
    | "work_proposal_identity_conflict"
    | "work_creation_conflict"
    | "idempotency_mismatch"
    | "writer_conflict"
    | "provider_unavailable"
    | "work_turn_rejected"
    | "work_turn_unavailable"
    | "work_attach_unavailable"
    | "work_attachment_capacity"
    | "attachment_fenced"
    | "attachment_in_use"
    | "control_operation_terminal"
    | "control_operation_not_found"
    | "control_operation_unavailable"
    | "causal_projection_degraded";
  category:
    | "authentication"
    | "invalid_request"
    | "not_found"
    | "conflict"
    | "version"
    | "availability"
    | "degraded";
  retryable: boolean;
  action_hints: (
    | "upgrade_client"
    | "refresh_work"
    | "retry_read"
    | "retry_write"
    | "retry_attach"
  )[];
  request_id?: string;
};
