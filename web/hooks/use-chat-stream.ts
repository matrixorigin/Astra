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
};

/**
 * Hook that manages a streaming chat conversation via POST /chat/stream (SSE).
 * Handles text, thinking, tool calls, plan events, and token usage.
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

  // Refs for mutable state during stream processing
  const controllerRef = useRef<AbortController | null>(null);
  const assistantIdRef = useRef<string>('');
  const accumulatedTextRef = useRef('');
  const accumulatedThinkingRef = useRef('');
  const toolCallMapRef = useRef<Map<string, ToolCall>>(new Map());
  // Track config.sessionId to detect external changes
  const configSessionIdRef = useRef(config.sessionId);

  // Reset state when config.sessionId changes externally (session switch)
  useEffect(() => {
    if (configSessionIdRef.current !== config.sessionId) {
      configSessionIdRef.current = config.sessionId;
      // Abort any in-flight request
      controllerRef.current?.abort();
      controllerRef.current = null;
      // Clear all state for the new session
      setMessages([]);
      setToolCalls([]);
      setSessionId(config.sessionId ?? null);
      setRunId(null);
      setIsStreaming(false);
      setError(null);
      setPlan(null);
      setUsage(EMPTY_USAGE);
      setConnectionState('idle');
      accumulatedTextRef.current = '';
      accumulatedThinkingRef.current = '';
      toolCallMapRef.current.clear();
    }
  }, [config.sessionId]);

  // Abort in-flight request on unmount
  useEffect(() => {
    return () => {
      controllerRef.current?.abort();
      controllerRef.current = null;
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
          const id = assistantIdRef.current;
          const finalTools = Array.from(toolCallMapRef.current.values());
          setMessages((prev) =>
            prev.map((m) =>
              m.id === id
                ? {
                    ...m,
                    streaming: false,
                    toolCalls:
                      finalTools.length > 0 ? finalTools : undefined,
                  }
                : m,
            ),
          );
          setIsStreaming(false);
          setConnectionState('idle');
          break;
        }

        case 'error': {
          setError(event.message);
          setIsStreaming(false);
          setConnectionState('error');
          break;
        }

        case 'agent_delegated':
        case 'agent_progress':
        case 'agent_completed':
        case 'warning':
        case 'explain':
          break;
      }
    },
    [],
  );

  const stop = useCallback(() => {
    controllerRef.current?.abort();
    controllerRef.current = null;
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
  }, []);

  const sendMessage = useCallback(
    (content: string) => {
      if (isStreaming) return;

      setError(null);
      setIsStreaming(true);
      setConnectionState('streaming');

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

      const controller = new AbortController();
      controllerRef.current = controller;

      const body = JSON.stringify({
        message: content,
        session_id: sessionId,
        agent_id: config.agentId ?? undefined,
        model: config.model ?? undefined,
      });

      // Route through Next.js so auth stays server-side and the browser stays same-origin.
      const streamUrl = `/api/backend/chat/stream`;

      fetch(streamUrl, {
        method: 'POST',
        headers: {
          'Content-Type': 'application/json',
          Accept: 'text/event-stream',
        },
        body,
        signal: controller.signal,
      })
        .then(async (response) => {
          if (!response.ok) {
            const text = await response.text().catch(() => '');
            throw new Error(`Chat request failed: ${response.status} ${text}`);
          }

          if (!response.body) {
            throw new Error('No response body');
          }

          const reader = response.body.getReader();
          const decoder = new TextDecoder();
          let buffer = '';

          for (;;) {
            const { done, value } = await reader.read();
            if (done) break;

            buffer += decoder.decode(value, { stream: true });
            const parts = buffer.split('\n\n');
            buffer = parts.pop() ?? '';

            for (const part of parts) {
              for (const line of part.split('\n')) {
                const trimmed = line.trim();
                if (trimmed.startsWith('data: ')) {
                  try {
                    const event = JSON.parse(trimmed.slice(6)) as StreamEvent;
                    processEvent(event);
                  } catch {
                    // Non-JSON data line
                  }
                }
              }
            }
          }

          setIsStreaming(false);
          setConnectionState('idle');
        })
        .catch((err) => {
          if (err instanceof DOMException && err.name === 'AbortError') {
            // User stopped — don't show as error
            return;
          }
          setError(err instanceof Error ? err.message : 'Unknown streaming error');
          setIsStreaming(false);
          setConnectionState('error');
        });
    },
    [isStreaming, sessionId, config, processEvent],
  );

  const reset = useCallback(() => {
    controllerRef.current?.abort();
    controllerRef.current = null;
    setMessages([]);
    setToolCalls([]);
    setSessionId(config.sessionId ?? null);
    setRunId(null);
    setIsStreaming(false);
    setError(null);
    setPlan(null);
    setUsage(EMPTY_USAGE);
    setConnectionState('idle');
    accumulatedTextRef.current = '';
    accumulatedThinkingRef.current = '';
    toolCallMapRef.current.clear();
  }, [config.sessionId]);

  return {
    sessionId,
    runId,
    messages,
    toolCalls,
    isStreaming,
    error,
    plan,
    usage,
    connectionState,
    sendMessage,
    stop,
    reset,
  };
}
