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

export type ChatConfig = {
  sessionId?: string;
  agentId?: string;
  model?: string;
};

// ─── Client Configuration ──────────────────────────────────────────

export type AstraClientConfig = {
  baseUrl: string;
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
};

// ─── API Types ─────────────────────────────────────────────────────

export type ChatRequest = {
  message: string;
  sessionId?: string;
  model?: string;
  maxCandidates?: number;
  context?: Record<string, unknown>;
};

export type RunStatus = {
  runId: string;
  sessionId: string;
  status: 'running' | 'completed' | 'failed' | 'cancelled' | 'paused';
  eventsCount: number;
};

export type SessionInfo = {
  sessionId: string;
  createdAt: string;
  lastActive: string;
};

// ─── Auth Types ────────────────────────────────────────────────────

export type AuthResult = {
  access_token: string;
  refresh_token: string;
  user_id: string;
};

export type UserInfo = {
  user_id: string;
  username: string;
  created_at: string;
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

// ─── Audit Types ───────────────────────────────────────────────────

export type SessionActivity = {
  timestamp: string;
  event_type: string;
  details: Record<string, unknown>;
};

export type SessionAudit = {
  session_id: string;
  events: SessionActivity[];
  tool_calls: number;
  turns: number;
};
