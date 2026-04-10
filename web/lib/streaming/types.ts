// Stream event types matching the Rust backend's SSE/WebSocket protocol.

export type StreamEventType =
  | 'session_info'
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
  | 'agent_completed';

export type SessionInfoEvent = {
  type: 'session_info';
  session_id: string;
  run_id?: string;
};

export type TextDeltaEvent = {
  type: 'text_delta';
  content: string;
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

export type ThinkingDeltaEvent = {
  type: 'reasoning_delta';
  content: string;
};

export type ThinkingDoneEvent = {
  type: 'reasoning_done';
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

export type StreamEvent =
  | SessionInfoEvent
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
  | AgentCompletedEvent;

export type ConnectionState = 'disconnected' | 'connecting' | 'connected' | 'error';
