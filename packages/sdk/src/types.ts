// ─── SSE Event Types ────────────────────────────────────────────────
// Stream event types matching the Rust backend's SSE/WebSocket protocol.

export type StreamEventType =
  | 'session_info'
  | 'run_started'
  | 'run_paused'
  | 'run_resumed'
  | 'run_finished'
  | 'run_cancelled'
  | 'text_delta'
  | 'reasoning_delta'
  | 'reasoning_done'
  | 'tool_call_start'
  | 'tool_call_end'
  | 'usage'
  | 'turn_complete'
  | 'error'
  | 'warning'
  | 'explain'
  | 'plan_created'
  | 'plan_revised'
  | 'plan_step_start'
  | 'plan_step_done'
  | 'agent_delegated'
  | 'agent_spawned'
  | 'agent_progress'
  | 'agent_completed'
  | 'tool_approval_request'
  | 'tool_execution_started'
  | 'tool_output_delta'
  | 'tool_execution_completed';

export type SessionInfoEvent = {
  type: 'session_info';
  session_id: string;
  run_id?: string;
};

export type RunStartedEvent = {
  type: 'run_started';
  run_id?: string;
  session_id?: string;
};

export type RunPausedEvent = {
  type: 'run_paused';
  run_id?: string;
};

export type RunResumedEvent = {
  type: 'run_resumed';
  run_id?: string;
};

export type RunFinishedEvent = {
  type: 'run_finished';
  run_id?: string;
  status?: string;
  error?: string | null;
};

export type RunCancelledEvent = {
  type: 'run_cancelled';
  run_id: string;
};

export type TextDeltaEvent = {
  type: 'text_delta';
  content: string;
};

export type ThinkingDeltaEvent = {
  type: 'reasoning_delta';
  content: string;
};

export type ThinkingDoneEvent = {
  type: 'reasoning_done';
};

export type ToolCallStartEvent = {
  type: 'tool_call_start';
  tool: string;
  call_id: string;
  arguments?: string;
};

export type ToolCallEndEvent = {
  type: 'tool_call_end';
  call_id: string;
  result?: string;
};

export type UsageEvent = {
  type: 'usage';
  prompt_tokens: number;
  completion_tokens: number;
  cache_creation_tokens?: number;
  cache_read_tokens?: number;
  // Also accept alternative field names from backend
  cache_creation_input_tokens?: number;
  cache_read_input_tokens?: number;
};

export type TurnCompleteEvent = {
  type: 'turn_complete';
  followup_suggestion?: string;
};

export type StreamErrorEvent = {
  type: 'error';
  code?: string;
  message: string;
  retryable?: boolean;
  retry_after_ms?: number;
};

export type WarningEvent = {
  type: 'warning';
  message: string;
  claims_failed?: number;
};

export type ExplainEvent = {
  type: 'explain';
  content: string;
};

export type PlanCreatedEvent = {
  type: 'plan_created';
  plan: {
    plan_id?: string;
    title?: string;
    subtasks: Array<{ id: string; title: string; status?: string }>;
  };
};

export type PlanRevisedEvent = {
  type: 'plan_revised';
  plan: {
    plan_id?: string;
    title?: string;
    subtasks: Array<{ id: string; title: string; status?: string }>;
  };
};

export type PlanStepStartEvent = {
  type: 'plan_step_start';
  step: string;
  subtask_id?: string;
};

export type PlanStepDoneEvent = {
  type: 'plan_step_done';
  step: string;
  subtask_id?: string;
  result?: string;
};

export type AgentDelegatedEvent = {
  type: 'agent_delegated';
  agent_id: string;
  task: string;
};

export type AgentSpawnedEvent = {
  type: 'agent_spawned';
  agent_id: string;
  run_id: string;
  parent_run_id: string;
  agent_type: string;
  description: string;
  timestamp?: number;
};

export type AgentProgressEvent = {
  type: 'agent_progress';
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

export type AgentCompletedEvent = {
  type: 'agent_completed';
  agent_id: string;
  status: 'completed' | 'failed' | 'cancelled';
  result_summary?: string;
  error?: string;
  reason?: string;
  total_tool_calls?: number;
  total_tokens?: { prompt: number; completion: number };
  duration_ms?: number;
  timestamp?: number;
};

export type ToolApprovalRequestEvent = {
  type: 'tool_approval_request';
  request_id: string;
  tool: string;
  args: Record<string, unknown>;
};

export type ToolExecutionStartedEvent = {
  type: 'tool_execution_started';
  call_id: string;
  tool: string;
};

export type ToolOutputDeltaEvent = {
  type: 'tool_output_delta';
  call_id: string;
  content: string;
};

export type ToolExecutionCompletedEvent = {
  type: 'tool_execution_completed';
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
  | TextDeltaEvent
  | ThinkingDeltaEvent
  | ThinkingDoneEvent
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
  | AgentProgressEvent
  | AgentCompletedEvent
  | ToolApprovalRequestEvent
  | ToolExecutionStartedEvent
  | ToolOutputDeltaEvent
  | ToolExecutionCompletedEvent) & { index?: number };

export type ConnectionState = 'disconnected' | 'connecting' | 'connected' | 'error';

// ─── Chat / Workspace Types ────────────────────────────────────────

export type ChatRole = 'user' | 'assistant' | 'system';

export type ToolCall = {
  callId: string;
  tool: string;
  arguments?: string;
  result?: string;
  status: 'running' | 'done' | 'error';
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
  status: 'pending' | 'running' | 'done' | 'error';
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

export type ChatConfig = {
  sessionId?: string;
  agentId?: string;
  model?: string;
  /** When set and non-empty, sent as `allow_skills` on chat requests. */
  allowSkills?: string[];
  /** When set and non-empty, sent as `allow_tools` on chat requests. */
  allowTools?: string[];
  /** Catalog surfacing — sent as `skill_search` (snake_case fields on the wire). */
  skillSearch?: SkillSearchSettings;
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
  onTokenRefresh?: (tokens: { accessToken: string; refreshToken: string }) => void;
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
  signal?: AbortSignal;
  /** HTTP method. Defaults to 'GET'. Use 'POST' for streaming chat endpoints. */
  method?: 'GET' | 'POST';
  /** Request body for POST requests. */
  body?: string;
};

// ─── API Types ─────────────────────────────────────────────────────

export type ChatRequest = {
  message: string;
  sessionId?: string;
  agentId?: string;
  model?: string;
  maxCandidates?: number;
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
  skillSearch?: SkillSearchSettings;
};

export type RunStatus = {
  runId: string;
  sessionId: string;
  status: 'running' | 'completed' | 'failed' | 'cancelled' | 'paused' | string;
  eventsCount: number;
  waitingFor?: string | null;
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

// ─── Auth Types ────────────────────────────────────────────────────

/** Login / refresh token payload (`AuthTokenResponse` on the server). */
export type AuthResult = {
  access_token: string;
  refresh_token: string;
  token_type: string;
  expires_in: number;
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
  memory_type?: 'semantic' | 'episodic' | 'procedural';
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
  triggers?: string[];
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

export type SessionActivityResponse = {
  session_id: string;
  activities: SessionActivityEntryResponse[];
  total: number;
};

// ─── Run list ─────────────────────────────────────────────────────

export type RunListResponse = {
  runs: RunStatus[];
  total: number;
  limit: number;
  offset: number;
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
  total: number;
  limit: number;
  offset: number;
};

export type EventListFilters = {
  sessionId?: string;
  eventType?: string;
  agentId?: string;
  causalChainId?: string;
  limit?: number;
  offset?: number;
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

export type ApprovalDecision = 'allow' | 'deny' | 'allow_session';

export type ApprovalKind = 'standard' | 'explicit';

export type ApprovalRespondRequestBody = {
  request_id: string;
  decision: ApprovalDecision;
  reason?: string;
  session_id?: string;
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
