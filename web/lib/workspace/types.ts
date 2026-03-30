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

export type ChatMessage = {
  id: string;
  role: ChatRole;
  content: string;
  toolCalls?: ToolCall[];
  timestamp: number;
  /** Whether this message is still being streamed. */
  streaming?: boolean;
};

export type WorkspaceState = {
  sessionId: string | null;
  runId: string | null;
  messages: ChatMessage[];
  toolCalls: ToolCall[];
  isStreaming: boolean;
  error: string | null;
};

export type ChatConfig = {
  apiUrl: string;
  token: string;
  sessionId?: string;
  agentId?: string;
  model?: string;
};
