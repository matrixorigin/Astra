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
  agentEvents: import('@/lib/streaming/types').StreamEvent[];
};

export type ChatConfig = {
  sessionId?: string;
  agentId?: string;
  model?: string;
};
