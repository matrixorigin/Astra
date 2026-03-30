// Stream event types matching the Rust backend's SSE/WebSocket protocol.

export type StreamEventType =
  | 'session_info'
  | 'text_delta'
  | 'tool_call_start'
  | 'tool_call_end'
  | 'usage'
  | 'turn_complete'
  | 'error'
  | 'warning'
  | 'explain';

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

export type StreamEvent =
  | SessionInfoEvent
  | TextDeltaEvent
  | ToolCallStartEvent
  | ToolCallEndEvent
  | UsageEvent
  | TurnCompleteEvent
  | StreamErrorEvent
  | WarningEvent
  | ExplainEvent;

export type ConnectionState = 'disconnected' | 'connecting' | 'connected' | 'error';
