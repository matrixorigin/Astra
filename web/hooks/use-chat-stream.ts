'use client';

import { useCallback, useEffect, useRef, useState } from 'react';
import type {
  ChatConfig,
  ChatMessage,
  ToolCall,
  WorkspaceState,
  PlanState,
  PlanSubtask,
  TokenUsage,
} from '@/lib/workspace/types';
import type { StreamEvent } from '@/lib/streaming/types';
import { chatRequestToWire } from '@astra/sdk';
import { SSEClient } from '@/lib/streaming/sse-client';
import { suggestFollowupPrompt } from '@/lib/workspace/followup-suggestion';
import {
  formatRunErrorBubbleText,
  streamEndedWithNoAssistantMarkdown,
} from '@/lib/workspace/format-run-error-bubble';

let nextId = 0;
function uid(): string {
  return `msg_${Date.now()}_${++nextId}`;
}

const EMPTY_USAGE: TokenUsage = {
  promptTokens: 0,
  completionTokens: 0,
  totalTokens: 0,
  cacheCreationTokens: 0,
  cacheReadTokens: 0,
};

export type ConnectionState = 'idle' | 'streaming' | 'error';

type UseChatStreamReturn = WorkspaceState & {
  sendMessage: (content: string) => void;
  stop: () => void;
  reset: () => void;
  connectionState: ConnectionState;
  dismissError: () => void;
};

/**
 * Hook that manages a streaming chat conversation via POST /chat/stream (SSE).
 * Uses @astra/sdk SSEClient for stream parsing; routes through the Next.js
 * backend proxy so cookie-based auth stays server-side.
 */
export function useChatStream(config: ChatConfig): UseChatStreamReturn {
  const [messages, setMessages] = useState<ChatMessage[]>([]);
  const [toolCalls, setToolCalls] = useState<ToolCall[]>([]);
  const [sessionId, setSessionId] = useState<string | null>(config.sessionId ?? null);
  const [runId, setRunId] = useState<string | null>(null);
  const [isStreaming, setIsStreaming] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [plan, setPlan] = useState<PlanState | null>(null);
  const [usage, setUsage] = useState<TokenUsage>(EMPTY_USAGE);
  const [connectionState, setConnectionState] = useState<ConnectionState>('idle');
  const [agentEvents, setAgentEvents] = useState<StreamEvent[]>([]);
  const [followupSuggestion, setFollowupSuggestion] = useState<string | null>(null);

  // Refs for mutable state during stream processing
  const sseClientRef = useRef<SSEClient | null>(null);
  const assistantIdRef = useRef<string>('');
  const accumulatedTextRef = useRef('');
  const accumulatedThinkingRef = useRef('');
  const toolCallMapRef = useRef<Map<string, ToolCall>>(new Map());
  const lastUserMessageRef = useRef('');
  const sawErrorEventRef = useRef(false);
  // Track config.sessionId to detect external changes
  const configSessionIdRef = useRef(config.sessionId);

  // Reset state when config.sessionId changes externally (session switch)
  useEffect(() => {
    if (configSessionIdRef.current !== config.sessionId) {
      configSessionIdRef.current = config.sessionId;
      sseClientRef.current?.close();
      sseClientRef.current = null;
      setMessages([]);
      setToolCalls([]);
      setSessionId(config.sessionId ?? null);
      setRunId(null);
      setIsStreaming(false);
      setError(null);
      setPlan(null);
      setUsage(EMPTY_USAGE);
      setConnectionState('idle');
      setAgentEvents([]);
      setFollowupSuggestion(null);
      accumulatedTextRef.current = '';
      accumulatedThinkingRef.current = '';
      lastUserMessageRef.current = '';
      sawErrorEventRef.current = false;
      toolCallMapRef.current.clear();
    }
  }, [config.sessionId]);

  // Cleanup on unmount
  useEffect(() => {
    return () => {
      sseClientRef.current?.close();
      sseClientRef.current = null;
    };
  }, []);

  const processEvent = useCallback(
    (event: StreamEvent) => {
      switch (event.type) {
        case 'session_info': {
          setSessionId(event.session_id);
          setRunId(event.run_id ?? null);
          break;
        }

        case 'text_delta': {
          accumulatedTextRef.current += event.content;
          const text = accumulatedTextRef.current;
          const id = assistantIdRef.current;
          setMessages((prev) =>
            prev.map((m) => (m.id === id ? { ...m, content: text } : m)),
          );
          break;
        }

        case 'reasoning_delta': {
          accumulatedThinkingRef.current += event.content;
          const thinking = accumulatedThinkingRef.current;
          const id = assistantIdRef.current;
          setMessages((prev) =>
            prev.map((m) =>
              m.id === id
                ? { ...m, thinking: { content: thinking, done: false } }
                : m,
            ),
          );
          break;
        }

        case 'reasoning_done': {
          const id = assistantIdRef.current;
          setMessages((prev) =>
            prev.map((m) =>
              m.id === id && m.thinking
                ? { ...m, thinking: { ...m.thinking, done: true } }
                : m,
            ),
          );
          break;
        }

        case 'tool_call_start': {
          const tc: ToolCall = {
            callId: event.call_id,
            tool: event.tool,
            arguments: event.arguments,
            status: 'running',
            startedAt: Date.now(),
          };
          toolCallMapRef.current.set(event.call_id, tc);
          setToolCalls(Array.from(toolCallMapRef.current.values()));
          break;
        }

        case 'tool_call_end': {
          const existing = toolCallMapRef.current.get(event.call_id);
          if (existing) {
            const updated = {
              ...existing,
              result: event.result,
              status: 'done' as const,
              finishedAt: Date.now(),
            };
            toolCallMapRef.current.set(event.call_id, updated);
            setToolCalls(Array.from(toolCallMapRef.current.values()));
          }
          break;
        }

        case 'usage': {
          setUsage((prev) => ({
            promptTokens: prev.promptTokens + event.prompt_tokens,
            completionTokens: prev.completionTokens + event.completion_tokens,
            totalTokens:
              prev.totalTokens + event.prompt_tokens + event.completion_tokens,
            cacheCreationTokens:
              prev.cacheCreationTokens +
              (event.cache_creation_tokens ?? event.cache_creation_input_tokens ?? 0),
            cacheReadTokens:
              prev.cacheReadTokens +
              (event.cache_read_tokens ?? event.cache_read_input_tokens ?? 0),
          }));
          break;
        }

        case 'plan_created':
        case 'plan_revised': {
          const subtasks: PlanSubtask[] = event.plan.subtasks.map((s) => ({
            id: s.id,
            title: s.title,
            status: (s.status as PlanSubtask['status']) ?? 'pending',
          }));
          setPlan({
            planId: event.plan.plan_id,
            title: event.plan.title,
            subtasks,
          });
          break;
        }

        case 'plan_step_start': {
          setPlan((prev) => {
            if (!prev) return prev;
            return {
              ...prev,
              activeStepId: event.subtask_id ?? event.step,
              subtasks: prev.subtasks.map((s) =>
                s.id === event.subtask_id || s.title === event.step
                  ? { ...s, status: 'running' as const }
                  : s,
              ),
            };
          });
          break;
        }

        case 'plan_step_done': {
          setPlan((prev) => {
            if (!prev) return prev;
            return {
              ...prev,
              activeStepId:
                prev.activeStepId === (event.subtask_id ?? event.step)
                  ? undefined
                  : prev.activeStepId,
              subtasks: prev.subtasks.map((s) =>
                s.id === event.subtask_id || s.title === event.step
                  ? { ...s, status: 'done' as const }
                  : s,
              ),
            };
          });
          break;
        }

        case 'turn_complete': {
          const hadError = sawErrorEventRef.current;
          const id = assistantIdRef.current;
          const finalTools = Array.from(toolCallMapRef.current.values());
          if (hadError) {
            setFollowupSuggestion(null);
          } else {
            setFollowupSuggestion(
              event.followup_suggestion ??
                suggestFollowupPrompt({
                  userMessage: lastUserMessageRef.current,
                  assistantMessage: accumulatedTextRef.current,
                  toolCalls: finalTools,
                }),
            );
          }
          const noAssistantText = !accumulatedTextRef.current.trim();
          setMessages((prev) =>
            prev.map((m) =>
              m.id === id
                ? {
                    ...m,
                    streaming: false,
                    content:
                      !hadError && noAssistantText && !m.content?.trim()
                        ? streamEndedWithNoAssistantMarkdown
                        : m.content,
                    toolCalls:
                      finalTools.length > 0 ? finalTools : undefined,
                  }
                : m,
            ),
          );
          setIsStreaming(false);
          if (!hadError) {
            setConnectionState('idle');
          }
          break;
        }

        case 'error': {
          sawErrorEventRef.current = true;
          const errorLine =
            event.code && event.message ? `${event.code}: ${event.message}` : event.message;
          setError(errorLine);
          setIsStreaming(false);
          setConnectionState('error');
          setFollowupSuggestion(null);
          {
            const id = assistantIdRef.current;
            if (id) {
              // Must not require m.streaming: the server may emit turn_complete before error,
              // which clears streaming and would leave the bubble empty.
              setMessages((prev) =>
                prev.map((m) =>
                  m.id === id && m.role === 'assistant'
                    ? {
                        ...m,
                        streaming: false,
                        content: formatRunErrorBubbleText(errorLine, m.content),
                      }
                    : m,
                ),
              );
            }
          }
          break;
        }

        case 'agent_delegated':
        case 'agent_spawned':
        case 'agent_progress':
        case 'agent_completed':
          setAgentEvents((prev) => [...prev, event]);
          break;
        case 'warning':
        case 'explain':
          break;
      }
    },
    [],
  );

  const stop = useCallback(() => {
    sseClientRef.current?.close();
    sseClientRef.current = null;
    // Mark the current assistant message as done
    const id = assistantIdRef.current;
    if (id) {
      const finalTools = Array.from(toolCallMapRef.current.values());
      setMessages((prev) =>
        prev.map((m) =>
          m.id === id ? { ...m, streaming: false, toolCalls: finalTools.length > 0 ? finalTools : undefined } : m,
        ),
      );
    }
    setIsStreaming(false);
    setConnectionState('idle');
    setFollowupSuggestion(null);
  }, []);

  const sendMessage = useCallback(
    (content: string) => {
      if (isStreaming) return;

      setError(null);
      setIsStreaming(true);
      setConnectionState('streaming');
      setFollowupSuggestion(null);
      lastUserMessageRef.current = content;
      sawErrorEventRef.current = false;

      const userMsg: ChatMessage = {
        id: uid(),
        role: 'user',
        content,
        timestamp: Date.now(),
      };

      const assistantMsg: ChatMessage = {
        id: uid(),
        role: 'assistant',
        content: '',
        timestamp: Date.now(),
        streaming: true,
      };
      assistantIdRef.current = assistantMsg.id;
      accumulatedTextRef.current = '';
      accumulatedThinkingRef.current = '';
      toolCallMapRef.current.clear();
      setToolCalls([]);

      setMessages((prev) => [...prev, userMsg, assistantMsg]);

      // Close any previous SSE client
      sseClientRef.current?.close();

      const client = new SSEClient({
        url: `/api/backend/chat/stream`,
        method: 'POST',
        body: JSON.stringify(
          chatRequestToWire({
            message: content,
            sessionId: sessionId ?? undefined,
            agentId: config.agentId,
            model: config.model,
            allowSkills: config.allowSkills,
            allowTools: config.allowTools,
            skillSearch: config.skillSearch,
          }),
        ),
        onEvent: processEvent,
        onStateChange: (state) => {
          if (state === 'error') {
            setConnectionState('error');
          } else if (state === 'disconnected') {
            setIsStreaming(false);
            if (!sawErrorEventRef.current) {
              setConnectionState('idle');
            }
          }
        },
        maxRetries: 0, // Chat requests should not auto-retry
      });

      sseClientRef.current = client;
      client.connect().catch((err) => {
        if (err instanceof DOMException && err.name === 'AbortError') return;
        const msg = err instanceof Error ? err.message : 'Unknown streaming error';
        setError(msg);
        setIsStreaming(false);
        setConnectionState('error');
        const id = assistantIdRef.current;
        if (id) {
          setMessages((prev) =>
            prev.map((m) =>
              m.id === id && m.role === 'assistant'
                ? {
                    ...m,
                    streaming: false,
                    content: formatRunErrorBubbleText(
                      `Request did not return a live stream. ${msg}`,
                      m.content,
                    ),
                  }
                : m,
            ),
          );
        }
      });
    },
    [isStreaming, sessionId, config, processEvent],
  );

  const reset = useCallback(() => {
    sseClientRef.current?.close();
    sseClientRef.current = null;
    setMessages([]);
    setToolCalls([]);
    setSessionId(config.sessionId ?? null);
    setRunId(null);
    setIsStreaming(false);
    setError(null);
    setPlan(null);
    setUsage(EMPTY_USAGE);
    setConnectionState('idle');
    setAgentEvents([]);
    setFollowupSuggestion(null);
    accumulatedTextRef.current = '';
    accumulatedThinkingRef.current = '';
    lastUserMessageRef.current = '';
    toolCallMapRef.current.clear();
  }, [config.sessionId]);

  const dismissError = useCallback(() => {
    setError(null);
  }, []);

  return {
    sessionId,
    runId,
    messages,
    toolCalls,
    followupSuggestion,
    isStreaming,
    error,
    plan,
    usage,
    agentEvents,
    connectionState,
    sendMessage,
    stop,
    reset,
    dismissError,
  };
}
