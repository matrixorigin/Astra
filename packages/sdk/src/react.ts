// @astra/sdk/react — React-specific exports
export { useAstraChat, useAstraRun } from './hooks';
export type {
  UseAstraChatConfig,
  UseAstraChatReturn,
  UseAstraRunConfig,
  UseAstraRunReturn,
} from './hooks';

// Re-export core types for convenience
export type {
  StreamEvent,
  ConnectionState,
  ChatMessage,
  ToolCall,
  WorkspaceBinding,
  ExecutorBinding,
  PlanState,
  TokenUsage,
  SessionTask,
  AgentActivity,
  ChatConfig,
  AgentBindingSelection,
  RuntimeProfile,
  ExecutionBudget,
  AstraClientConfig,
} from './types';

export { AstraClient } from './client';
