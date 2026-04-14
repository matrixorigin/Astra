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
  PlanState,
  TokenUsage,
  ChatConfig,
  AstraClientConfig,
} from './types';

export { AstraClient } from './client';
