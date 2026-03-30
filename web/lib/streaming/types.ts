// Stream event types matching the Rust backend's SSE/WebSocket protocol.

export type StreamEventType =
  | 'session_info'
  | 'text_delta'
  | 'thinking_delta'
  | 'thinking_done'
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
  type: 'thinking_delta';
  content: string;
};

export type ThinkingDoneEvent = {
  type: 'thinking_done';
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

export type AgentProgressEvent = {
  type: 'agent_progress';
  agent_id: string;
  progress: string;
};

export type AgentCompletedEvent = {
  type: 'agent_completed';
  agent_id: string;
  result: string;
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
  | AgentProgressEvent
  | AgentCompletedEvent;

export type ConnectionState = 'disconnected' | 'connecting' | 'connected' | 'error';
